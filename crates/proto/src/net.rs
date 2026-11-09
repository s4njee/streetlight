//! QUIC/TLS plumbing shared by host and clients (feature `net`).
//!
//! Both sides use self-signed certificates. TLS signature checks run normally, but
//! chain-of-trust is replaced by fingerprint pinning established during PIN pairing.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use rustls::DigitallySignedStruct;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, verify_tls12_signature, verify_tls13_signature};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use serde::{Serialize, de::DeserializeOwned};

use crate::control::{ALPN, MAX_CONTROL_MSG};

pub struct Identity {
    pub cert: CertificateDer<'static>,
    pub key: PrivatePkcs8KeyDer<'static>,
}

impl Identity {
    pub fn fingerprint(&self) -> [u8; 32] {
        crate::pair::fingerprint(&self.cert)
    }

    /// Load `<dir>/<name>.{crt,key}.der`, generating a new self-signed pair if missing.
    pub fn load_or_create(dir: &Path, name: &str) -> std::io::Result<Self> {
        std::fs::create_dir_all(dir)?;
        let (cp, kp) = (dir.join(format!("{name}.crt.der")), dir.join(format!("{name}.key.der")));
        if let (Ok(c), Ok(k)) = (std::fs::read(&cp), std::fs::read(&kp)) {
            return Ok(Self { cert: CertificateDer::from(c), key: PrivatePkcs8KeyDer::from(k) });
        }
        let ck = rcgen::generate_simple_self_signed(vec!["streetlight".to_string()]).map_err(std::io::Error::other)?;
        let (c, k) = (ck.cert.der().to_vec(), ck.signing_key.serialize_der());
        std::fs::write(&cp, &c)?;
        write_private(&kp, &k)?;
        Ok(Self { cert: CertificateDer::from(c), key: PrivatePkcs8KeyDer::from(k) })
    }
}

fn write_private(path: &Path, data: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(path)?;
        f.write_all(data)
    }
    #[cfg(not(unix))]
    std::fs::write(path, data)
}

fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

#[derive(Debug)]
struct AnyCert(Arc<CryptoProvider>);

impl ServerCertVerifier for AnyCert {
    fn verify_server_cert(&self, _: &CertificateDer<'_>, _: &[CertificateDer<'_>], _: &ServerName<'_>, _: &[u8], _: UnixTime) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(&self, m: &[u8], c: &CertificateDer<'_>, d: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(m, c, d, &self.0.signature_verification_algorithms)
    }
    fn verify_tls13_signature(&self, m: &[u8], c: &CertificateDer<'_>, d: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(m, c, d, &self.0.signature_verification_algorithms)
    }
    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

impl ClientCertVerifier for AnyCert {
    fn root_hint_subjects(&self) -> &[rustls::DistinguishedName] {
        &[]
    }
    fn verify_client_cert(&self, _: &CertificateDer<'_>, _: &[CertificateDer<'_>], _: UnixTime) -> Result<ClientCertVerified, rustls::Error> {
        Ok(ClientCertVerified::assertion())
    }
    fn verify_tls12_signature(&self, m: &[u8], c: &CertificateDer<'_>, d: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(m, c, d, &self.0.signature_verification_algorithms)
    }
    fn verify_tls13_signature(&self, m: &[u8], c: &CertificateDer<'_>, d: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(m, c, d, &self.0.signature_verification_algorithms)
    }
    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

fn transport() -> Arc<quinn::TransportConfig> {
    let mut t = quinn::TransportConfig::default();
    t.keep_alive_interval(Some(Duration::from_millis(500)));
    t.max_idle_timeout(Some(Duration::from_secs(6).try_into().unwrap()));
    t.datagram_receive_buffer_size(Some(1 << 20));
    Arc::new(t)
}

pub fn server_config(id: &Identity) -> anyhow::Result<quinn::ServerConfig> {
    let p = provider();
    let mut tls = rustls::ServerConfig::builder_with_provider(p.clone())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_client_cert_verifier(Arc::new(AnyCert(p)))
        .with_single_cert(vec![id.cert.clone()], PrivateKeyDer::Pkcs8(id.key.clone_key()))?;
    tls.alpn_protocols = vec![ALPN.to_vec()];
    let mut cfg = quinn::ServerConfig::with_crypto(Arc::new(QuicServerConfig::try_from(tls)?));
    cfg.transport_config(transport());
    Ok(cfg)
}

pub fn client_config(id: &Identity) -> anyhow::Result<quinn::ClientConfig> {
    let p = provider();
    let mut tls = rustls::ClientConfig::builder_with_provider(p.clone())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AnyCert(p)))
        .with_client_auth_cert(vec![id.cert.clone()], PrivateKeyDer::Pkcs8(id.key.clone_key()))?;
    tls.alpn_protocols = vec![ALPN.to_vec()];
    let mut cfg = quinn::ClientConfig::new(Arc::new(QuicClientConfig::try_from(tls)?));
    cfg.transport_config(transport());
    Ok(cfg)
}

/// SHA-256 of the peer's leaf certificate.
pub fn peer_fingerprint(conn: &quinn::Connection) -> Option<[u8; 32]> {
    let any = conn.peer_identity()?;
    let certs = any.downcast::<Vec<CertificateDer<'static>>>().ok()?;
    certs.first().map(|c| crate::pair::fingerprint(c))
}

pub async fn send_msg<T: Serialize>(s: &mut quinn::SendStream, msg: &T) -> anyhow::Result<()> {
    s.write_all(&crate::control::encode(msg)).await?;
    Ok(())
}

pub async fn recv_msg<T: DeserializeOwned>(r: &mut quinn::RecvStream) -> anyhow::Result<Option<T>> {
    let mut len = [0u8; 4];
    match r.read_exact(&mut len).await {
        Ok(()) => {}
        Err(quinn::ReadExactError::FinishedEarly(0)) => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let n = u32::from_le_bytes(len) as usize;
    anyhow::ensure!(n <= MAX_CONTROL_MSG, "control message too large ({n})");
    let mut buf = vec![0u8; n];
    r.read_exact(&mut buf).await?;
    Ok(Some(postcard::from_bytes(&buf)?))
}
