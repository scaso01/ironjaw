use ring::digest::{digest, SHA256};
use ring::rand::SystemRandom;
use ring::signature::{Ed25519KeyPair, KeyPair, UnparsedPublicKey, ED25519};
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ManifestError {
    #[error("key generation failed: {0}")]
    KeyGeneration(String),
    #[error("signing failed: {0}")]
    Signing(String),
    #[error("verification failed: {0}")]
    Verification(String),
    #[error("serialization failed: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("invalid key: {0}")]
    InvalidKey(String),
}

/// A file entry in a plugin manifest.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ManifestFile {
    pub path: String,
    pub sha256_hash: String,
    pub size: u64,
}

/// A signed plugin manifest containing metadata and file hashes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginManifest {
    pub name: String,
    pub version: String,
    pub files: Vec<ManifestFile>,
    /// Ed25519 signature over the canonical JSON (excluding this field).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<Vec<u8>>,
}

impl PluginManifest {
    /// Produce the canonical JSON bytes for signing (signature field excluded).
    fn canonical_bytes(&self) -> Result<Vec<u8>, ManifestError> {
        // Clone without signature for canonical form
        let canonical = PluginManifest {
            name: self.name.clone(),
            version: self.version.clone(),
            files: self.files.clone(),
            signature: None,
        };
        Ok(serde_json::to_vec(&canonical)?)
    }
}

/// Ed25519 manifest signer wrapping a keypair.
pub struct ManifestSigner {
    keypair: Ed25519KeyPair,
}

impl ManifestSigner {
    /// Create a signer from PKCS#8 private key bytes.
    pub fn from_pkcs8(pkcs8_bytes: &[u8]) -> Result<Self, ManifestError> {
        let keypair = Ed25519KeyPair::from_pkcs8(pkcs8_bytes)
            .map_err(|e| ManifestError::InvalidKey(format!("{e}")))?;
        Ok(Self { keypair })
    }

    /// Get the public key bytes.
    pub fn public_key_bytes(&self) -> &[u8] {
        self.keypair.public_key().as_ref()
    }

    /// Sign a manifest, returning the Ed25519 signature bytes.
    pub fn sign_manifest(&self, manifest: &PluginManifest) -> Result<Vec<u8>, ManifestError> {
        let canonical = manifest.canonical_bytes()?;
        let sig = self.keypair.sign(&canonical);
        Ok(sig.as_ref().to_vec())
    }
}

/// Generate a new Ed25519 keypair. Returns (pkcs8_private_key, public_key_bytes).
pub fn generate_keypair() -> Result<(Vec<u8>, Vec<u8>), ManifestError> {
    let rng = SystemRandom::new();
    let pkcs8 = Ed25519KeyPair::generate_pkcs8(&rng)
        .map_err(|e| ManifestError::KeyGeneration(format!("{e}")))?;
    let keypair = Ed25519KeyPair::from_pkcs8(pkcs8.as_ref())
        .map_err(|e| ManifestError::KeyGeneration(format!("{e}")))?;
    let public_key = keypair.public_key().as_ref().to_vec();
    Ok((pkcs8.as_ref().to_vec(), public_key))
}

/// Verify a manifest signature against a public key.
pub fn verify_manifest(
    public_key: &[u8],
    manifest: &PluginManifest,
) -> Result<bool, ManifestError> {
    let signature = match &manifest.signature {
        Some(sig) => sig,
        None => return Ok(false),
    };
    let canonical = manifest.canonical_bytes()?;
    let peer_public_key = UnparsedPublicKey::new(&ED25519, public_key);
    match peer_public_key.verify(&canonical, signature) {
        Ok(()) => Ok(true),
        Err(_) => Ok(false),
    }
}

/// Compute SHA-256 hex digest of content.
pub fn compute_file_hash(content: &[u8]) -> String {
    let d = digest(&SHA256, content);
    d.as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_manifest() -> PluginManifest {
        PluginManifest {
            name: "test-plugin".to_string(),
            version: "1.0.0".to_string(),
            files: vec![
                ManifestFile {
                    path: "plugin.wasm".to_string(),
                    sha256_hash: compute_file_hash(b"wasm binary content"),
                    size: 19,
                },
                ManifestFile {
                    path: "config.toml".to_string(),
                    sha256_hash: compute_file_hash(b"[plugin]\nname = \"test\""),
                    size: 21,
                },
            ],
            signature: None,
        }
    }

    #[test]
    fn test_generate_keypair() {
        let (privkey, pubkey) = generate_keypair().unwrap();
        assert!(!privkey.is_empty());
        assert_eq!(pubkey.len(), 32); // Ed25519 public key is 32 bytes
    }

    #[test]
    fn test_generate_keypair_unique() {
        let (_, pub1) = generate_keypair().unwrap();
        let (_, pub2) = generate_keypair().unwrap();
        assert_ne!(pub1, pub2, "Two keypairs should have different public keys");
    }

    #[test]
    fn test_signer_from_pkcs8() {
        let (privkey, pubkey) = generate_keypair().unwrap();
        let signer = ManifestSigner::from_pkcs8(&privkey).unwrap();
        assert_eq!(signer.public_key_bytes(), pubkey.as_slice());
    }

    #[test]
    fn test_signer_invalid_key() {
        let result = ManifestSigner::from_pkcs8(b"not a valid key");
        assert!(result.is_err());
    }

    #[test]
    fn test_sign_verify_roundtrip() {
        let (privkey, pubkey) = generate_keypair().unwrap();
        let signer = ManifestSigner::from_pkcs8(&privkey).unwrap();
        let mut manifest = sample_manifest();

        let sig = signer.sign_manifest(&manifest).unwrap();
        assert_eq!(sig.len(), 64); // Ed25519 signature is 64 bytes
        manifest.signature = Some(sig);

        assert!(verify_manifest(&pubkey, &manifest).unwrap());
    }

    #[test]
    fn test_verify_no_signature() {
        let (_, pubkey) = generate_keypair().unwrap();
        let manifest = sample_manifest(); // no signature
        assert!(!verify_manifest(&pubkey, &manifest).unwrap());
    }

    #[test]
    fn test_tamper_detection_name() {
        let (privkey, pubkey) = generate_keypair().unwrap();
        let signer = ManifestSigner::from_pkcs8(&privkey).unwrap();
        let mut manifest = sample_manifest();

        let sig = signer.sign_manifest(&manifest).unwrap();
        manifest.signature = Some(sig);

        // Tamper with name
        manifest.name = "evil-plugin".to_string();
        assert!(!verify_manifest(&pubkey, &manifest).unwrap());
    }

    #[test]
    fn test_tamper_detection_version() {
        let (privkey, pubkey) = generate_keypair().unwrap();
        let signer = ManifestSigner::from_pkcs8(&privkey).unwrap();
        let mut manifest = sample_manifest();

        let sig = signer.sign_manifest(&manifest).unwrap();
        manifest.signature = Some(sig);

        // Tamper with version
        manifest.version = "9.9.9".to_string();
        assert!(!verify_manifest(&pubkey, &manifest).unwrap());
    }

    #[test]
    fn test_tamper_detection_files() {
        let (privkey, pubkey) = generate_keypair().unwrap();
        let signer = ManifestSigner::from_pkcs8(&privkey).unwrap();
        let mut manifest = sample_manifest();

        let sig = signer.sign_manifest(&manifest).unwrap();
        manifest.signature = Some(sig);

        // Tamper with file hash
        manifest.files[0].sha256_hash = "0000000000000000".to_string();
        assert!(!verify_manifest(&pubkey, &manifest).unwrap());
    }

    #[test]
    fn test_tamper_detection_add_file() {
        let (privkey, pubkey) = generate_keypair().unwrap();
        let signer = ManifestSigner::from_pkcs8(&privkey).unwrap();
        let mut manifest = sample_manifest();

        let sig = signer.sign_manifest(&manifest).unwrap();
        manifest.signature = Some(sig);

        // Add an extra file
        manifest.files.push(ManifestFile {
            path: "backdoor.wasm".to_string(),
            sha256_hash: "deadbeef".to_string(),
            size: 666,
        });
        assert!(!verify_manifest(&pubkey, &manifest).unwrap());
    }

    #[test]
    fn test_tamper_detection_remove_file() {
        let (privkey, pubkey) = generate_keypair().unwrap();
        let signer = ManifestSigner::from_pkcs8(&privkey).unwrap();
        let mut manifest = sample_manifest();

        let sig = signer.sign_manifest(&manifest).unwrap();
        manifest.signature = Some(sig);

        // Remove a file
        manifest.files.pop();
        assert!(!verify_manifest(&pubkey, &manifest).unwrap());
    }

    #[test]
    fn test_invalid_signature_bytes() {
        let (_, pubkey) = generate_keypair().unwrap();
        let mut manifest = sample_manifest();
        manifest.signature = Some(vec![0xFF; 64]);
        assert!(!verify_manifest(&pubkey, &manifest).unwrap());
    }

    #[test]
    fn test_wrong_public_key() {
        let (privkey, _) = generate_keypair().unwrap();
        let (_, wrong_pubkey) = generate_keypair().unwrap();
        let signer = ManifestSigner::from_pkcs8(&privkey).unwrap();
        let mut manifest = sample_manifest();

        let sig = signer.sign_manifest(&manifest).unwrap();
        manifest.signature = Some(sig);

        // Verify with wrong key
        assert!(!verify_manifest(&wrong_pubkey, &manifest).unwrap());
    }

    #[test]
    fn test_invalid_public_key() {
        let mut manifest = sample_manifest();
        manifest.signature = Some(vec![0xAA; 64]);
        // A 5-byte public key is invalid for Ed25519
        let result = verify_manifest(&[0x00; 5], &manifest);
        // ring rejects invalid public key length during verify, returning error
        assert!(!result.unwrap());
    }

    #[test]
    fn test_compute_file_hash() {
        let hash = compute_file_hash(b"hello world");
        assert_eq!(hash.len(), 64); // SHA-256 = 32 bytes = 64 hex chars
                                    // Known SHA-256 of "hello world"
        assert_eq!(
            hash,
            "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9"
        );
    }

    #[test]
    fn test_compute_file_hash_empty() {
        let hash = compute_file_hash(b"");
        // Known SHA-256 of empty string
        assert_eq!(
            hash,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn test_compute_file_hash_deterministic() {
        let h1 = compute_file_hash(b"same content");
        let h2 = compute_file_hash(b"same content");
        assert_eq!(h1, h2);
    }

    #[test]
    fn test_canonical_bytes_excludes_signature() {
        let mut manifest = sample_manifest();
        let canonical_without = manifest.canonical_bytes().unwrap();

        manifest.signature = Some(vec![0xAA; 64]);
        let canonical_with = manifest.canonical_bytes().unwrap();

        assert_eq!(
            canonical_without, canonical_with,
            "Canonical bytes must be the same regardless of signature field"
        );
    }

    #[test]
    fn test_manifest_file_size() {
        let (privkey, pubkey) = generate_keypair().unwrap();
        let signer = ManifestSigner::from_pkcs8(&privkey).unwrap();
        let mut manifest = sample_manifest();

        let sig = signer.sign_manifest(&manifest).unwrap();
        manifest.signature = Some(sig);

        // Tamper with file size
        manifest.files[0].size = 999999;
        assert!(!verify_manifest(&pubkey, &manifest).unwrap());
    }
}
