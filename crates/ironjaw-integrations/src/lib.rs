pub mod agents;
pub mod android;
pub mod cc;
pub mod data;
pub mod git;
pub mod homeassistant;
pub mod infra;
pub mod migration;
pub mod monitoring;
pub mod network;
pub mod oauth2;
pub mod research;
pub mod scrape;
pub mod search;
pub mod secrets;
pub mod workstation;

use serde::Deserialize;
use std::sync::Arc;

/// Configuration for integration tools.
#[derive(Debug, Clone, Deserialize)]
pub struct IntegrationsConfig {
    #[serde(default)]
    pub enabled: bool,

    // Search
    #[serde(default = "default_searxng_url")]
    pub searxng_url: String,
    #[serde(default = "default_vane_url")]
    pub vane_url: String,
    #[serde(default = "default_zoekt_url")]
    pub zoekt_url: String,
    #[serde(default = "default_khoj_url")]
    pub khoj_url: String,
    #[serde(default = "default_meilisearch_url")]
    pub meilisearch_url: String,

    // Scrape
    #[serde(default = "default_crawl4ai_url")]
    pub crawl4ai_url: String,
    #[serde(default = "default_byparr_url")]
    pub byparr_url: String,

    // Research
    #[serde(default = "default_deep_research_url")]
    pub deep_research_url: String,
    #[serde(default = "default_gpt_researcher_url")]
    pub gpt_researcher_url: String,

    // Monitoring
    #[serde(default = "default_uptime_kuma_url")]
    pub uptime_kuma_url: String,
    #[serde(default = "default_wud_url")]
    pub wud_url: String,
    #[serde(default = "default_changedetection_url")]
    pub changedetection_url: String,
    #[serde(default)]
    pub changedetection_api_key: Option<String>,
    #[serde(default = "default_prometheus_url")]
    pub prometheus_url: String,
    #[serde(default = "default_adguard_url")]
    pub adguard_url: String,
    #[serde(default)]
    pub adguard_auth_basic: Option<String>,

    // Data
    #[serde(default = "default_whodat_url")]
    pub whodat_url: String,
    #[serde(default = "default_gitea_url")]
    pub gitea_url: String,
    #[serde(default)]
    pub gitea_token: Option<String>,
    #[serde(default = "default_rsshub_url")]
    pub rsshub_url: String,
    #[serde(default = "default_cve_search_url")]
    pub cve_search_url: String,
    #[serde(default = "default_ransomwatch_url")]
    pub ransomwatch_url: String,
    #[serde(default = "default_tiktok_sig_url")]
    pub tiktok_sig_url: String,
    #[serde(default = "default_docker_registry_url")]
    pub docker_registry_url: String,

    // CC bridge / chronicle
    #[serde(default = "default_cc_bridge_url")]
    pub cc_bridge_url: String,
    #[serde(default = "default_cc_chronicle_url")]
    pub cc_chronicle_url: String,
    #[serde(default = "default_claude_bridge_url")]
    pub claude_bridge_url: String,

    // Infra (existing)
    #[serde(default = "default_remote_host")]
    pub remote_host: String,
    #[serde(default = "default_remote_user")]
    pub remote_user: String,
    #[serde(default = "default_secrets_db")]
    pub secrets_database_path: String,

    // Home Assistant (optional)
    #[serde(default)]
    pub ha_url: Option<String>,
    #[serde(default)]
    pub ha_token: Option<String>,
}

impl Default for IntegrationsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            searxng_url: default_searxng_url(),
            vane_url: default_vane_url(),
            zoekt_url: default_zoekt_url(),
            khoj_url: default_khoj_url(),
            meilisearch_url: default_meilisearch_url(),
            crawl4ai_url: default_crawl4ai_url(),
            byparr_url: default_byparr_url(),
            deep_research_url: default_deep_research_url(),
            gpt_researcher_url: default_gpt_researcher_url(),
            uptime_kuma_url: default_uptime_kuma_url(),
            wud_url: default_wud_url(),
            changedetection_url: default_changedetection_url(),
            changedetection_api_key: None,
            prometheus_url: default_prometheus_url(),
            adguard_url: default_adguard_url(),
            adguard_auth_basic: None,
            whodat_url: default_whodat_url(),
            gitea_url: default_gitea_url(),
            gitea_token: None,
            rsshub_url: default_rsshub_url(),
            cve_search_url: default_cve_search_url(),
            ransomwatch_url: default_ransomwatch_url(),
            tiktok_sig_url: default_tiktok_sig_url(),
            docker_registry_url: default_docker_registry_url(),
            cc_bridge_url: default_cc_bridge_url(),
            cc_chronicle_url: default_cc_chronicle_url(),
            claude_bridge_url: default_claude_bridge_url(),
            remote_host: default_remote_host(),
            remote_user: default_remote_user(),
            secrets_database_path: default_secrets_db(),
            ha_url: None,
            ha_token: None,
        }
    }
}

fn default_searxng_url() -> String {
    "http://192.0.2.20:8080".to_string()
}
fn default_vane_url() -> String {
    "http://192.0.2.20:3004".to_string()
}
fn default_zoekt_url() -> String {
    "http://192.0.2.20:6070".to_string()
}
fn default_khoj_url() -> String {
    "http://192.0.2.20:42110".to_string()
}
fn default_meilisearch_url() -> String {
    "http://192.0.2.20:7700".to_string()
}
fn default_crawl4ai_url() -> String {
    "http://192.0.2.20:11235".to_string()
}
fn default_byparr_url() -> String {
    "http://192.0.2.20:8191".to_string()
}
fn default_deep_research_url() -> String {
    "http://192.0.2.20:8300".to_string()
}
fn default_gpt_researcher_url() -> String {
    "http://192.0.2.20:8301".to_string()
}
fn default_uptime_kuma_url() -> String {
    "http://192.0.2.20:3002".to_string()
}
fn default_wud_url() -> String {
    "http://192.0.2.20:3000".to_string()
}
fn default_changedetection_url() -> String {
    "http://192.0.2.20:8086".to_string()
}
fn default_prometheus_url() -> String {
    "http://192.0.2.20:9091".to_string()
}
fn default_adguard_url() -> String {
    "http://192.0.2.20:3006".to_string()
}
fn default_whodat_url() -> String {
    "http://192.0.2.20:8083".to_string()
}
fn default_gitea_url() -> String {
    "http://192.0.2.20:3300".to_string()
}
fn default_rsshub_url() -> String {
    "http://192.0.2.20:1200".to_string()
}
fn default_cve_search_url() -> String {
    "http://192.0.2.20:5555".to_string()
}
fn default_ransomwatch_url() -> String {
    "http://192.0.2.20:8000".to_string()
}
fn default_tiktok_sig_url() -> String {
    "http://192.0.2.20:18800".to_string()
}
fn default_docker_registry_url() -> String {
    "http://192.0.2.20:5000".to_string()
}
fn default_cc_bridge_url() -> String {
    "http://127.0.0.1:18790".to_string()
}
fn default_cc_chronicle_url() -> String {
    "http://127.0.0.1:18793".to_string()
}
fn default_claude_bridge_url() -> String {
    "http://127.0.0.1:18796".to_string()
}
fn default_remote_host() -> String {
    "192.0.2.20".to_string()
}
fn default_remote_user() -> String {
    "deploy".to_string()
}
fn default_secrets_db() -> String {
    "data/secrets.db".to_string()
}

/// Register all integration tools into the tool registry.
pub async fn register_integrations(
    registry: &mut ironjaw_tools::ToolRegistry,
    config: &IntegrationsConfig,
) {
    if !config.enabled {
        tracing::info!("Integrations disabled, skipping registration");
        return;
    }

    let before = registry.count();

    // ---- Search (8) ----
    registry.register(Box::new(search::searxng::SearxngTool::new(
        &config.searxng_url,
    )));
    registry.register(Box::new(search::vane::VaneTool::new(&config.vane_url)));
    registry.register(Box::new(search::web_search::WebSearchTool::new(
        &config.searxng_url,
        &config.vane_url,
    )));
    registry.register(Box::new(search::monster_search::MonsterSearchTool::new()));
    registry.register(Box::new(search::zoekt_search::ZoektSearchTool::new(
        &config.zoekt_url,
    )));
    registry.register(Box::new(search::khoj_query::KhojQueryTool::new(
        &config.khoj_url,
    )));
    registry.register(Box::new(search::meili_search::MeiliSearchTool::new(
        &config.meilisearch_url,
    )));
    registry.register(Box::new(search::fyin_search::FyinSearchTool::new(
        &config.remote_host,
        &config.remote_user,
    )));

    // ---- Scrape (2) ----
    registry.register(Box::new(scrape::crawl_page::CrawlPageTool::new(
        &config.crawl4ai_url,
    )));
    registry.register(Box::new(scrape::byparr_fetch::ByparrFetchTool::new(
        &config.byparr_url,
    )));

    // ---- Research (2) ----
    registry.register(Box::new(research::deep_research::DeepResearchTool::new(
        &config.deep_research_url,
    )));
    registry.register(Box::new(research::gpt_researcher::GptResearcherTool::new(
        &config.gpt_researcher_url,
    )));

    // ---- Monitoring (5) ----
    registry.register(Box::new(monitoring::uptime_status::UptimeStatusTool::new(
        &config.uptime_kuma_url,
    )));
    registry.register(Box::new(
        monitoring::container_updates::ContainerUpdatesTool::new(&config.wud_url),
    ));
    registry.register(Box::new(monitoring::watched_urls::WatchedUrlsTool::new(
        &config.changedetection_url,
        config.changedetection_api_key.as_deref(),
    )));
    registry.register(Box::new(monitoring::prom_query::PromQueryTool::new(
        &config.prometheus_url,
    )));
    registry.register(Box::new(monitoring::adguard_stats::AdguardStatsTool::new(
        &config.adguard_url,
        config.adguard_auth_basic.as_deref(),
    )));

    // ---- Data (7) ----
    registry.register(Box::new(data::whois_lookup::WhoisLookupTool::new(
        &config.whodat_url,
    )));
    registry.register(Box::new(data::gitea_repo::GiteaRepoTool::new(
        &config.gitea_url,
        config.gitea_token.as_deref(),
    )));
    registry.register(Box::new(data::rss_feed::RssFeedTool::new(
        &config.rsshub_url,
    )));
    registry.register(Box::new(data::cve_lookup::CveLookupTool::new(
        &config.cve_search_url,
    )));
    registry.register(Box::new(data::ransomwatch::RansomwatchTool::new(
        &config.ransomwatch_url,
    )));
    registry.register(Box::new(data::tiktok_sig::TiktokSigTool::new(
        &config.tiktok_sig_url,
    )));
    registry.register(Box::new(data::docker_registry::DockerRegistryTool::new(
        &config.docker_registry_url,
    )));

    // ---- Network (2) ----
    registry.register(Box::new(network::vpn_proxy::VpnProxyTool::new()));
    registry.register(Box::new(network::nordvpn_status::NordvpnStatusTool::new()));

    // ---- workstation local (6) ----
    registry.register(Box::new(workstation::jcm::JcmTool::new()));
    registry.register(Box::new(workstation::yt_dlp::YtDlpTool::new()));
    registry.register(Box::new(workstation::ffmpeg_probe::FfmpegProbeTool::new()));
    registry.register(Box::new(workstation::task_control::TaskControlTool::new()));
    registry.register(Box::new(workstation::gh_cli::GhCliTool::new()));
    registry.register(Box::new(
        workstation::claude_subagent::ClaudeSubagentTool::new(&config.claude_bridge_url),
    ));

    // ---- CC (2) ----
    registry.register(Box::new(cc::sessions::CcSessionsTool::new(
        &config.cc_bridge_url,
    )));
    registry.register(Box::new(cc::chronicle::CcChronicleTool::new(
        &config.cc_chronicle_url,
    )));

    // ---- Infra (4) ----
    registry.register(Box::new(infra::workstation::HostStatusTool::new()));
    registry.register(Box::new(infra::remote_host::RemoteHostStatusTool::new(
        &config.remote_host,
        &config.remote_user,
    )));
    registry.register(Box::new(infra::docker::DockerManageTool::new(
        &config.remote_host,
        &config.remote_user,
    )));
    registry.register(Box::new(infra::docker_exec::DockerExecTool::new(
        &config.remote_host,
        &config.remote_user,
    )));

    // ---- Android (1) ----
    registry.register(Box::new(android::adb::AdbTool::new()));

    // ---- Git (2) ----
    registry.register(Box::new(git::tools::GitStatusTool::new()));
    registry.register(Box::new(git::tools::GitLogTool::new()));

    // ---- Home Assistant (optional, +1 if configured) ----
    if let (Some(url), Some(token)) = (&config.ha_url, &config.ha_token) {
        registry.register(Box::new(homeassistant::HomeAssistantTool::new(url, token)));
        tracing::info!(url = %url, "Home Assistant tool registered");
    }

    // ---- Secrets vault (optional, +2 if vault opens) ----
    match secrets::vault::SecretsVault::open(&config.secrets_database_path).await {
        Ok(vault) => {
            let vault = Arc::new(vault);
            registry.register(Box::new(secrets::tools::SecretsGetTool::new(vault.clone())));
            registry.register(Box::new(secrets::tools::SecretsSetTool::new(vault)));
            tracing::info!(db = %config.secrets_database_path, "Secrets vault ready");
        }
        Err(e) => {
            tracing::warn!(error = %e, "Failed to open secrets vault — secrets tools unavailable");
        }
    }

    let added = registry.count() - before;
    tracing::info!(count = added, "Integration tools registered");
}

#[cfg(test)]
mod tests {
    use super::*;
    use ironjaw_tools::ToolRegistry;

    /// Base count with secrets vault opening successfully (8 search + 2 scrape +
    /// 2 research + 5 monitoring + 7 data + 2 network + 6 workstation + 2 cc +
    /// 4 infra + 1 android + 2 git + 2 secrets) = 43.
    const BASE_INTEGRATION_COUNT: usize = 43;

    #[tokio::test]
    async fn test_register_integrations_enabled() {
        let mut registry = ToolRegistry::new();
        let config = IntegrationsConfig {
            enabled: true,
            secrets_database_path: ":memory:".to_string(),
            ..Default::default()
        };
        register_integrations(&mut registry, &config).await;
        assert_eq!(registry.count(), BASE_INTEGRATION_COUNT);
    }

    #[tokio::test]
    async fn test_register_integrations_disabled() {
        let mut registry = ToolRegistry::new();
        let config = IntegrationsConfig::default();
        register_integrations(&mut registry, &config).await;
        assert_eq!(registry.count(), 0);
    }

    #[tokio::test]
    async fn test_register_integrations_with_ha() {
        let mut registry = ToolRegistry::new();
        let config = IntegrationsConfig {
            enabled: true,
            secrets_database_path: ":memory:".to_string(),
            ha_url: Some("http://ha.local:8123".to_string()),
            ha_token: Some("test-token".to_string()),
            ..Default::default()
        };
        register_integrations(&mut registry, &config).await;
        assert_eq!(registry.count(), BASE_INTEGRATION_COUNT + 1);
    }
}
