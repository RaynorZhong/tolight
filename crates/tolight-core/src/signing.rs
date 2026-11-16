//! Canonical authority and seven-line Ed25519 request signatures.
use crate::models::ApiError;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use sha2::{Digest, Sha256};
use std::net::{Ipv4Addr, Ipv6Addr};

pub fn b64_encode(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}
pub fn b64_decode<const N: usize>(text: &str) -> Result<[u8; N], ApiError> {
    let bytes = URL_SAFE_NO_PAD
        .decode(text)
        .map_err(|_| ApiError::new("unauthorized"))?;
    if bytes.len() != N || b64_encode(&bytes) != text {
        return Err(ApiError::new("unauthorized"));
    }
    bytes.try_into().map_err(|_| ApiError::new("unauthorized"))
}
pub fn decode_public_key(text: &str) -> Result<[u8; 32], ApiError> {
    let bytes = b64_decode(text)?;
    let key = VerifyingKey::from_bytes(&bytes).map_err(|_| ApiError::new("unauthorized"))?;
    if key.is_weak() {
        return Err(ApiError::new("unauthorized"));
    }
    Ok(bytes)
}
pub fn public_key_from_seed(seed: &[u8; 32]) -> String {
    b64_encode(&SigningKey::from_bytes(seed).verifying_key().to_bytes())
}
pub fn parse_timestamp(value: &str) -> Result<u64, ApiError> {
    if value.is_empty()
        || value.len() > 12
        || !value.bytes().all(|c| c.is_ascii_digit())
        || (value.len() > 1 && value.starts_with('0'))
    {
        return Err(ApiError::new("unauthorized"));
    }
    value.parse().map_err(|_| ApiError::new("unauthorized"))
}
pub fn validate_headers(key: &str, timestamp: &str, signature: &str) -> Result<(), ApiError> {
    decode_public_key(key)?;
    parse_timestamp(timestamp)?;
    b64_decode::<64>(signature)?;
    Ok(())
}
pub fn canonical_authority(authority: &str, scheme: &str) -> Result<String, ApiError> {
    let fail = || ApiError::new("misdirected_request");
    let default = match scheme {
        "http" => 80,
        "https" => 443,
        _ => return Err(fail()),
    };
    if authority.is_empty()
        || !authority.is_ascii()
        || authority
            .bytes()
            .any(|c| c.is_ascii_whitespace() || c.is_ascii_control() || b"@/?#%\\".contains(&c))
    {
        return Err(fail());
    }
    let (host, port) = if authority.starts_with('[') {
        let close = authority.find(']').ok_or_else(fail)?;
        let ip: Ipv6Addr = authority[1..close].parse().map_err(|_| fail())?;
        let host = format!("[{}]", ip.segments().map(|v| format!("{v:04x}")).join(":"));
        let rest = &authority[close + 1..];
        if !rest.is_empty() && !rest.starts_with(':') {
            return Err(fail());
        }
        (
            host,
            if rest.is_empty() {
                None
            } else {
                Some(&rest[1..])
            },
        )
    } else {
        if authority.matches(':').count() > 1 || authority.contains(['[', ']']) {
            return Err(fail());
        }
        let (host, port) = authority
            .split_once(':')
            .map_or((authority, None), |(a, b)| (a, Some(b)));
        if host.is_empty()
            || host.len() > 254
            || !host
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'.')
        {
            return Err(fail());
        }
        let numeric_ipv4 = host.split('.').count() == 4
            && host
                .split('.')
                .all(|part| !part.is_empty() && part.bytes().all(|c| c.is_ascii_digit()));
        let normalized = if numeric_ipv4 {
            host.parse::<Ipv4Addr>().map_err(|_| fail())?.to_string()
        } else {
            if host.trim_end_matches('.').split('.').any(|part| {
                part.is_empty() || part.len() > 63 || part.starts_with('-') || part.ends_with('-')
            }) {
                return Err(fail());
            }
            host.to_ascii_lowercase()
        };
        (normalized, port)
    };
    let port = port
        .map(|value| {
            if value.is_empty() || !value.bytes().all(|c| c.is_ascii_digit()) {
                return Err(fail());
            }
            // Parse via u64 so canonical ports may have harmless leading zeroes.
            let value = value.parse::<u64>().map_err(|_| fail())?;
            if !(1..=65535).contains(&value) {
                return Err(fail());
            }
            Ok(value)
        })
        .transpose()?;
    Ok(match port {
        Some(value) if value != default => format!("{host}:{value}"),
        _ => host,
    })
}
#[allow(clippy::too_many_arguments)]
pub fn string_to_sign(
    method: &str,
    authority: &str,
    scheme: &str,
    path: &str,
    query: &str,
    timestamp: &str,
    body: &[u8],
) -> Result<String, ApiError> {
    parse_timestamp(timestamp)?;
    if method.is_empty()
        || !method.bytes().all(|c| c.is_ascii_uppercase())
        || !path.starts_with('/')
        || !path.is_ascii()
        || !query.is_ascii()
        || path.contains(['\n', '\r', '?', '#'])
        || query.contains(['\n', '\r', '#'])
    {
        return Err(ApiError::new("unauthorized"));
    }
    let host = canonical_authority(authority, scheme)?;
    Ok(format!(
        "tolight-participant-v1\n{method}\n{host}\n{path}\n{query}\n{timestamp}\n{:x}",
        Sha256::digest(body)
    ))
}
#[derive(Clone, Debug)]
pub struct SignedHeaders {
    pub key: String,
    pub timestamp: String,
    pub signature: String,
}
#[allow(clippy::too_many_arguments)]
pub fn sign_request(
    seed: &[u8; 32],
    method: &str,
    authority: &str,
    scheme: &str,
    path: &str,
    query: &str,
    timestamp: &str,
    body: &[u8],
) -> Result<SignedHeaders, ApiError> {
    let text = string_to_sign(method, authority, scheme, path, query, timestamp, body)?;
    let key = SigningKey::from_bytes(seed);
    Ok(SignedHeaders {
        key: b64_encode(&key.verifying_key().to_bytes()),
        timestamp: timestamp.into(),
        signature: b64_encode(&key.sign(text.as_bytes()).to_bytes()),
    })
}
#[allow(clippy::too_many_arguments)]
pub fn verify_request(
    key: &str,
    timestamp: &str,
    signature: &str,
    method: &str,
    authority: &str,
    scheme: &str,
    path: &str,
    query: &str,
    body: &[u8],
) -> Result<(), ApiError> {
    validate_headers(key, timestamp, signature)?;
    let text = string_to_sign(method, authority, scheme, path, query, timestamp, body)?;
    let key = VerifyingKey::from_bytes(&decode_public_key(key)?)
        .map_err(|_| ApiError::new("unauthorized"))?;
    let signature = Signature::from_bytes(&b64_decode::<64>(signature)?);
    key.verify_strict(text.as_bytes(), &signature)
        .map_err(|_| ApiError::new("unauthorized"))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn signing_public_vectors() {
        let values: serde_json::Value =
            serde_json::from_str(include_str!("../../../docs/signing-vectors.json")).unwrap();
        for value in values["vectors"].as_array().unwrap() {
            let s = |name: &str| value[name].as_str().unwrap();
            let body = URL_SAFE_NO_PAD.decode(s("body_base64url")).unwrap();
            let seed = b64_decode::<32>(s("seed")).unwrap();
            let scheme = s("url").split(':').next().unwrap();
            assert_eq!(
                canonical_authority(s("authority"), scheme).unwrap(),
                s("host")
            );
            let headers = sign_request(
                &seed,
                s("method"),
                s("authority"),
                scheme,
                s("path"),
                s("query"),
                s("timestamp"),
                &body,
            )
            .unwrap();
            assert_eq!(headers.key, s("public_key"));
            assert_eq!(headers.signature, s("signature"));
            assert_eq!(
                string_to_sign(
                    s("method"),
                    s("authority"),
                    scheme,
                    s("path"),
                    s("query"),
                    s("timestamp"),
                    &body
                )
                .unwrap(),
                s("string_to_sign")
            );
            verify_request(
                &headers.key,
                &headers.timestamp,
                &headers.signature,
                s("method"),
                s("authority"),
                scheme,
                s("path"),
                s("query"),
                &body,
            )
            .unwrap();
            for equivalent in value["equivalent_authorities"]
                .as_array()
                .into_iter()
                .flatten()
            {
                verify_request(
                    &headers.key,
                    &headers.timestamp,
                    &headers.signature,
                    s("method"),
                    equivalent.as_str().unwrap(),
                    scheme,
                    s("path"),
                    s("query"),
                    &body,
                )
                .unwrap();
            }
            for (method, path, query, body) in [
                ("DELETE", s("path"), s("query"), body.clone()),
                (s("method"), "/changed", s("query"), body.clone()),
                (s("method"), s("path"), "changed=1", body.clone()),
                (s("method"), s("path"), s("query"), b"changed".to_vec()),
            ] {
                assert!(
                    verify_request(
                        &headers.key,
                        &headers.timestamp,
                        &headers.signature,
                        method,
                        s("authority"),
                        scheme,
                        path,
                        query,
                        &body
                    )
                    .is_err()
                );
            }
        }
    }
    #[test]
    fn signing_rejects_noncanonical_keys_weak_keys_and_timestamps() {
        assert!(decode_public_key(&b64_encode(&[0; 32])).is_err());
        let valid = public_key_from_seed(&[3; 32]);
        assert!(decode_public_key(&(valid.clone() + "=")).is_err());
        let mut alias = valid.into_bytes();
        let last = alias.len() - 1;
        alias[last] = b'B';
        assert!(decode_public_key(std::str::from_utf8(&alias).unwrap()).is_err());
        for value in ["+1", "-1", "01", "", "1.0", "1234567890123", "1\n"] {
            assert!(parse_timestamp(value).is_err());
        }
        assert_eq!(parse_timestamp("0").unwrap(), 0);
    }
    #[test]
    fn signing_authority_edge_cases() {
        for value in [
            "user@host",
            "host:",
            "host:0",
            "host:65536",
            "host/path",
            "host?x",
            "host#x",
            "host%zone",
            "[::1%lo0]",
            "[::1]junk",
            "::1",
            "a..b",
            "-host",
            "host-",
            "é.example",
            "01.2.3.4",
            "127.0.0.999",
            "a\nb",
        ] {
            assert!(canonical_authority(value, "http").is_err(), "{value}");
        }
        assert_eq!(
            canonical_authority("API.Example:00443", "https").unwrap(),
            "api.example"
        );
        assert_eq!(
            canonical_authority("[::ffff:192.0.2.1]:80", "http").unwrap(),
            "[0000:0000:0000:0000:0000:ffff:c000:0201]"
        );
        assert_eq!(
            canonical_authority("api.example:443", "http").unwrap(),
            "api.example:443"
        );
    }
}
