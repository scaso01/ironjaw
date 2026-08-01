mod config;
pub mod hot_reload;
pub mod ipc;
pub mod telemetry;
pub mod updater;

use config::Config;
use ironjaw_audit::AuditStore;
use ironjaw_brain::{Brain, BrainConfig, BrainStore, Embedder, MemoryMaintenance, NewMemory};
use ironjaw_llm::{
    anthropic::AnthropicProvider, llama_server::LlamaServerProvider, openai::OpenAiProvider,
    ApiKeySource, CloudModelCatalog, LlamaServerState, LlamaStatusMonitor, LlmRouter,
    ModelRegistry, Provider, RoutingOverrides,
};
use ironjaw_mcp::{register_brain_tools, McpClient, McpRemoteTool, McpServer, McpServerConfig};
use ironjaw_proactive::{ProactiveEngine, ProactiveStore};
use ironjaw_security::{HitlGate, RateLimiter, SanitizeConfig};
use ironjaw_session::SessionStore;
use ironjaw_tools::ToolRegistry;
use ironjaw_vision::ImageProcessor;
use ironjaw_voice::{SttClient, TtsClient, VoiceEngine};
use ironjaw_ws::{AppState, ChatAgenticConfig, IntegrationUrls, LocalModelConfig};
use std::path::PathBuf;
use std::sync::Arc;
use tracing::{info, warn};

/// What MCP clients call this server, sent as `serverInfo.name` at handshake.
///
/// Keep it DISTINCTIVE. On 2026-07-29 this was renamed to "My Projects and
/// Personal Notes" on the theory that Gemini Spark skips the connector because the
/// name matches nothing a question is about. It made things worse: the generic
/// wording collides with Google's own Keep Notes and Drive, so "use my projects and
/// notes" routed straight to those, and the reliable escape hatch — naming the brain
/// out loud — stopped working, because "ironjaw brain" no longer matched anything.
/// Spark went from handshaking on every question to not connecting at all.
///
/// An odd, unique name is the feature, not the bug: it is the one string a user can
/// say that cannot be confused with a built-in integration. Reverted.
///
/// Note the name only travels during `initialize`, and clients cache it from when
/// the connector was ADDED — Gemini keeps showing the old label after a mere
/// off/on toggle. Changing this requires removing and re-adding the connector.
const MCP_SERVER_NAME: &str = "ironjaw-brain";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Parse command line args
    let args: Vec<String> = std::env::args().collect();
    let is_mcp_mode = args.iter().any(|a| a == "--mcp-server");

    let config_path = if is_mcp_mode {
        // In MCP mode, the config path follows --mcp-server or is second arg
        args.iter()
            .skip(1)
            .find(|a| a.ends_with(".toml"))
            .cloned()
            .unwrap_or_else(|| "config/ironjaw.toml".to_string())
    } else {
        args.get(1)
            .map(|s| {
                if s == "--config" {
                    args.get(2)
                        .cloned()
                        .unwrap_or_else(|| "config/ironjaw.toml".to_string())
                } else {
                    s.clone()
                }
            })
            .unwrap_or_else(|| "config/ironjaw.toml".to_string())
    };

    // Load config
    let config = Config::load(&PathBuf::from(&config_path)).map_err(|e| {
        eprintln!("Failed to load config from '{config_path}': {e}");
        e
    })?;

    // Initialize logging (stderr for MCP mode so stdout stays clean for JSON-RPC)
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(&config.logging.level))
        // Phase 4: mdns_sd emits benign malformed-packet noise from other devices
        // on the LAN; cap it at warn so it doesn't drown the real logs.
        .add_directive(
            "mdns_sd=warn"
                .parse()
                .expect("static tracing directive is valid"),
        );

    match config.logging.format.as_str() {
        "json" => {
            tracing_subscriber::fmt()
                .with_env_filter(env_filter)
                .json()
                .with_writer(std::io::stderr)
                .init();
        }
        _ => {
            tracing_subscriber::fmt()
                .with_env_filter(env_filter)
                .with_writer(std::io::stderr)
                .init();
        }
    }

    // --mcp-server mode: run as an MCP server over stdio, then exit.
    // Resolve CWD to the project root so relative config paths (data/brain.db etc.) work
    // regardless of where Claude Code spawns the process.
    // Config lives at <project>/config/ironjaw.toml, so project root = config_path/../..
    if is_mcp_mode {
        let config_abs =
            std::fs::canonicalize(&config_path).unwrap_or_else(|_| PathBuf::from(&config_path));
        if let Some(project_root) = config_abs.parent().and_then(|p| p.parent()) {
            if project_root.exists() {
                if let Err(e) = std::env::set_current_dir(project_root) {
                    eprintln!(
                        "Warning: could not chdir to project root {}: {e}",
                        project_root.display()
                    );
                }
            }
        }
        return run_mcp_server(&config).await;
    }

    info!(
        config = %config_path,
        version = env!("CARGO_PKG_VERSION"),
        "Ironjaw starting"
    );

    // Open audit store
    let audit = AuditStore::open(&config.audit.database_path).await?;
    info!(
        db = %config.audit.database_path,
        "Audit store ready"
    );

    // Open session store
    let sessions = SessionStore::open(&config.session.database_path).await?;
    info!(
        db = %config.session.database_path,
        "Session store ready"
    );

    // Load model registry early (needed for auto-load in llama-server provider)
    let llama_models_path = std::path::Path::new("C:\\Users\\deploy\\Scripts\\llama-models.json");
    let model_registry = if llama_models_path.exists() {
        match ModelRegistry::load(llama_models_path) {
            Ok(registry) => {
                info!(
                    count = registry.len(),
                    default = %registry.default,
                    "Model registry loaded from llama-models.json"
                );
                Arc::new(registry)
            }
            Err(e) => {
                tracing::warn!(error = %e, "Failed to load llama-models.json, using empty registry");
                Arc::new(ModelRegistry {
                    default: String::new(),
                    active: std::sync::RwLock::new(None),
                    models: std::collections::HashMap::new(),
                    source_path: None,
                })
            }
        }
    } else {
        Arc::new(ModelRegistry {
            default: String::new(),
            active: std::sync::RwLock::new(None),
            models: std::collections::HashMap::new(),
            source_path: None,
        })
    };

    // Create shared llama-server state (used by provider + status monitor + AppState)
    let llama_state = Arc::new(tokio::sync::RwLock::new(LlamaServerState::default()));

    // Build LLM providers from config
    let mut providers = Vec::new();
    let mut llama_base_url: Option<String> = None;
    for pc in &config.llm.providers {
        let provider = match pc.provider_type.as_str() {
            "llama-server" => {
                llama_base_url = Some(pc.base_url.clone());
                Provider::LlamaServer(LlamaServerProvider::new_with_state(
                    &pc.base_url,
                    pc.timeout_secs,
                    llama_state.clone(),
                    model_registry.clone(),
                ))
            }
            "openai" => Provider::OpenAi(OpenAiProvider::new(
                &pc.base_url,
                pc.resolve_api_key(),
                &pc.name,
                pc.timeout_secs,
                pc.extra_headers.clone(),
            )),
            "anthropic" => match pc.api_key_source() {
                Some(ApiKeySource::OAuthFile { credentials_path }) => {
                    info!(
                        name = %pc.name,
                        path = %credentials_path.display(),
                        "Anthropic provider using OAuth (dynamic token refresh)"
                    );
                    Provider::Anthropic(AnthropicProvider::new_oauth(
                        credentials_path,
                        &pc.name,
                        pc.timeout_secs,
                        Some(pc.base_url.clone()),
                    ))
                }
                Some(ApiKeySource::Static { key }) => {
                    info!(
                        name = %pc.name,
                        "Anthropic provider using static API key"
                    );
                    Provider::Anthropic(AnthropicProvider::new(
                        key,
                        &pc.name,
                        pc.timeout_secs,
                        Some(pc.base_url.clone()),
                    ))
                }
                None => {
                    tracing::warn!(
                        name = %pc.name,
                        "Anthropic provider has no API key or OAuth credentials, skipping"
                    );
                    continue;
                }
            },
            other => {
                tracing::warn!(provider_type = other, "Unknown provider type, skipping");
                continue;
            }
        };
        info!(name = %pc.name, provider_type = %pc.provider_type, "Registered LLM provider");
        providers.push(provider);
    }

    let routing = if config.llm.routing.enabled {
        info!(
            simple = ?config.llm.routing.simple_model,
            medium = ?config.llm.routing.medium_model,
            complex = ?config.llm.routing.complex_model,
            "Complexity-based model routing enabled"
        );
        Some(RoutingOverrides {
            simple_model: config.llm.routing.simple_model.clone(),
            medium_model: config.llm.routing.medium_model.clone(),
            complex_model: config.llm.routing.complex_model.clone(),
        })
    } else {
        None
    };

    // Use config default_model if set, otherwise fall back to registry active_or_default
    let effective_default = config
        .llm
        .default_model
        .clone()
        .or_else(|| Some(model_registry.active_or_default()));
    let llm = Arc::new(LlmRouter::new(providers, effective_default, routing));

    // G3: watch llama-models.json for external edits (llama-switch run from a
    // shell) and retarget the in-memory registry + router default live.
    if llama_models_path.exists() {
        let watcher_registry = model_registry.clone();
        let watcher_router = llm.clone();
        let watcher = hot_reload::ModelRegistryWatcher::new(llama_models_path.to_path_buf(), 5);
        let (_handle, shutdown_tx) = watcher.start(move |active| {
            watcher_registry.set_active(active.clone());
            watcher_router.set_default_model(Some(active));
        });
        // The watcher lives for the whole gateway process; dropping the
        // shutdown sender would stop it (recv() yields None), so leak it.
        std::mem::forget(shutdown_tx);
    }

    // Build security components
    let rate_limiter = if config.security.rate_limit_enabled {
        info!("Rate limiter enabled");
        RateLimiter::with_defaults()
    } else {
        info!("Rate limiter disabled");
        RateLimiter::new(std::collections::HashMap::new())
    };

    let hitl_gate = if config.security.hitl_enabled {
        HitlGate::new(config.security.hitl_timeout_secs)
    } else {
        HitlGate::auto_approve()
    };
    info!(
        enabled = config.security.hitl_enabled,
        timeout_secs = config.security.hitl_timeout_secs,
        "HITL gate ready"
    );

    let sanitize = if config.security.sanitize_enabled {
        SanitizeConfig::default_patterns()
    } else {
        SanitizeConfig::disabled()
    };
    info!(
        enabled = config.security.sanitize_enabled,
        "Sanitizer ready"
    );

    // Build tool registry
    let mut tools = ToolRegistry::new();
    ironjaw_tools::register_builtins(&mut tools);
    ironjaw_tools::register_todo(&mut tools);
    ironjaw_integrations::register_integrations(&mut tools, &config.integrations).await;
    info!(count = tools.count(), "Tool registry ready");

    // Wire MCP clients — one per [[mcp.servers]] entry (step 13)
    // Clients are initialized now so that their tools land in `tools` before
    // it is moved into AppState. Clients are stored in `mcp_clients` and
    // transferred onto `state.mcp_clients` after AppState is built, keeping
    // the background actor tasks alive for the gateway lifetime.
    let mut mcp_clients: Vec<Arc<McpClient>> = Vec::new();
    for server in &config.mcp.servers {
        let client = match server {
            config::McpServerConfigEntry::Http { name, url, api_key } => {
                let cfg = McpServerConfig {
                    name: name.clone(),
                    url: url.clone(),
                    api_key: api_key.clone(),
                };
                Arc::new(McpClient::http(&cfg, 30))
            }
            config::McpServerConfigEntry::Stdio {
                name,
                command,
                args,
            } => match McpClient::stdio(name.clone(), command, args, 30) {
                Ok(c) => Arc::new(c),
                Err(e) => {
                    warn!(server = %name, error = %e, "Failed to spawn stdio MCP server; skipping");
                    continue;
                }
            },
        };
        match client.initialize().await {
            Ok(_) => info!(server = %client.server_name(), "MCP server initialized"),
            Err(e) => {
                warn!(server = %client.server_name(), error = %e, "MCP initialize failed; skipping");
                continue;
            }
        }
        match client.discover_tools().await {
            Ok(mcp_tools) => {
                let count = mcp_tools.len();
                for mcp_tool in mcp_tools {
                    let remote = McpRemoteTool::new(Arc::clone(&client), mcp_tool);
                    tools.register(Box::new(remote));
                }
                info!(
                    server = %client.server_name(),
                    tools = count,
                    "MCP tools registered"
                );
                mcp_clients.push(client);
            }
            Err(e) => warn!(
                server = %client.server_name(),
                error = %e,
                "MCP tool discovery failed; skipping"
            ),
        }
    }
    info!(count = mcp_clients.len(), "MCP clients wired");

    // Build brain (Phase 6 + Brain Architecture Redesign)
    let brain = if config.brain.enabled {
        let store = BrainStore::open(&config.brain.database_path).await?;
        let embedder = if config.brain.embedding_backend == "fastembed" {
            Embedder::new_fastembed(&config.brain.embedding_url, &config.brain.embedding_model)
        } else {
            Embedder::new(&config.brain.embedding_url, &config.brain.embedding_model)
        };
        let brain_config = BrainConfig {
            max_memories: config.brain.max_memories,
            auto_recall_top_k: config.brain.auto_recall_top_k,
            brain_files_dir: config.brain.brain_files_dir.clone(),
            layer1_budget_tokens: config.brain.layer1_budget_tokens,
            memory_relevance_threshold: config.brain.memory_relevance_threshold,
            auto_learn: config.brain.auto_learn,
            learn_dedup_threshold: config.brain.learn_dedup_threshold,
            learn_after_turns: config.brain.learn_after_turns,
            learn_use_llm: config.brain.learn_use_llm,
            learn_novelty_threshold: config.brain.learn_novelty_threshold,
            consolidation_interval_hours: config.brain.consolidation_interval_hours,
            salience_boost_per_access: config.brain.salience_boost_per_access,
            context_var_max_tokens: config.brain.context_var_max_tokens,
            auto_entity_extraction: config.brain.auto_entity_extraction,
            session_summary_enabled: config.brain.session_summary_enabled,
            session_summary_on_compact: config.brain.session_summary_on_compact,
            session_summary_on_switch: config.brain.session_summary_on_switch,
            auto_inject: ironjaw_brain::AutoInjectConfig {
                enabled: config.brain.auto_inject.enabled,
                max_memories: config.brain.auto_inject.max_memories,
                min_score: config.brain.auto_inject.min_score,
                max_context_budget: config.brain.auto_inject.max_context_budget,
                exclude_voice: config.brain.auto_inject.exclude_voice,
                timeout_ms: config.brain.auto_inject.timeout_ms,
            },
            stable_prefix: config.brain.stable_prefix,
            live_state: ironjaw_brain::BrainLiveStateConfig {
                enabled: config.brain.live_state.enabled,
                skip_on_live_state: config.brain.live_state.skip_on_live_state,
                freshness_window_secs: config.brain.live_state.freshness_window_secs,
                confidence_threshold: config.brain.live_state.confidence_threshold,
                llm_fallback: config.brain.live_state.llm_fallback,
                decay_rate: config.brain.live_state.decay_rate,
            },
        };
        info!(
            db = %config.brain.database_path,
            embed_url = %config.brain.embedding_url,
            embed_model = %config.brain.embedding_model,
            brain_files = %config.brain.brain_files_dir,
            "Brain ready (enabled)"
        );
        let consolidation_interval_hours = brain_config.consolidation_interval_hours;

        // Initialize knowledge graph on the same pool for auto entity extraction
        let brain = Arc::new(
            match ironjaw_brain::KnowledgeGraph::init(store.pool().clone()).await {
                Ok(graph) => {
                    info!("KnowledgeGraph initialized — auto entity extraction enabled");
                    Brain::with_graph(store, embedder, brain_config, graph)
                }
                Err(e) => {
                    warn!(error = %e, "KnowledgeGraph init failed — entity extraction disabled");
                    Brain::new(store, embedder, brain_config)
                }
            },
        );

        // Initialize FTS5 hybrid search index (non-fatal: degrades to vector-only on failure)
        if let Err(e) = brain.init_fts().await {
            tracing::warn!(error = %e, "brain FTS init failed; recall will be vector-only");
        }

        // Bootstrap brain from layer files
        match brain.bootstrap_from_files().await {
            Ok(report) => info!(
                layers = report.layers_loaded,
                core = report.core_tokens,
                total = report.total_tokens,
                "Brain bootstrap: {} layers loaded, core={} tokens, total={} tokens",
                report.layers_loaded,
                report.core_tokens,
                report.total_tokens
            ),
            Err(e) => warn!(error = %e, "Brain bootstrap failed (non-fatal)"),
        }

        // Spawn scheduled consolidation timer (with optional LLM sleep agents)
        let interval_hours = consolidation_interval_hours;
        let salience_hot = config.brain.salience_hot_days;
        let salience_warm = config.brain.salience_warm_days;
        let sleep_agents_enabled = config.brain.sleep_agents_enabled;
        let sleep_batch = config.brain.sleep_agents_batch_size;
        let pattern_days = config.brain.sleep_agents_pattern_days;
        if interval_hours > 0 {
            let maint_store = brain.store().clone();
            // Share the LlmRouter for sleep agents via Arc
            let maint_llm: Option<Arc<LlmRouter>> = if sleep_agents_enabled {
                Some(Arc::clone(&llm))
            } else {
                None
            };
            tokio::spawn(async move {
                let maintenance = MemoryMaintenance::new(maint_store);
                let period = std::time::Duration::from_secs(interval_hours * 3600);
                let mut interval = tokio::time::interval(period);
                // Skip the first tick (fires immediately)
                interval.tick().await;
                info!(
                    interval_hours,
                    sleep_agents = sleep_agents_enabled,
                    "Memory consolidation timer started"
                );
                let mut cycle_count = 0u64;
                loop {
                    interval.tick().await;
                    let llm_ref = maint_llm.as_deref();
                    match maintenance
                        .run_all_with_llm(
                            llm_ref,
                            salience_hot,
                            salience_warm,
                            sleep_batch,
                            pattern_days,
                            cycle_count,
                        )
                        .await
                    {
                        Ok(report) => info!(
                            decayed = report.decayed,
                            pruned = report.pruned,
                            merged = report.merged,
                            boosted = report.boosted,
                            deduplicated = report.deduplicated,
                            cleaned = report.cleaned,
                            cycle = cycle_count,
                            "Scheduled consolidation complete"
                        ),
                        Err(e) => warn!(error = %e, "Scheduled consolidation failed"),
                    }
                    cycle_count += 1;
                }
            });
        }

        // Spawn chronicle indexer (if enabled)
        if config.brain.indexers.chronicle.enabled {
            let chronicle_config = ironjaw_brain::indexers::chronicle::ChronicleConfig {
                enabled: true,
                db_path: config.brain.indexers.chronicle.db_path.clone(),
                poll_interval_secs: config.brain.indexers.chronicle.poll_interval_secs,
            };
            let indexer = ironjaw_brain::ChronicleIndexer::new(brain.clone(), chronicle_config);
            let cancel = tokio_util::sync::CancellationToken::new();
            indexer.spawn(cancel);
            info!("Chronicle indexer started");
        }

        // Spawn markdown watcher (if enabled)
        if config.brain.indexers.markdown.enabled {
            let md_config = ironjaw_brain::indexers::markdown::MarkdownConfig {
                enabled: true,
                watch_dirs: config.brain.indexers.markdown.watch_dirs.clone(),
                debounce_secs: config.brain.indexers.markdown.debounce_secs,
            };
            let md_embedder = if config.brain.embedding_backend == "fastembed" {
                Embedder::new_fastembed(&config.brain.embedding_url, &config.brain.embedding_model)
            } else {
                Embedder::new(&config.brain.embedding_url, &config.brain.embedding_model)
            };
            let watcher = ironjaw_brain::MarkdownIndexer::new(
                brain.store().clone(),
                Arc::new(md_embedder),
                md_config,
            );
            let cancel = tokio_util::sync::CancellationToken::new();
            watcher.spawn(cancel);
            info!("Markdown watcher started");
        }

        // Startup quality checks: log unembedded count, backfill vec, re-embed nulls
        {
            let total = brain.store().count_memories().await.unwrap_or(0);
            let unembedded = brain.store().count_unembedded().await.unwrap_or(0);
            if total > 0 {
                let pct = if total > 0 {
                    (unembedded as f64 / total as f64 * 100.0) as u64
                } else {
                    0
                };
                info!(
                    total,
                    unembedded,
                    pct,
                    "Brain: {total} memories, {unembedded} without embeddings ({pct}%)"
                );
            }

            // Auto-backfill sqlite-vec virtual table
            if brain.store().has_sqlite_vec() {
                match brain.store().backfill_vec_memories().await {
                    Ok(n) if n > 0 => info!(count = n, "Backfilled vec_memories on startup"),
                    Err(e) => warn!(error = %e, "vec_memories backfill failed"),
                    _ => {}
                }
            }

            // Re-embed memories with NULL embeddings
            if unembedded > 0 {
                match brain.reembed_missing().await {
                    Ok((processed, failed)) => {
                        if processed > 0 || failed > 0 {
                            info!(processed, failed, "Startup re-embed complete");
                        }
                    }
                    Err(e) => warn!(error = %e, "Startup re-embed failed"),
                }
            }
        }

        brain
    } else {
        let store = BrainStore::open_memory().await?;
        let embedder = Embedder::mock(64);
        info!("Brain ready (disabled — mock embedder)");
        Arc::new(Brain::new(store, embedder, BrainConfig::default()))
    };

    // Inject runtime model/provider info into brain context
    {
        let model_raw = llm.default_model().unwrap_or_else(|| "unknown".to_string());
        // Model may have provider prefix (e.g. "anthropic/claude-sonnet-4-6")
        let (provider, model) = if let Some((p, m)) = model_raw.split_once('/') {
            (p.to_string(), m.to_string())
        } else {
            // No prefix — infer from first provider
            let p = config
                .llm
                .providers
                .first()
                .map(|p| p.name.clone())
                .unwrap_or_else(|| "unknown".to_string());
            (p, model_raw)
        };
        let backend = if provider.contains("anthropic") || provider.contains("claude") {
            format!("cloud model ({provider} / {model})")
        } else {
            format!("local LLM ({provider} / {model})")
        };
        brain.set_model_info(format!(
            "Your exact model is: {model}. Provider: {provider}. Backend: {backend}. Do NOT guess or substitute a different model name — use this exact identifier when asked."
        ));
        info!(backend, "Brain model info injected");
    }

    // Build proactive engine (Phase 8)
    let proactive = if config.proactive.enabled {
        let proactive_store = ProactiveStore::open(&config.proactive.database_path).await?;
        info!(
            db = %config.proactive.database_path,
            check_interval = config.proactive.check_interval_secs,
            "Proactive engine ready (enabled)"
        );
        ProactiveEngine::new(proactive_store, config.proactive.clone())
    } else {
        let proactive_store = ProactiveStore::open_memory().await?;
        info!("Proactive engine ready (disabled — in-memory store)");
        ProactiveEngine::new(proactive_store, config.proactive.clone())
    };

    // Build voice engine (Phase 9)
    let voice = if config.voice.enabled {
        let stt = SttClient::new(&config.voice.stt_url, &config.voice.stt_model);
        let tts = TtsClient::new(
            &config.voice.tts_url,
            &config.voice.tts_model,
            &config.voice.tts_voice,
        );
        info!(
            stt_url = %config.voice.stt_url,
            tts_url = %config.voice.tts_url,
            tts_voice = %config.voice.tts_voice,
            "Voice engine ready (enabled)"
        );
        VoiceEngine::new(stt, tts, config.voice.clone())
    } else {
        info!("Voice engine ready (disabled — mock clients)");
        VoiceEngine::mock()
    };

    // Build image processor (Phase 10)
    let image_processor = ImageProcessor::new(config.vision.max_image_size_bytes);
    info!(
        enabled = config.vision.enabled,
        max_size = config.vision.max_image_size_bytes,
        "Image processor ready"
    );

    // Build integration URLs
    let llm_base_url = config
        .llm
        .providers
        .first()
        .map(|p| p.base_url.clone())
        .unwrap_or_default();
    let integration_urls = IntegrationUrls {
        cc_bridge_url: config.integrations.cc_bridge_url.clone(),
        cc_chronicle_url: config.integrations.cc_chronicle_url.clone(),
        llm_base_url,
    };

    // Build app state
    let mut state = AppState::with_security(
        config.auth.token.clone(),
        audit,
        Arc::clone(&llm),
        sessions,
        config.session.context_token_limit,
        rate_limiter,
        hitl_gate,
        sanitize,
        tools,
        brain,
        proactive,
        voice,
        image_processor,
        integration_urls,
    );
    state.mcp_clients = mcp_clients;

    // Wire extraction config from brain config (extraction_enabled default false)
    state.brain.extraction_pipeline = Arc::new(ironjaw_brain::ExtractionPipeline::new(
        ironjaw_brain::ExtractionConfig {
            enabled: config.brain.extraction_enabled,
            auto_approve_confidence: config.brain.auto_approve_confidence,
            ..Default::default()
        },
    ));

    // Initialize Claude Code subprocess tracker
    if config.claude.enabled {
        let cc_tracker = std::sync::Arc::new(ironjaw_claude::ActiveSessionTracker::new(
            config.claude.max_concurrent,
        ));
        state.claude.tracker = cc_tracker;
        state.claude.config = config.claude.clone();
        info!(
            max_concurrent = config.claude.max_concurrent,
            "CC session tracker initialized"
        );
    }

    // Enable OpenAI-compat endpoints if configured
    if config.server.openai_compat {
        state.openai_compat = true;
        info!("OpenAI-compatible /v1/* endpoints enabled");
    }

    // Native MCP over Streamable HTTP (POST /mcp). Uses the gateway's own Brain,
    // so remote MCP clients no longer need the supergateway bridge — which spawned
    // an `--mcp-server` child per session, never reaped it, and left a pile of
    // processes fighting over brain.db.
    if !config.server.mcp_token.is_empty() {
        let mut mcp_server = McpServer::new(MCP_SERVER_NAME, env!("CARGO_PKG_VERSION"));
        register_brain_tools(&mut mcp_server);

        // Same block_in_place pattern as `run_mcp_server`: the tool handler is a
        // sync callback, so it needs a runtime handle to drive the async brain.
        let handler_brain = Arc::clone(&state.brain.engine);
        let rt = tokio::runtime::Handle::current();
        mcp_server.set_tool_handler(Arc::new(move |name, args| {
            let brain = Arc::clone(&handler_brain);
            tokio::task::block_in_place(|| {
                rt.block_on(async move { dispatch_brain_tool(&brain, name, args).await })
            })
        }));

        info!(
            tools = mcp_server.tool_count(),
            "Native MCP endpoint enabled (POST /mcp)"
        );
        state.mcp_http = Some(ironjaw_ws::mcp_http::McpHttp {
            server: Arc::new(mcp_server),
            token: config.server.mcp_token.clone(),
        });
    }

    // Wire WebSocket auth timeout from [server] config
    state.auth_timeout_secs = config.server.auth_timeout_secs;

    // Wire agentic loop config (Phase 4 killswitch; enabled=false by default)
    state.chat_agentic = ChatAgenticConfig {
        enabled: config.chat.agentic.enabled,
        max_iterations: config.chat.agentic.max_iterations,
        tool_timeout_secs: config.chat.agentic.tool_timeout_secs,
        nudger_enabled: config.chat.agentic.nudger_enabled,
        nudger_max_retries: config.chat.agentic.nudger_max_retries,
        hitl_session_cache_enabled: config.chat.agentic.hitl_session_cache_enabled,
        tool_memory_writeback_enabled: config.chat.agentic.tool_memory_writeback_enabled,
        run_token_ceiling: config.chat.agentic.run_token_ceiling,
        tool_result_max_chars: config.chat.agentic.tool_result_max_chars,
        min_answer_chars: config.chat.agentic.min_answer_chars,
        turn_deadline_secs: config.chat.agentic.turn_deadline_secs,
    };
    if config.chat.agentic.enabled {
        info!(
            max_iterations = config.chat.agentic.max_iterations,
            tool_timeout_secs = config.chat.agentic.tool_timeout_secs,
            "Agentic tool-call loop ENABLED"
        );
    }

    // Wire [chat.local] — behavioral base prompt + curated tool set for local
    // (llama-server) models. Cloud models are unaffected.
    state.local_model = LocalModelConfig {
        enabled: config.chat.local.enabled,
        system_prompt_file: config.chat.local.system_prompt_file.clone(),
        tools: config.chat.local.tools.clone(),
    };
    if config.chat.local.enabled {
        info!(
            prompt_file = %config.chat.local.system_prompt_file,
            tool_count = config.chat.local.tools.len(),
            "Local-model base prompt + curated tool set ENABLED"
        );
    }

    // C1 tier-3: wire `[tools] working_dir` global default into AppState. Skip the
    // default ".", which is equivalent to falling through to current_dir.
    let tools_wd_raw = config.tools.working_dir.trim();
    if !tools_wd_raw.is_empty() && tools_wd_raw != "." {
        let path = PathBuf::from(tools_wd_raw);
        if path.exists() && path.is_dir() {
            *state.tools_working_dir.write().await = Some(path.clone());
            info!(
                working_dir = %path.display(),
                "Wired [tools] working_dir global default for tool resolution",
            );
        } else {
            warn!(
                configured_path = %tools_wd_raw,
                "[tools] working_dir does not exist or is not a directory; ignoring (will fall through to current_dir)",
            );
        }
    }

    // Load model aliases
    if !config.llm.aliases.is_empty() {
        state.llm.aliases = std::sync::Arc::new(config.llm.aliases.clone());
        info!(count = config.llm.aliases.len(), "Model aliases loaded");
    }

    // Wire pre-loaded model registry and shared llama state into AppState
    state.llm.model_registry = model_registry.clone();
    state.llm.llama_state = llama_state.clone();

    // Start llama-server status monitor (polls health, syncs to shared state)
    // Also subscribes to model change events for WS broadcast + Chronicle invalidation.
    if let Some(base_url) = &llama_base_url {
        let monitor = LlamaStatusMonitor::new(base_url);
        let model_change_tx = monitor.model_change_sender();

        // Store the sender on AppState so handler.rs can emit UserSwitch events
        state.llm.model_change_tx = Some(model_change_tx);

        // Subscribe for WS broadcast + Chronicle invalidation
        let mut model_change_rx = monitor.subscribe_model_changes();

        // Initial poll to seed state
        monitor.check_now().await;
        let initial = monitor.state().await;
        {
            let mut s = llama_state.write().await;
            s.loaded_model = initial.loaded_model;
            s.last_known_model = initial.last_known_model;
            s.status = initial.status;
            s.last_check = initial.last_check;
            s.consecutive_failures = initial.consecutive_failures;
        }
        info!("llama-server initial status seeded");

        // Spawn polling loop (syncs monitor state to shared llama_state)
        let monitor_state = llama_state.clone();
        let default_model = model_registry.active_or_default();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(15));
            loop {
                interval.tick().await;
                monitor.check_now().await;
                let polled = monitor.state().await;
                let mut s = monitor_state.write().await;
                s.loaded_model = polled.loaded_model;
                s.last_known_model = polled.last_known_model;
                s.status = polled.status;
                s.last_check = polled.last_check;
                s.consecutive_failures = polled.consecutive_failures;
            }
        });

        // Spawn model change event listener (WS broadcast + Chronicle invalidation)
        let device_registry = state.device.registry.clone();
        let chronicle_url = state.integration_urls.cc_chronicle_url.clone();
        let http_client = state.http_client.clone();
        tokio::spawn(async move {
            loop {
                match model_change_rx.recv().await {
                    Ok(event) => {
                        info!(
                            previous = ?event.previous,
                            current = %event.current,
                            source = ?event.source,
                            "Broadcasting model.changed event"
                        );

                        // Broadcast model.changed event to all WS clients
                        let frame = ironjaw_ws::Frame::event(
                            "model.changed",
                            serde_json::json!({
                                "previous": event.previous,
                                "current": event.current,
                                "source": event.source,
                            }),
                        );
                        if let Ok(json) = serde_json::to_string(&frame) {
                            device_registry.broadcast_text(json).await;
                        }

                        // POST to CC Chronicle /api/llm/invalidate
                        if !chronicle_url.is_empty() {
                            let url = format!(
                                "{}/api/llm/invalidate",
                                chronicle_url.trim_end_matches('/')
                            );
                            let client = http_client.clone();
                            tokio::spawn(async move {
                                match client.post(&url).send().await {
                                    Ok(resp) => {
                                        if resp.status().is_success() {
                                            info!("CC Chronicle cache invalidated on model change");
                                        } else {
                                            warn!(
                                                status = %resp.status(),
                                                "CC Chronicle invalidation returned non-success"
                                            );
                                        }
                                    }
                                    Err(e) => {
                                        warn!(error = %e, "Failed to invalidate CC Chronicle cache");
                                    }
                                }
                            });
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        warn!(
                            skipped = n,
                            "Model change listener lagged, some events dropped"
                        );
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        info!("Model change channel closed, stopping listener");
                        break;
                    }
                }
            }
        });

        info!(
            url = %base_url,
            default = %default_model,
            "llama-server status monitor started (15s interval, model change events enabled)"
        );
    }

    // Load cloud model catalogs from config
    if !config.llm.cloud_models.is_empty() {
        let catalogs: Vec<CloudModelCatalog> = config
            .llm
            .cloud_models
            .iter()
            .map(|c| CloudModelCatalog {
                provider: c.provider.clone(),
                models: c
                    .models
                    .iter()
                    .map(|m| ironjaw_llm::CloudModel {
                        id: m.id.clone(),
                        description: m.description.clone(),
                    })
                    .collect(),
            })
            .collect();
        let total: usize = catalogs.iter().map(|c| c.models.len()).sum();
        info!(
            catalogs = catalogs.len(),
            models = total,
            "Cloud model catalogs loaded"
        );
        state.llm.cloud_catalogs = std::sync::Arc::new(catalogs);
    }

    // Load skills from config into SkillRegistry
    if !config.skills.is_empty() {
        let mut skill_registry = ironjaw_ws::SkillRegistry::new();
        for sc in &config.skills {
            let skill = ironjaw_ws::Skill {
                name: sc.name.clone(),
                description: sc.description.clone(),
                version: "1.0.0".to_string(),
                tools: sc.tools.clone(),
                system_prompt: sc.system_prompt.clone(),
                enabled: sc.enabled,
                model: sc.model.clone(),
                fallback_chain: sc.fallback_chain.clone(),
                cloud_models: sc.cloud_models.clone(),
                temperature: sc.temperature,
                original_model: sc.model.clone(),
            };
            if let Err(e) = skill_registry.register(skill) {
                tracing::warn!(name = %sc.name, error = %e, "Skipping duplicate skill");
            }
        }
        info!(count = skill_registry.count(), "Skills loaded from config");
        state.skill.registry = std::sync::Arc::new(std::sync::RwLock::new(skill_registry));
    }

    // B2 CP6: hook framework discovery. Master killswitch is `[hooks] enabled`.
    // When false, the registry stays empty and no HooksRuntime is wired into
    // AppState (hooks.reload returns HOOKS_DISABLED). When true, scan the
    // hooks dir, register every parseable WASM hook, and register subprocess
    // hooks ONLY for events listed in `[hooks.subprocess].enabled_events`
    // (see hooks/config.rs for the full gating rules).
    if config.hooks.enabled {
        // Resolve `~` so canonicalize works even on first run. Cross-platform:
        // prefer $HOME, fall back to $USERPROFILE on Windows, then to literal.
        let home_dir = std::env::var("HOME")
            .ok()
            .or_else(|| std::env::var("USERPROFILE").ok())
            .map(std::path::PathBuf::from);
        let resolved_dir = {
            let raw = &config.hooks.dir;
            let expanded = if let Some(stripped) = raw.strip_prefix("~/") {
                match home_dir.as_ref() {
                    Some(home) => home.join(stripped),
                    None => std::path::PathBuf::from(raw),
                }
            } else if raw == "~" {
                home_dir
                    .clone()
                    .unwrap_or_else(|| std::path::PathBuf::from(raw))
            } else {
                std::path::PathBuf::from(raw)
            };
            // Best-effort create; discovery still warns if missing.
            if !expanded.exists() {
                if let Err(e) = std::fs::create_dir_all(&expanded) {
                    tracing::warn!(dir = %expanded.display(), error = %e, "could not create hooks dir");
                }
            }
            expanded
        };

        let sandbox = std::sync::Arc::new(ironjaw_ws::wasm_sandbox::WasmSandbox::with_defaults());
        let runtime = ironjaw_ws::HooksRuntime {
            dir: resolved_dir.clone(),
            subprocess_enabled: config.hooks.subprocess.enabled,
            subprocess_enabled_events: std::sync::Arc::new(
                config.hooks.subprocess.enabled_events.clone(),
            ),
            subprocess_default_timeout_secs: config.hooks.subprocess.default_timeout_secs,
            sandbox: std::sync::Arc::clone(&sandbox),
        };

        match ironjaw_ws::hooks::discover_and_register(
            &state.hooks,
            sandbox,
            Some(state.audit.clone()),
            &resolved_dir,
            runtime.subprocess_enabled,
            &runtime.subprocess_enabled_events,
            runtime.subprocess_default_timeout_secs,
        )
        .await
        {
            Ok(entries) => {
                info!(
                    discovered = entries.len(),
                    registered = entries.iter().filter(|e| e.enabled).count(),
                    dir = %resolved_dir.display(),
                    "B2 hook discovery complete"
                );
                *state.hooks_catalog.write().await = entries;
            }
            Err(e) => {
                warn!(error = %e, "Hook discovery failed; continuing with empty registry");
            }
        }

        state.hooks_runtime = Some(runtime);
    } else {
        info!("Hooks disabled in config ([hooks] enabled = false)");
    }

    // Register mDNS service for LAN discovery
    let mdns_daemon = {
        let mdns = mdns_sd::ServiceDaemon::new()
            .map_err(|e| anyhow::anyhow!("mDNS daemon init failed: {e}"))?;
        let service_type = "_ironjaw._tcp.local.";
        let instance = mdns_sd::ServiceInfo::new(
            service_type,
            "ironjaw",
            &format!(
                "{}.local.",
                hostname::get().unwrap_or_default().to_string_lossy()
            ),
            "",
            config.server.port,
            std::collections::HashMap::from([(
                "version".to_string(),
                env!("CARGO_PKG_VERSION").to_string(),
            )]),
        );
        match instance {
            Ok(info) => {
                if let Err(e) = mdns.register(info) {
                    warn!("mDNS registration failed: {e}");
                } else {
                    info!(
                        service_type,
                        port = config.server.port,
                        "mDNS service registered"
                    );
                }
                Some(mdns)
            }
            Err(e) => {
                warn!("mDNS service info creation failed: {e}");
                None
            }
        }
    };

    // Start server
    info!(
        host = %config.server.host,
        port = config.server.port,
        "Starting WebSocket server"
    );

    ironjaw_ws::serve(&config.server.host, config.server.port, state).await?;

    // Cleanup mDNS on shutdown
    if let Some(mdns) = mdns_daemon {
        let _ = mdns.shutdown();
    }

    Ok(())
}

/// Run Ironjaw as an MCP server over stdio.
///
/// Minimal initialization: only config, BrainStore, and Embedder. No axum
/// server, no WebSocket, no full AppState. Reads JSON-RPC from stdin, writes
/// responses to stdout.
async fn run_mcp_server(config: &Config) -> anyhow::Result<()> {
    info!(
        version = env!("CARGO_PKG_VERSION"),
        "Ironjaw MCP server starting"
    );

    // Init brain (full Brain instance for semantic recall)
    let store = if config.brain.enabled {
        BrainStore::open(&config.brain.database_path).await?
    } else {
        BrainStore::open_memory().await?
    };
    let embedder = if config.brain.embedding_backend == "fastembed" {
        Embedder::new_fastembed(&config.brain.embedding_url, &config.brain.embedding_model)
    } else {
        Embedder::new(&config.brain.embedding_url, &config.brain.embedding_model)
    };
    let brain_config = BrainConfig {
        max_memories: config.brain.max_memories,
        auto_recall_top_k: config.brain.auto_recall_top_k,
        brain_files_dir: config.brain.brain_files_dir.clone(),
        layer1_budget_tokens: config.brain.layer1_budget_tokens,
        memory_relevance_threshold: config.brain.memory_relevance_threshold,
        auto_learn: config.brain.auto_learn,
        learn_dedup_threshold: config.brain.learn_dedup_threshold,
        learn_after_turns: config.brain.learn_after_turns,
        learn_use_llm: config.brain.learn_use_llm,
        learn_novelty_threshold: config.brain.learn_novelty_threshold,
        consolidation_interval_hours: config.brain.consolidation_interval_hours,
        salience_boost_per_access: config.brain.salience_boost_per_access,
        context_var_max_tokens: config.brain.context_var_max_tokens,
        auto_entity_extraction: config.brain.auto_entity_extraction,
        session_summary_enabled: config.brain.session_summary_enabled,
        session_summary_on_compact: config.brain.session_summary_on_compact,
        session_summary_on_switch: config.brain.session_summary_on_switch,
        auto_inject: ironjaw_brain::AutoInjectConfig {
            enabled: config.brain.auto_inject.enabled,
            max_memories: config.brain.auto_inject.max_memories,
            min_score: config.brain.auto_inject.min_score,
            max_context_budget: config.brain.auto_inject.max_context_budget,
            exclude_voice: config.brain.auto_inject.exclude_voice,
            timeout_ms: config.brain.auto_inject.timeout_ms,
        },
        stable_prefix: config.brain.stable_prefix,
        live_state: ironjaw_brain::BrainLiveStateConfig {
            enabled: config.brain.live_state.enabled,
            skip_on_live_state: config.brain.live_state.skip_on_live_state,
            freshness_window_secs: config.brain.live_state.freshness_window_secs,
            confidence_threshold: config.brain.live_state.confidence_threshold,
            llm_fallback: config.brain.live_state.llm_fallback,
            decay_rate: config.brain.live_state.decay_rate,
        },
    };
    // Initialize the knowledge graph on the same pool so the MCP recall path
    // gets graph-aware recall (graph as a third RRF retriever), matching the
    // gateway. Without this, brain_recall over MCP would silently run
    // vector+FTS only and ignore the entity graph.
    let brain = Arc::new(
        match ironjaw_brain::KnowledgeGraph::init(store.pool().clone()).await {
            Ok(graph) => {
                info!("MCP: KnowledgeGraph initialized — graph-aware recall enabled");
                Brain::with_graph(store, embedder, brain_config, graph)
            }
            Err(e) => {
                warn!(error = %e, "MCP: KnowledgeGraph init failed — graph-aware recall disabled");
                Brain::new(store, embedder, brain_config)
            }
        },
    );
    // Initialize FTS5 hybrid search index (non-fatal: degrades to vector-only on failure)
    if let Err(e) = brain.init_fts().await {
        tracing::warn!(error = %e, "brain FTS init failed; recall will be vector-only");
    }
    info!("MCP: brain ready");

    // Build MCP server with brain tools
    let mut mcp_server = McpServer::new(MCP_SERVER_NAME, env!("CARGO_PKG_VERSION"));
    register_brain_tools(&mut mcp_server);

    // Set the tool handler that routes to Brain methods.
    // block_in_place is required because run_stdio() is synchronous and runs on a
    // tokio runtime thread — calling rt.block_on() directly would panic with
    // "Cannot start a runtime from within a runtime".
    let handler_brain = brain;
    let rt = tokio::runtime::Handle::current();
    mcp_server.set_tool_handler(Arc::new(move |name, args| {
        let brain = handler_brain.clone();
        tokio::task::block_in_place(|| {
            rt.block_on(async move { dispatch_brain_tool(&brain, name, args).await })
        })
    }));

    info!(tools = mcp_server.tool_count(), "MCP: tools registered");

    // Run the blocking stdio loop
    mcp_server.run_stdio()?;
    Ok(())
}

/// Dispatch an MCP tool call to the appropriate Brain method.
async fn dispatch_brain_tool(
    brain: &Brain,
    name: &str,
    args: serde_json::Value,
) -> anyhow::Result<serde_json::Value> {
    let store = brain.store();
    match name {
        "brain_remember" => {
            let content = args["content"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("content is required"))?;
            let memory_type_str = args.get("memory_type").and_then(|v| v.as_str());
            let confidence = args.get("confidence").and_then(|v| v.as_f64());
            let trust_level_raw = args.get("trust_level").and_then(|v| v.as_i64());
            let importance_override = args.get("importance").and_then(|v| v.as_f64());
            let tags = args.get("tags").and_then(|v| v.as_str());
            let source = args.get("source").and_then(|v| v.as_str());
            let evidence_session_id = args
                .get("evidence_session_id")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let evidence_message_id = args
                .get("evidence_message_id")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let scope = args
                .get("scope")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let supersedes = args.get("supersedes").and_then(|v| v.as_i64());

            // Resolve importance: explicit override > confidence > memory_type base > None
            let importance = importance_override.or_else(|| {
                confidence.or_else(|| {
                    memory_type_str.and_then(|mt| {
                        use ironjaw_brain::types::MemoryType;
                        mt.parse::<MemoryType>().ok().map(|t| t.base_importance())
                    })
                })
            });

            let trust_level =
                trust_level_raw.map(|v| ironjaw_brain::types::TrustLevel::from(v as i32));

            let mut new_mem = NewMemory {
                content: content.to_string(),
                source: source.map(|s| s.to_string()),
                tags: tags.map(|t| t.to_string()),
                embedding: None,
                memory_type: memory_type_str.map(|s| s.to_string()),
                importance,
                strength: None,
                supersedes,
                content_hash: None,
                valid_from: None,
                valid_to: None,
                source_type: Some("mcp".to_string()),
                reflection: None,
                trust_level,
                evidence_session_id,
                evidence_message_id,
                scope,
                procedure: None,
                quality_score: None,
                quality_flag: None,
            };
            match brain.remember_full(&mut new_mem).await {
                Ok((memory, embedded)) => Ok(serde_json::json!({
                    "ok": true,
                    "id": memory.id,
                    "embedded": embedded,
                    "memory_type": memory.memory_type,
                    "importance": memory.importance,
                    "trust_level": memory.trust_level.to_string(),
                })),
                Err(e) if e.to_string().contains("memory rejected") => Ok(serde_json::json!({
                    "ok": true,
                    "skipped": true,
                    "reason": e.to_string(),
                })),
                Err(e) => Err(e),
            }
        }

        "brain_recall" => {
            let query = args["query"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("query is required"))?;
            let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(10) as usize;

            // Semantic search via Brain::recall() (embedding → KNN → hybrid re-ranking)
            let results = brain.recall(query, Some(limit)).await?;

            let memories: Vec<serde_json::Value> = results
                .iter()
                .map(|m| {
                    serde_json::json!({
                        "id": m.id,
                        "content": m.content,
                        "memory_type": m.memory_type,
                        "importance": m.importance,
                        "strength": m.strength,
                        "created_at": m.created_at,
                        "tags": m.tags,
                    })
                })
                .collect();

            Ok(serde_json::json!({"memories": memories, "count": memories.len()}))
        }

        "brain_forget" => {
            let id = args["id"]
                .as_i64()
                .ok_or_else(|| anyhow::anyhow!("id is required"))?;
            let deleted = store.delete_memory(id).await?;
            Ok(serde_json::json!({"ok": deleted, "id": id}))
        }

        "brain_timeline" => {
            let at = args["at"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("at is required"))?;
            let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(20) as usize;

            let memories = store.query_point_in_time(at, limit).await?;
            let results: Vec<serde_json::Value> = memories
                .iter()
                .map(|m| {
                    serde_json::json!({
                        "id": m.id,
                        "content": m.content,
                        "memory_type": m.memory_type,
                        "importance": m.importance,
                        "valid_from": m.valid_from,
                        "valid_to": m.valid_to,
                    })
                })
                .collect();

            Ok(serde_json::json!({"memories": results, "count": results.len()}))
        }

        "brain_summaries" => {
            let session_id = args.get("session_id").and_then(|v| v.as_str());
            let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(20) as usize;

            let memories = store.get_summaries(session_id, limit).await?;
            let results: Vec<serde_json::Value> = memories
                .iter()
                .map(|m| {
                    serde_json::json!({
                        "id": m.id,
                        "content": m.content,
                        "source": m.source,
                        "created_at": m.created_at,
                    })
                })
                .collect();

            Ok(serde_json::json!({"summaries": results, "count": results.len()}))
        }

        "brain_context_set" => {
            let name = args["name"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("name is required"))?;
            let content = args["content"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("content is required"))?;

            let token_count = (content.len() / 4) as i64;
            store
                .set_context_var(name, content, None, token_count)
                .await?;
            Ok(serde_json::json!({"ok": true, "name": name}))
        }

        "brain_context_list" => {
            let vars = store.list_context_vars().await?;
            let results: Vec<serde_json::Value> = vars
                .iter()
                .map(|v| {
                    serde_json::json!({
                        "name": v.name,
                        "token_count": v.token_count,
                        "loaded": v.loaded,
                        "created_at": v.created_at,
                    })
                })
                .collect();

            Ok(serde_json::json!({"variables": results, "count": results.len()}))
        }

        "brain_status" => {
            let memory_count = store.count_memories().await.unwrap_or(0);
            let temporal_active = store.count_temporal_active().await.unwrap_or(0);
            let temporal_expired = store.count_temporal_expired().await.unwrap_or(0);
            let (ctx_count, ctx_tokens) = store.count_context_vars().await.unwrap_or((0, 0));

            Ok(serde_json::json!({
                "memory_count": memory_count,
                "temporal_active": temporal_active,
                "temporal_expired": temporal_expired,
                "context_vars": {
                    "count": ctx_count,
                    "total_tokens": ctx_tokens,
                },
            }))
        }

        "brain_pending_list" => {
            let limit = args.get("limit").and_then(|v| v.as_i64()).unwrap_or(50);
            let offset = args.get("offset").and_then(|v| v.as_i64()).unwrap_or(0);
            let memories = store.list_pending_memories(limit, offset).await?;
            let results: Vec<serde_json::Value> = memories
                .iter()
                .map(|m| {
                    serde_json::json!({
                        "id": m.id,
                        "content": m.content,
                        "source": m.source,
                        "memory_type": m.memory_type,
                        "tags": m.tags,
                        "importance": m.importance,
                        "confidence": m.confidence,
                        "evidence_session_id": m.evidence_session_id,
                        "created_at": m.created_at,
                    })
                })
                .collect();
            Ok(serde_json::json!({"pending": results, "count": results.len()}))
        }

        "brain_pending_approve" => {
            let id = args
                .get("id")
                .and_then(|v| v.as_i64())
                .ok_or_else(|| anyhow::anyhow!("Missing required param: id"))?;
            let memory_id = store.approve_pending_memory(id).await?;
            Ok(serde_json::json!({
                "approved": true,
                "memory_id": memory_id,
                "pending_id": id,
            }))
        }

        "brain_pending_reject" => {
            let id = args
                .get("id")
                .and_then(|v| v.as_i64())
                .ok_or_else(|| anyhow::anyhow!("Missing required param: id"))?;
            store.reject_pending_memory(id).await?;
            Ok(serde_json::json!({
                "rejected": true,
                "pending_id": id,
            }))
        }

        "brain_recent" => {
            let limit = args.get("limit").and_then(|v| v.as_i64()).unwrap_or(20) as usize;
            let scope_filter = args
                .get("scope")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());

            let mut memories = store.get_recent_active_memories(limit).await?;

            // Post-filter by scope prefix if requested (store has no scope parameter).
            if let Some(ref scope) = scope_filter {
                memories.retain(|m| {
                    m.scope
                        .as_deref()
                        .map(|s| s.starts_with(scope.as_str()))
                        .unwrap_or(false)
                });
            }

            let results: Vec<serde_json::Value> = memories
                .iter()
                .map(|m| {
                    serde_json::json!({
                        "id": m.id,
                        "content": m.content,
                        "memory_type": m.memory_type,
                        "importance": m.importance,
                        "trust_level": m.trust_level.to_string(),
                        "scope": m.scope,
                        "tags": m.tags,
                        "created_at": m.created_at,
                    })
                })
                .collect();

            Ok(serde_json::json!({"memories": results, "count": results.len()}))
        }

        "brain_forget_query" => {
            let q = args["q"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("q is required"))?;
            let scope_filter = args
                .get("scope")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let max_to_delete = args
                .get("max_to_delete")
                .and_then(|v| v.as_u64())
                .unwrap_or(100) as usize;
            let dry_run = args
                .get("dry_run")
                .and_then(|v| v.as_bool())
                .unwrap_or(true);

            // Semantic search to find candidate memories.
            let candidates = brain.recall(q, Some(max_to_delete)).await?;

            // Apply scope filter if provided.
            let candidates: Vec<_> = if let Some(ref scope) = scope_filter {
                candidates
                    .into_iter()
                    .filter(|m| {
                        m.scope
                            .as_deref()
                            .map(|s| s.starts_with(scope.as_str()))
                            .unwrap_or(false)
                    })
                    .collect()
            } else {
                candidates
            };

            let matched_ids: Vec<i64> = candidates.iter().map(|m| m.id).collect();

            if dry_run {
                return Ok(serde_json::json!({
                    "dry_run": true,
                    "matched_count": matched_ids.len(),
                    "matched_ids": matched_ids,
                }));
            }

            // Real delete — use the same store method as brain_forget.
            let mut deleted_ids: Vec<i64> = Vec::new();
            for id in &matched_ids {
                if store.delete_memory(*id).await? {
                    deleted_ids.push(*id);
                }
            }

            Ok(serde_json::json!({
                "dry_run": false,
                "deleted_count": deleted_ids.len(),
                "deleted_ids": deleted_ids,
            }))
        }

        _ => Err(anyhow::anyhow!("Unknown tool: {name}")),
    }
}
