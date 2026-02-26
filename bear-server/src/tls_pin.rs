// ---------------------------------------------------------------------------
// TLS SPKI pinning — capture and enforce relay certificate identity
// ---------------------------------------------------------------------------
//
// During pairing, `capture_spki_pin` connects to the relay URL and extracts
// SHA-256(SubjectPublicKeyInfo DER) from the leaf TLS certificate.
//
// During polling, `build_pinned_client` creates a reqwest::Client whose
// underlying rustls config uses a custom ServerCertVerifier that:
//   1. Performs standard WebPKI CA chain verification
//   2. Additionally checks the leaf cert's SPKI hash against the pin
//
// Pin format: lowercase hex SHA-256 of the DER-encoded SPKI.

use std::sync::Arc;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, Error as TlsError, SignatureScheme};
use sha2::{Digest, Sha256};

/// Extract the SPKI SHA-256 pin (lowercase hex) from a relay URL.
///
/// Makes a dummy HTTPS GET to the relay and captures the leaf cert's SPKI
/// fingerprint via a one-shot custom verifier.
pub async fn capture_spki_pin(relay_url: &str) -> anyhow::Result<String> {
    let captured = Arc::new(std::sync::Mutex::new(None::<String>));
    let captured_clone = captured.clone();

    let verifier = CapturingVerifier {
        inner: default_verifier(),
        captured: captured_clone,
    };

    let tls_config = ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier))
        .with_no_client_auth();

    let client = reqwest::Client::builder()
        .use_preconfigured_tls(tls_config)
        .build()?;

    // Make a lightweight probe request
    let _ = client
        .get(relay_url)
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("TLS probe to relay failed: {e}"))?;

    let pin = captured
        .lock()
        .unwrap()
        .take()
        .ok_or_else(|| anyhow::anyhow!("failed to capture relay TLS certificate SPKI"))?;

    Ok(pin)
}

/// Build a reqwest::Client that enforces SPKI pinning against the given hex hash.
pub fn build_pinned_client(expected_pin: &str) -> anyhow::Result<reqwest::Client> {
    let verifier = PinningVerifier {
        inner: default_verifier(),
        expected_pin: expected_pin.to_string(),
    };

    let tls_config = ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier))
        .with_no_client_auth();

    reqwest::Client::builder()
        .use_preconfigured_tls(tls_config)
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(300))
        .build()
        .map_err(|e| anyhow::anyhow!("failed to build pinned HTTP client: {e}"))
}

/// Compute SHA-256 hex of the SPKI portion of a DER certificate.
fn spki_sha256_hex(cert_der: &[u8]) -> Option<String> {
    // Parse the certificate to extract SubjectPublicKeyInfo
    // X.509 structure: SEQUENCE { tbsCertificate, signatureAlgorithm, signatureValue }
    // tbsCertificate contains: version, serialNumber, signature, issuer, validity, subject, subjectPublicKeyInfo, ...
    // We use a minimal ASN.1 DER parser to locate the SPKI field.
    let spki_bytes = extract_spki_from_cert(cert_der)?;
    let hash = Sha256::digest(spki_bytes);
    Some(hash.iter().map(|b| format!("{b:02x}")).collect())
}

/// Minimal ASN.1 DER parser to extract SubjectPublicKeyInfo from an X.509 cert.
fn extract_spki_from_cert(cert_der: &[u8]) -> Option<&[u8]> {
    // Certificate ::= SEQUENCE { tbsCertificate, ... }
    let (tbs, _) = read_sequence(cert_der)?;
    // tbsCertificate ::= SEQUENCE { version[0], serialNumber, signature, issuer, validity, subject, subjectPublicKeyInfo, ... }
    let tbs_inner = read_sequence_inner(tbs)?;
    let mut pos = 0;

    // version [0] EXPLICIT — optional, tagged
    if tbs_inner.get(pos)? & 0xE0 == 0xA0 {
        let (_, next) = read_tlv(&tbs_inner[pos..])?;
        pos += next;
    }

    // serialNumber INTEGER
    let (_, next) = read_tlv(&tbs_inner[pos..])?;
    pos += next;

    // signature AlgorithmIdentifier (SEQUENCE)
    let (_, next) = read_tlv(&tbs_inner[pos..])?;
    pos += next;

    // issuer Name (SEQUENCE)
    let (_, next) = read_tlv(&tbs_inner[pos..])?;
    pos += next;

    // validity Validity (SEQUENCE)
    let (_, next) = read_tlv(&tbs_inner[pos..])?;
    pos += next;

    // subject Name (SEQUENCE)
    let (_, next) = read_tlv(&tbs_inner[pos..])?;
    pos += next;

    // subjectPublicKeyInfo SubjectPublicKeyInfo (SEQUENCE) — this is what we want
    let (_, spki_len) = read_tlv(&tbs_inner[pos..])?;
    Some(&tbs_inner[pos..pos + spki_len])
}

/// Read a DER SEQUENCE tag+length, return (inner content, total consumed bytes).
fn read_sequence(data: &[u8]) -> Option<(&[u8], usize)> {
    if data.first()? != &0x30 {
        return None;
    }
    let (content_start, content_len) = read_length(&data[1..])?;
    let total = 1 + content_start + content_len;
    Some((&data[1 + content_start..total], total))
}

/// Read the inner content of a SEQUENCE (skip tag+length, return content slice).
fn read_sequence_inner(data: &[u8]) -> Option<&[u8]> {
    read_sequence(data).map(|(inner, _)| inner)
}

/// Read any TLV: returns (value_slice, total_consumed_bytes).
fn read_tlv(data: &[u8]) -> Option<(&[u8], usize)> {
    if data.is_empty() {
        return None;
    }
    let (content_start, content_len) = read_length(&data[1..])?;
    let total = 1 + content_start + content_len;
    if total > data.len() {
        return None;
    }
    Some((&data[1 + content_start..total], total))
}

/// Parse DER length encoding. Returns (bytes consumed for length field, content length).
fn read_length(data: &[u8]) -> Option<(usize, usize)> {
    let first = *data.first()?;
    if first < 0x80 {
        Some((1, first as usize))
    } else {
        let num_bytes = (first & 0x7F) as usize;
        if num_bytes == 0 || num_bytes > 4 || data.len() < 1 + num_bytes {
            return None;
        }
        let mut len: usize = 0;
        for i in 0..num_bytes {
            len = (len << 8) | (data[1 + i] as usize);
        }
        Some((1 + num_bytes, len))
    }
}

// ---------------------------------------------------------------------------
// Custom ServerCertVerifier implementations
// ---------------------------------------------------------------------------

fn default_verifier() -> Arc<dyn ServerCertVerifier> {
    let root_store =
        rustls::RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    rustls::client::WebPkiServerVerifier::builder(Arc::new(root_store))
        .build()
        .expect("failed to build default WebPKI verifier")
}

/// One-shot verifier that captures the leaf cert's SPKI pin.
#[derive(Debug)]
struct CapturingVerifier {
    inner: Arc<dyn ServerCertVerifier>,
    captured: Arc<std::sync::Mutex<Option<String>>>,
}

impl ServerCertVerifier for CapturingVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, TlsError> {
        // First, do standard verification
        let result = self.inner.verify_server_cert(
            end_entity,
            intermediates,
            server_name,
            ocsp_response,
            now,
        )?;

        // Capture the SPKI hash
        if let Some(pin) = spki_sha256_hex(end_entity.as_ref()) {
            *self.captured.lock().unwrap() = Some(pin);
        }

        Ok(result)
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        self.inner.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        self.inner.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.inner.supported_verify_schemes()
    }
}

/// Verifier that enforces SPKI pin matching.
#[derive(Debug)]
struct PinningVerifier {
    inner: Arc<dyn ServerCertVerifier>,
    expected_pin: String,
}

impl ServerCertVerifier for PinningVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, TlsError> {
        // Standard CA verification first
        let result = self.inner.verify_server_cert(
            end_entity,
            intermediates,
            server_name,
            ocsp_response,
            now,
        )?;

        // SPKI pin check
        let actual_pin = spki_sha256_hex(end_entity.as_ref()).ok_or_else(|| {
            TlsError::General("failed to extract SPKI from server certificate".into())
        })?;

        if actual_pin != self.expected_pin {
            return Err(TlsError::General(format!(
                "TLS SPKI pin mismatch: expected={}, actual={}",
                self.expected_pin, actual_pin
            )));
        }

        Ok(result)
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        self.inner.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        self.inner.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.inner.supported_verify_schemes()
    }
}
