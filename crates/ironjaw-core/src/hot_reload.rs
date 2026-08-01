use crate::config::{Config, LoggingConfig, SecurityConfig, ToolsConfig};
use std::path::PathBuf;
use std::time::{Duration, SystemTime};
use tokio::sync::mpsc;
use tracing::{info, warn};

/// Configuration for the hot-reload watcher.
#[derive(Debug, Clone)]
pub struct HotReloadConfig {
    /// Whether hot-reload is enabled.
    pub enabled: bool,
    /// How often to poll the config file for changes (seconds).
    pub poll_interval_secs: u64,
    /// Path to the config file to watch.
    pub config_path: PathBuf,
}

impl Default for HotReloadConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            poll_interval_secs: 5,
            config_path: PathBuf::from("config/ironjaw.toml"),
        }
    }
}

/// The subset of Config that can be safely reloaded at runtime.
///
/// Non-reloadable sections (require restart):
/// - `[server]` (host, port)
/// - `[auth]` (token)
/// - `[llm]` (providers)
#[derive(Debug, Clone, PartialEq)]
pub struct ReloadableConfig {
    pub security: SecurityConfig,
    pub brain: ReloadableBrainConfig,
    pub tools: ToolsConfig,
    pub logging: LoggingConfig,
}

/// Only the brain fields that are safe to reload at runtime.
/// Excludes database_path, embedding_url, embedding_model (require restart).
#[derive(Debug, Clone, PartialEq)]
pub struct ReloadableBrainConfig {
    pub auto_recall_top_k: usize,
    pub max_memories: usize,
    pub auto_learn: bool,
    pub learn_dedup_threshold: f32,
    pub learn_after_turns: usize,
}

impl ReloadableConfig {
    /// Extract reloadable fields from a full Config.
    pub fn from_config(config: &Config) -> Self {
        Self {
            security: config.security.clone(),
            brain: ReloadableBrainConfig {
                auto_recall_top_k: config.brain.auto_recall_top_k,
                max_memories: config.brain.max_memories,
                auto_learn: config.brain.auto_learn,
                learn_dedup_threshold: config.brain.learn_dedup_threshold,
                learn_after_turns: config.brain.learn_after_turns,
            },
            tools: config.tools.clone(),
            logging: config.logging.clone(),
        }
    }
}

/// Watches a config file for changes by polling its modification time.
pub struct HotReloadWatcher {
    config_path: PathBuf,
    poll_interval: Duration,
    last_mtime: Option<SystemTime>,
}

impl HotReloadWatcher {
    pub fn new(config: HotReloadConfig) -> Self {
        Self {
            config_path: config.config_path,
            poll_interval: Duration::from_secs(config.poll_interval_secs),
            last_mtime: None,
        }
    }

    /// Read the file's modification time. Returns None on error.
    fn file_mtime(path: &PathBuf) -> Option<SystemTime> {
        std::fs::metadata(path).and_then(|m| m.modified()).ok()
    }

    /// Start the polling loop. Calls `on_change` whenever the config
    /// file's mtime changes and the new content parses successfully.
    ///
    /// Returns a JoinHandle for the background task and a shutdown sender.
    pub fn start<F>(mut self, on_change: F) -> (tokio::task::JoinHandle<()>, mpsc::Sender<()>)
    where
        F: Fn(ReloadableConfig) + Send + 'static,
    {
        let (shutdown_tx, mut shutdown_rx) = mpsc::channel::<()>(1);

        // Capture initial mtime so we don't fire on first poll
        self.last_mtime = Self::file_mtime(&self.config_path);

        let handle = tokio::spawn(async move {
            let mut interval = tokio::time::interval(self.poll_interval);
            // Skip the first tick (fires immediately)
            interval.tick().await;

            info!(
                path = %self.config_path.display(),
                poll_secs = self.poll_interval.as_secs(),
                "Hot-reload watcher started"
            );

            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        self.poll_once(&on_change);
                    }
                    _ = shutdown_rx.recv() => {
                        info!("Hot-reload watcher shutting down");
                        break;
                    }
                }
            }
        });

        (handle, shutdown_tx)
    }

    /// Single poll iteration: check mtime, parse if changed, call callback.
    fn poll_once<F>(&mut self, on_change: &F)
    where
        F: Fn(ReloadableConfig),
    {
        let current_mtime = match Self::file_mtime(&self.config_path) {
            Some(t) => t,
            None => {
                warn!(
                    path = %self.config_path.display(),
                    "Hot-reload: could not read file mtime"
                );
                return;
            }
        };

        let changed = match self.last_mtime {
            Some(last) => current_mtime != last,
            None => true,
        };

        if !changed {
            return;
        }

        self.last_mtime = Some(current_mtime);

        // File changed — attempt to parse
        let content = match std::fs::read_to_string(&self.config_path) {
            Ok(c) => c,
            Err(e) => {
                warn!(
                    path = %self.config_path.display(),
                    error = %e,
                    "Hot-reload: could not read config file"
                );
                return;
            }
        };

        let config: Config = match toml::from_str(&content) {
            Ok(c) => c,
            Err(e) => {
                warn!(
                    path = %self.config_path.display(),
                    error = %e,
                    "Hot-reload: invalid TOML in config file"
                );
                return;
            }
        };

        let reloadable = ReloadableConfig::from_config(&config);
        info!(
            path = %self.config_path.display(),
            "Hot-reload: config changed, applying reloadable sections"
        );
        on_change(reloadable);
    }
}

/// G3: watches llama-models.json for external edits (e.g. `llama-switch` run
/// from a shell) by polling its mtime. On a successful parse it reports the
/// new active-or-default model name so the gateway can retarget the in-memory
/// registry and router default without a restart.
///
/// Independent of [`HotReloadWatcher`], which is hard-wired to the TOML
/// `Config`; only the polling skeleton is mirrored.
pub struct ModelRegistryWatcher {
    path: PathBuf,
    poll_interval: Duration,
    last_mtime: Option<SystemTime>,
}

impl ModelRegistryWatcher {
    pub fn new(path: PathBuf, poll_interval_secs: u64) -> Self {
        Self {
            path,
            poll_interval: Duration::from_secs(poll_interval_secs),
            last_mtime: None,
        }
    }

    /// Start the polling loop. Calls `on_change(active_or_default)` whenever
    /// the file's mtime changes and the new content parses successfully.
    ///
    /// Returns a JoinHandle for the background task and a shutdown sender.
    /// NOTE: dropping the shutdown sender stops the watcher (recv() yields
    /// None) — keep it alive for the watcher's intended lifetime.
    pub fn start<F>(mut self, on_change: F) -> (tokio::task::JoinHandle<()>, mpsc::Sender<()>)
    where
        F: Fn(String) + Send + 'static,
    {
        let (shutdown_tx, mut shutdown_rx) = mpsc::channel::<()>(1);

        // Capture initial mtime so we don't fire on first poll
        self.last_mtime = HotReloadWatcher::file_mtime(&self.path);

        let handle = tokio::spawn(async move {
            let mut interval = tokio::time::interval(self.poll_interval);
            // Skip the first tick (fires immediately)
            interval.tick().await;

            info!(
                path = %self.path.display(),
                poll_secs = self.poll_interval.as_secs(),
                "Model registry watcher started"
            );

            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        self.poll_once(&on_change);
                    }
                    _ = shutdown_rx.recv() => {
                        info!("Model registry watcher shutting down");
                        break;
                    }
                }
            }
        });

        (handle, shutdown_tx)
    }

    /// Single poll iteration: check mtime, parse if changed, call callback.
    fn poll_once<F>(&mut self, on_change: &F)
    where
        F: Fn(String),
    {
        let current_mtime = match HotReloadWatcher::file_mtime(&self.path) {
            Some(t) => t,
            // Missing/unreadable file — keep the current model, no noise.
            None => return,
        };

        let changed = match self.last_mtime {
            Some(last) => current_mtime != last,
            None => true,
        };
        if !changed {
            return;
        }
        self.last_mtime = Some(current_mtime);

        match ironjaw_llm::ModelRegistry::load(&self.path) {
            Ok(registry) => {
                let active = registry.active_or_default();
                info!(
                    path = %self.path.display(),
                    active = %active,
                    "Model registry changed on disk — applying new active model"
                );
                on_change(active);
            }
            Err(e) => {
                warn!(
                    path = %self.path.display(),
                    error = %e,
                    "Model registry watcher: invalid JSON, keeping current model"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as IoWrite;

    fn minimal_toml() -> &'static str {
        r#"
[server]
host = "127.0.0.1"
port = 18794

[auth]
token = "test-token"

[audit]
database_path = "data/ironjaw.db"

[logging]
level = "info"
"#
    }

    fn write_config(dir: &std::path::Path, content: &str) -> PathBuf {
        let path = dir.join("hot_reload_test.toml");
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(content.as_bytes()).unwrap();
        f.sync_all().unwrap();
        path
    }

    #[test]
    fn test_hot_reload_config_defaults() {
        let cfg = HotReloadConfig::default();
        assert!(!cfg.enabled);
        assert_eq!(cfg.poll_interval_secs, 5);
        assert_eq!(cfg.config_path, PathBuf::from("config/ironjaw.toml"));
    }

    #[test]
    fn test_reloadable_config_extraction() {
        let toml_content = r#"
[server]
host = "127.0.0.1"
port = 18794

[auth]
token = "test-token"

[audit]
database_path = "data/ironjaw.db"

[logging]
level = "debug"
format = "json"

[security]
hitl_timeout_secs = 60
rate_limit_enabled = true
sanitize_enabled = false

[brain]
auto_recall_top_k = 5
max_memories = 5000

[tools]
working_dir = "/tmp"
shell_timeout_secs = 10
"#;
        let config: Config = toml::from_str(toml_content).unwrap();
        let reloadable = ReloadableConfig::from_config(&config);

        assert_eq!(reloadable.security.hitl_timeout_secs, 60);
        assert!(reloadable.security.rate_limit_enabled);
        assert!(!reloadable.security.sanitize_enabled);
        assert_eq!(reloadable.brain.auto_recall_top_k, 5);
        assert_eq!(reloadable.brain.max_memories, 5000);
        assert_eq!(reloadable.tools.working_dir, "/tmp");
        assert_eq!(reloadable.tools.shell_timeout_secs, 10);
        assert_eq!(reloadable.logging.level, "debug");
        assert_eq!(reloadable.logging.format, "json");
    }

    #[test]
    fn test_reloadable_config_defaults_when_absent() {
        let config: Config = toml::from_str(minimal_toml()).unwrap();
        let reloadable = ReloadableConfig::from_config(&config);

        // Security defaults
        assert_eq!(reloadable.security.hitl_timeout_secs, 30);
        assert!(!reloadable.security.rate_limit_enabled);
        assert!(reloadable.security.sanitize_enabled);
        // Brain defaults
        assert_eq!(reloadable.brain.auto_recall_top_k, 3);
        assert_eq!(reloadable.brain.max_memories, 10_000);
        // Tools defaults
        assert_eq!(reloadable.tools.working_dir, ".");
        assert_eq!(reloadable.tools.shell_timeout_secs, 30);
        // Logging
        assert_eq!(reloadable.logging.level, "info");
        assert_eq!(reloadable.logging.format, "pretty");
    }

    #[test]
    fn test_watcher_detects_mtime_change() {
        let dir = std::env::temp_dir().join("ironjaw-hot-reload-test-mtime");
        std::fs::create_dir_all(&dir).unwrap();
        let path = write_config(&dir, minimal_toml());

        let cfg = HotReloadConfig {
            enabled: true,
            poll_interval_secs: 1,
            config_path: path.clone(),
        };
        let mut watcher = HotReloadWatcher::new(cfg);

        // Initialize mtime
        watcher.last_mtime = HotReloadWatcher::file_mtime(&path);

        // Same mtime — callback should NOT fire
        let fired = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let fired_clone = fired.clone();
        watcher.poll_once(&move |_| {
            fired_clone.store(true, std::sync::atomic::Ordering::SeqCst);
        });
        assert!(!fired.load(std::sync::atomic::Ordering::SeqCst));

        // Touch the file to change mtime
        std::thread::sleep(std::time::Duration::from_millis(50));
        write_config(&dir, minimal_toml());

        let fired2 = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let fired2_clone = fired2.clone();
        watcher.poll_once(&move |_| {
            fired2_clone.store(true, std::sync::atomic::Ordering::SeqCst);
        });
        assert!(fired2.load(std::sync::atomic::Ordering::SeqCst));

        // Cleanup
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_watcher_bad_toml_does_not_crash() {
        let dir = std::env::temp_dir().join("ironjaw-hot-reload-test-bad");
        std::fs::create_dir_all(&dir).unwrap();
        let path = write_config(&dir, "this is not valid toml {{{{");

        let cfg = HotReloadConfig {
            enabled: true,
            poll_interval_secs: 1,
            config_path: path,
        };
        let mut watcher = HotReloadWatcher::new(cfg);
        // last_mtime = None, so it will see a "change"

        let fired = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let fired_clone = fired.clone();
        watcher.poll_once(&move |_| {
            fired_clone.store(true, std::sync::atomic::Ordering::SeqCst);
        });
        // Callback should NOT fire because TOML is invalid
        assert!(!fired.load(std::sync::atomic::Ordering::SeqCst));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_watcher_missing_file_does_not_crash() {
        let cfg = HotReloadConfig {
            enabled: true,
            poll_interval_secs: 1,
            config_path: PathBuf::from("/nonexistent/path/config.toml"),
        };
        let mut watcher = HotReloadWatcher::new(cfg);

        let fired = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let fired_clone = fired.clone();
        watcher.poll_once(&move |_| {
            fired_clone.store(true, std::sync::atomic::Ordering::SeqCst);
        });
        assert!(!fired.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[test]
    fn test_reloadable_config_equality() {
        let config: Config = toml::from_str(minimal_toml()).unwrap();
        let a = ReloadableConfig::from_config(&config);
        let b = ReloadableConfig::from_config(&config);
        assert_eq!(a, b);
    }

    #[tokio::test]
    async fn test_watcher_start_and_shutdown() {
        let dir = std::env::temp_dir().join("ironjaw-hot-reload-test-start");
        std::fs::create_dir_all(&dir).unwrap();
        let path = write_config(&dir, minimal_toml());

        let cfg = HotReloadConfig {
            enabled: true,
            poll_interval_secs: 1,
            config_path: path,
        };
        let watcher = HotReloadWatcher::new(cfg);

        let (handle, shutdown_tx) = watcher.start(|_cfg| {
            // no-op callback
        });

        // Let it run briefly
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Shutdown
        shutdown_tx.send(()).await.unwrap();
        handle.await.unwrap();

        std::fs::remove_dir_all(&dir).ok();
    }

    fn models_json(active: &str) -> String {
        format!(
            r#"{{
                "default": "model-a",
                "active": "{active}",
                "models": {{
                    "model-a": {{"path": "a.gguf", "context": 8192, "description": "A"}},
                    "model-b": {{"path": "b.gguf", "context": 8192, "description": "B"}}
                }}
            }}"#
        )
    }

    fn write_models(dir: &std::path::Path, content: &str) -> PathBuf {
        let path = dir.join("llama-models.json");
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(content.as_bytes()).unwrap();
        f.sync_all().unwrap();
        path
    }

    #[test]
    fn test_model_registry_watcher_detects_active_change() {
        let dir = std::env::temp_dir().join("ironjaw-model-watcher-test-change");
        std::fs::create_dir_all(&dir).unwrap();
        let path = write_models(&dir, &models_json("model-a"));

        let mut watcher = ModelRegistryWatcher::new(path.clone(), 1);
        watcher.last_mtime = HotReloadWatcher::file_mtime(&path);

        // Same mtime — callback should NOT fire
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let seen_clone = seen.clone();
        watcher.poll_once(&move |active| {
            seen_clone.lock().unwrap().push(active);
        });
        assert!(seen.lock().unwrap().is_empty());

        // External edit switches active to model-b
        std::thread::sleep(std::time::Duration::from_millis(50));
        write_models(&dir, &models_json("model-b"));

        let seen2 = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let seen2_clone = seen2.clone();
        watcher.poll_once(&move |active| {
            seen2_clone.lock().unwrap().push(active);
        });
        assert_eq!(*seen2.lock().unwrap(), vec!["model-b".to_string()]);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_model_registry_watcher_ignores_malformed_json() {
        let dir = std::env::temp_dir().join("ironjaw-model-watcher-test-bad");
        std::fs::create_dir_all(&dir).unwrap();
        let path = write_models(&dir, "not json at all {{{{");

        let mut watcher = ModelRegistryWatcher::new(path, 1);
        // last_mtime = None → sees a "change" on first poll

        let fired = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let fired_clone = fired.clone();
        watcher.poll_once(&move |_| {
            fired_clone.store(true, std::sync::atomic::Ordering::SeqCst);
        });
        assert!(!fired.load(std::sync::atomic::Ordering::SeqCst));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_model_registry_watcher_missing_file_does_not_crash() {
        let mut watcher =
            ModelRegistryWatcher::new(PathBuf::from("/nonexistent/llama-models.json"), 1);

        let fired = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let fired_clone = fired.clone();
        watcher.poll_once(&move |_| {
            fired_clone.store(true, std::sync::atomic::Ordering::SeqCst);
        });
        assert!(!fired.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[tokio::test]
    async fn test_watcher_fires_callback_on_file_change() {
        let dir = std::env::temp_dir().join("ironjaw-hot-reload-test-fire");
        std::fs::create_dir_all(&dir).unwrap();
        let path = write_config(&dir, minimal_toml());

        let cfg = HotReloadConfig {
            enabled: true,
            poll_interval_secs: 1,
            config_path: path.clone(),
        };
        let watcher = HotReloadWatcher::new(cfg);

        let fired = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let fired_clone = fired.clone();

        let (handle, shutdown_tx) = watcher.start(move |_cfg| {
            fired_clone.store(true, std::sync::atomic::Ordering::SeqCst);
        });

        // Wait for initial tick to pass, then modify file
        tokio::time::sleep(Duration::from_millis(500)).await;

        // Rewrite the config with a different value to ensure mtime changes
        let modified = minimal_toml().replace("info", "debug");
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(modified.as_bytes()).unwrap();
        f.sync_all().unwrap();

        // Wait for the watcher to detect the change
        tokio::time::sleep(Duration::from_millis(2000)).await;

        assert!(fired.load(std::sync::atomic::Ordering::SeqCst));

        shutdown_tx.send(()).await.unwrap();
        handle.await.unwrap();

        std::fs::remove_dir_all(&dir).ok();
    }
}
