// SPDX-License-Identifier: AGPL-3.0-only
//! Certificate loading and fingerprint helpers for QUIC pinning.
//!
//! The node uses a self-signed certificate. A client therefore needs the DER
//! bytes, not a system CA bundle. DER can come from `--cert` (DER or PEM), from
//! the node's `/api/v1/transport` response (base64 DER), or out of band.
//! Fingerprints are SHA-256 hex and are compared after normalising common
//! separators and the `sha256:` prefix.

use std::path::Path;

use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD, URL_SAFE, URL_SAFE_NO_PAD};
use base64::Engine as _;
use ozma_protocol::ApiError;
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::error;

const PEM_BEGIN: &str = "-----BEGIN CERTIFICATE-----";
const PEM_END: &str = "-----END CERTIFICATE-----";

/// Load a certificate from a file containing either DER or one or more PEM
/// `CERTIFICATE` blocks. The first certificate is returned.
pub fn load_certificate(path: impl AsRef<Path>) -> Result<Vec<u8>, ApiError> {
    let path = path.as_ref();
    let bytes = std::fs::read(path).map_err(|error| {
        error::certificate(format!(
            "Could not read certificate '{}': {error}",
            path.display()
        ))
        .with_remediation("Pass --cert with a readable DER or PEM certificate file.")
    })?;
    parse_certificate(&bytes)
        .map_err(|error| error.with_details(json!({ "path": path.display().to_string() })))
}

/// Parse DER or PEM certificate bytes. DER is returned unchanged; PEM blocks
/// are base64-decoded and the first certificate is returned.
pub fn parse_certificate(bytes: &[u8]) -> Result<Vec<u8>, ApiError> {
    if bytes.is_empty() {
        return Err(error::certificate("The certificate file is empty"));
    }

    let text = String::from_utf8_lossy(bytes);
    if text.contains(PEM_BEGIN) {
        return decode_first_pem(&text);
    }

    // A DER X.509 certificate starts with a SEQUENCE tag (0x30). Reject
    // obvious text so a wrong file produces a clear error rather than a
    // confusing rustls failure.
    if bytes[0] != 0x30 {
        return Err(error::certificate(
            "The certificate is neither DER nor a PEM CERTIFICATE block",
        )
        .with_remediation("Export the node certificate as DER or PEM and retry."));
    }
    Ok(bytes.to_vec())
}

fn decode_first_pem(text: &str) -> Result<Vec<u8>, ApiError> {
    let start = text
        .find(PEM_BEGIN)
        .ok_or_else(|| error::certificate("No PEM CERTIFICATE block was found"))?;
    let after_begin = &text[start + PEM_BEGIN.len()..];
    let end = after_begin.find(PEM_END).ok_or_else(|| {
        error::certificate("The PEM certificate block has no matching END marker")
    })?;
    let payload: String = after_begin[..end]
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect();
    if payload.is_empty() {
        return Err(error::certificate("The PEM certificate block is empty"));
    }
    STANDARD.decode(payload.as_bytes()).map_err(|error| {
        error::certificate(format!(
            "The PEM certificate block is not valid base64: {error}"
        ))
    })
}

/// Decode the base64 DER certificate returned by the node's
/// `GET /api/v1/transport` endpoint.
pub fn certificate_der_from_base64(value: &str) -> Result<Vec<u8>, ApiError> {
    let trimmed = value.trim();
    let trimmed = trimmed
        .strip_prefix("data:application/pkix-cert;base64,")
        .or_else(|| trimmed.strip_prefix("data:application/x-x509-ca-cert;base64,"))
        .unwrap_or(trimmed);
    if trimmed.is_empty() {
        return Err(error::certificate(
            "The node transport response contains an empty certificate",
        ));
    }
    let compact: String = trimmed
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect();

    let decoded = STANDARD
        .decode(compact.as_bytes())
        .or_else(|_| STANDARD_NO_PAD.decode(compact.as_bytes()))
        .or_else(|_| URL_SAFE.decode(compact.as_bytes()))
        .or_else(|_| URL_SAFE_NO_PAD.decode(compact.as_bytes()))
        .map_err(|error| {
            error::certificate(format!("The node certificate is not valid base64: {error}"))
        })?;
    if decoded.is_empty() {
        return Err(error::certificate("The node returned an empty certificate"));
    }
    Ok(decoded)
}

/// Hex-encoded SHA-256 fingerprint of a DER certificate.
pub fn certificate_fingerprint(certificate_der: &[u8]) -> String {
    Sha256::digest(certificate_der)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Compare a supplied fingerprint with the DER bytes, accepting common
/// formatting (`sha256:`, uppercase, `:` or `-` separators). An empty expected
/// fingerprint is not checked.
pub fn verify_certificate_fingerprint(
    certificate_der: &[u8],
    expected: &str,
) -> Result<(), ApiError> {
    let expected = expected.trim();
    if expected.is_empty() {
        return Ok(());
    }
    let actual = certificate_fingerprint(certificate_der);
    if normalize_fingerprint(expected) != actual {
        return Err(error::certificate(
            "The certificate does not match the expected SHA-256 fingerprint",
        )
        .with_details(json!({
            "expected": expected,
            "actual": actual,
        }))
        .with_remediation(
            "Use the node's current certificate (--cert, or fetch /api/v1/transport with --node).",
        ));
    }
    Ok(())
}

/// Normalise a SHA-256 fingerprint for comparison.
pub fn normalize_fingerprint(value: &str) -> String {
    let value = value.trim().to_ascii_lowercase();
    let value = value.strip_prefix("sha256:").unwrap_or(&value);
    value
        .chars()
        .filter(|character| character.is_ascii_hexdigit())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose::STANDARD;
    use ozma_transport::SelfSignedCertificate;

    fn der() -> Vec<u8> {
        SelfSignedCertificate::generate(&["localhost".to_string()])
            .expect("generate certificate")
            .certificate_der()
            .to_vec()
    }

    #[test]
    fn der_and_pem_both_parse() {
        let certificate = der();
        assert_eq!(parse_certificate(&certificate).unwrap(), certificate);

        let pem = format!(
            "{PEM_BEGIN}\n{}\n{PEM_END}\n",
            STANDARD.encode(&certificate)
        );
        assert_eq!(parse_certificate(pem.as_bytes()).unwrap(), certificate);
    }

    #[test]
    fn base64_node_certificate_decodes() {
        let certificate = der();
        let encoded = STANDARD.encode(&certificate);
        assert_eq!(certificate_der_from_base64(&encoded).unwrap(), certificate);
    }

    #[test]
    fn fingerprints_accept_common_formatting() {
        let certificate = der();
        let fingerprint = certificate_fingerprint(&certificate);
        assert_eq!(fingerprint.len(), 64);
        let formatted = format!(
            "SHA256:{}",
            fingerprint
                .as_bytes()
                .chunks(2)
                .map(|chunk| std::str::from_utf8(chunk).unwrap().to_ascii_uppercase())
                .collect::<Vec<_>>()
                .join(":")
        );
        verify_certificate_fingerprint(&certificate, &formatted).unwrap();
    }

    #[test]
    fn mismatched_fingerprint_is_a_typed_error() {
        let error = verify_certificate_fingerprint(&der(), &"00".repeat(32)).unwrap_err();
        assert_eq!(error.code, crate::error::codes::CLIENT_CERTIFICATE);
    }

    #[test]
    fn garbage_is_rejected() {
        assert!(parse_certificate(b"not a certificate").is_err());
        assert!(certificate_der_from_base64("!!!!").is_err());
    }
}
