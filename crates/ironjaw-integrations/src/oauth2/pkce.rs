use ring::digest::{digest, SHA256};
use ring::rand::{SecureRandom, SystemRandom};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum PkceError {
    #[error("random generation failed")]
    RandomFailed,
}

/// URL-safe base64 encoding without padding (RFC 7636).
fn base64url_encode(data: &[u8]) -> String {
    use std::fmt::Write;
    let mut result = String::with_capacity((data.len() * 4).div_ceil(3));
    let mut i = 0;
    while i < data.len() {
        let b0 = data[i] as u32;
        let b1 = if i + 1 < data.len() {
            data[i + 1] as u32
        } else {
            0
        };
        let b2 = if i + 2 < data.len() {
            data[i + 2] as u32
        } else {
            0
        };

        let triple = (b0 << 16) | (b1 << 8) | b2;
        let remaining = data.len() - i;

        let chars: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        write!(
            result,
            "{}",
            chars[((triple >> 18) & 0x3F) as usize] as char
        )
        .unwrap();
        write!(
            result,
            "{}",
            chars[((triple >> 12) & 0x3F) as usize] as char
        )
        .unwrap();
        if remaining > 1 {
            write!(result, "{}", chars[((triple >> 6) & 0x3F) as usize] as char).unwrap();
        }
        if remaining > 2 {
            write!(result, "{}", chars[(triple & 0x3F) as usize] as char).unwrap();
        }
        i += 3;
    }
    result
}

/// Characters allowed in a PKCE code_verifier (RFC 7636 Section 4.1).
const UNRESERVED: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-._~";

/// Generate a cryptographically random code_verifier (43-128 URL-safe chars).
pub fn generate_code_verifier() -> Result<String, PkceError> {
    let rng = SystemRandom::new();
    let mut bytes = [0u8; 64]; // 64 random bytes -> pick from UNRESERVED
    rng.fill(&mut bytes).map_err(|_| PkceError::RandomFailed)?;

    let verifier: String = bytes
        .iter()
        .map(|b| UNRESERVED[(*b as usize) % UNRESERVED.len()] as char)
        .collect();

    Ok(verifier)
}

/// Compute S256 code_challenge from a code_verifier (SHA256 + base64url, no padding).
pub fn compute_code_challenge(verifier: &str) -> String {
    let hash = digest(&SHA256, verifier.as_bytes());
    base64url_encode(hash.as_ref())
}

/// A PKCE challenge pair for OAuth2 authorization.
#[derive(Debug, Clone)]
pub struct PkceChallenge {
    pub verifier: String,
    pub challenge: String,
    pub method: &'static str,
}

impl PkceChallenge {
    /// Generate a fresh PKCE challenge pair.
    pub fn new() -> Result<Self, PkceError> {
        let verifier = generate_code_verifier()?;
        let challenge = compute_code_challenge(&verifier);
        Ok(Self {
            verifier,
            challenge,
            method: "S256",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_verifier_length() {
        let verifier = generate_code_verifier().unwrap();
        assert!(
            verifier.len() >= 43 && verifier.len() <= 128,
            "Verifier length {} must be 43-128",
            verifier.len()
        );
    }

    #[test]
    fn test_verifier_charset() {
        let verifier = generate_code_verifier().unwrap();
        for ch in verifier.chars() {
            assert!(
                UNRESERVED.contains(&(ch as u8)),
                "Character '{ch}' not in UNRESERVED set"
            );
        }
    }

    #[test]
    fn test_verifier_uniqueness() {
        let v1 = generate_code_verifier().unwrap();
        let v2 = generate_code_verifier().unwrap();
        assert_ne!(v1, v2, "Two verifiers should be unique");
    }

    #[test]
    fn test_challenge_computation() {
        // RFC 7636 Appendix B test vector
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let challenge = compute_code_challenge(verifier);
        assert_eq!(challenge, "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
    }

    #[test]
    fn test_challenge_not_empty() {
        let verifier = generate_code_verifier().unwrap();
        let challenge = compute_code_challenge(&verifier);
        assert!(!challenge.is_empty());
    }

    #[test]
    fn test_challenge_no_padding() {
        let verifier = generate_code_verifier().unwrap();
        let challenge = compute_code_challenge(&verifier);
        assert!(
            !challenge.contains('='),
            "Challenge must not contain padding"
        );
    }

    #[test]
    fn test_challenge_url_safe() {
        let verifier = generate_code_verifier().unwrap();
        let challenge = compute_code_challenge(&verifier);
        assert!(!challenge.contains('+'), "Challenge must not contain '+'");
        assert!(!challenge.contains('/'), "Challenge must not contain '/'");
    }

    #[test]
    fn test_pkce_challenge_new() {
        let pkce = PkceChallenge::new().unwrap();
        assert_eq!(pkce.method, "S256");
        assert!(!pkce.verifier.is_empty());
        assert!(!pkce.challenge.is_empty());
    }

    #[test]
    fn test_pkce_challenge_consistency() {
        let pkce = PkceChallenge::new().unwrap();
        let recomputed = compute_code_challenge(&pkce.verifier);
        assert_eq!(pkce.challenge, recomputed);
    }

    #[test]
    fn test_base64url_encode_known_vector() {
        // SHA-256 of "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk" should be
        // E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM (base64url, no padding)
        let hash = ring::digest::digest(
            &ring::digest::SHA256,
            b"dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk",
        );
        let encoded = base64url_encode(hash.as_ref());
        assert_eq!(encoded, "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
    }

    #[test]
    fn test_challenge_deterministic() {
        let challenge1 = compute_code_challenge("same-verifier");
        let challenge2 = compute_code_challenge("same-verifier");
        assert_eq!(challenge1, challenge2);
    }
}
