pub mod openclaw;

use std::path::Path;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum MigrationError {
    #[error("failed to read config file: {0}")]
    IoError(#[from] std::io::Error),
    #[error("failed to parse JSON: {0}")]
    JsonError(#[from] serde_json::Error),
    #[error("failed to serialize TOML: {0}")]
    TomlError(#[from] toml::ser::Error),
    #[error("missing required field: {0}")]
    MissingField(String),
    #[error("unsupported value: {0}")]
    Unsupported(String),
}

/// Report produced after a migration, detailing warnings and unmapped fields.
#[derive(Debug, Clone)]
pub struct MigrationReport {
    pub warnings: Vec<String>,
    pub unmapped_fields: Vec<String>,
    pub success: bool,
}

impl MigrationReport {
    pub fn new() -> Self {
        Self {
            warnings: Vec::new(),
            unmapped_fields: Vec::new(),
            success: true,
        }
    }
}

impl Default for MigrationReport {
    fn default() -> Self {
        Self::new()
    }
}

/// Orchestrates config migration from OpenClaw JSON to Ironjaw TOML.
pub struct MigrationEngine;

impl MigrationEngine {
    /// Read an OpenClaw JSON config file, migrate it, and return Ironjaw TOML.
    pub fn from_file(path: &Path) -> Result<(String, MigrationReport), MigrationError> {
        let content = std::fs::read_to_string(path)?;
        let oc = openclaw::parse_openclaw_config(&content)?;
        let (ironjaw, report) = openclaw::migrate_config(&oc);
        let toml_str = openclaw::to_toml(&ironjaw)?;
        Ok((toml_str, report))
    }

    /// Migrate from a JSON string directly.
    pub fn from_json_str(json_str: &str) -> Result<(String, MigrationReport), MigrationError> {
        let oc = openclaw::parse_openclaw_config(json_str)?;
        let (ironjaw, report) = openclaw::migrate_config(&oc);
        let toml_str = openclaw::to_toml(&ironjaw)?;
        Ok((toml_str, report))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal_json() -> &'static str {
        r#"{
            "agents": [],
            "tools": {},
            "providers": {}
        }"#
    }

    #[test]
    fn test_migration_report_default() {
        let report = MigrationReport::default();
        assert!(report.success);
        assert!(report.warnings.is_empty());
        assert!(report.unmapped_fields.is_empty());
    }

    #[test]
    fn test_from_str_minimal() {
        let (toml_str, report) = MigrationEngine::from_json_str(minimal_json()).unwrap();
        assert!(report.success);
        // Minimal config with all empty collections produces empty TOML (all fields skipped)
        let _parsed: toml::Value = toml::from_str(&toml_str).unwrap();
    }

    #[test]
    fn test_from_str_invalid_json() {
        let result = MigrationEngine::from_json_str("not json");
        assert!(result.is_err());
    }

    #[test]
    fn test_from_file_not_found() {
        let result = MigrationEngine::from_file(Path::new("/nonexistent/path.json"));
        assert!(result.is_err());
    }
}
