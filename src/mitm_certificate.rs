use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime};

use chrono::{Months, Utc};
use rand::rngs::OsRng;
use rsa::pkcs1::{DecodeRsaPrivateKey, EncodeRsaPrivateKey};
use rsa::pkcs1v15::{Signature, SigningKey, VerifyingKey};
use rsa::pkcs8::DecodePublicKey;
use rsa::signature::{Keypair, Verifier};
use rsa::traits::PublicKeyParts;
use rsa::{RsaPrivateKey, RsaPublicKey};
use sha2::Sha256;
use tempfile::NamedTempFile;
use x509_cert::Certificate;
use x509_cert::builder::{Builder, CertificateBuilder, Profile};
use x509_cert::der::pem::LineEnding;
use x509_cert::der::{DecodePem, Encode, EncodePem};
use x509_cert::ext::pkix::{BasicConstraints, KeyUsage};
use x509_cert::name::Name;
use x509_cert::serial_number::SerialNumber;
use x509_cert::spki::SubjectPublicKeyInfoOwned;
use x509_cert::time::{Time, Validity};

const SUBJECT: &str = "CN=9Router MITM Root CA,O=9Router,C=US";
const CA_KEY: &str = "rootCA.key";
pub(super) const CA_CERT: &str = "rootCA.crt";
const ROTATION_LEAD: Duration = Duration::from_secs(30 * 24 * 60 * 60);

pub(super) fn ensure(dir: &Path) -> io::Result<PathBuf> {
    directory(dir)?;
    let cert_path = dir.join(CA_CERT);
    let key_path = dir.join(CA_KEY);
    let now = SystemTime::now();
    let usable = now.checked_add(ROTATION_LEAD).and_then(|threshold| {
        let certificate = owned_root(dir, now).ok()?;
        Some(SystemTime::from(certificate.tbs_certificate.validity.not_after) >= threshold)
    });
    if usable == Some(true) {
        return Ok(cert_path);
    }

    let (certificate, key) = generate(now).ok_or_else(invalid)?;
    let key = key.to_pkcs1_pem(LineEnding::LF).map_err(|_| invalid())?;
    let staged_key = stage(dir, key.as_bytes(), false)?;
    let staged_cert = stage(dir, certificate.as_bytes(), true)?;
    let previous_key = match read_regular(&key_path) {
        Ok(previous) => Some(stage(dir, &previous, false)?),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };
    if fs::symlink_metadata(&cert_path).is_ok_and(|meta| !meta.is_file()) {
        return Err(invalid());
    }
    persist(staged_key, &key_path)?;
    if let Err(error) = persist(staged_cert, &cert_path) {
        restore(previous_key, &key_path)?;
        return Err(error);
    }
    Ok(cert_path)
}

fn generate(now: SystemTime) -> Option<(String, RsaPrivateKey)> {
    let expiry = chrono::DateTime::<Utc>::from(now).checked_add_months(Months::new(120))?;
    let validity = Validity {
        not_before: Time::try_from(now).ok()?,
        not_after: Time::try_from(SystemTime::from(expiry)).ok()?,
    };
    let key = RsaPrivateKey::new(&mut OsRng, 2048).ok()?;
    let signer = SigningKey::<Sha256>::new(key.clone());
    let public = SubjectPublicKeyInfoOwned::from_key(signer.verifying_key()).ok()?;
    let subject: Name = SUBJECT.parse().ok()?;
    let certificate = CertificateBuilder::new(
        Profile::Root,
        SerialNumber::from(1_u32),
        validity,
        subject,
        public,
        &signer,
    )
    .ok()?
    .build::<Signature>()
    .ok()?;
    Some((certificate.to_pem(LineEnding::LF).ok()?, key))
}

pub(super) fn owned_root(dir: &Path, now: SystemTime) -> io::Result<Certificate> {
    root_material(dir, now).map(|(certificate, _)| certificate)
}

pub(crate) fn root_material(
    dir: &Path,
    now: SystemTime,
) -> io::Result<(Certificate, RsaPrivateKey)> {
    if fs::symlink_metadata(dir)?.file_type().is_symlink() {
        return Err(invalid());
    }
    let pem = read_regular(&dir.join(CA_CERT))?;
    let certificate = Certificate::from_pem(&pem).map_err(|_| invalid())?;
    let fields = &certificate.tbs_certificate;
    let subject = fields.subject.to_string();
    let mut attributes: Vec<_> = subject.split(',').collect();
    attributes.sort_unstable();
    if fields.subject != fields.issuer
        || attributes != ["C=US", "CN=9Router MITM Root CA", "O=9Router"]
        || SystemTime::from(fields.validity.not_before) > now
        || SystemTime::from(fields.validity.not_after) <= now
        || !fields.get::<BasicConstraints>().is_ok_and(|value| {
            value.is_some_and(|(critical, constraint)| critical && constraint.ca)
        })
        || !fields.get::<KeyUsage>().is_ok_and(|value| {
            value.is_some_and(|(critical, usage)| {
                critical && usage.key_cert_sign() && usage.crl_sign()
            })
        })
    {
        return Err(invalid());
    }
    let pem = read_regular(&dir.join(CA_KEY))?;
    let key = RsaPrivateKey::from_pkcs1_pem(std::str::from_utf8(&pem).map_err(|_| invalid())?)
        .map_err(|_| invalid())?;
    let encoded = fields
        .subject_public_key_info
        .to_der()
        .map_err(|_| invalid())?;
    let public = RsaPublicKey::from_public_key_der(&encoded).map_err(|_| invalid())?;
    let verifier = VerifyingKey::<Sha256>::new(public.clone());
    let signature = certificate
        .signature
        .as_bytes()
        .and_then(|bytes| Signature::try_from(bytes).ok())
        .ok_or_else(invalid)?;
    let payload = fields.to_der().map_err(|_| invalid())?;
    if public.n().bits() < 2048
        || key.to_public_key() != public
        || verifier.verify(&payload, &signature).is_err()
    {
        return Err(invalid());
    }
    Ok((certificate, key))
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(super) fn owned_by_controller(dir: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    let controller = rustix::process::geteuid().as_raw();
    let owned = |path: &Path| {
        fs::symlink_metadata(path).is_ok_and(|meta| {
            !meta.is_symlink() && meta.uid() == controller && meta.mode() & 0o022 == 0
        })
    };
    dir.parent().is_some_and(owned)
        && owned(dir)
        && owned(&dir.join(CA_CERT))
        && owned(&dir.join(CA_KEY))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub(super) fn owned_by_controller(dir: &Path) -> bool {
    fs::symlink_metadata(dir).is_ok_and(|meta| meta.is_dir())
}

pub(super) fn install(dir: &Path, root: &Certificate, fixture: bool) -> io::Result<()> {
    let pem = root.to_pem(LineEnding::LF).map_err(|_| invalid())?;
    if fixture {
        let directory = dir.join("trust");
        self::directory(&directory)?;
        return install_anchor(&directory, pem.as_bytes(), || Ok(()));
    }
    install_system(dir, pem.as_bytes())
}

#[cfg(target_os = "linux")]
struct LinuxAnchor {
    path: &'static str,
    refresh: &'static [&'static str],
}

#[cfg(target_os = "linux")]
const LINUX_ANCHORS: &[LinuxAnchor] = &[
    LinuxAnchor {
        path: "/usr/local/share/ca-certificates",
        refresh: &[
            "/usr/sbin/update-ca-certificates",
            "/usr/bin/update-ca-certificates",
        ],
    },
    LinuxAnchor {
        path: "/etc/ca-certificates/trust-source/anchors",
        refresh: &["/usr/bin/update-ca-trust", "/usr/sbin/update-ca-trust"],
    },
    LinuxAnchor {
        path: "/etc/pki/ca-trust/source/anchors",
        refresh: &["/usr/bin/update-ca-trust", "/usr/sbin/update-ca-trust"],
    },
    LinuxAnchor {
        path: "/etc/pki/trust/anchors",
        refresh: &[
            "/usr/sbin/update-ca-certificates",
            "/usr/bin/update-ca-certificates",
        ],
    },
];

#[cfg(target_os = "linux")]
fn linux_anchor() -> Option<&'static LinuxAnchor> {
    LINUX_ANCHORS
        .iter()
        .find(|anchor| Path::new(anchor.path).is_dir())
}

#[cfg(target_os = "linux")]
pub(super) fn linux_trusted(dir: &Path, cert: &Path, fixture: bool) -> bool {
    let anchor = if fixture {
        Some(dir.join("trust"))
    } else {
        linux_anchor().map(|anchor| PathBuf::from(anchor.path))
    };
    let installed =
        anchor.and_then(|anchor| read_regular(&anchor.join("9router-root-ca.crt")).ok());
    let local = read_regular(cert).ok();
    match (local, installed) {
        (Some(local), Some(installed)) => Certificate::from_pem(&local)
            .ok()
            .zip(Certificate::from_pem(&installed).ok())
            .is_some_and(|(local, installed)| local == installed),
        _ => false,
    }
}

#[cfg(target_os = "linux")]
fn install_system(_: &Path, pem: &[u8]) -> io::Result<()> {
    let anchor = linux_anchor().ok_or_else(unavailable)?;
    let program = anchor
        .refresh
        .iter()
        .copied()
        .find(|path| Path::new(path).is_file())
        .ok_or_else(unavailable)?;
    let refresh = || run(Command::new(program).env("PATH", "/usr/sbin:/usr/bin:/sbin:/bin"));
    install_anchor(Path::new(anchor.path), pem, refresh)
}

pub(super) fn install_anchor(
    dir: &Path,
    pem: &[u8],
    refresh: impl Fn() -> io::Result<()>,
) -> io::Result<()> {
    let target = dir.join("9router-root-ca.crt");
    let previous = match read_regular(&target) {
        Ok(previous) => Some(stage(dir, &previous, true)?),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };
    persist(stage(dir, pem, true)?, &target)?;
    if let Err(error) = refresh() {
        restore(previous, &target)?;
        let _ = refresh();
        return Err(error);
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn install_system(dir: &Path, pem: &[u8]) -> io::Result<()> {
    let staged = stage(dir, pem, true)?;
    run(Command::new("/usr/bin/security")
        .args([
            "add-trusted-cert",
            "-d",
            "-r",
            "trustRoot",
            "-k",
            "/Library/Keychains/System.keychain",
        ])
        .arg(staged.path()))
}

#[cfg(windows)]
fn install_system(dir: &Path, pem: &[u8]) -> io::Result<()> {
    let staged = stage(dir, pem, true)?;
    let program =
        PathBuf::from(std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into()))
            .join("System32/certutil.exe");
    run(Command::new(program)
        .args(["-addstore", "Root"])
        .arg(staged.path()))
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn install_system(_: &Path, _: &[u8]) -> io::Result<()> {
    Err(unavailable())
}

fn run(command: &mut Command) -> io::Result<()> {
    if command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?
        .success()
    {
        Ok(())
    } else {
        Err(unavailable())
    }
}

fn unavailable() -> io::Error {
    io::Error::other("System root CA installation failed")
}

pub(super) fn read_regular(path: &Path) -> io::Result<Vec<u8>> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() || metadata.len() > 64 * 1024 {
        return Err(invalid());
    }
    fs::read(path)
}

pub(super) fn stage(dir: &Path, bytes: &[u8], public: bool) -> io::Result<NamedTempFile> {
    let mut file = NamedTempFile::new_in(dir)?;
    #[cfg(unix)]
    if public {
        use std::os::unix::fs::PermissionsExt;
        file.as_file()
            .set_permissions(fs::Permissions::from_mode(0o644))?;
    }
    #[cfg(not(unix))]
    let _ = public;
    file.write_all(bytes)?;
    file.as_file().sync_all()?;
    Ok(file)
}

pub(super) fn persist(file: NamedTempFile, path: &Path) -> io::Result<()> {
    file.persist(path).map_err(|error| error.error)?;
    Ok(())
}

pub(super) fn restore(file: Option<NamedTempFile>, path: &Path) -> io::Result<()> {
    match file {
        Some(file) => persist(file, path),
        None => fs::remove_file(path),
    }
}

fn directory(dir: &Path) -> io::Result<()> {
    fs::create_dir_all(dir)?;
    if !fs::symlink_metadata(dir)?.is_dir() {
        return Err(invalid());
    }
    Ok(())
}

fn invalid() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "Invalid application root CA")
}
