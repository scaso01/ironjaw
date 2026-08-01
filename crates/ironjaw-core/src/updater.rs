// Self-update checker: GitHub releases API, semver comparison, staged downloads.

use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::path::{Path, PathBuf};

/// Errors from the update subsystem.
#[derive(Debug, thiserror::Error)]
pub enum UpdateError {
    #[error("HTTP request failed: {0}")]
    Http(String),
    #[error("Failed to parse release info: {0}")]
    Parse(String),
    #[error("Invalid version format: {0}")]
    InvalidVersion(String),
    #[error("Download failed: {0}")]
    Download(String),
    #[error("Checksum mismatch: expected {expected}, got {actual}")]
    ChecksumMismatch { expected: String, actual: String },
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

/// Configuration for the self-update system.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateConfig {
    /// Whether to check for updates on startup.
    #[serde(default)]
    pub check_on_startup: bool,
    /// GitHub repository in "owner/repo" format.
    #[serde(default = "default_repo")]
    pub repo: String,
    /// Directory where staged updates are downloaded before applying.
    #[serde(default = "default_staging_dir")]
    pub staging_dir: PathBuf,
}

fn default_repo() -> String {
    "scaso01/ironjaw".to_string()
}

fn default_staging_dir() -> PathBuf {
    PathBuf::from("data/updates")
}

impl Default for UpdateConfig {
    fn default() -> Self {
        Self {
            check_on_startup: false,
            repo: default_repo(),
            staging_dir: default_staging_dir(),
        }
    }
}

/// Information about a GitHub release.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReleaseInfo {
    /// The git tag name (e.g. "v0.2.0").
    pub tag_name: String,
    /// Parsed version string (e.g. "0.2.0").
    pub version: String,
    /// Download URL for the release binary.
    pub download_url: String,
    /// ISO 8601 published date.
    pub published_at: String,
    /// Release notes body (markdown).
    pub body: String,
}

/// A staged update ready to be applied.
#[derive(Debug, Clone)]
pub struct StagedUpdate {
    /// Path to the downloaded binary.
    pub binary_path: PathBuf,
    /// SHA-256 checksum of the downloaded binary.
    pub checksum: String,
    /// Version of the staged update.
    pub version: String,
}

/// Checks GitHub for new releases and stages downloads.
pub struct UpdateChecker {
    /// GitHub repository in "owner/repo" format.
    repo: String,
    /// Current running version.
    current_version: String,
}

impl UpdateChecker {
    /// Create a new update checker with the given config.
    pub fn new(config: &UpdateConfig) -> Self {
        Self {
            repo: config.repo.clone(),
            current_version: env!("CARGO_PKG_VERSION").to_string(),
        }
    }

    /// Create an update checker with explicit version (for testing).
    pub fn with_version(repo: &str, version: &str) -> Self {
        Self {
            repo: repo.to_string(),
            current_version: version.to_string(),
        }
    }

    /// Returns the current version.
    pub fn current_version(&self) -> &str {
        &self.current_version
    }

    /// Returns the configured repo.
    pub fn repo(&self) -> &str {
        &self.repo
    }

    /// Build the GitHub API URL for latest release.
    pub fn releases_url(&self) -> String {
        format!("https://api.github.com/repos/{}/releases/latest", self.repo)
    }

    /// Check if an update is available by comparing current vs latest version.
    /// Returns `Ok(Some(release))` if a newer version exists, `Ok(None)` if up-to-date.
    ///
    /// This method requires network access. In tests, use `check_against_release` instead.
    pub async fn check_for_update(&self) -> Result<Option<ReleaseInfo>, UpdateError> {
        let url = self.releases_url();
        // Use reqwest to hit GitHub API
        let client = reqwest::Client::builder()
            .user_agent("ironjaw-updater")
            .build()
            .map_err(|e| UpdateError::Http(e.to_string()))?;

        let resp = client
            .get(&url)
            .send()
            .await
            .map_err(|e| UpdateError::Http(e.to_string()))?;

        if !resp.status().is_success() {
            return Err(UpdateError::Http(format!(
                "GitHub API returned {}",
                resp.status()
            )));
        }

        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| UpdateError::Parse(e.to_string()))?;

        let release = Self::parse_release(&body)?;
        Ok(self.check_against_release(&release))
    }

    /// Parse a GitHub release JSON object into ReleaseInfo.
    pub fn parse_release(json: &serde_json::Value) -> Result<ReleaseInfo, UpdateError> {
        let tag_name = json["tag_name"]
            .as_str()
            .ok_or_else(|| UpdateError::Parse("missing tag_name".to_string()))?
            .to_string();

        let version = tag_name.strip_prefix('v').unwrap_or(&tag_name).to_string();

        let download_url = json["assets"]
            .as_array()
            .and_then(|assets| assets.first())
            .and_then(|a| a["browser_download_url"].as_str())
            .unwrap_or("")
            .to_string();

        let published_at = json["published_at"].as_str().unwrap_or("").to_string();

        let body = json["body"].as_str().unwrap_or("").to_string();

        Ok(ReleaseInfo {
            tag_name,
            version,
            download_url,
            published_at,
            body,
        })
    }

    /// Compare current version against a release and return it if newer.
    pub fn check_against_release(&self, release: &ReleaseInfo) -> Option<ReleaseInfo> {
        match compare_versions(&self.current_version, &release.version) {
            Ordering::Less => Some(release.clone()),
            _ => None,
        }
    }

    /// Download a release binary to the staging directory.
    pub async fn download_update(
        &self,
        release: &ReleaseInfo,
        staging_dir: &Path,
    ) -> Result<StagedUpdate, UpdateError> {
        if release.download_url.is_empty() {
            return Err(UpdateError::Download(
                "No download URL in release".to_string(),
            ));
        }

        std::fs::create_dir_all(staging_dir)?;

        let file_name = format!("ironjaw-{}", release.version);
        let binary_path = staging_dir.join(&file_name);

        let client = reqwest::Client::builder()
            .user_agent("ironjaw-updater")
            .build()
            .map_err(|e| UpdateError::Download(e.to_string()))?;

        let resp = client
            .get(&release.download_url)
            .send()
            .await
            .map_err(|e| UpdateError::Download(e.to_string()))?;

        if !resp.status().is_success() {
            return Err(UpdateError::Download(format!(
                "Download returned {}",
                resp.status()
            )));
        }

        let bytes = resp
            .bytes()
            .await
            .map_err(|e| UpdateError::Download(e.to_string()))?;

        std::fs::write(&binary_path, &bytes)?;

        // Compute SHA-256 checksum
        let checksum = sha256_hex(&bytes);

        Ok(StagedUpdate {
            binary_path,
            checksum,
            version: release.version.clone(),
        })
    }
}

/// Compare two semver strings. Returns Ordering of `current` relative to `latest`.
/// Handles major.minor.patch and optional pre-release suffixes.
pub fn compare_versions(current: &str, latest: &str) -> Ordering {
    let parse = |v: &str| -> (Vec<u64>, Option<String>) {
        let stripped = v.strip_prefix('v').unwrap_or(v);
        let (version_part, pre_release) = match stripped.split_once('-') {
            Some((ver, pre)) => (ver, Some(pre.to_string())),
            None => (stripped, None),
        };
        let parts: Vec<u64> = version_part
            .split('.')
            .map(|s| s.parse::<u64>().unwrap_or(0))
            .collect();
        (parts, pre_release)
    };

    let (cur_parts, cur_pre) = parse(current);
    let (lat_parts, lat_pre) = parse(latest);

    // Compare numeric parts
    let max_len = cur_parts.len().max(lat_parts.len());
    for i in 0..max_len {
        let c = cur_parts.get(i).copied().unwrap_or(0);
        let l = lat_parts.get(i).copied().unwrap_or(0);
        match c.cmp(&l) {
            Ordering::Equal => continue,
            ord => return ord,
        }
    }

    // If numeric parts are equal, handle pre-release:
    // A version with pre-release is LESS than one without (1.0.0-alpha < 1.0.0)
    match (&cur_pre, &lat_pre) {
        (None, None) => Ordering::Equal,
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (Some(a), Some(b)) => a.cmp(b),
    }
}

/// Compute hex-encoded SHA-256 of bytes.
fn sha256_hex(data: &[u8]) -> String {
    use ring::digest;
    let hash = digest::digest(&digest::SHA256, data);
    hash.as_ref().iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- Semver comparison tests ---

    #[test]
    fn test_compare_equal_versions() {
        assert_eq!(compare_versions("0.1.0", "0.1.0"), Ordering::Equal);
    }

    #[test]
    fn test_compare_newer_available() {
        assert_eq!(compare_versions("0.1.0", "0.2.0"), Ordering::Less);
    }

    #[test]
    fn test_compare_older_available() {
        assert_eq!(compare_versions("1.0.0", "0.9.0"), Ordering::Greater);
    }

    #[test]
    fn test_compare_major_bump() {
        assert_eq!(compare_versions("0.9.9", "1.0.0"), Ordering::Less);
    }

    #[test]
    fn test_compare_patch_bump() {
        assert_eq!(compare_versions("1.2.3", "1.2.4"), Ordering::Less);
    }

    #[test]
    fn test_compare_with_v_prefix() {
        assert_eq!(compare_versions("v0.1.0", "v0.2.0"), Ordering::Less);
        assert_eq!(compare_versions("v1.0.0", "1.0.0"), Ordering::Equal);
    }

    #[test]
    fn test_compare_prerelease_vs_release() {
        // Pre-release is less than release with same numeric version
        assert_eq!(compare_versions("1.0.0-alpha", "1.0.0"), Ordering::Less);
    }

    #[test]
    fn test_compare_release_vs_prerelease() {
        assert_eq!(compare_versions("1.0.0", "1.0.0-beta"), Ordering::Greater);
    }

    #[test]
    fn test_compare_two_prereleases() {
        assert_eq!(
            compare_versions("1.0.0-alpha", "1.0.0-beta"),
            Ordering::Less
        );
    }

    #[test]
    fn test_compare_different_length() {
        assert_eq!(compare_versions("1.0", "1.0.0"), Ordering::Equal);
        assert_eq!(compare_versions("1.0.0", "1.0"), Ordering::Equal);
    }

    // --- Release parsing tests ---

    #[test]
    fn test_parse_release_info() {
        let json = serde_json::json!({
            "tag_name": "v0.2.0",
            "published_at": "2026-03-12T10:00:00Z",
            "body": "Release notes here",
            "assets": [{
                "browser_download_url": "https://example.com/ironjaw-v0.2.0"
            }]
        });
        let release = UpdateChecker::parse_release(&json).unwrap();
        assert_eq!(release.tag_name, "v0.2.0");
        assert_eq!(release.version, "0.2.0");
        assert_eq!(release.download_url, "https://example.com/ironjaw-v0.2.0");
        assert_eq!(release.published_at, "2026-03-12T10:00:00Z");
        assert_eq!(release.body, "Release notes here");
    }

    #[test]
    fn test_parse_release_missing_tag() {
        let json = serde_json::json!({"body": "no tag"});
        assert!(UpdateChecker::parse_release(&json).is_err());
    }

    #[test]
    fn test_parse_release_no_assets() {
        let json = serde_json::json!({
            "tag_name": "v1.0.0",
            "published_at": "",
            "body": "",
            "assets": []
        });
        let release = UpdateChecker::parse_release(&json).unwrap();
        assert_eq!(release.download_url, "");
    }

    // --- Config defaults ---

    #[test]
    fn test_config_defaults() {
        let config = UpdateConfig::default();
        assert!(!config.check_on_startup);
        assert_eq!(config.repo, "scaso01/ironjaw");
        assert_eq!(config.staging_dir, PathBuf::from("data/updates"));
    }

    // --- UpdateChecker logic ---

    #[test]
    fn test_checker_detects_newer_version() {
        let checker = UpdateChecker::with_version("scaso01/ironjaw", "0.1.0");
        let release = ReleaseInfo {
            tag_name: "v0.2.0".into(),
            version: "0.2.0".into(),
            download_url: "https://example.com/dl".into(),
            published_at: "2026-03-12T00:00:00Z".into(),
            body: "New release".into(),
        };
        let result = checker.check_against_release(&release);
        assert!(result.is_some());
        assert_eq!(result.unwrap().version, "0.2.0");
    }

    #[test]
    fn test_checker_no_update_when_current() {
        let checker = UpdateChecker::with_version("scaso01/ironjaw", "0.2.0");
        let release = ReleaseInfo {
            tag_name: "v0.2.0".into(),
            version: "0.2.0".into(),
            download_url: String::new(),
            published_at: String::new(),
            body: String::new(),
        };
        assert!(checker.check_against_release(&release).is_none());
    }

    #[test]
    fn test_checker_no_update_when_ahead() {
        let checker = UpdateChecker::with_version("scaso01/ironjaw", "1.0.0");
        let release = ReleaseInfo {
            tag_name: "v0.9.0".into(),
            version: "0.9.0".into(),
            download_url: String::new(),
            published_at: String::new(),
            body: String::new(),
        };
        assert!(checker.check_against_release(&release).is_none());
    }

    #[test]
    fn test_checker_releases_url() {
        let checker = UpdateChecker::with_version("scaso01/ironjaw", "0.1.0");
        assert_eq!(
            checker.releases_url(),
            "https://api.github.com/repos/scaso01/ironjaw/releases/latest"
        );
    }

    #[test]
    fn test_checker_accessors() {
        let checker = UpdateChecker::with_version("myorg/myrepo", "1.2.3");
        assert_eq!(checker.current_version(), "1.2.3");
        assert_eq!(checker.repo(), "myorg/myrepo");
    }

    #[test]
    fn test_staging_path_creation() {
        let dir = std::env::temp_dir().join("ironjaw-updater-test-staging");
        // Ensure clean state
        let _ = std::fs::remove_dir_all(&dir);
        assert!(!dir.exists());

        std::fs::create_dir_all(&dir).unwrap();
        assert!(dir.exists());

        // Cleanup
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_sha256_hex() {
        let hash = sha256_hex(b"hello world");
        assert_eq!(
            hash,
            "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9"
        );
    }

    #[test]
    fn test_update_config_serde_roundtrip() {
        let config = UpdateConfig {
            check_on_startup: true,
            repo: "test/repo".to_string(),
            staging_dir: PathBuf::from("/tmp/staging"),
        };
        let json = serde_json::to_string(&config).unwrap();
        let parsed: UpdateConfig = serde_json::from_str(&json).unwrap();
        assert!(parsed.check_on_startup);
        assert_eq!(parsed.repo, "test/repo");
        assert_eq!(parsed.staging_dir, PathBuf::from("/tmp/staging"));
    }

    #[test]
    fn test_release_info_serde_roundtrip() {
        let release = ReleaseInfo {
            tag_name: "v1.0.0".into(),
            version: "1.0.0".into(),
            download_url: "https://example.com".into(),
            published_at: "2026-01-01T00:00:00Z".into(),
            body: "notes".into(),
        };
        let json = serde_json::to_string(&release).unwrap();
        let parsed: ReleaseInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.tag_name, "v1.0.0");
        assert_eq!(parsed.version, "1.0.0");
    }
}
