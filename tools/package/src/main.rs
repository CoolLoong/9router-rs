use std::collections::{BTreeMap, HashSet};
use std::fs::{self, File};
use std::io::{self, Cursor, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use flate2::{Compression, GzBuilder};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use zip::write::SimpleFileOptions;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
type Members = BTreeMap<String, Member>;

const PACKAGER: &str = env!("CARGO_MANIFEST_DIR");
const USAGE: &str = "Usage: ninerouter-package [--target TRIPLE] [--target-dir DIR] [--output DIR]";

enum Member {
    File(PathBuf, u32),
    Json(Vec<u8>),
}

impl Member {
    fn open(&self) -> io::Result<(Box<dyn Read + '_>, u64, u32)> {
        match self {
            Self::File(path, mode) => {
                let file = File::open(path)?;
                let size = file.metadata()?.len();
                Ok((Box::new(file), size, *mode))
            }
            Self::Json(bytes) => Ok((Box::new(Cursor::new(bytes)), bytes.len() as u64, 0o644)),
        }
    }
}

fn main() -> ExitCode {
    match package() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

fn package() -> Result<()> {
    let packager = Path::new(PACKAGER);
    let root = packager.join("../..").canonicalize()?;
    let mut output = root.join("dist");
    let mut target_dir = root.join("target");
    let mut target = None;
    let mut args = std::env::args_os().skip(1);
    while let Some(option) = args.next() {
        match option.to_str() {
            Some("--help" | "-h") => {
                println!("{USAGE}");
                return Ok(());
            }
            Some("--target") => {
                target = Some(
                    args.next()
                        .and_then(|value| value.into_string().ok())
                        .ok_or(USAGE)?,
                );
            }
            Some("--output") => output = args.next().ok_or(USAGE)?.into(),
            Some("--target-dir") => target_dir = args.next().ok_or(USAGE)?.into(),
            _ => return Err(USAGE.into()),
        }
    }
    let target = match target {
        Some(target) => target,
        None => capture(Command::new("rustc").arg("-vV"))?
            .lines()
            .find_map(|line| line.strip_prefix("host: "))
            .ok_or("Rust compiler did not report its host target")?
            .to_owned(),
    };
    if target.is_empty()
        || !target
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err("Pass a Rust target triple".into());
    }
    let config = capture(Command::new("rustc").args(["--print", "cfg", "--target", &target]))?;
    let has = |key| config.lines().any(|line| line == key);
    let windows = has("target_os=\"windows\"");
    let linux = has("target_os=\"linux\"");
    if !windows && !linux && !has("target_os=\"macos\"") {
        return Err("Native packages support Linux, macOS and Windows targets".into());
    }

    let manifest = root.join("Cargo.toml");
    let mut cargo = Command::new("cargo");
    cargo.args([
        "metadata",
        "--format-version",
        "1",
        "--filter-platform",
        &target,
    ]);
    cargo
        .args(["--locked", "--offline", "--manifest-path"])
        .arg(&manifest);
    let metadata: Value = serde_json::from_str(&capture(&mut cargo)?)?;
    let main_id = metadata["resolve"]["root"]
        .as_str()
        .ok_or("Missing Cargo root package")?;
    let packages = metadata["packages"]
        .as_array()
        .ok_or("Missing Cargo packages")?;
    let main = packages
        .iter()
        .find(|package| package["id"] == main_id)
        .ok_or("Missing root package metadata")?;
    let version = main["version"].as_str().ok_or("Missing package version")?;
    let folder = format!("9router-{version}-native-preview-{target}");
    let archive_path = output.join(format!(
        "{folder}.{}",
        if windows { "zip" } else { "tar.gz" }
    ));
    if archive_path.exists() {
        return Err(format!("Archive already exists: {}", archive_path.display()).into());
    }

    let mut members = dependency_notices(packager, &metadata, main_id)?;
    for entry in fs::read_dir(root.join("src/pxpipe_assets"))? {
        let path = entry?.path();
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default();
        if name.contains("LICENSE") && name.ends_with(".txt") {
            add_file(
                &mut members,
                format!("licenses/embedded/{name}"),
                path,
                0o644,
            )?;
        }
    }
    if linux && has("target_env=\"musl\"") && has("target_feature=\"crt-static\"") {
        let sysroot = capture(Command::new("rustc").args(["--print", "sysroot"]))?;
        let notice = Path::new(sysroot.trim()).join("share/doc/rust/COPYRIGHT-library.html");
        add_file(
            &mut members,
            "licenses/toolchain/COPYRIGHT-library.html".into(),
            notice,
            0o644,
        )?;
    }

    let status = Command::new("cargo")
        .args([
            "build",
            "--locked",
            "--offline",
            "--release",
            "--bin",
            "ninerouter",
            "--target",
            &target,
        ])
        .arg("--manifest-path")
        .arg(&manifest)
        .arg("--target-dir")
        .arg(&target_dir)
        .status()?;
    if !status.success() {
        return Err(format!("Cargo build exited with {status}").into());
    }
    let binary = target_dir.join(&target).join("release").join(if windows {
        "ninerouter.exe"
    } else {
        "ninerouter"
    });
    let executable = if windows { "9router.exe" } else { "9router" };
    let mut digest = Sha256::new();
    io::copy(&mut File::open(&binary)?, &mut digest)?;
    add_file(&mut members, executable.into(), binary, 0o755)?;
    add_file(
        &mut members,
        "README.txt".into(),
        packager.join("README.txt"),
        0o644,
    )?;
    add_file(&mut members, "LICENSE".into(), root.join("LICENSE"), 0o644)?;
    add_file(
        &mut members,
        "licenses/CLI-LICENSE".into(),
        root.join("licenses/CLI-LICENSE"),
        0o644,
    )?;
    add_file(
        &mut members,
        "licenses/supplements.json".into(),
        packager.join("licenses/supplements.json"),
        0o644,
    )?;
    add_json(
        &mut members,
        "manifest.json",
        &json!({
            "name": "9router", "version": version, "target": target,
            "kind": "native-preview", "executable": executable,
            "sha256": format!("{:x}", digest.finalize()), "legacyRuntimeIncluded": false,
        }),
    )?;

    fs::create_dir_all(&output)?;
    let mut archive = tempfile::NamedTempFile::new_in(&output)?;
    write_archive(archive.as_file_mut(), &members, &folder, windows)?;
    archive.as_file().sync_all()?;
    archive.persist_noclobber(&archive_path)?;
    println!("{}", archive_path.display());
    Ok(())
}

fn capture(command: &mut Command) -> Result<String> {
    let output = command.output()?;
    if !output.status.success() {
        io::stderr().write_all(&output.stderr)?;
        return Err(format!("{:?} exited with {}", command.get_program(), output.status).into());
    }
    Ok(String::from_utf8(output.stdout)?)
}

fn dependency_notices(packager: &Path, metadata: &Value, main: &str) -> Result<Members> {
    let packages: BTreeMap<_, _> = metadata["packages"]
        .as_array()
        .ok_or("Missing Cargo packages")?
        .iter()
        .filter_map(|package| Some((package["id"].as_str()?, package)))
        .collect();
    let nodes: BTreeMap<_, _> = metadata["resolve"]["nodes"]
        .as_array()
        .ok_or("Missing Cargo resolution")?
        .iter()
        .filter_map(|node| Some((node["id"].as_str()?, node)))
        .collect();
    let rules: Vec<Value> = serde_json::from_str(include_str!("../licenses/supplements.json"))?;
    let mut visited = HashSet::new();
    let mut pending = vec![main];
    let mut members = Members::new();
    let mut notices = BTreeMap::new();
    while let Some(id) = pending.pop() {
        if !visited.insert(id) {
            continue;
        }
        let node = nodes.get(id).ok_or("Missing resolved node")?;
        let dependencies = node["deps"]
            .as_array()
            .ok_or("Missing resolved dependencies")?;
        pending.extend(dependencies.iter().filter_map(|dep| {
            dep["dep_kinds"]
                .as_array()?
                .iter()
                .any(|kind| kind["kind"] != "dev")
                .then_some(dep["pkg"].as_str()?)
        }));
        let package = packages.get(id).ok_or("Missing resolved package")?;
        if package["source"].is_null() {
            continue;
        }
        let name = package["name"].as_str().ok_or("Missing crate name")?;
        let version = package["version"].as_str().ok_or("Missing crate version")?;
        let source = Path::new(
            package["manifest_path"]
                .as_str()
                .ok_or("Missing crate path")?,
        )
        .parent()
        .ok_or("Missing crate directory")?;
        let licenses = crate_licenses(packager, source, package, &rules, name, version)?;
        let mut files = Vec::new();
        for (file, path) in licenses {
            let member = format!("licenses/crates/{name}-{version}/{file}");
            add_file(&mut members, member.clone(), path, 0o644)?;
            files.push(member);
        }
        notices.insert(
            (name, version),
            json!({
                "name": name, "version": version, "files": files,
                "license": package["license"], "repository": package["repository"],
            }),
        );
    }
    add_json(
        &mut members,
        "licenses/dependencies.json",
        &notices.into_values().collect::<Vec<_>>(),
    )?;
    Ok(members)
}

fn crate_licenses(
    packager: &Path,
    source: &Path,
    package: &Value,
    rules: &[Value],
    name: &str,
    version: &str,
) -> Result<BTreeMap<String, PathBuf>> {
    let mut files = BTreeMap::new();
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let file = entry.file_name().to_string_lossy().into_owned();
        let lower = file.to_ascii_lowercase();
        if entry.file_type()?.is_file() && is_notice_name(&lower) {
            files.insert(file, entry.path());
        } else if matches!(lower.as_str(), "license" | "licenses") && entry.file_type()?.is_dir() {
            license_directory(source, &entry.path(), &mut files)?;
        }
    }
    if let Some(file) = package["license_file"].as_str() {
        let path = source.join(file);
        if !path.is_file() {
            return Err(format!("Missing declared license for {name} {version}").into());
        }
        let name = path
            .file_name()
            .ok_or("Invalid license path")?
            .to_string_lossy()
            .into_owned();
        files.entry(name).or_insert(path);
    }
    if !files.is_empty() {
        return Ok(files);
    }
    for rule in rules {
        let name_rule = rule["name"].as_str().ok_or("Invalid supplement name")?;
        let family = rule["family"] == true
            && name
                .strip_prefix(name_rule)
                .is_some_and(|suffix| suffix.starts_with('-'));
        if rule["version"] != version || (name_rule != name && !family) {
            continue;
        }
        if rule
            .get("repository")
            .is_some_and(|repo| repo != &package["repository"])
        {
            continue;
        }
        if let Some(revision) = rule["revision"].as_str() {
            let vcs: Value =
                serde_json::from_slice(&fs::read(source.join(".cargo_vcs_info.json"))?)?;
            if vcs["git"]["sha1"] != revision {
                continue;
            }
        }
        let file = rule["file"].as_str().ok_or("Invalid supplement path")?;
        let path = packager.join("licenses").join(file);
        let index_name = path
            .file_name()
            .ok_or("Invalid supplement path")?
            .to_string_lossy()
            .into_owned();
        files.insert(index_name, path);
        for related in rule["include"].as_array().into_iter().flatten() {
            let file = related.as_str().ok_or("Invalid related license path")?;
            files.insert(file.into(), source.join(file));
        }
        return Ok(files);
    }
    Err(format!("Missing license text for {name} {version}").into())
}

fn is_notice_name(name: &str) -> bool {
    const NAMES: &[&str] = &[
        "license",
        "licence",
        "notice",
        "copyright",
        "copying",
        "unlicense",
        "third-party-notices",
        "third_party_notices",
        "thirdpartynotices",
    ];
    NAMES.iter().any(|prefix| {
        name.strip_prefix(prefix)
            .is_some_and(|suffix| suffix.is_empty() || suffix.starts_with(['-', '_', '.', ' ']))
    })
}

fn license_directory(
    base: &Path,
    folder: &Path,
    files: &mut BTreeMap<String, PathBuf>,
) -> Result<()> {
    for entry in fs::read_dir(folder)? {
        let entry = entry?;
        let path = entry.path();
        let kind = entry.file_type()?;
        if kind.is_dir() {
            license_directory(base, &path, files)?;
        } else if kind.is_file() {
            let name = path
                .strip_prefix(base)?
                .to_string_lossy()
                .replace('\\', "/");
            files.insert(name, path);
        }
    }
    Ok(())
}

fn add_file(members: &mut Members, name: String, path: PathBuf, mode: u32) -> Result<()> {
    if !path.is_file() {
        return Err(format!("Missing package file: {}", path.display()).into());
    }
    if members
        .insert(name.clone(), Member::File(path, mode))
        .is_some()
    {
        return Err(format!("Duplicate package file: {name}").into());
    }
    Ok(())
}

fn add_json(members: &mut Members, name: &str, value: &impl serde::Serialize) -> Result<()> {
    let mut bytes = serde_json::to_vec_pretty(value)?;
    bytes.push(b'\n');
    if members
        .insert(name.to_owned(), Member::Json(bytes))
        .is_some()
    {
        return Err(format!("Duplicate package file: {name}").into());
    }
    Ok(())
}

fn write_archive(output: &mut File, members: &Members, folder: &str, windows: bool) -> Result<()> {
    if windows {
        let mut archive = zip::ZipWriter::new(output);
        for (name, member) in members {
            let (mut reader, _, mode) = member.open()?;
            let options = SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated)
                .compression_level(Some(6))
                .unix_permissions(mode);
            archive.start_file(format!("{folder}/{name}"), options)?;
            io::copy(&mut reader, &mut archive)?;
        }
        archive.finish()?;
    } else {
        let encoder = GzBuilder::new()
            .mtime(0)
            .write(output, Compression::default());
        let mut archive = tar::Builder::new(encoder);
        for (name, member) in members {
            let (reader, size, mode) = member.open()?;
            let mut header = tar::Header::new_gnu();
            header.set_size(size);
            header.set_mode(mode);
            header.set_mtime(0);
            archive.append_data(&mut header, format!("{folder}/{name}"), reader)?;
        }
        archive.into_inner()?.finish()?;
    }
    Ok(())
}
