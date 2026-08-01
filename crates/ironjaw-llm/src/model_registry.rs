use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// A single model entry from `llama-models.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelEntry {
    pub path: String,
    pub context: u32,
    pub description: String,
    #[serde(default)]
    pub mmproj: Option<String>,
}

/// A cloud model available via a cloud provider (e.g. Anthropic).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CloudModel {
    pub id: String,
    #[serde(default)]
    pub description: String,
}

/// A catalog of cloud models for a specific provider.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CloudModelCatalog {
    pub provider: String,
    pub models: Vec<CloudModel>,
}

/// Parsed model registry from the `llama-models.json` file.
///
/// Contains the default model name, the active model (user's last intentional
/// choice), and a map of model name -> entry.
/// Launch config and other top-level fields are intentionally ignored.
///
/// G3: `active` is interior-mutable so a `model.switch` (or an external edit
/// to llama-models.json picked up by the watcher) takes effect on the shared
/// `Arc<ModelRegistry>` without a gateway restart. Parsing goes through the
/// manual `from_json`, so no serde derives are needed on the whole struct.
#[derive(Debug)]
pub struct ModelRegistry {
    pub default: String,
    /// The user's last intentional model choice. Falls back to `default` if absent.
    pub active: std::sync::RwLock<Option<String>>,
    pub models: HashMap<String, ModelEntry>,
    /// Path to the source JSON file, used by `persist_active()`.
    pub source_path: Option<PathBuf>,
}

impl ModelRegistry {
    /// Load from a `llama-models.json` file.
    ///
    /// Tolerant of extra top-level keys (`launch`, `_comment`, etc.) — only
    /// reads `default`, `active`, and `models`.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let content = std::fs::read_to_string(path)?;
        let mut registry = Self::from_json(&content)?;
        registry.source_path = Some(path.to_path_buf());
        Ok(registry)
    }

    /// Parse from a JSON string (useful for tests).
    pub fn from_json(json: &str) -> anyhow::Result<Self> {
        let raw: serde_json::Value = serde_json::from_str(json)?;

        let default = raw["default"].as_str().unwrap_or("").to_string();
        let active = raw.get("active").and_then(|v| v.as_str()).map(String::from);

        let models_obj = raw.get("models").and_then(|m| m.as_object());

        let mut models = HashMap::new();
        if let Some(obj) = models_obj {
            for (name, value) in obj {
                match serde_json::from_value::<ModelEntry>(value.clone()) {
                    Ok(entry) => {
                        models.insert(name.clone(), entry);
                    }
                    Err(e) => {
                        tracing::warn!(
                            model = %name,
                            error = %e,
                            "Skipping malformed model entry"
                        );
                    }
                }
            }
        }

        Ok(Self {
            default,
            active: std::sync::RwLock::new(active),
            models,
            source_path: None,
        })
    }

    /// Returns the active model if set, otherwise the default.
    pub fn active_or_default(&self) -> String {
        self.active
            .read()
            .unwrap()
            .clone()
            .unwrap_or_else(|| self.default.clone())
    }

    /// G3: update the in-memory active model on the shared registry.
    pub fn set_active(&self, name: impl Into<String>) {
        *self.active.write().unwrap() = Some(name.into());
    }

    /// Atomically update the `"active"` field in llama-models.json.
    ///
    /// Reads the JSON, updates the `"active"` key, writes to a temp file,
    /// then renames over the original.
    pub fn persist_active(&self, name: &str) -> Result<(), Box<dyn std::error::Error>> {
        let path = self
            .source_path
            .as_ref()
            .ok_or("No source_path set on ModelRegistry — cannot persist")?;

        let content = std::fs::read_to_string(path)?;
        let mut raw: serde_json::Value = serde_json::from_str(&content)?;

        raw["active"] = serde_json::Value::String(name.to_string());

        let updated = serde_json::to_string_pretty(&raw)?;

        // Write to temp file in the same directory, then rename for atomicity
        let parent = path.parent().unwrap_or(Path::new("."));
        let tmp_path = parent.join(".llama-models.json.tmp");
        std::fs::write(&tmp_path, &updated)?;
        std::fs::rename(&tmp_path, path)?;

        // G3: the disk write succeeded — update the in-memory active model so
        // the running gateway resolves to the new model without a restart.
        self.set_active(name);

        Ok(())
    }

    /// List all model names (unordered).
    pub fn model_names(&self) -> Vec<&str> {
        self.models.keys().map(|s| s.as_str()).collect()
    }

    /// Get a specific model entry by name.
    pub fn get(&self, name: &str) -> Option<&ModelEntry> {
        self.models.get(name)
    }

    /// Number of registered models.
    pub fn len(&self) -> usize {
        self.models.len()
    }

    /// True if no models are registered.
    pub fn is_empty(&self) -> bool {
        self.models.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_JSON: &str = r#"{
        "_comment": "Test registry",
        "default": "qwen3-coder-abliterated",
        "active": "deepseek-r1:32b",
        "launch": {
            "host": "0.0.0.0",
            "port": 8080
        },
        "models": {
            "qwen3-coder": {
                "path": "C:\\Users\\deploy\\Models\\qwen3-coder.gguf",
                "context": 32768,
                "description": "Qwen3 Coder (primary coding model)"
            },
            "qwen3-vl:30b-a3b": {
                "path": "C:\\Users\\deploy\\Models\\qwen3-vl-30b-a3b.gguf",
                "mmproj": "C:\\Users\\deploy\\Models\\mmproj-qwen3-vl-30b-a3b.gguf",
                "context": 32768,
                "description": "Qwen3 VL 30B MoE (vision+text)"
            },
            "deepseek-r1:32b": {
                "path": "C:\\Users\\deploy\\Models\\deepseek-r1-32b.gguf",
                "context": 65536,
                "description": "DeepSeek R1 32B (reasoning)"
            }
        }
    }"#;

    #[test]
    fn test_parse_sample_registry() {
        let registry = ModelRegistry::from_json(SAMPLE_JSON).unwrap();
        assert_eq!(registry.default, "qwen3-coder-abliterated");
        assert_eq!(
            *registry.active.read().unwrap(),
            Some("deepseek-r1:32b".to_string())
        );
        assert_eq!(registry.len(), 3);
    }

    #[test]
    fn test_model_names() {
        let registry = ModelRegistry::from_json(SAMPLE_JSON).unwrap();
        let names = registry.model_names();
        assert!(names.contains(&"qwen3-coder"));
        assert!(names.contains(&"qwen3-vl:30b-a3b"));
        assert!(names.contains(&"deepseek-r1:32b"));
    }

    #[test]
    fn test_get_existing_model() {
        let registry = ModelRegistry::from_json(SAMPLE_JSON).unwrap();
        let entry = registry.get("qwen3-coder").unwrap();
        assert_eq!(entry.context, 32768);
        assert!(entry.description.contains("Coder"));
        assert!(entry.mmproj.is_none());
    }

    #[test]
    fn test_get_model_with_mmproj() {
        let registry = ModelRegistry::from_json(SAMPLE_JSON).unwrap();
        let entry = registry.get("qwen3-vl:30b-a3b").unwrap();
        assert!(entry.mmproj.is_some());
        assert!(entry.mmproj.as_ref().unwrap().contains("mmproj"));
    }

    #[test]
    fn test_get_nonexistent_model() {
        let registry = ModelRegistry::from_json(SAMPLE_JSON).unwrap();
        assert!(registry.get("nonexistent-model").is_none());
    }

    #[test]
    fn test_is_empty() {
        let empty_json = r#"{"default": "", "models": {}}"#;
        let registry = ModelRegistry::from_json(empty_json).unwrap();
        assert!(registry.is_empty());
        assert_eq!(registry.len(), 0);
    }

    #[test]
    fn test_active_or_default_returns_active() {
        let registry = ModelRegistry::from_json(SAMPLE_JSON).unwrap();
        assert_eq!(registry.active_or_default(), "deepseek-r1:32b");
    }

    #[test]
    fn test_active_or_default_falls_back_to_default() {
        let json = r#"{"default": "fallback-model", "models": {}}"#;
        let registry = ModelRegistry::from_json(json).unwrap();
        assert!(registry.active.read().unwrap().is_none());
        assert_eq!(registry.active_or_default(), "fallback-model");
    }

    #[test]
    fn test_persist_active_requires_source_path() {
        let registry = ModelRegistry::from_json(SAMPLE_JSON).unwrap();
        assert!(registry.source_path.is_none());
        let err = registry.persist_active("some-model");
        assert!(err.is_err());
        assert!(err.unwrap_err().to_string().contains("No source_path set"));
    }

    #[test]
    fn test_persist_active_writes_to_file() {
        let dir = std::env::temp_dir().join("ironjaw-test-persist-active");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("test-models.json");
        let initial = r#"{
            "default": "model-a",
            "active": "model-a",
            "models": {
                "model-a": {"path": "a.gguf", "context": 8192, "description": "A"},
                "model-b": {"path": "b.gguf", "context": 8192, "description": "B"}
            }
        }"#;
        std::fs::write(&path, initial).unwrap();

        let registry = ModelRegistry::load(&path).unwrap();
        assert_eq!(registry.active_or_default(), "model-a");

        registry.persist_active("model-b").unwrap();

        // G3: the ORIGINAL in-process registry must reflect the switch too,
        // without a reload from disk.
        assert_eq!(registry.active_or_default(), "model-b");

        // Re-read and verify
        let updated = ModelRegistry::load(&path).unwrap();
        assert_eq!(*updated.active.read().unwrap(), Some("model-b".to_string()));
        assert_eq!(updated.active_or_default(), "model-b");
        // default should be unchanged
        assert_eq!(updated.default, "model-a");

        // Cleanup
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_set_active_updates_active_or_default() {
        let registry = ModelRegistry::from_json(SAMPLE_JSON).unwrap();
        assert_eq!(registry.active_or_default(), "deepseek-r1:32b");
        registry.set_active("qwen3-coder");
        assert_eq!(registry.active_or_default(), "qwen3-coder");
    }

    #[test]
    fn test_set_active_works_through_shared_arc() {
        // The gateway shares one Arc<ModelRegistry> between the provider,
        // router, and handlers — a switch through one clone must be visible
        // through the others.
        let registry = std::sync::Arc::new(ModelRegistry::from_json(SAMPLE_JSON).unwrap());
        let clone = registry.clone();
        clone.set_active("qwen3-vl:30b-a3b");
        assert_eq!(registry.active_or_default(), "qwen3-vl:30b-a3b");
    }

    #[test]
    fn test_missing_default() {
        let json =
            r#"{"models": {"m1": {"path": "a.gguf", "context": 8192, "description": "test"}}}"#;
        let registry = ModelRegistry::from_json(json).unwrap();
        assert_eq!(registry.default, "");
        assert_eq!(registry.len(), 1);
    }

    #[test]
    fn test_missing_models_key() {
        let json = r#"{"default": "something"}"#;
        let registry = ModelRegistry::from_json(json).unwrap();
        assert!(registry.is_empty());
        assert_eq!(registry.default, "something");
    }

    #[test]
    fn test_malformed_entry_skipped() {
        // Missing required "context" field should skip that entry
        let json = r#"{
            "default": "ok-model",
            "models": {
                "ok-model": {"path": "a.gguf", "context": 8192, "description": "ok"},
                "bad-model": {"path": "b.gguf", "description": "missing context"}
            }
        }"#;
        let registry = ModelRegistry::from_json(json).unwrap();
        assert_eq!(registry.len(), 1);
        assert!(registry.get("ok-model").is_some());
        assert!(registry.get("bad-model").is_none());
    }

    #[test]
    fn test_load_actual_file() {
        // Load the real llama-models.json from disk
        let path = Path::new("C:\\Users\\deploy\\Scripts\\llama-models.json");
        if path.exists() {
            let registry = ModelRegistry::load(path).unwrap();
            assert!(!registry.default.is_empty());
            assert!(!registry.is_empty());
            // Should have the default model in the registry
            assert!(registry.get(&registry.default).is_some());
            // source_path should be set when loaded from file
            assert!(registry.source_path.is_some());
        }
    }

    #[test]
    fn test_load_nonexistent_file() {
        let path = Path::new("/nonexistent/path/llama-models.json");
        assert!(ModelRegistry::load(path).is_err());
    }

    #[test]
    fn test_cloud_model_struct() {
        let model = CloudModel {
            id: "claude-sonnet-4-6".to_string(),
            description: "Fast and balanced".to_string(),
        };
        assert_eq!(model.id, "claude-sonnet-4-6");
        assert_eq!(model.description, "Fast and balanced");
    }

    #[test]
    fn test_cloud_model_catalog() {
        let catalog = CloudModelCatalog {
            provider: "anthropic".to_string(),
            models: vec![
                CloudModel {
                    id: "claude-sonnet-4-6".to_string(),
                    description: "Fast".to_string(),
                },
                CloudModel {
                    id: "claude-opus-4-6".to_string(),
                    description: "Capable".to_string(),
                },
            ],
        };
        assert_eq!(catalog.provider, "anthropic");
        assert_eq!(catalog.models.len(), 2);
        assert_eq!(catalog.models[0].id, "claude-sonnet-4-6");
    }

    #[test]
    fn test_cloud_model_serialization() {
        let model = CloudModel {
            id: "claude-haiku-4-5".to_string(),
            description: "Fastest".to_string(),
        };
        let json = serde_json::to_string(&model).unwrap();
        let parsed: CloudModel = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.id, model.id);
        assert_eq!(parsed.description, model.description);
    }

    #[test]
    fn test_cloud_model_catalog_serialization() {
        let catalog = CloudModelCatalog {
            provider: "anthropic".to_string(),
            models: vec![CloudModel {
                id: "test-model".to_string(),
                description: "Test".to_string(),
            }],
        };
        let json = serde_json::to_string(&catalog).unwrap();
        let parsed: CloudModelCatalog = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.provider, "anthropic");
        assert_eq!(parsed.models.len(), 1);
    }

    #[test]
    fn test_cloud_model_empty_description_default() {
        let json = r#"{"id": "test-model"}"#;
        let model: CloudModel = serde_json::from_str(json).unwrap();
        assert_eq!(model.id, "test-model");
        assert_eq!(model.description, "");
    }

    #[test]
    fn test_extra_fields_ignored() {
        // Extra fields in model entries should be silently ignored by serde
        let json = r#"{
            "default": "m1",
            "launch": {"host": "0.0.0.0", "port": 8080, "threads": 12},
            "_comment": "ignored",
            "models": {
                "m1": {
                    "path": "a.gguf",
                    "context": 8192,
                    "description": "test",
                    "extra_field": "should be ignored"
                }
            }
        }"#;
        let registry = ModelRegistry::from_json(json).unwrap();
        assert_eq!(registry.len(), 1);
        assert!(registry.get("m1").is_some());
    }
}
