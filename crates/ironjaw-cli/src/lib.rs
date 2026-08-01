pub mod commands;

use clap::{Parser, Subcommand};

/// Ironjaw CLI client error type.
#[derive(Debug, thiserror::Error)]
pub enum CliError {
    #[error("HTTP error: {0}")]
    Http(String),
    #[error("Auth required: provide --token or set IRONJAW_TOKEN")]
    AuthRequired,
    #[error("Server error: {0}")]
    Server(String),
    #[error("Parse error: {0}")]
    Parse(String),
}

/// Top-level CLI definition.
#[derive(Parser, Debug)]
#[command(name = "ironjaw", version, about = "Ironjaw CLI client")]
pub struct Cli {
    /// Server URL
    #[arg(long, default_value = "http://localhost:18794")]
    pub server: String,

    /// Auth token
    #[arg(long, env = "IRONJAW_TOKEN")]
    pub token: Option<String>,

    /// Output as JSON
    #[arg(long)]
    pub json: bool,

    #[command(subcommand)]
    pub command: Command,
}

/// Top-level commands.
#[derive(Subcommand, Debug)]
pub enum Command {
    /// Send a chat message
    Chat {
        /// Message to send
        message: String,
        /// Session ID
        #[arg(long)]
        session: Option<String>,
        /// Model to use
        #[arg(long)]
        model: Option<String>,
    },
    /// Manage sessions
    Session {
        #[command(subcommand)]
        action: SessionAction,
    },
    /// List and execute tools
    Tools {
        #[command(subcommand)]
        action: ToolsAction,
    },
    /// Brain/memory operations
    Brain {
        #[command(subcommand)]
        action: BrainAction,
    },
    /// Context compaction management
    Compact {
        #[command(subcommand)]
        action: CompactAction,
    },
    /// Permission rules management
    Rules {
        #[command(subcommand)]
        action: RulesAction,
    },
    /// Proactive daemon management
    Daemon {
        #[command(subcommand)]
        action: DaemonAction,
    },
    /// Sleep-agent dream consolidation
    Dream {
        #[command(subcommand)]
        action: DreamAction,
    },
    /// Prompt cache management
    Cache {
        #[command(subcommand)]
        action: CacheAction,
    },
    /// Server status
    Status,
}

/// Session subcommands.
#[derive(Subcommand, Debug, Clone)]
pub enum SessionAction {
    /// List all sessions
    List,
    /// Get session details
    Get {
        /// Session ID
        id: String,
    },
    /// Create a new session
    Create {
        /// Session name
        name: String,
    },
    /// Delete a session
    Delete {
        /// Session ID
        id: String,
    },
    /// Rename a session
    Rename {
        /// Session ID
        id: String,
        /// New name
        name: String,
    },
}

/// Tools subcommands.
#[derive(Subcommand, Debug, Clone)]
pub enum ToolsAction {
    /// List available tools
    List,
    /// Execute a tool
    Execute {
        /// Tool name
        name: String,
        /// Tool arguments as JSON string
        #[arg(long, default_value = "{}")]
        args: String,
    },
}

/// Brain subcommands.
#[derive(Subcommand, Debug, Clone)]
pub enum BrainAction {
    /// Remember something
    Remember {
        /// Content to remember
        content: String,
    },
    /// Recall memories by query
    Recall {
        /// Search query
        query: String,
    },
    /// Forget a memory
    Forget {
        /// Memory ID
        id: String,
    },
}

/// Compact subcommands.
#[derive(Subcommand, Debug, Clone)]
pub enum CompactAction {
    /// Show current context/compaction status
    Status,
    /// Trigger micro-compaction
    Micro,
    /// Trigger full compaction
    Full,
}

/// Rules subcommands.
#[derive(Subcommand, Debug, Clone)]
pub enum RulesAction {
    /// List permission rules
    List,
    /// Add a permission rule
    Add {
        /// Rule scope (global or project)
        #[arg(long, default_value = "project")]
        scope: String,
        /// Pattern to match (e.g. "shell(git *)")
        pattern: String,
        /// Decision: allow or deny
        #[arg(long, default_value = "allow")]
        decision: String,
    },
    /// Remove a permission rule by index
    Remove {
        /// Rule index
        index: usize,
    },
    /// Test a command against rules
    Test {
        /// Command to test (e.g. "shell(rm -rf /)")
        command: String,
    },
}

/// Daemon subcommands.
#[derive(Subcommand, Debug, Clone)]
pub enum DaemonAction {
    /// Show daemon status
    Status,
    /// Show daemon log
    Log {
        /// Number of lines to show
        #[arg(long, default_value = "20")]
        lines: usize,
    },
}

/// Dream subcommands.
#[derive(Subcommand, Debug, Clone)]
pub enum DreamAction {
    /// Show dream consolidation status
    Status,
    /// Trigger dream consolidation
    Trigger,
}

/// Cache subcommands.
#[derive(Subcommand, Debug, Clone)]
pub enum CacheAction {
    /// Show prompt cache statistics
    Stats,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn parse_status_command() {
        let cli = Cli::parse_from(["ironjaw", "status"]);
        assert!(matches!(cli.command, Command::Status));
        assert_eq!(cli.server, "http://localhost:18794");
        assert!(!cli.json);
        assert!(cli.token.is_none());
    }

    #[test]
    fn parse_status_with_json_flag() {
        let cli = Cli::parse_from(["ironjaw", "--json", "status"]);
        assert!(matches!(cli.command, Command::Status));
        assert!(cli.json);
    }

    #[test]
    fn parse_custom_server_url() {
        let cli = Cli::parse_from(["ironjaw", "--server", "http://192.0.2.10:9999", "status"]);
        assert_eq!(cli.server, "http://192.0.2.10:9999");
    }

    #[test]
    fn parse_token_flag() {
        let cli = Cli::parse_from(["ironjaw", "--token", "secret123", "status"]);
        assert_eq!(cli.token, Some("secret123".to_string()));
    }

    #[test]
    fn parse_chat_command() {
        let cli = Cli::parse_from(["ironjaw", "chat", "hello world"]);
        match &cli.command {
            Command::Chat {
                message,
                session,
                model,
            } => {
                assert_eq!(message, "hello world");
                assert!(session.is_none());
                assert!(model.is_none());
            }
            _ => panic!("expected Chat command"),
        }
    }

    #[test]
    fn parse_chat_with_session_and_model() {
        let cli = Cli::parse_from([
            "ironjaw",
            "chat",
            "test msg",
            "--session",
            "abc-123",
            "--model",
            "qwen3:30b",
        ]);
        match &cli.command {
            Command::Chat {
                message,
                session,
                model,
            } => {
                assert_eq!(message, "test msg");
                assert_eq!(session.as_deref(), Some("abc-123"));
                assert_eq!(model.as_deref(), Some("qwen3:30b"));
            }
            _ => panic!("expected Chat command"),
        }
    }

    #[test]
    fn parse_session_list() {
        let cli = Cli::parse_from(["ironjaw", "session", "list"]);
        match &cli.command {
            Command::Session { action } => {
                assert!(matches!(action, SessionAction::List));
            }
            _ => panic!("expected Session command"),
        }
    }

    #[test]
    fn parse_session_get() {
        let cli = Cli::parse_from(["ironjaw", "session", "get", "sess-42"]);
        match &cli.command {
            Command::Session { action } => match action {
                SessionAction::Get { id } => assert_eq!(id, "sess-42"),
                _ => panic!("expected Get"),
            },
            _ => panic!("expected Session command"),
        }
    }

    #[test]
    fn parse_session_create() {
        let cli = Cli::parse_from(["ironjaw", "session", "create", "my-session"]);
        match &cli.command {
            Command::Session { action } => match action {
                SessionAction::Create { name } => assert_eq!(name, "my-session"),
                _ => panic!("expected Create"),
            },
            _ => panic!("expected Session command"),
        }
    }

    #[test]
    fn parse_session_delete() {
        let cli = Cli::parse_from(["ironjaw", "session", "delete", "sess-99"]);
        match &cli.command {
            Command::Session { action } => match action {
                SessionAction::Delete { id } => assert_eq!(id, "sess-99"),
                _ => panic!("expected Delete"),
            },
            _ => panic!("expected Session command"),
        }
    }

    #[test]
    fn parse_session_rename() {
        let cli = Cli::parse_from(["ironjaw", "session", "rename", "sess-1", "new-name"]);
        match &cli.command {
            Command::Session { action } => match action {
                SessionAction::Rename { id, name } => {
                    assert_eq!(id, "sess-1");
                    assert_eq!(name, "new-name");
                }
                _ => panic!("expected Rename"),
            },
            _ => panic!("expected Session command"),
        }
    }

    #[test]
    fn parse_tools_list() {
        let cli = Cli::parse_from(["ironjaw", "tools", "list"]);
        match &cli.command {
            Command::Tools { action } => {
                assert!(matches!(action, ToolsAction::List));
            }
            _ => panic!("expected Tools command"),
        }
    }

    #[test]
    fn parse_tools_execute() {
        let cli = Cli::parse_from([
            "ironjaw",
            "tools",
            "execute",
            "read_file",
            "--args",
            r#"{"path":"/tmp/x"}"#,
        ]);
        match &cli.command {
            Command::Tools { action } => match action {
                ToolsAction::Execute { name, args } => {
                    assert_eq!(name, "read_file");
                    assert_eq!(args, r#"{"path":"/tmp/x"}"#);
                }
                _ => panic!("expected Execute"),
            },
            _ => panic!("expected Tools command"),
        }
    }

    #[test]
    fn parse_tools_execute_default_args() {
        let cli = Cli::parse_from(["ironjaw", "tools", "execute", "shell"]);
        match &cli.command {
            Command::Tools { action } => match action {
                ToolsAction::Execute { name, args } => {
                    assert_eq!(name, "shell");
                    assert_eq!(args, "{}");
                }
                _ => panic!("expected Execute"),
            },
            _ => panic!("expected Tools command"),
        }
    }

    #[test]
    fn parse_brain_remember() {
        let cli = Cli::parse_from(["ironjaw", "brain", "remember", "the cat is on the mat"]);
        match &cli.command {
            Command::Brain { action } => match action {
                BrainAction::Remember { content } => {
                    assert_eq!(content, "the cat is on the mat");
                }
                _ => panic!("expected Remember"),
            },
            _ => panic!("expected Brain command"),
        }
    }

    #[test]
    fn parse_brain_recall() {
        let cli = Cli::parse_from(["ironjaw", "brain", "recall", "cat"]);
        match &cli.command {
            Command::Brain { action } => match action {
                BrainAction::Recall { query } => assert_eq!(query, "cat"),
                _ => panic!("expected Recall"),
            },
            _ => panic!("expected Brain command"),
        }
    }

    #[test]
    fn parse_brain_forget() {
        let cli = Cli::parse_from(["ironjaw", "brain", "forget", "mem-7"]);
        match &cli.command {
            Command::Brain { action } => match action {
                BrainAction::Forget { id } => assert_eq!(id, "mem-7"),
                _ => panic!("expected Forget"),
            },
            _ => panic!("expected Brain command"),
        }
    }

    #[test]
    fn error_display_http() {
        let err = CliError::Http("connection refused".to_string());
        assert_eq!(err.to_string(), "HTTP error: connection refused");
    }

    #[test]
    fn error_display_auth_required() {
        let err = CliError::AuthRequired;
        assert_eq!(
            err.to_string(),
            "Auth required: provide --token or set IRONJAW_TOKEN"
        );
    }

    #[test]
    fn error_display_server() {
        let err = CliError::Server("500 internal".to_string());
        assert_eq!(err.to_string(), "Server error: 500 internal");
    }

    #[test]
    fn error_display_parse() {
        let err = CliError::Parse("invalid json".to_string());
        assert_eq!(err.to_string(), "Parse error: invalid json");
    }

    #[test]
    fn default_server_url() {
        let cli = Cli::parse_from(["ironjaw", "status"]);
        assert_eq!(cli.server, "http://localhost:18794");
    }

    #[test]
    fn parse_compact_status() {
        let cli = Cli::parse_from(["ironjaw", "compact", "status"]);
        match &cli.command {
            Command::Compact { action } => {
                assert!(matches!(action, CompactAction::Status));
            }
            _ => panic!("expected Compact command"),
        }
    }

    #[test]
    fn parse_compact_micro() {
        let cli = Cli::parse_from(["ironjaw", "compact", "micro"]);
        match &cli.command {
            Command::Compact { action } => {
                assert!(matches!(action, CompactAction::Micro));
            }
            _ => panic!("expected Compact command"),
        }
    }

    #[test]
    fn parse_compact_full() {
        let cli = Cli::parse_from(["ironjaw", "compact", "full"]);
        match &cli.command {
            Command::Compact { action } => {
                assert!(matches!(action, CompactAction::Full));
            }
            _ => panic!("expected Compact command"),
        }
    }

    #[test]
    fn parse_rules_list() {
        let cli = Cli::parse_from(["ironjaw", "rules", "list"]);
        match &cli.command {
            Command::Rules { action } => {
                assert!(matches!(action, RulesAction::List));
            }
            _ => panic!("expected Rules command"),
        }
    }

    #[test]
    fn parse_rules_add() {
        let cli = Cli::parse_from([
            "ironjaw",
            "rules",
            "add",
            "shell(git *)",
            "--scope",
            "global",
            "--decision",
            "allow",
        ]);
        match &cli.command {
            Command::Rules { action } => match action {
                RulesAction::Add {
                    scope,
                    pattern,
                    decision,
                } => {
                    assert_eq!(scope, "global");
                    assert_eq!(pattern, "shell(git *)");
                    assert_eq!(decision, "allow");
                }
                _ => panic!("expected Add"),
            },
            _ => panic!("expected Rules command"),
        }
    }

    #[test]
    fn parse_rules_add_defaults() {
        let cli = Cli::parse_from(["ironjaw", "rules", "add", "shell(cargo *)"]);
        match &cli.command {
            Command::Rules { action } => match action {
                RulesAction::Add {
                    scope,
                    pattern,
                    decision,
                } => {
                    assert_eq!(scope, "project");
                    assert_eq!(pattern, "shell(cargo *)");
                    assert_eq!(decision, "allow");
                }
                _ => panic!("expected Add"),
            },
            _ => panic!("expected Rules command"),
        }
    }

    #[test]
    fn parse_rules_remove() {
        let cli = Cli::parse_from(["ironjaw", "rules", "remove", "3"]);
        match &cli.command {
            Command::Rules { action } => match action {
                RulesAction::Remove { index } => assert_eq!(*index, 3),
                _ => panic!("expected Remove"),
            },
            _ => panic!("expected Rules command"),
        }
    }

    #[test]
    fn parse_rules_test() {
        let cli = Cli::parse_from(["ironjaw", "rules", "test", "shell(rm -rf /)"]);
        match &cli.command {
            Command::Rules { action } => match action {
                RulesAction::Test { command } => {
                    assert_eq!(command, "shell(rm -rf /)");
                }
                _ => panic!("expected Test"),
            },
            _ => panic!("expected Rules command"),
        }
    }

    #[test]
    fn parse_daemon_status() {
        let cli = Cli::parse_from(["ironjaw", "daemon", "status"]);
        match &cli.command {
            Command::Daemon { action } => {
                assert!(matches!(action, DaemonAction::Status));
            }
            _ => panic!("expected Daemon command"),
        }
    }

    #[test]
    fn parse_daemon_log() {
        let cli = Cli::parse_from(["ironjaw", "daemon", "log", "--lines", "50"]);
        match &cli.command {
            Command::Daemon { action } => match action {
                DaemonAction::Log { lines } => assert_eq!(*lines, 50),
                _ => panic!("expected Log"),
            },
            _ => panic!("expected Daemon command"),
        }
    }

    #[test]
    fn parse_daemon_log_default_lines() {
        let cli = Cli::parse_from(["ironjaw", "daemon", "log"]);
        match &cli.command {
            Command::Daemon { action } => match action {
                DaemonAction::Log { lines } => assert_eq!(*lines, 20),
                _ => panic!("expected Log"),
            },
            _ => panic!("expected Daemon command"),
        }
    }

    #[test]
    fn parse_dream_status() {
        let cli = Cli::parse_from(["ironjaw", "dream", "status"]);
        match &cli.command {
            Command::Dream { action } => {
                assert!(matches!(action, DreamAction::Status));
            }
            _ => panic!("expected Dream command"),
        }
    }

    #[test]
    fn parse_dream_trigger() {
        let cli = Cli::parse_from(["ironjaw", "dream", "trigger"]);
        match &cli.command {
            Command::Dream { action } => {
                assert!(matches!(action, DreamAction::Trigger));
            }
            _ => panic!("expected Dream command"),
        }
    }

    #[test]
    fn parse_cache_stats() {
        let cli = Cli::parse_from(["ironjaw", "cache", "stats"]);
        match &cli.command {
            Command::Cache { action } => {
                assert!(matches!(action, CacheAction::Stats));
            }
            _ => panic!("expected Cache command"),
        }
    }

    #[test]
    fn all_flags_combined() {
        let cli = Cli::parse_from([
            "ironjaw",
            "--server",
            "http://example.com:8080",
            "--token",
            "tok",
            "--json",
            "chat",
            "hi",
        ]);
        assert_eq!(cli.server, "http://example.com:8080");
        assert_eq!(cli.token, Some("tok".to_string()));
        assert!(cli.json);
        assert!(matches!(cli.command, Command::Chat { .. }));
    }
}
