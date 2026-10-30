//! The certificate authority used to intercept HTTPS traffic.
//!
//! The CA is created on first use and persisted. Per-host certificates are
//! issued on demand and cached both in memory and on disk.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use rcgen::{
    BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
    KeyUsagePurpose,
};
use rustls::ServerConfig;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};

use crate::{Error, write_private};

const CA_NAME: &str = "rflow CA";
const HOST_CERT_DAYS: i64 = 365;
/// Cached host certificates are reissued well before they expire.
const HOST_CERT_MAX_AGE: Duration = Duration::from_secs(300 * 24 * 60 * 60);

pub struct CertificateAuthority {
    issuer: Issuer<'static, KeyPair>,
    cert_pem: String,
    certs_dir: PathBuf,
    configs: Mutex<HashMap<String, Arc<ServerConfig>>>,
}

impl CertificateAuthority {
    /// Loads the CA from `dir`, creating and saving a new one if none exists.
    pub fn load_or_create(dir: &Path) -> Result<Self, Error> {
        let certs_dir = dir.join("certs");
        fs::create_dir_all(&certs_dir)?;

        let (cert_path, key_path) = (dir.join("ca.crt"), dir.join("ca.key"));
        let (key, cert_pem) = if key_path.exists() && cert_path.exists() {
            let key = KeyPair::from_pem(&fs::read_to_string(&key_path)?)?;
            (key, fs::read_to_string(&cert_path)?)
        } else {
            let key = KeyPair::generate()?;
            let cert_pem = ca_params().self_signed(&key)?.pem();
            write_private(&key_path, key.serialize_pem())?;
            fs::write(&cert_path, &cert_pem)?;
            (key, cert_pem)
        };

        // The issuer only needs the CA's name and key, both of which are
        // stable, so it can be rebuilt from the params rather than parsed.
        Ok(Self {
            issuer: Issuer::new(ca_params(), key),
            cert_pem,
            certs_dir,
            configs: Mutex::default(),
        })
    }

    /// The CA certificate in PEM format, for adding to trust stores.
    pub fn cert_pem(&self) -> &str {
        &self.cert_pem
    }

    /// Returns a TLS server config presenting a certificate for `host`.
    pub fn server_config(&self, host: &str) -> Result<Arc<ServerConfig>, Error> {
        if let Some(config) = self.configs.lock().unwrap().get(host) {
            return Ok(config.clone());
        }

        let pem = self.host_cert_pem(host)?;
        let certs = CertificateDer::pem_slice_iter(pem.as_bytes()).collect::<Result<Vec<_>, _>>()?;
        let key = PrivateKeyDer::from_pem_slice(pem.as_bytes())?;
        let mut config = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(certs, key)?;
        config.alpn_protocols = vec![b"http/1.1".to_vec()];

        let config = Arc::new(config);
        self.configs.lock().unwrap().insert(host.to_string(), config.clone());
        Ok(config)
    }

    /// Returns the certificate and key for `host` as PEM, from the disk cache if fresh.
    fn host_cert_pem(&self, host: &str) -> Result<String, Error> {
        let file_name: String = host
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '-' { c } else { '_' })
            .collect();
        let path = self.certs_dir.join(format!("{file_name}.pem"));

        let fresh = fs::metadata(&path)
            .and_then(|m| m.modified())
            .is_ok_and(|t| SystemTime::now().duration_since(t).unwrap_or_default() < HOST_CERT_MAX_AGE);
        if fresh {
            return Ok(fs::read_to_string(&path)?);
        }

        let mut params = CertificateParams::new(vec![host.to_string()])?;
        params.distinguished_name.push(DnType::CommonName, host);
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        let now = time::OffsetDateTime::now_utc();
        params.not_before = now - time::Duration::days(1);
        params.not_after = now + time::Duration::days(HOST_CERT_DAYS);

        let key = KeyPair::generate()?;
        let cert = params.signed_by(&key, &self.issuer)?;
        let pem = cert.pem() + &key.serialize_pem();
        write_private(&path, &pem)?;
        Ok(pem)
    }
}

fn ca_params() -> CertificateParams {
    let mut params = CertificateParams::default();
    params.distinguished_name.push(DnType::CommonName, CA_NAME);
    params.distinguished_name.push(DnType::OrganizationName, CA_NAME);
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    params
}
