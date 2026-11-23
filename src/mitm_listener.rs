use std::io;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use hyper::service::service_fn;
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto::Builder;
use rand::rngs::OsRng;
use rsa::RsaPrivateKey;
use rsa::pkcs1::EncodeRsaPrivateKey;
use rsa::pkcs1v15::{Signature, SigningKey};
use rsa::signature::Keypair;
use rustls::crypto::ring;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs1KeyDer};
use rustls::server::ResolvesServerCertUsingSni;
use rustls::sign::CertifiedKey;
use sha2::Sha256;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;
use tokio::task::JoinSet;
use tokio_rustls::TlsAcceptor;
use x509_cert::builder::{Builder as _, CertificateBuilder, Profile};
use x509_cert::der::Encode;
use x509_cert::der::asn1::Ia5String;
use x509_cert::ext::pkix::{SubjectAltName, name::GeneralName};
use x509_cert::time::{Time, Validity};

use crate::cli_tools::mitm::{ListenerConfig, ListenerTask, certificate};

#[path = "mitm_routing.rs"]
mod routing;

const MAX_CONNECTIONS: usize = 128;
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

pub(crate) fn configure(
    listener: TcpListener,
    config: ListenerConfig,
    shutdown: oneshot::Receiver<()>,
) -> io::Result<ListenerTask> {
    if !listener.local_addr()?.ip().is_loopback() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "MITM listener requires a loopback address",
        ));
    }
    let tls = tls_config(&config.data_dir)?;
    let state = Arc::new(routing::State::new(config)?);
    Ok(Box::pin(serve(listener, tls, state, shutdown)))
}

async fn serve(
    listener: TcpListener,
    tls: TlsAcceptor,
    state: Arc<routing::State>,
    mut shutdown: oneshot::Receiver<()>,
) -> io::Result<()> {
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            _ = &mut shutdown => break,
            _ = connections.join_next(), if !connections.is_empty() => (),
            accepted = listener.accept(), if connections.len() < MAX_CONNECTIONS => {
                let (socket, peer) = accepted?;
                if peer.ip().is_loopback() {
                    connections.spawn(serve_connection(socket, tls.clone(), state.clone()));
                }
            }
        }
    }
    connections.shutdown().await;
    Ok(())
}

async fn serve_connection(socket: TcpStream, tls: TlsAcceptor, state: Arc<routing::State>) {
    let Ok(Ok(socket)) = tokio::time::timeout(HANDSHAKE_TIMEOUT, tls.accept(socket)).await else {
        return;
    };
    let Some(server_name) = socket.get_ref().1.server_name().and_then(routing::host) else {
        return;
    };
    let service = service_fn(move |request| {
        let state = state.clone();
        async move { Ok::<_, std::convert::Infallible>(state.handle(server_name, request).await) }
    });
    let _ = Builder::new(TokioExecutor::new())
        .serve_connection(TokioIo::new(socket), service)
        .await;
}

fn tls_config(directory: &Path) -> io::Result<TlsAcceptor> {
    let now = SystemTime::now();
    let (root, signer) = certificate::root_material(directory, now)?;
    let signer = SigningKey::<Sha256>::new(signer);
    let provider = Arc::new(ring::default_provider());
    let key = RsaPrivateKey::new(&mut OsRng, 2048).map_err(invalid)?;
    let private = key.to_pkcs1_der().map_err(invalid)?;
    let private = PrivateKeyDer::Pkcs1(PrivatePkcs1KeyDer::from(private.as_bytes().to_vec()));
    let public = x509_cert::spki::SubjectPublicKeyInfoOwned::from_key(
        SigningKey::<Sha256>::new(key).verifying_key(),
    )
    .map_err(invalid)?;
    let now = Time::try_from(now).map_err(invalid)?;
    let validity = Validity {
        not_before: now,
        not_after: root.tbs_certificate.validity.not_after,
    };
    let root_der = CertificateDer::from(root.to_der().map_err(invalid)?);
    let mut resolver = ResolvesServerCertUsingSni::new();
    for host in routing::hosts() {
        let profile = Profile::Leaf {
            issuer: root.tbs_certificate.subject.clone(),
            enable_key_agreement: false,
            enable_key_encipherment: true,
        };
        let mut serial = [0; 16];
        getrandom::fill(&mut serial).map_err(invalid)?;
        let mut builder = CertificateBuilder::new(
            profile,
            x509_cert::serial_number::SerialNumber::new(&serial).map_err(invalid)?,
            validity,
            format!("CN={host}").parse().map_err(invalid)?,
            public.clone(),
            &signer,
        )
        .map_err(invalid)?;
        builder
            .add_extension(&SubjectAltName(vec![GeneralName::DnsName(
                Ia5String::new(host).map_err(invalid)?,
            )]))
            .map_err(invalid)?;
        let leaf = builder.build::<Signature>().map_err(invalid)?;
        let leaf = CertificateDer::from(leaf.to_der().map_err(invalid)?);
        let key =
            CertifiedKey::from_der(vec![leaf, root_der.clone()], private.clone_key(), &provider)
                .map_err(invalid)?;
        resolver.add(host, key).map_err(invalid)?;
    }
    let mut server = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(invalid)?
        .with_no_client_auth()
        .with_cert_resolver(Arc::new(resolver));
    server.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(TlsAcceptor::from(Arc::new(server)))
}

fn invalid(error: impl std::fmt::Display) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("Could not initialize MITM TLS: {error}"),
    )
}
