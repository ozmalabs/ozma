// SPDX-License-Identifier: AGPL-3.0-only
//! Certificate pinning helpers.
//!
//! These are pure functions (no `JsValue`, no `web_sys`) so the base64,
//! SHA-256, and fingerprint handling is unit-tested on the host. `client`
//! converts the string errors into typed JavaScript errors.

use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD, URL_SAFE, URL_SAFE_NO_PAD};
use base64::Engine as _;
use sha2::{Digest, Sha256};

/// Decode the node's base64 DER certificate.
///
/// Accepts standard/URL-safe alphabets with or without padding, arbitrary
/// whitespace, and the `data:` URI prefixes a proxy or browser may add.
pub fn decode_der_base64(value: &str) -> Result<Vec<u8>, String> {
    let trimmed = value.trim();
    let trimmed = trimmed
        .strip_prefix("data:application/pkix-cert;base64,")
        .or_else(|| trimmed.strip_prefix("data:application/x-x509-ca-cert;base64,"))
        .unwrap_or(trimmed);
    let compact: String = trimmed
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect();
    if compact.is_empty() {
        return Err(
            "No certificate DER was supplied; fetch GET /api/v1/webtransport (or /api/v1/transport) first"
                .to_string(),
        );
    }
    let decoded = STANDARD
        .decode(compact.as_bytes())
        .or_else(|_| STANDARD_NO_PAD.decode(compact.as_bytes()))
        .or_else(|_| URL_SAFE.decode(compact.as_bytes()))
        .or_else(|_| URL_SAFE_NO_PAD.decode(compact.as_bytes()))
        .map_err(|error| format!("The certificate is not valid base64: {error}"))?;
    if decoded.is_empty() {
        return Err("The certificate decoded to zero bytes".to_string());
    }
    Ok(decoded)
}

/// SHA-256 of DER bytes.
pub fn sha256_der(der: &[u8]) -> [u8; 32] {
    let digest = Sha256::digest(der);
    let mut hash = [0u8; 32];
    hash.copy_from_slice(&digest);
    hash
}

/// Normalise a hex fingerprint: drop the `sha256:` prefix and separators, and
/// lowercase.
pub fn normalize_fingerprint(value: &str) -> String {
    value
        .trim()
        .trim_start_matches("sha256:")
        .trim_start_matches("sha-256:")
        .chars()
        .filter(|character| character.is_ascii_hexdigit())
        .flat_map(char::to_lowercase)
        .collect()
}

/// Hex SHA-256 of a DER certificate.
pub fn fingerprint_hex(hash: &[u8; 32]) -> String {
    hash.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Decode the certificate, hash it, and check the offer's fingerprint.
///
/// A missing fingerprint is allowed: the WebTransport descriptor omits it when
/// the listener shares the raw QUIC certificate, and the hash is still used for
/// `serverCertificateHashes`.
pub fn certificate_sha256(
    certificate_der_base64: &str,
    expected_fingerprint: Option<&str>,
) -> Result<[u8; 32], String> {
    let der = decode_der_base64(certificate_der_base64)?;
    let hash = sha256_der(&der);
    if let Some(expected) = expected_fingerprint
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        let expected = normalize_fingerprint(expected);
        let actual = fingerprint_hex(&hash);
        if expected != actual {
            return Err(format!(
                "The certificate from the node does not match the offer fingerprint (expected {expected}, got {actual})"
            ));
        }
    }
    Ok(hash)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_decoding_tolerates_whitespace_and_data_uris() {
        let der = b"\x30\x82not a real certificate";
        let encoded = STANDARD.encode(der);
        let wrapped = format!("data:application/pkix-cert;base64,{}\n  ", encoded);
        assert_eq!(decode_der_base64(&wrapped).unwrap(), der);
        assert_eq!(decode_der_base64(&encoded).unwrap(), der);
    }

    #[test]
    fn empty_and_invalid_certificates_are_rejected() {
        assert!(decode_der_base64("   ").is_err());
        assert!(decode_der_base64("not base64 !!!").is_err());
    }

    #[test]
    fn fingerprints_are_normalised_and_checked() {
        let hash = sha256_der(b"fixture");
        let hex = fingerprint_hex(&hash);
        assert_eq!(hex.len(), 64);
        assert_eq!(normalize_fingerprint(&hex.to_uppercase()), hex);
        assert_eq!(normalize_fingerprint(&format!("sha-256:{hex}")), hex);
        assert_eq!(
            normalize_fingerprint(
                &hex.as_bytes()
                    .chunks(2)
                    .map(|pair| { std::str::from_utf8(pair).unwrap().to_string() })
                    .collect::<Vec<_>>()
                    .join(":")
            ),
            hex
        );
    }

    #[test]
    fn matching_and_mismatching_offer_fingerprints() {
        let der = b"node certificate fixture";
        let encoded = STANDARD.encode(der);
        let hash = sha256_der(der);
        let hex = fingerprint_hex(&hash);
        assert_eq!(certificate_sha256(&encoded, Some(&hex)).unwrap(), hash);
        assert_eq!(
            certificate_sha256(&encoded, Some(&format!("sha256:{hex}"))).unwrap(),
            hash
        );
        assert_eq!(certificate_sha256(&encoded, None).unwrap(), hash);
        let wrong = "0".repeat(64);
        assert!(certificate_sha256(&encoded, Some(&wrong)).is_err());
    }
}
