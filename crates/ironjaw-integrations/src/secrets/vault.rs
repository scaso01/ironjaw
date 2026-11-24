use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, AES_256_GCM, CHACHA20_POLY1305};
use ring::rand::{SecureRandom, SystemRandom};
use serde::{Deserialize, Serialize};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::SqlitePool;
use std::fmt;
use std::str::FromStr;

const NONCE_LEN: usize = 12;

/// Cipher version tag prepended to encrypted payloads.
/// V1 = AES-256-GCM (legacy), V2 = ChaCha20-Poly1305 (current).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum CipherVersion {
    /// AES-256-GCM — original cipher (legacy).
    V1 = 0x01,
    /// ChaCha20-Poly1305 — current cipher.
    V2 = 0x02,
}

impl CipherVersion {
    /// Detect cipher version from the first byte of encrypted data.
    pub fn detect(data: &[u8]) -> Option<Self> {
        match data.first() {
            Some(0x01) => Some(CipherVersion::V1),
            Some(0x02) => Some(CipherVersion::V2),
            _ => None,
        }
    }

    /// Return the version tag byte.
    pub fn tag(self) -> u8 {
        self as u8
    }
}

/// The current cipher version used for all new encryptions.
const CURRENT_CIPHER: CipherVersion = CipherVersion::V2;

/// Open `data` (ciphertext plus AEAD tag) under `key`; None when authentication fails.
fn open_with(key: &LessSafeKey, nonce: [u8; NONCE_LEN], data: &[u8]) -> Option<Vec<u8>> {
    let mut buf = data.to_vec();
    key.open_in_place(Nonce::assume_unique_for_key(nonce), Aad::empty(), &mut buf)
        .ok()
        .map(|plaintext| plaintext.to_vec())
}

/// Category for a stored secret, controlling visibility and access.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SecretCategory {
    /// Never exposed to subprocesses or LLM (internal keys, auth tokens).
    System,
    /// Injected as environment variables when tools execute.
    Tool,
    /// User-managed, accessible via secrets_get/secrets_set.
    User,
}

impl fmt::Display for SecretCategory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SecretCategory::System => write!(f, "system"),
            SecretCategory::Tool => write!(f, "tool"),
            SecretCategory::User => write!(f, "user"),
        }
    }
}

impl SecretCategory {
    /// Parse a category from its string representation.
    pub fn from_str_value(s: &str) -> Option<Self> {
        match s {
            "system" => Some(SecretCategory::System),
            "tool" => Some(SecretCategory::Tool),
            "user" => Some(SecretCategory::User),
            _ => None,
        }
    }
}

/// Encrypted secrets vault backed by SQLite with versioned ciphers.
/// V1 = AES-256-GCM (legacy), V2 = ChaCha20-Poly1305 (current).
/// Master key is protected by Windows DPAPI (tied to logged-in user).
pub struct SecretsVault {
    pool: SqlitePool,
    /// AES-256-GCM key for decrypting V1 (legacy) entries.
    key_v1: LessSafeKey,
    /// ChaCha20-Poly1305 key for V2 (current) encrypt/decrypt.
    key_v2: LessSafeKey,
    rng: SystemRandom,
}

impl SecretsVault {
    /// Open or create a secrets vault at the given path.
    /// Pass ":memory:" for an in-memory vault (testing).
    pub async fn open(db_path: &str) -> anyhow::Result<Self> {
        let options = if db_path == ":memory:" {
            SqliteConnectOptions::from_str("sqlite::memory:")?
        } else {
            SqliteConnectOptions::from_str(&format!("sqlite:{db_path}"))?
                .create_if_missing(true)
                .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
        };

        let pool = SqlitePoolOptions::new()
            .max_connections(2)
            .connect_with(options)
            .await?;

        Self::init(pool).await
    }

    async fn init(pool: SqlitePool) -> anyhow::Result<Self> {
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS vault_meta (
                key TEXT PRIMARY KEY,
                value BLOB NOT NULL
            )",
        )
        .execute(&pool)
        .await?;

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS secrets (
                key TEXT PRIMARY KEY,
                value BLOB NOT NULL,
                nonce BLOB NOT NULL,
                category TEXT NOT NULL DEFAULT 'user',
                created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f', 'now')),
                updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f', 'now'))
            )",
        )
        .execute(&pool)
        .await?;

        // Migration: add category column to existing DBs that lack it
        Self::migrate_add_category(&pool).await?;

        let master_key = Self::load_or_create_master_key(&pool).await?;

        let unbound_v1 = UnboundKey::new(&AES_256_GCM, &master_key)
            .map_err(|_| anyhow::anyhow!("Failed to create AES-256-GCM key"))?;
        let key_v1 = LessSafeKey::new(unbound_v1);

        let unbound_v2 = UnboundKey::new(&CHACHA20_POLY1305, &master_key)
            .map_err(|_| anyhow::anyhow!("Failed to create ChaCha20-Poly1305 key"))?;
        let key_v2 = LessSafeKey::new(unbound_v2);

        Ok(Self {
            pool,
            key_v1,
            key_v2,
            rng: SystemRandom::new(),
        })
    }

    /// Add `category` column if it doesn't exist (migration for older DBs).
    async fn migrate_add_category(pool: &SqlitePool) -> anyhow::Result<()> {
        // Check if column exists by querying table_info
        let rows: Vec<(String,)> =
            sqlx::query_as("SELECT name FROM pragma_table_info('secrets') WHERE name = 'category'")
                .fetch_all(pool)
                .await?;

        if rows.is_empty() {
            tracing::info!("Migrating secrets table: adding category column");
            sqlx::query("ALTER TABLE secrets ADD COLUMN category TEXT NOT NULL DEFAULT 'user'")
                .execute(pool)
                .await?;
        }
        Ok(())
    }

    async fn load_or_create_master_key(pool: &SqlitePool) -> anyhow::Result<[u8; 32]> {
        let row: Option<(Vec<u8>,)> =
            sqlx::query_as("SELECT value FROM vault_meta WHERE key = 'master_key'")
                .fetch_optional(pool)
                .await?;

        if let Some((encrypted_key,)) = row {
            let decrypted = dpapi::unprotect(&encrypted_key)?;
            let mut key = [0u8; 32];
            if decrypted.len() != 32 {
                return Err(anyhow::anyhow!(
                    "Invalid master key length: {}",
                    decrypted.len()
                ));
            }
            key.copy_from_slice(&decrypted);
            Ok(key)
        } else {
            let rng = SystemRandom::new();
            let mut key = [0u8; 32];
            rng.fill(&mut key)
                .map_err(|_| anyhow::anyhow!("Failed to generate random master key"))?;

            let encrypted = dpapi::protect(&key)?;

            sqlx::query("INSERT INTO vault_meta (key, value) VALUES ('master_key', ?)")
                .bind(&encrypted)
                .execute(pool)
                .await?;

            Ok(key)
        }
    }

    /// Retrieve a secret by key. Returns the value and its category, or None if not found.
    /// Automatically detects cipher version from the stored data.
    pub async fn get(&self, secret_key: &str) -> anyhow::Result<Option<(String, SecretCategory)>> {
        let row: Option<(Vec<u8>, Vec<u8>, String)> =
            sqlx::query_as("SELECT value, nonce, category FROM secrets WHERE key = ?")
                .bind(secret_key)
                .fetch_optional(&self.pool)
                .await?;

        match row {
            Some((ciphertext, nonce_vec, cat_str)) => {
                let plaintext_bytes = self.decrypt_blob(&ciphertext, &nonce_vec)?;
                let category =
                    SecretCategory::from_str_value(&cat_str).unwrap_or(SecretCategory::User);
                Ok(Some((String::from_utf8(plaintext_bytes)?, category)))
            }
            None => Ok(None),
        }
    }

    /// Decrypt a blob that may be versioned (V1/V2 tag) or unversioned (legacy V1).
    ///
    /// Legacy blobs carry no tag, so their first byte is random and reads as a V1 or V2
    /// tag about 2 times in 256. A tagged open that fails authentication therefore falls
    /// back to the legacy reading; the AEAD tag rejects every wrong guess.
    fn decrypt_blob(&self, ciphertext: &[u8], nonce_vec: &[u8]) -> anyhow::Result<Vec<u8>> {
        let nonce: [u8; NONCE_LEN] = nonce_vec
            .try_into()
            .map_err(|_| anyhow::anyhow!("Invalid nonce length"))?;

        let (tagged, failure) = match CipherVersion::detect(ciphertext) {
            Some(CipherVersion::V1) => (
                open_with(&self.key_v1, nonce, &ciphertext[1..]),
                "V1 decryption failed",
            ),
            Some(CipherVersion::V2) => (
                open_with(&self.key_v2, nonce, &ciphertext[1..]),
                "V2 decryption failed",
            ),
            None => (None, "Legacy V1 decryption failed"),
        };
        tagged
            .or_else(|| open_with(&self.key_v1, nonce, ciphertext))
            .ok_or_else(|| anyhow::anyhow!(failure))
    }

    /// True only for a blob that really is V2: tagged V2 and authenticates under the V2 key.
    fn is_v2(&self, ciphertext: &[u8], nonce_vec: &[u8]) -> bool {
        let Ok(nonce) = <[u8; NONCE_LEN]>::try_from(nonce_vec) else {
            return false;
        };
        CipherVersion::detect(ciphertext) == Some(CipherVersion::V2)
            && open_with(&self.key_v2, nonce, &ciphertext[1..]).is_some()
    }

    /// Store or update a secret with a category. Value is encrypted at rest using V2
    /// (ChaCha20-Poly1305) with a 1-byte version tag prepended.
    pub async fn set(
        &self,
        secret_key: &str,
        value: &str,
        category: SecretCategory,
    ) -> anyhow::Result<()> {
        let tagged_ciphertext = self.encrypt_v2(value.as_bytes())?;

        sqlx::query(
            "INSERT INTO secrets (key, value, nonce, category) VALUES (?, ?, ?, ?)
             ON CONFLICT(key) DO UPDATE SET
                value = excluded.value,
                nonce = excluded.nonce,
                category = excluded.category,
                updated_at = strftime('%Y-%m-%dT%H:%M:%f', 'now')",
        )
        .bind(secret_key)
        .bind(&tagged_ciphertext.0)
        .bind(&tagged_ciphertext.1[..])
        .bind(category.to_string())
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    /// Encrypt plaintext with V2 (ChaCha20-Poly1305), returning (version_tag + ciphertext, nonce).
    fn encrypt_v2(&self, plaintext: &[u8]) -> anyhow::Result<(Vec<u8>, [u8; NONCE_LEN])> {
        let mut nonce_bytes = [0u8; NONCE_LEN];
        self.rng
            .fill(&mut nonce_bytes)
            .map_err(|_| anyhow::anyhow!("Failed to generate nonce"))?;
        let nonce = Nonce::assume_unique_for_key(nonce_bytes);

        let mut in_out = plaintext.to_vec();
        self.key_v2
            .seal_in_place_append_tag(nonce, Aad::empty(), &mut in_out)
            .map_err(|_| anyhow::anyhow!("V2 encryption failed"))?;

        // Prepend version tag
        let mut tagged = Vec::with_capacity(1 + in_out.len());
        tagged.push(CURRENT_CIPHER.tag());
        tagged.extend_from_slice(&in_out);

        Ok((tagged, nonce_bytes))
    }

    /// Delete a secret by key. Returns true if it existed.
    pub async fn delete(&self, secret_key: &str) -> anyhow::Result<bool> {
        let result = sqlx::query("DELETE FROM secrets WHERE key = ?")
            .bind(secret_key)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Migrate a single entry to V2 cipher. Decrypts with detected version,
    /// re-encrypts with V2 (ChaCha20-Poly1305). No-op if already V2.
    pub async fn migrate_entry(&self, secret_key: &str) -> anyhow::Result<()> {
        let row: Option<(Vec<u8>, Vec<u8>, String)> =
            sqlx::query_as("SELECT value, nonce, category FROM secrets WHERE key = ?")
                .bind(secret_key)
                .fetch_optional(&self.pool)
                .await?;

        let (ciphertext, nonce_vec, cat_str) = match row {
            Some(r) => r,
            None => return Err(anyhow::anyhow!("Secret not found: {secret_key}")),
        };

        if self.is_v2(&ciphertext, &nonce_vec) {
            return Ok(());
        }

        // Decrypt with detected (or legacy) cipher
        let plaintext = self.decrypt_blob(&ciphertext, &nonce_vec)?;

        // Re-encrypt with V2
        let (new_ciphertext, new_nonce) = self.encrypt_v2(&plaintext)?;

        sqlx::query(
            "UPDATE secrets SET value = ?, nonce = ?, updated_at = strftime('%Y-%m-%dT%H:%M:%f', 'now') WHERE key = ?",
        )
        .bind(&new_ciphertext)
        .bind(&new_nonce[..])
        .bind(secret_key)
        .execute(&self.pool)
        .await?;

        tracing::info!(key = %secret_key, category = %cat_str, "Migrated secret to V2 cipher");
        Ok(())
    }

    /// Migrate all entries to V2 cipher. Returns the number of entries migrated.
    pub async fn migrate_all(&self) -> anyhow::Result<usize> {
        let rows: Vec<(String, Vec<u8>, Vec<u8>)> =
            sqlx::query_as("SELECT key, value, nonce FROM secrets ORDER BY key")
                .fetch_all(&self.pool)
                .await?;

        let mut count = 0;
        for (key, ciphertext, nonce) in &rows {
            if self.is_v2(ciphertext, nonce) {
                continue;
            }
            self.migrate_entry(key).await?;
            count += 1;
        }

        tracing::info!(
            migrated = count,
            total = rows.len(),
            "Cipher migration complete"
        );
        Ok(count)
    }

    /// List all secret key names with their categories (values are NOT returned).
    pub async fn list_secrets(&self) -> anyhow::Result<Vec<(String, SecretCategory)>> {
        let rows: Vec<(String, String)> =
            sqlx::query_as("SELECT key, category FROM secrets ORDER BY key")
                .fetch_all(&self.pool)
                .await?;
        Ok(rows
            .into_iter()
            .map(|(k, c)| {
                let cat = SecretCategory::from_str_value(&c).unwrap_or(SecretCategory::User);
                (k, cat)
            })
            .collect())
    }

    /// Get all secrets in a given category, returning (key, decrypted value) pairs.
    /// Useful for injecting tool secrets as environment variables.
    pub async fn get_secrets_by_category(
        &self,
        category: SecretCategory,
    ) -> anyhow::Result<Vec<(String, String)>> {
        let rows: Vec<(String, Vec<u8>, Vec<u8>)> =
            sqlx::query_as("SELECT key, value, nonce FROM secrets WHERE category = ? ORDER BY key")
                .bind(category.to_string())
                .fetch_all(&self.pool)
                .await?;

        let mut result = Vec::with_capacity(rows.len());
        for (key, ciphertext, nonce_vec) in rows {
            match self.decrypt_blob(&ciphertext, &nonce_vec) {
                Ok(plaintext_bytes) => {
                    let value = String::from_utf8(plaintext_bytes)?;
                    result.push((key, value));
                }
                Err(e) => {
                    tracing::warn!(secret_key = %key, error = %e, "Failed to decrypt secret, skipping");
                }
            }
        }
        Ok(result)
    }
}

// --- DPAPI: Windows master key protection ---

#[cfg(windows)]
mod dpapi {
    use std::ptr;

    #[repr(C)]
    struct DataBlob {
        cb_data: u32,
        pb_data: *mut u8,
    }

    #[link(name = "crypt32")]
    extern "system" {
        fn CryptProtectData(
            pDataIn: *const DataBlob,
            szDataDescr: *const u16,
            pOptionalEntropy: *const DataBlob,
            pvReserved: *mut std::ffi::c_void,
            pPromptStruct: *mut std::ffi::c_void,
            dwFlags: u32,
            pDataOut: *mut DataBlob,
        ) -> i32;

        fn CryptUnprotectData(
            pDataIn: *const DataBlob,
            ppszDataDescr: *mut *mut u16,
            pOptionalEntropy: *const DataBlob,
            pvReserved: *mut std::ffi::c_void,
            pPromptStruct: *mut std::ffi::c_void,
            dwFlags: u32,
            pDataOut: *mut DataBlob,
        ) -> i32;
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn LocalFree(hMem: *mut std::ffi::c_void) -> *mut std::ffi::c_void;
    }

    pub fn protect(data: &[u8]) -> anyhow::Result<Vec<u8>> {
        unsafe {
            let input = DataBlob {
                cb_data: data.len() as u32,
                pb_data: data.as_ptr() as *mut u8,
            };
            let mut output = DataBlob {
                cb_data: 0,
                pb_data: ptr::null_mut(),
            };

            let ok = CryptProtectData(
                &input,
                ptr::null(),
                ptr::null(),
                ptr::null_mut(),
                ptr::null_mut(),
                0,
                &mut output,
            );

            if ok == 0 {
                return Err(anyhow::anyhow!(
                    "CryptProtectData failed: {}",
                    std::io::Error::last_os_error()
                ));
            }

            let result =
                std::slice::from_raw_parts(output.pb_data, output.cb_data as usize).to_vec();
            LocalFree(output.pb_data as *mut _);
            Ok(result)
        }
    }

    pub fn unprotect(data: &[u8]) -> anyhow::Result<Vec<u8>> {
        unsafe {
            let input = DataBlob {
                cb_data: data.len() as u32,
                pb_data: data.as_ptr() as *mut u8,
            };
            let mut output = DataBlob {
                cb_data: 0,
                pb_data: ptr::null_mut(),
            };

            let ok = CryptUnprotectData(
                &input,
                ptr::null_mut(),
                ptr::null(),
                ptr::null_mut(),
                ptr::null_mut(),
                0,
                &mut output,
            );

            if ok == 0 {
                return Err(anyhow::anyhow!(
                    "CryptUnprotectData failed: {}",
                    std::io::Error::last_os_error()
                ));
            }

            let result =
                std::slice::from_raw_parts(output.pb_data, output.cb_data as usize).to_vec();
            LocalFree(output.pb_data as *mut _);
            Ok(result)
        }
    }
}

// --- Non-Windows fallback (dev only, NOT SECURE) ---

#[cfg(not(windows))]
mod dpapi {
    pub fn protect(data: &[u8]) -> anyhow::Result<Vec<u8>> {
        tracing::warn!("DPAPI not available — using passthrough (NOT SECURE, dev only)");
        Ok(data.to_vec())
    }

    pub fn unprotect(data: &[u8]) -> anyhow::Result<Vec<u8>> {
        tracing::warn!("DPAPI not available — using passthrough (NOT SECURE, dev only)");
        Ok(data.to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_set_and_get() {
        let vault = SecretsVault::open(":memory:").await.unwrap();
        vault
            .set("api_key", "sk-12345", SecretCategory::User)
            .await
            .unwrap();
        let result = vault.get("api_key").await.unwrap();
        assert_eq!(result, Some(("sk-12345".to_string(), SecretCategory::User)));
    }

    #[tokio::test]
    async fn test_get_nonexistent() {
        let vault = SecretsVault::open(":memory:").await.unwrap();
        let value = vault.get("nonexistent").await.unwrap();
        assert_eq!(value, None);
    }

    #[tokio::test]
    async fn test_overwrite() {
        let vault = SecretsVault::open(":memory:").await.unwrap();
        vault
            .set("key", "value1", SecretCategory::User)
            .await
            .unwrap();
        vault
            .set("key", "value2", SecretCategory::Tool)
            .await
            .unwrap();
        let result = vault.get("key").await.unwrap();
        assert_eq!(result, Some(("value2".to_string(), SecretCategory::Tool)));
    }

    #[tokio::test]
    async fn test_delete() {
        let vault = SecretsVault::open(":memory:").await.unwrap();
        vault
            .set("key", "value", SecretCategory::User)
            .await
            .unwrap();
        assert!(vault.delete("key").await.unwrap());
        assert_eq!(vault.get("key").await.unwrap(), None);
        assert!(!vault.delete("key").await.unwrap());
    }

    #[tokio::test]
    async fn test_list_secrets() {
        let vault = SecretsVault::open(":memory:").await.unwrap();
        vault.set("bravo", "2", SecretCategory::Tool).await.unwrap();
        vault
            .set("alpha", "1", SecretCategory::System)
            .await
            .unwrap();
        vault
            .set("charlie", "3", SecretCategory::User)
            .await
            .unwrap();
        let secrets = vault.list_secrets().await.unwrap();
        assert_eq!(secrets.len(), 3);
        assert_eq!(secrets[0], ("alpha".to_string(), SecretCategory::System));
        assert_eq!(secrets[1], ("bravo".to_string(), SecretCategory::Tool));
        assert_eq!(secrets[2], ("charlie".to_string(), SecretCategory::User));
    }

    #[tokio::test]
    async fn test_get_secrets_by_category() {
        let vault = SecretsVault::open(":memory:").await.unwrap();
        vault
            .set("sys1", "s1", SecretCategory::System)
            .await
            .unwrap();
        vault
            .set("tool1", "t1", SecretCategory::Tool)
            .await
            .unwrap();
        vault
            .set("tool2", "t2", SecretCategory::Tool)
            .await
            .unwrap();
        vault
            .set("user1", "u1", SecretCategory::User)
            .await
            .unwrap();

        let tool_secrets = vault
            .get_secrets_by_category(SecretCategory::Tool)
            .await
            .unwrap();
        assert_eq!(tool_secrets.len(), 2);
        assert_eq!(tool_secrets[0], ("tool1".to_string(), "t1".to_string()));
        assert_eq!(tool_secrets[1], ("tool2".to_string(), "t2".to_string()));

        let system_secrets = vault
            .get_secrets_by_category(SecretCategory::System)
            .await
            .unwrap();
        assert_eq!(system_secrets.len(), 1);
        assert_eq!(system_secrets[0], ("sys1".to_string(), "s1".to_string()));

        let user_secrets = vault
            .get_secrets_by_category(SecretCategory::User)
            .await
            .unwrap();
        assert_eq!(user_secrets.len(), 1);
    }

    #[tokio::test]
    async fn test_category_display() {
        assert_eq!(SecretCategory::System.to_string(), "system");
        assert_eq!(SecretCategory::Tool.to_string(), "tool");
        assert_eq!(SecretCategory::User.to_string(), "user");
    }

    #[tokio::test]
    async fn test_category_from_str() {
        assert_eq!(
            SecretCategory::from_str_value("system"),
            Some(SecretCategory::System)
        );
        assert_eq!(
            SecretCategory::from_str_value("tool"),
            Some(SecretCategory::Tool)
        );
        assert_eq!(
            SecretCategory::from_str_value("user"),
            Some(SecretCategory::User)
        );
        assert_eq!(SecretCategory::from_str_value("invalid"), None);
    }

    // --- Cipher version tests ---

    #[test]
    fn test_cipher_version_detect_v1() {
        let data = vec![0x01, 0xAA, 0xBB];
        assert_eq!(CipherVersion::detect(&data), Some(CipherVersion::V1));
    }

    #[test]
    fn test_cipher_version_detect_v2() {
        let data = vec![0x02, 0xCC, 0xDD];
        assert_eq!(CipherVersion::detect(&data), Some(CipherVersion::V2));
    }

    #[test]
    fn test_cipher_version_detect_unknown() {
        let data = vec![0x03, 0xEE];
        assert_eq!(CipherVersion::detect(&data), None);
    }

    #[test]
    fn test_cipher_version_detect_empty() {
        assert_eq!(CipherVersion::detect(&[]), None);
    }

    #[test]
    fn test_cipher_version_tag_values() {
        assert_eq!(CipherVersion::V1.tag(), 0x01);
        assert_eq!(CipherVersion::V2.tag(), 0x02);
    }

    #[test]
    fn test_current_cipher_is_v2() {
        assert_eq!(CURRENT_CIPHER, CipherVersion::V2);
    }

    #[tokio::test]
    async fn test_v2_encrypt_decrypt_roundtrip() {
        let vault = SecretsVault::open(":memory:").await.unwrap();
        let plaintext = b"super secret value";
        let (tagged, nonce) = vault.encrypt_v2(plaintext).unwrap();

        // First byte should be V2 tag
        assert_eq!(tagged[0], 0x02);

        // Should decrypt back to original
        let decrypted = vault.decrypt_blob(&tagged, &nonce).unwrap();
        assert_eq!(decrypted, plaintext);
    }

    #[tokio::test]
    async fn test_v2_tagged_storage_roundtrip() {
        let vault = SecretsVault::open(":memory:").await.unwrap();
        vault
            .set("tagged_key", "tagged_value", SecretCategory::User)
            .await
            .unwrap();

        // Verify the stored data has a V2 tag
        let row: (Vec<u8>,) = sqlx::query_as("SELECT value FROM secrets WHERE key = 'tagged_key'")
            .fetch_one(&vault.pool)
            .await
            .unwrap();
        assert_eq!(row.0[0], 0x02, "Stored ciphertext must start with V2 tag");

        // Verify decryption works
        let result = vault.get("tagged_key").await.unwrap();
        assert_eq!(
            result,
            Some(("tagged_value".to_string(), SecretCategory::User))
        );
    }

    #[tokio::test]
    async fn test_legacy_v1_untagged_decrypt() {
        // Simulate legacy V1 data (no version tag, raw AES-256-GCM)
        let vault = SecretsVault::open(":memory:").await.unwrap();

        // Encrypt directly with key_v1 (no tag) to simulate legacy data
        let mut nonce_bytes = [0u8; NONCE_LEN];
        vault.rng.fill(&mut nonce_bytes).unwrap();
        let nonce = Nonce::assume_unique_for_key(nonce_bytes);
        let mut ciphertext = b"legacy_secret".to_vec();
        vault
            .key_v1
            .seal_in_place_append_tag(nonce, Aad::empty(), &mut ciphertext)
            .unwrap();

        // Insert directly without version tag
        sqlx::query("INSERT INTO secrets (key, value, nonce, category) VALUES (?, ?, ?, ?)")
            .bind("legacy_key")
            .bind(&ciphertext)
            .bind(&nonce_bytes[..])
            .bind("user")
            .execute(&vault.pool)
            .await
            .unwrap();

        // Decrypt should work via legacy fallback
        let result = vault.get("legacy_key").await.unwrap();
        assert_eq!(
            result,
            Some(("legacy_secret".to_string(), SecretCategory::User))
        );
    }

    #[tokio::test]
    async fn test_migrate_entry_from_legacy() {
        let vault = SecretsVault::open(":memory:").await.unwrap();

        // Insert legacy V1 (untagged) data
        let mut nonce_bytes = [0u8; NONCE_LEN];
        vault.rng.fill(&mut nonce_bytes).unwrap();
        let nonce = Nonce::assume_unique_for_key(nonce_bytes);
        let mut ciphertext = b"migrate_me".to_vec();
        vault
            .key_v1
            .seal_in_place_append_tag(nonce, Aad::empty(), &mut ciphertext)
            .unwrap();

        sqlx::query("INSERT INTO secrets (key, value, nonce, category) VALUES (?, ?, ?, ?)")
            .bind("mig_key")
            .bind(&ciphertext)
            .bind(&nonce_bytes[..])
            .bind("tool")
            .execute(&vault.pool)
            .await
            .unwrap();

        // Migrate
        vault.migrate_entry("mig_key").await.unwrap();

        // Verify stored data now has V2 tag
        let row: (Vec<u8>,) = sqlx::query_as("SELECT value FROM secrets WHERE key = 'mig_key'")
            .fetch_one(&vault.pool)
            .await
            .unwrap();
        assert_eq!(row.0[0], 0x02, "Migrated data must have V2 tag");

        // Verify decryption still works
        let result = vault.get("mig_key").await.unwrap();
        assert_eq!(
            result,
            Some(("migrate_me".to_string(), SecretCategory::Tool))
        );
    }

    #[tokio::test]
    async fn test_migrate_entry_v2_noop() {
        let vault = SecretsVault::open(":memory:").await.unwrap();
        vault
            .set("already_v2", "value", SecretCategory::User)
            .await
            .unwrap();

        // Should be a no-op (already V2)
        vault.migrate_entry("already_v2").await.unwrap();

        let result = vault.get("already_v2").await.unwrap();
        assert_eq!(result, Some(("value".to_string(), SecretCategory::User)));
    }

    #[tokio::test]
    async fn test_migrate_entry_not_found() {
        let vault = SecretsVault::open(":memory:").await.unwrap();
        let err = vault.migrate_entry("nonexistent").await;
        assert!(err.is_err());
        assert!(err.unwrap_err().to_string().contains("Secret not found"));
    }

    #[tokio::test]
    async fn test_migrate_all() {
        let vault = SecretsVault::open(":memory:").await.unwrap();

        // Insert 2 legacy V1 entries and 1 V2 entry
        for name in ["leg1", "leg2"] {
            let mut nonce_bytes = [0u8; NONCE_LEN];
            vault.rng.fill(&mut nonce_bytes).unwrap();
            let nonce = Nonce::assume_unique_for_key(nonce_bytes);
            let mut ct = name.as_bytes().to_vec();
            vault
                .key_v1
                .seal_in_place_append_tag(nonce, Aad::empty(), &mut ct)
                .unwrap();
            sqlx::query("INSERT INTO secrets (key, value, nonce, category) VALUES (?, ?, ?, ?)")
                .bind(name)
                .bind(&ct)
                .bind(&nonce_bytes[..])
                .bind("user")
                .execute(&vault.pool)
                .await
                .unwrap();
        }
        vault
            .set("modern", "v2data", SecretCategory::User)
            .await
            .unwrap();

        // Migrate all — should migrate 2 (not the V2 one)
        let count = vault.migrate_all().await.unwrap();
        assert_eq!(count, 2);

        // All three should still decrypt
        assert_eq!(
            vault.get("leg1").await.unwrap(),
            Some(("leg1".to_string(), SecretCategory::User))
        );
        assert_eq!(
            vault.get("leg2").await.unwrap(),
            Some(("leg2".to_string(), SecretCategory::User))
        );
        assert_eq!(
            vault.get("modern").await.unwrap(),
            Some(("v2data".to_string(), SecretCategory::User))
        );
    }

    #[tokio::test]
    async fn test_migrate_all_empty_vault() {
        let vault = SecretsVault::open(":memory:").await.unwrap();
        let count = vault.migrate_all().await.unwrap();
        assert_eq!(count, 0);
    }

    /// Store an untagged legacy V1 entry whose ciphertext starts with `first`.
    async fn insert_legacy_starting_with(vault: &SecretsVault, name: &str, first: u8) {
        loop {
            let mut nonce_bytes = [0u8; NONCE_LEN];
            vault.rng.fill(&mut nonce_bytes).unwrap();
            let mut ct = name.as_bytes().to_vec();
            vault
                .key_v1
                .seal_in_place_append_tag(
                    Nonce::assume_unique_for_key(nonce_bytes),
                    Aad::empty(),
                    &mut ct,
                )
                .unwrap();
            if ct[0] != first {
                continue;
            }
            sqlx::query("INSERT INTO secrets (key, value, nonce, category) VALUES (?, ?, ?, ?)")
                .bind(name)
                .bind(&ct)
                .bind(&nonce_bytes[..])
                .bind("user")
                .execute(&vault.pool)
                .await
                .unwrap();
            return;
        }
    }

    #[tokio::test]
    async fn test_legacy_entries_that_look_tagged_still_read_and_migrate() {
        let vault = SecretsVault::open(":memory:").await.unwrap();
        insert_legacy_starting_with(&vault, "looks_v1", CipherVersion::V1.tag()).await;
        insert_legacy_starting_with(&vault, "looks_v2", CipherVersion::V2.tag()).await;

        for name in ["looks_v1", "looks_v2"] {
            assert_eq!(
                vault.get(name).await.unwrap(),
                Some((name.to_string(), SecretCategory::User))
            );
        }

        assert_eq!(vault.migrate_all().await.unwrap(), 2);
        assert_eq!(vault.migrate_all().await.unwrap(), 0);
        for name in ["looks_v1", "looks_v2"] {
            assert_eq!(
                vault.get(name).await.unwrap(),
                Some((name.to_string(), SecretCategory::User))
            );
        }
    }

    #[tokio::test]
    async fn test_v2_different_nonces_different_ciphertext() {
        let vault = SecretsVault::open(":memory:").await.unwrap();
        let (ct1, _) = vault.encrypt_v2(b"same plaintext").unwrap();
        let (ct2, _) = vault.encrypt_v2(b"same plaintext").unwrap();
        assert_ne!(
            ct1, ct2,
            "Different nonces should produce different ciphertext"
        );
    }

    #[tokio::test]
    async fn test_decrypt_invalid_version_tag() {
        let vault = SecretsVault::open(":memory:").await.unwrap();

        // Fabricate data with invalid version tag 0x03
        let nonce = [0u8; NONCE_LEN];
        let bad_data = vec![0x03, 0xFF, 0xFF, 0xFF, 0xFF];

        // Should fail because 0x03 is not a valid tag, and the data can't be
        // decrypted as legacy V1 either
        let result = vault.decrypt_blob(&bad_data, &nonce);
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_v2_empty_plaintext() {
        let vault = SecretsVault::open(":memory:").await.unwrap();
        vault
            .set("empty", "", SecretCategory::System)
            .await
            .unwrap();
        let result = vault.get("empty").await.unwrap();
        assert_eq!(result, Some(("".to_string(), SecretCategory::System)));
    }

    #[tokio::test]
    async fn test_v2_large_plaintext() {
        let vault = SecretsVault::open(":memory:").await.unwrap();
        let large_value = "A".repeat(100_000);
        vault
            .set("large", &large_value, SecretCategory::User)
            .await
            .unwrap();
        let result = vault.get("large").await.unwrap();
        assert_eq!(result, Some((large_value, SecretCategory::User)));
    }

    #[tokio::test]
    async fn test_migrate_preserves_category() {
        let vault = SecretsVault::open(":memory:").await.unwrap();

        // Insert legacy V1 with "system" category
        let mut nonce_bytes = [0u8; NONCE_LEN];
        vault.rng.fill(&mut nonce_bytes).unwrap();
        let nonce = Nonce::assume_unique_for_key(nonce_bytes);
        let mut ct = b"sys_value".to_vec();
        vault
            .key_v1
            .seal_in_place_append_tag(nonce, Aad::empty(), &mut ct)
            .unwrap();
        sqlx::query("INSERT INTO secrets (key, value, nonce, category) VALUES (?, ?, ?, ?)")
            .bind("sys_key")
            .bind(&ct)
            .bind(&nonce_bytes[..])
            .bind("system")
            .execute(&vault.pool)
            .await
            .unwrap();

        vault.migrate_entry("sys_key").await.unwrap();

        let result = vault.get("sys_key").await.unwrap();
        assert_eq!(
            result,
            Some(("sys_value".to_string(), SecretCategory::System))
        );
    }
}
