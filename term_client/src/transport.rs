//! Transport for address-only frontend connections.
use std::{
    io::{self, Write},
    path::PathBuf,
    sync::{Arc, Mutex, OnceLock},
    time::Duration,
};

use rustls::{
    DigitallySignedStruct, SignatureScheme,
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    pki_types::{CertificateDer, ServerName, UnixTime},
};
use sha2::{Digest, Sha256};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::TcpStream,
};

pub(crate) trait ProtocolStream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> ProtocolStream for T {}
pub(crate) type BoxStream = Box<dyn ProtocolStream>;

type Fingerprint = [u8; 32];

pub(crate) struct ExternalEndpoint {
    pub addr: String,
    pin_path: Option<PathBuf>,
    fingerprint: Mutex<Option<Fingerprint>>,
    pub token: Mutex<Option<String>>,
}

static EXTERNAL_ENDPOINT: OnceLock<ExternalEndpoint> = OnceLock::new();

pub fn daemon_trust_dir() -> Option<PathBuf> {
    dirs::data_local_dir().map(|dir| dir.join("easyterm").join("known-daemons"))
}

/// Allow this frontend process to use the network listener at its selected address.
pub fn configure_external_endpoint(addr: String, trust_dir: Option<PathBuf>) -> Result<(), String> {
    if addr.trim().is_empty() {
        return Err("daemon address must contain a host and port".into());
    }
    let pin_path = trust_dir.map(|dir| {
        let id: String = Sha256::digest(addr.to_ascii_lowercase().as_bytes())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        dir.join(id)
    });
    EXTERNAL_ENDPOINT
        .set(ExternalEndpoint {
            addr,
            pin_path,
            fingerprint: Mutex::new(None),
            token: Mutex::new(None),
        })
        .map_err(|_| "a daemon address is already configured".into())
}

pub(crate) fn external_endpoint() -> Option<&'static ExternalEndpoint> {
    EXTERNAL_ENDPOINT.get()
}

fn changed_certificate() -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        "The daemon's TLS certificate has changed",
    )
}

impl ExternalEndpoint {
    fn read_pin(&self) -> io::Result<Option<Fingerprint>> {
        let Some(path) = &self.pin_path else {
            return Ok(None);
        };
        match std::fs::read(path) {
            Ok(bytes) => bytes.try_into().map(Some).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Invalid saved daemon certificate fingerprint",
                )
            }),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn remember(&self, fingerprint: Fingerprint) -> io::Result<()> {
        let mut known = self.fingerprint.lock().unwrap();
        if let Some(expected) = *known {
            return if expected == fingerprint {
                Ok(())
            } else {
                Err(changed_certificate())
            };
        }
        if let Some(path) = &self.pin_path {
            let dir = path.parent().expect("certificate pin has a directory");
            std::fs::create_dir_all(dir)?;
            let mut pending = tempfile::NamedTempFile::new_in(dir)?;
            pending.write_all(&fingerprint)?;
            match pending.persist_noclobber(path) {
                Ok(_) => {}
                Err(error) if error.error.kind() == io::ErrorKind::AlreadyExists => {
                    if self.read_pin()? != Some(fingerprint) {
                        return Err(changed_certificate());
                    }
                }
                Err(error) => return Err(error.error),
            }
        }
        *known = Some(fingerprint);
        Ok(())
    }
}

/// Trust the first certificate and require the same identity on later connections.
#[derive(Debug)]
struct SelfSignedDaemon {
    algorithms: rustls::crypto::WebPkiSupportedAlgorithms,
    expected: Option<Fingerprint>,
    presented: Arc<OnceLock<Fingerprint>>,
}

impl ServerCertVerifier for SelfSignedDaemon {
    fn verify_server_cert(
        &self,
        cert: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let fingerprint = Sha256::digest(cert.as_ref()).into();
        if self
            .expected
            .is_some_and(|expected| expected != fingerprint)
        {
            return Err(rustls::Error::General(changed_certificate().to_string()));
        }
        self.presented.get_or_init(|| fingerprint);
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        sig: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, sig, &self.algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        sig: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, sig, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}

pub(crate) async fn tls(
    sock: TcpStream,
    endpoint: Option<&ExternalEndpoint>,
) -> io::Result<BoxStream> {
    let expected = if let Some(endpoint) = endpoint {
        let cached = *endpoint.fingerprint.lock().unwrap();
        if cached.is_some() {
            cached
        } else {
            endpoint.read_pin()?
        }
    } else {
        None
    };
    let presented = Arc::new(OnceLock::new());
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let verifier = Arc::new(SelfSignedDaemon {
        algorithms: provider.signature_verification_algorithms,
        expected,
        presented: presented.clone(),
    });
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(io::Error::other)?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();
    let name = ServerName::try_from("easyterm-daemon").map_err(io::Error::other)?;
    let connected = tokio::time::timeout(
        Duration::from_secs(10),
        tokio_rustls::TlsConnector::from(Arc::new(config)).connect(name, sock),
    )
    .await
    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "daemon TLS handshake timed out"))??;
    if let Some(endpoint) = endpoint {
        let fingerprint = presented.get().copied().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "The daemon did not present a TLS certificate",
            )
        })?;
        endpoint.remember(fingerprint)?;
    }
    Ok(Box::new(connected))
}
