use super::{MigrationError, MigrationReport};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// ── OpenClaw source config ───────────────────────────────

/// Represents the OpenClaw `openclaw.json` configuration structure.
#[derive(Debug, Clone, Deserialize)]
pub struct OpenClawConfig {
    #[serde(default)]
    pub agents: Vec<OpenClawAgent>,
    #[serde(default)]
    pub tools: HashMap<String, OpenClawTool>,
    #[serde(default)]
    pub providers: HashMap<String, OpenClawProvider>,
    #[serde(default)]
    pub general: Option<OpenClawGeneral>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct OpenClawAgent {
    pub name: String,
    #[serde(default)]
    pub tools: Vec<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub system_prompt: Option<String>,
    #[serde(default)]
    pub timeout_seconds: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct OpenClawTool {
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub timeout: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct OpenClawProvider {
    #[serde(rename = "type", default)]
    pub provider_type: Option<String>,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default)]
    pub models: Vec<OpenClawModel>,
    #[serde(default)]
    pub timeout_secs: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct OpenClawModel {
    pub name: String,
    #[serde(default)]
    pub context_length: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct OpenClawGeneral {
    #[serde(default)]
    pub host: Option<String>,
    #[serde(default)]
    pub port: Option<u16>,
    #[serde(default)]
    pub log_level: Option<String>,
}

// ── Ironjaw target config ────────────────────────────────

/// Ironjaw target config, serializable to TOML.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IronjawConfig {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sessions: Vec<IronjawSession>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub integrations: HashMap<String, IronjawIntegration>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub llm_providers: Vec<IronjawLlmProvider>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server: Option<IronjawServer>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logging: Option<IronjawLogging>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IronjawSession {
    pub name: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub integration_refs: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_seconds: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IronjawIntegration {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default)]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IronjawLlmProvider {
    pub name: String,
    #[serde(rename = "type")]
    pub provider_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IronjawServer {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IronjawLogging {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub level: Option<String>,
}

// ── Parsing ──────────────────────────────────────────────

/// Parse an OpenClaw JSON config string.
pub fn parse_openclaw_config(json_str: &str) -> Result<OpenClawConfig, MigrationError> {
    let config: OpenClawConfig = serde_json::from_str(json_str)?;
    Ok(config)
}

// ── Migration logic ──────────────────────────────────────

/// Transform an OpenClaw config into an Ironjaw config.
pub fn migrate_config(oc: &OpenClawConfig) -> (IronjawConfig, MigrationReport) {
    let mut report = MigrationReport::new();

    // agents → sessions
    let sessions: Vec<IronjawSession> = oc
        .agents
        .iter()
        .map(|agent| IronjawSession {
            name: agent.name.clone(),
            integration_refs: agent.tools.clone(),
            model: agent.model.clone(),
            system_prompt: agent.system_prompt.clone(),
            timeout_seconds: agent.timeout_seconds,
        })
        .collect();

    // tools → integrations
    let integrations: HashMap<String, IronjawIntegration> = oc
        .tools
        .iter()
        .map(|(name, tool)| {
            let integration = IronjawIntegration {
                description: tool.description.clone(),
                command: tool.command.clone(),
                enabled: tool.enabled.unwrap_or(true),
                timeout: tool.timeout,
            };
            (name.clone(), integration)
        })
        .collect();

    // providers → llm_providers
    let llm_providers: Vec<IronjawLlmProvider> = oc
        .providers
        .iter()
        .map(|(name, provider)| {
            let provider_type = provider
                .provider_type
                .clone()
                .unwrap_or_else(|| "llama-server".to_string());

            if provider_type != "llama-server" && provider_type != "openai" {
                report.warnings.push(format!(
                    "Provider '{name}' has unknown type '{provider_type}', defaulting to llama-server"
                ));
            }

            IronjawLlmProvider {
                name: name.clone(),
                provider_type,
                base_url: provider.base_url.clone(),
                api_key: provider.api_key.clone(),
                models: provider.models.iter().map(|m| m.name.clone()).collect(),
                timeout_secs: provider.timeout_secs,
            }
        })
        .collect();

    // general → server + logging
    let (server, logging) = if let Some(general) = &oc.general {
        let srv = if general.host.is_some() || general.port.is_some() {
            Some(IronjawServer {
                host: general.host.clone(),
                port: general.port,
            })
        } else {
            None
        };
        let log = general.log_level.as_ref().map(|level| IronjawLogging {
            level: Some(level.clone()),
        });
        (srv, log)
    } else {
        (None, None)
    };

    let config = IronjawConfig {
        sessions,
        integrations,
        llm_providers,
        server,
        logging,
    };

    (config, report)
}

// ── Serialization ────────────────────────────────────────

/// Serialize an Ironjaw config to a TOML string.
pub fn to_toml(config: &IronjawConfig) -> Result<String, MigrationError> {
    let toml_str = toml::to_string_pretty(config)?;
    Ok(toml_str)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn full_openclaw_json() -> &'static str {
        r#"{
            "agents": [
                {
                    "name": "coder",
                    "tools": ["shell", "read_file"],
                    "model": "qwen3-coder",
                    "system_prompt": "You are a coding assistant.",
                    "timeout_seconds": 300
                },
                {
                    "name": "researcher",
                    "tools": ["web_search"],
                    "model": "qwen3:30b-a3b"
                }
            ],
            "tools": {
                "shell": {
                    "description": "Execute shell commands",
                    "command": "bash",
                    "enabled": true,
                    "timeout": 30
                },
                "read_file": {
                    "description": "Read file contents",
                    "enabled": true
                },
                "web_search": {
                    "description": "Search the web",
                    "enabled": false,
                    "timeout": 60
                }
            },
            "providers": {
                "llama-local": {
                    "type": "llama-server",
                    "base_url": "http://localhost:8080",
                    "models": [
                        {"name": "qwen3-coder", "context_length": 32768},
                        {"name": "qwen3:30b-a3b"}
                    ],
                    "timeout_secs": 900
                },
                "openai-cloud": {
                    "type": "openai",
                    "base_url": "https://api.openai.com/v1",
                    "api_key": "sk-test-key",
                    "models": [{"name": "gpt-4o"}]
                }
            },
            "general": {
                "host": "0.0.0.0",
                "port": 18789,
                "log_level": "info"
            }
        }"#
    }

    #[test]
    fn test_parse_full_config() {
        let oc = parse_openclaw_config(full_openclaw_json()).unwrap();
        assert_eq!(oc.agents.len(), 2);
        assert_eq!(oc.tools.len(), 3);
        assert_eq!(oc.providers.len(), 2);
        assert!(oc.general.is_some());
    }

    #[test]
    fn test_parse_minimal_config() {
        let json = r#"{"agents": [], "tools": {}, "providers": {}}"#;
        let oc = parse_openclaw_config(json).unwrap();
        assert!(oc.agents.is_empty());
        assert!(oc.tools.is_empty());
        assert!(oc.providers.is_empty());
        assert!(oc.general.is_none());
    }

    #[test]
    fn test_parse_empty_object() {
        let oc = parse_openclaw_config("{}").unwrap();
        assert!(oc.agents.is_empty());
    }

    #[test]
    fn test_parse_invalid_json() {
        assert!(parse_openclaw_config("not json").is_err());
    }

    #[test]
    fn test_migrate_agents_to_sessions() {
        let oc = parse_openclaw_config(full_openclaw_json()).unwrap();
        let (config, report) = migrate_config(&oc);
        assert!(report.success);
        assert_eq!(config.sessions.len(), 2);

        let coder = &config.sessions[0];
        assert_eq!(coder.name, "coder");
        assert_eq!(coder.integration_refs, vec!["shell", "read_file"]);
        assert_eq!(coder.model.as_deref(), Some("qwen3-coder"));
        assert_eq!(
            coder.system_prompt.as_deref(),
            Some("You are a coding assistant.")
        );
        assert_eq!(coder.timeout_seconds, Some(300));

        let researcher = &config.sessions[1];
        assert_eq!(researcher.name, "researcher");
        assert_eq!(researcher.integration_refs, vec!["web_search"]);
        assert!(researcher.system_prompt.is_none());
    }

    #[test]
    fn test_migrate_tools_to_integrations() {
        let oc = parse_openclaw_config(full_openclaw_json()).unwrap();
        let (config, _) = migrate_config(&oc);
        assert_eq!(config.integrations.len(), 3);

        let shell = config.integrations.get("shell").unwrap();
        assert_eq!(shell.description.as_deref(), Some("Execute shell commands"));
        assert_eq!(shell.command.as_deref(), Some("bash"));
        assert!(shell.enabled);
        assert_eq!(shell.timeout, Some(30));

        let web = config.integrations.get("web_search").unwrap();
        assert!(!web.enabled);
    }

    #[test]
    fn test_migrate_providers_to_llm_providers() {
        let oc = parse_openclaw_config(full_openclaw_json()).unwrap();
        let (config, _) = migrate_config(&oc);
        assert_eq!(config.llm_providers.len(), 2);

        let llama = config
            .llm_providers
            .iter()
            .find(|p| p.name == "llama-local")
            .unwrap();
        assert_eq!(llama.provider_type, "llama-server");
        assert_eq!(llama.base_url.as_deref(), Some("http://localhost:8080"));
        assert_eq!(llama.models, vec!["qwen3-coder", "qwen3:30b-a3b"]);
        assert_eq!(llama.timeout_secs, Some(900));

        let openai = config
            .llm_providers
            .iter()
            .find(|p| p.name == "openai-cloud")
            .unwrap();
        assert_eq!(openai.provider_type, "openai");
        assert_eq!(openai.api_key.as_deref(), Some("sk-test-key"));
        assert_eq!(openai.models, vec!["gpt-4o"]);
    }

    #[test]
    fn test_migrate_general_to_server_and_logging() {
        let oc = parse_openclaw_config(full_openclaw_json()).unwrap();
        let (config, _) = migrate_config(&oc);

        let server = config.server.as_ref().unwrap();
        assert_eq!(server.host.as_deref(), Some("0.0.0.0"));
        assert_eq!(server.port, Some(18789));

        let logging = config.logging.as_ref().unwrap();
        assert_eq!(logging.level.as_deref(), Some("info"));
    }

    #[test]
    fn test_migrate_no_general() {
        let json = r#"{"agents": [], "tools": {}, "providers": {}}"#;
        let oc = parse_openclaw_config(json).unwrap();
        let (config, _) = migrate_config(&oc);
        assert!(config.server.is_none());
        assert!(config.logging.is_none());
    }

    #[test]
    fn test_to_toml_roundtrip() {
        let oc = parse_openclaw_config(full_openclaw_json()).unwrap();
        let (config, _) = migrate_config(&oc);
        let toml_str = to_toml(&config).unwrap();
        assert!(!toml_str.is_empty());
        // Should be valid TOML
        let _parsed: toml::Value = toml::from_str(&toml_str).unwrap();
    }

    #[test]
    fn test_to_toml_contains_expected_sections() {
        let oc = parse_openclaw_config(full_openclaw_json()).unwrap();
        let (config, _) = migrate_config(&oc);
        let toml_str = to_toml(&config).unwrap();
        assert!(toml_str.contains("[[sessions]]"));
        assert!(toml_str.contains("[[llm_providers]]"));
        assert!(toml_str.contains("[integrations."));
        assert!(toml_str.contains("[server]"));
        assert!(toml_str.contains("[logging]"));
    }

    #[test]
    fn test_unknown_provider_type_warning() {
        let json = r#"{
            "agents": [],
            "tools": {},
            "providers": {
                "custom": {
                    "type": "custom-backend",
                    "base_url": "http://localhost:9999",
                    "models": []
                }
            }
        }"#;
        let oc = parse_openclaw_config(json).unwrap();
        let (_, report) = migrate_config(&oc);
        assert_eq!(report.warnings.len(), 1);
        assert!(report.warnings[0].contains("custom-backend"));
    }

    #[test]
    fn test_tool_default_enabled() {
        let json = r#"{
            "agents": [],
            "tools": {"my_tool": {"description": "A tool"}},
            "providers": {}
        }"#;
        let oc = parse_openclaw_config(json).unwrap();
        let (config, _) = migrate_config(&oc);
        let tool = config.integrations.get("my_tool").unwrap();
        assert!(tool.enabled); // default true when not specified
    }

    #[test]
    fn test_provider_default_type() {
        let json = r#"{
            "agents": [],
            "tools": {},
            "providers": {
                "local": {
                    "base_url": "http://localhost:8080",
                    "models": []
                }
            }
        }"#;
        let oc = parse_openclaw_config(json).unwrap();
        let (config, _) = migrate_config(&oc);
        assert_eq!(config.llm_providers[0].provider_type, "llama-server");
    }

    #[test]
    fn test_agent_with_no_optional_fields() {
        let json = r#"{
            "agents": [{"name": "bare"}],
            "tools": {},
            "providers": {}
        }"#;
        let oc = parse_openclaw_config(json).unwrap();
        let (config, _) = migrate_config(&oc);
        let session = &config.sessions[0];
        assert_eq!(session.name, "bare");
        assert!(session.integration_refs.is_empty());
        assert!(session.model.is_none());
        assert!(session.system_prompt.is_none());
        assert!(session.timeout_seconds.is_none());
    }

    #[test]
    fn test_empty_config_produces_valid_toml() {
        let oc = parse_openclaw_config("{}").unwrap();
        let (config, report) = migrate_config(&oc);
        assert!(report.success);
        let toml_str = to_toml(&config).unwrap();
        // Should produce empty or minimal TOML
        let _parsed: toml::Value = toml::from_str(&toml_str).unwrap();
    }
}
