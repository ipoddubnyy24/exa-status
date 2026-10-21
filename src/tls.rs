//! TLS: the dashboard's own self-signed certificate, and a client config that
//! trusts exactly EXA Reporter's self-signed certificate and nothing else.

use anyhow::{Context, Result, anyhow};
use chrono::{Datelike, Days, NaiveDate, Utc};
use rcgen::{CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, IsCa, KeyPair};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{WebPkiSupportedAlgorithms, verify_tls12_signature, verify_tls13_signature};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, SignatureScheme};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tracing::{info, warn};
use x509_parser::certificate::X509Certificate;

use crate::store;

/// Certificate and key file names in the certificate directory.
const CERT_FILE: &str = "cert.pem";
const KEY_FILE: &str = "key.pem";
/// Names the certificate is valid for.
const SUBJECT_NAMES: [&str; 2] = ["localhost", "127.0.0.1"];
/// Apple platforms reject TLS server certificates valid for more than 825 days.
const VALIDITY_DAYS: u64 = 820;
/// A certificate is replaced this many days before it expires.
const RENEW_BEFORE_DAYS: i64 = 30;

/// Where the dashboard's certificate and key live.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct CertPaths {
    /// PEM certificate.
    pub(crate) cert: PathBuf,
    /// PEM private key.
    pub(crate) key: PathBuf,
}

/// Makes ring the process-wide crypto provider (the server side needs one).
pub(crate) fn install_crypto_provider() -> Result<()> {
    rustls::crypto::ring::default_provider()
        .install_default()
        .map_err(|_| anyhow!("a TLS crypto provider was already installed"))
}

fn paths(dir: &Path) -> CertPaths {
    CertPaths {
        cert: dir.join(CERT_FILE),
        key: dir.join(KEY_FILE),
    }
}

/// The certificate in `dir`, created when missing, unreadable or within a
/// month of expiring. `utc_today` is today's date in UTC. A new certificate
/// must be trusted again in the keychain; deleting the directory also makes one.
pub(crate) fn ensure_server_cert(dir: &Path, utc_today: NaiveDate) -> Result<CertPaths> {
    Ok(renew_if_due(dir, utc_today)?.unwrap_or_else(|| paths(dir)))
}

/// A new certificate in `dir` when the current one is due, or when the key
/// is not the certificate's (a stop between writing the two leaves such a
/// pair, which the server cannot load); `None` when neither.
pub(crate) fn renew_if_due(dir: &Path, utc_today: NaiveDate) -> Result<Option<CertPaths>> {
    let paths = paths(dir);
    if !due(&paths.cert, Utc::now().timestamp()) && pair_matches(&paths) {
        return Ok(None);
    }
    let (cert, key) = self_signed(utc_today)?;
    store::write_atomic(&paths.key, key.as_bytes())?;
    store::write_atomic(&paths.cert, cert.as_bytes())?;
    info!(
        "created a new certificate in {}; trust it again if the old one was trusted",
        dir.display()
    );
    Ok(Some(paths))
}

/// The certificate is missing, unreadable, or expires within `RENEW_BEFORE_DAYS`
/// of `now` (seconds since the epoch). Its own expiry date counts, not the
/// file's age, so a copied file keeps its real end date.
fn due(cert: &Path, now: i64) -> bool {
    match not_after(cert) {
        Ok(expires) => expires - now < RENEW_BEFORE_DAYS * 24 * 60 * 60,
        Err(e) => {
            warn!("{e:#}; making a new certificate");
            true
        }
    }
}

/// `read` applied to the certificate in `path`.
fn with_cert<T>(path: &Path, read: impl FnOnce(&X509Certificate<'_>) -> T) -> Result<T> {
    let pem = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let (_, pem) = x509_parser::pem::parse_x509_pem(&pem)
        .map_err(|e| anyhow!("{} is not a PEM file: {e}", path.display()))?;
    let cert = pem
        .parse_x509()
        .map_err(|e| anyhow!("{} is not a certificate: {e}", path.display()))?;
    Ok(read(&cert))
}

/// The expiry of the certificate in `path`, in seconds since the epoch.
fn not_after(path: &Path) -> Result<i64> {
    with_cert(path, |cert| cert.validity().not_after.timestamp())
}

/// The key file holds the private key of the certificate's public key.
fn pair_matches(paths: &CertPaths) -> bool {
    let check = || -> Result<bool> {
        let cert_key = with_cert(&paths.cert, |cert| {
            cert.public_key().subject_public_key.data.to_vec()
        })?;
        let pem = std::fs::read_to_string(&paths.key)
            .with_context(|| format!("reading {}", paths.key.display()))?;
        let key = KeyPair::from_pem(&pem)
            .with_context(|| format!("{} is not a private key", paths.key.display()))?;
        Ok(key.public_key_raw() == cert_key.as_slice())
    };
    match check() {
        Ok(true) => true,
        Ok(false) => {
            warn!("the key does not belong to the certificate; making a new pair");
            false
        }
        Err(e) => {
            warn!("{e:#}; making a new certificate");
            false
        }
    }
}

/// A new self-signed server certificate and its key, both PEM, with what
/// Apple requires of a trusted TLS server certificate: names, serverAuth and
/// at most 825 days. It has no key-usage extension: one without keyCertSign
/// stops OpenSSL clients from trusting the certificate as its own anchor.
fn self_signed(utc_today: NaiveDate) -> Result<(String, String)> {
    let mut params = CertificateParams::new(SUBJECT_NAMES.map(String::from).to_vec())
        .context("building the certificate parameters")?;
    let mut name = DistinguishedName::new();
    name.push(DnType::CommonName, "EXA status (localhost)");
    params.distinguished_name = name;
    params.is_ca = IsCa::ExplicitNoCa;
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    // From yesterday, so a clock a few hours behind UTC still accepts it.
    let from = utc_today
        .checked_sub_days(Days::new(1))
        .ok_or_else(|| anyhow!("certificate start date out of range"))?;
    let until = from
        .checked_add_days(Days::new(VALIDITY_DAYS))
        .ok_or_else(|| anyhow!("certificate end date out of range"))?;
    let (year, month, day) = ymd(from)?;
    params.not_before = rcgen::date_time_ymd(year, month, day);
    let (year, month, day) = ymd(until)?;
    params.not_after = rcgen::date_time_ymd(year, month, day);
    let key = KeyPair::generate().context("generating the certificate key")?;
    let cert = params
        .self_signed(&key)
        .context("signing the certificate")?;
    Ok((cert.pem(), key.serialize_pem()))
}

/// Year, month and day in the integer widths rcgen takes.
fn ymd(d: NaiveDate) -> Result<(i32, u8, u8)> {
    Ok((d.year(), u8::try_from(d.month())?, u8::try_from(d.day())?))
}

/// A client config that accepts only the server certificate in `cert_pem`.
/// Host name and chain checks are replaced by the exact-certificate match;
/// handshake signatures are still verified.
pub(crate) fn pinned_client_config(cert_pem: &Path) -> Result<Arc<ClientConfig>> {
    let pinned = CertificateDer::from_pem_file(cert_pem)
        .map_err(|e| anyhow!("reading the pinned certificate {}: {e}", cert_pem.display()))?;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let verifier = PinnedCert {
        pinned,
        algorithms: provider.signature_verification_algorithms,
    };
    let config = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .context("selecting TLS versions")?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier))
        .with_no_client_auth();
    Ok(Arc::new(config))
}

/// Accepts one exact certificate.
#[derive(Debug)]
struct PinnedCert {
    pinned: CertificateDer<'static>,
    algorithms: WebPkiSupportedAlgorithms,
}

impl ServerCertVerifier for PinnedCert {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        // The trait hands over chain, name, OCSP and time; with an exact pin
        // none of them can change the answer.
        if end_entity.as_ref() == self.pinned.as_ref() {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General(
                "the server certificate is not the pinned one".into(),
            ))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(message, cert, dss, &self.algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(message, cert, dss, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("exa-status-tls-{name}-{}", std::process::id()));
        if dir.exists() {
            std::fs::remove_dir_all(&dir).unwrap();
        }
        dir
    }

    fn today() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 10, 21).unwrap()
    }

    #[test]
    fn a_certificate_is_renewed_a_month_before_it_expires() {
        let dir = temp_dir("renew");
        let paths = ensure_server_cert(&dir, today()).unwrap();
        let expires = not_after(&paths.cert).unwrap();
        let day = 24 * 60 * 60;

        assert!(!due(&paths.cert, expires - (RENEW_BEFORE_DAYS + 1) * day));
        assert!(due(&paths.cert, expires - (RENEW_BEFORE_DAYS - 1) * day));
        assert!(due(&dir.join("missing.pem"), 0));
        std::fs::write(&paths.cert, "not a certificate").unwrap();
        assert!(due(&paths.cert, 0), "an unreadable certificate is replaced");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn creates_a_key_only_the_owner_can_read_and_reuses_it() {
        let dir = temp_dir("create");

        let first = ensure_server_cert(&dir, Utc::now().date_naive()).unwrap();
        let cert = std::fs::read(&first.cert).unwrap();
        let second = ensure_server_cert(&dir, Utc::now().date_naive()).unwrap();

        assert_eq!(first, second);
        assert_eq!(
            std::fs::read(&second.cert).unwrap(),
            cert,
            "an existing certificate is kept"
        );
        assert_eq!(
            std::fs::metadata(&first.key).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(
            String::from_utf8(cert)
                .unwrap()
                .starts_with("-----BEGIN CERTIFICATE-----")
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_key_that_is_not_the_certificates_gets_a_new_pair() {
        let dir = temp_dir("mismatch");
        let paths = ensure_server_cert(&dir, today()).unwrap();
        assert!(pair_matches(&paths));
        let (_, other_key) = self_signed(today()).unwrap();
        std::fs::write(&paths.key, other_key).unwrap();

        assert!(!pair_matches(&paths));
        assert!(renew_if_due(&dir, today()).unwrap().is_some());
        assert!(pair_matches(&paths));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_certificate_carries_what_apple_requires() {
        let (pem, _) = self_signed(today()).unwrap();
        let (_, pem) = x509_parser::pem::parse_x509_pem(pem.as_bytes()).unwrap();
        let cert = pem.parse_x509().unwrap();

        let eku = cert.extended_key_usage().unwrap().unwrap().value;
        let ca = cert.basic_constraints().unwrap().unwrap().value.ca;
        let days = (cert.validity().not_after.timestamp() - cert.validity().not_before.timestamp())
            / (24 * 60 * 60);
        let names: Vec<String> = cert
            .subject_alternative_name()
            .unwrap()
            .unwrap()
            .value
            .general_names
            .iter()
            .map(ToString::to_string)
            .collect();

        assert!(eku.server_auth);
        assert!(!ca);
        assert!(cert.key_usage().unwrap().is_none());
        assert_eq!(days, i64::try_from(VALIDITY_DAYS).unwrap());
        assert!(days <= 825);
        assert_eq!(
            cert.validity().not_before.timestamp(),
            NaiveDate::from_ymd_opt(2026, 10, 20)
                .unwrap()
                .and_hms_opt(0, 0, 0)
                .unwrap()
                .and_utc()
                .timestamp()
        );
        assert_eq!(names, ["DNSName(localhost)", "IPAddress(7f:00:00:01)"]);
    }

    #[test]
    fn the_pinned_verifier_accepts_only_its_certificate() {
        let (cert_a, _) = self_signed(today()).unwrap();
        let (cert_b, _) = self_signed(today()).unwrap();
        let dir = temp_dir("pin");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.pem");
        std::fs::write(&path, &cert_a).unwrap();
        let der = |pem: &str| CertificateDer::from_pem_slice(pem.as_bytes()).unwrap();
        let verifier = PinnedCert {
            pinned: CertificateDer::from_pem_file(&path).unwrap(),
            algorithms: rustls::crypto::ring::default_provider().signature_verification_algorithms,
        };
        let name = ServerName::try_from("localhost").unwrap();

        assert!(
            verifier
                .verify_server_cert(&der(&cert_a), &[], &name, &[], UnixTime::now())
                .is_ok()
        );
        assert_eq!(
            verifier
                .verify_server_cert(&der(&cert_b), &[], &name, &[], UnixTime::now())
                .unwrap_err()
                .to_string(),
            "unexpected error: the server certificate is not the pinned one"
        );
        assert!(pinned_client_config(&path).is_ok());
        let missing = pinned_client_config(&dir.join("missing.pem"))
            .unwrap_err()
            .to_string();
        assert!(
            missing.starts_with("reading the pinned certificate")
                && missing.contains("missing.pem"),
            "{missing}"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
