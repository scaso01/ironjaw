use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

// === Health Check System ===

/// Status of an individual health check or the overall system.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum HealthStatus {
    Healthy,
    Degraded(String),
    Unhealthy(String),
}

/// A single health check result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthCheck {
    pub name: String,
    pub status: HealthStatus,
    pub last_checked: DateTime<Utc>,
    pub response_time_ms: Option<u64>,
    pub metadata: HashMap<String, serde_json::Value>,
}

/// Aggregated system health snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemHealth {
    pub overall: HealthStatus,
    pub checks: Vec<HealthCheck>,
    pub uptime_secs: u64,
    pub version: String,
    pub timestamp: DateTime<Utc>,
}

// === Deployment Tracking ===

/// Information about the current deployment.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeploymentInfo {
    pub version: String,
    pub commit_hash: Option<String>,
    pub deployed_at: DateTime<Utc>,
    pub environment: Environment,
    pub features: Vec<String>,
}

/// Deployment environment.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Environment {
    Development,
    Staging,
    Production,
}

// === Metrics Collection ===

/// Operational metrics snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpsMetrics {
    pub requests_total: u64,
    pub errors_total: u64,
    pub active_connections: u64,
    pub memory_usage_bytes: u64,
    pub cpu_usage_percent: f64,
    pub disk_usage_bytes: u64,
    pub uptime_secs: u64,
    pub start_time: DateTime<Utc>,
}

// === Service Registry ===

/// A registered external service entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceEntry {
    pub name: String,
    pub url: String,
    pub status: HealthStatus,
    pub last_checked: DateTime<Utc>,
    pub check_interval_secs: u64,
}

// === Operations Manager ===

/// Central operations manager for health checks, deployment tracking,
/// metrics collection, and service registry.
pub struct OpsManager {
    health_checks: Arc<RwLock<Vec<HealthCheck>>>,
    deployment: Arc<RwLock<DeploymentInfo>>,
    metrics: Arc<RwLock<OpsMetrics>>,
    services: Arc<RwLock<HashMap<String, ServiceEntry>>>,
    start_time: DateTime<Utc>,
}

impl OpsManager {
    /// Create a new operations manager with the given version and environment.
    pub fn new(version: &str, environment: Environment) -> Self {
        let now = Utc::now();
        Self {
            health_checks: Arc::new(RwLock::new(Vec::new())),
            deployment: Arc::new(RwLock::new(DeploymentInfo {
                version: version.to_string(),
                commit_hash: None,
                deployed_at: now,
                environment,
                features: Vec::new(),
            })),
            metrics: Arc::new(RwLock::new(OpsMetrics {
                requests_total: 0,
                errors_total: 0,
                active_connections: 0,
                memory_usage_bytes: 0,
                cpu_usage_percent: 0.0,
                disk_usage_bytes: 0,
                uptime_secs: 0,
                start_time: now,
            })),
            services: Arc::new(RwLock::new(HashMap::new())),
            start_time: now,
        }
    }

    // --- Health ---

    /// Register a new health check by name. Starts as Healthy.
    pub async fn register_health_check(&self, name: &str) {
        let mut checks = self.health_checks.write().await;
        checks.push(HealthCheck {
            name: name.to_string(),
            status: HealthStatus::Healthy,
            last_checked: Utc::now(),
            response_time_ms: None,
            metadata: HashMap::new(),
        });
    }

    /// Update the status of a named health check. Returns false if not found.
    pub async fn update_health_check(
        &self,
        name: &str,
        status: HealthStatus,
        response_time_ms: Option<u64>,
    ) -> bool {
        let mut checks = self.health_checks.write().await;
        if let Some(check) = checks.iter_mut().find(|c| c.name == name) {
            check.status = status;
            check.response_time_ms = response_time_ms;
            check.last_checked = Utc::now();
            true
        } else {
            false
        }
    }

    /// Build an aggregated system health snapshot.
    ///
    /// Overall logic:
    /// - If any check is Unhealthy, overall is Unhealthy.
    /// - Else if any check is Degraded, overall is Degraded.
    /// - Otherwise overall is Healthy.
    pub async fn get_system_health(&self) -> SystemHealth {
        let checks = self.health_checks.read().await;
        let overall = Self::compute_overall(&checks);
        let deployment = self.deployment.read().await;
        SystemHealth {
            overall,
            checks: checks.clone(),
            uptime_secs: self.uptime_secs_inner(),
            version: deployment.version.clone(),
            timestamp: Utc::now(),
        }
    }

    /// Returns true if the overall system health is Healthy.
    pub async fn is_healthy(&self) -> bool {
        let checks = self.health_checks.read().await;
        matches!(Self::compute_overall(&checks), HealthStatus::Healthy)
    }

    /// Compute overall health from a list of checks.
    fn compute_overall(checks: &[HealthCheck]) -> HealthStatus {
        let mut has_degraded = false;
        for check in checks {
            match &check.status {
                HealthStatus::Unhealthy(reason) => {
                    return HealthStatus::Unhealthy(reason.clone());
                }
                HealthStatus::Degraded(_) => {
                    has_degraded = true;
                }
                HealthStatus::Healthy => {}
            }
        }
        if has_degraded {
            HealthStatus::Degraded("one or more checks degraded".to_string())
        } else {
            HealthStatus::Healthy
        }
    }

    // --- Deployment ---

    /// Return a clone of the current deployment info.
    pub async fn get_deployment_info(&self) -> DeploymentInfo {
        self.deployment.read().await.clone()
    }

    /// Set the commit hash on the deployment info.
    pub async fn set_commit_hash(&self, hash: &str) {
        let mut dep = self.deployment.write().await;
        dep.commit_hash = Some(hash.to_string());
    }

    /// Add a feature flag. Duplicates are silently ignored.
    pub async fn add_feature_flag(&self, feature: &str) {
        let mut dep = self.deployment.write().await;
        if !dep.features.contains(&feature.to_string()) {
            dep.features.push(feature.to_string());
        }
    }

    /// Remove a feature flag. Returns false if not found.
    pub async fn remove_feature_flag(&self, feature: &str) -> bool {
        let mut dep = self.deployment.write().await;
        let len_before = dep.features.len();
        dep.features.retain(|f| f != feature);
        dep.features.len() < len_before
    }

    /// Check if a feature flag is set.
    pub async fn has_feature(&self, feature: &str) -> bool {
        let dep = self.deployment.read().await;
        dep.features.contains(&feature.to_string())
    }

    // --- Metrics ---

    /// Record one request.
    pub async fn record_request(&self) {
        let mut m = self.metrics.write().await;
        m.requests_total += 1;
    }

    /// Record one error.
    pub async fn record_error(&self) {
        let mut m = self.metrics.write().await;
        m.errors_total += 1;
    }

    /// Set the active connection count.
    pub async fn set_connections(&self, count: u64) {
        let mut m = self.metrics.write().await;
        m.active_connections = count;
    }

    /// Return a snapshot of the current metrics, with uptime refreshed.
    pub async fn get_metrics(&self) -> OpsMetrics {
        let mut m = self.metrics.write().await;
        m.uptime_secs = self.uptime_secs_inner();
        m.clone()
    }

    /// Return the uptime in seconds since this manager was created.
    pub async fn uptime_secs(&self) -> u64 {
        self.uptime_secs_inner()
    }

    /// Inner synchronous uptime calculation (no lock needed).
    fn uptime_secs_inner(&self) -> u64 {
        let elapsed = Utc::now() - self.start_time;
        elapsed.num_seconds().max(0) as u64
    }

    // --- Service Registry ---

    /// Register a service. Overwrites any existing entry with the same name.
    pub async fn register_service(&self, entry: ServiceEntry) {
        let mut services = self.services.write().await;
        services.insert(entry.name.clone(), entry);
    }

    /// Unregister a service by name. Returns false if not found.
    pub async fn unregister_service(&self, name: &str) -> bool {
        let mut services = self.services.write().await;
        services.remove(name).is_some()
    }

    /// Get a service entry by name.
    pub async fn get_service(&self, name: &str) -> Option<ServiceEntry> {
        let services = self.services.read().await;
        services.get(name).cloned()
    }

    /// List all registered services.
    pub async fn list_services(&self) -> Vec<ServiceEntry> {
        let services = self.services.read().await;
        services.values().cloned().collect()
    }

    /// Update the status of a registered service. Returns false if not found.
    pub async fn update_service_status(&self, name: &str, status: HealthStatus) -> bool {
        let mut services = self.services.write().await;
        if let Some(entry) = services.get_mut(name) {
            entry.status = status;
            entry.last_checked = Utc::now();
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_manager() -> OpsManager {
        OpsManager::new("1.0.0", Environment::Production)
    }

    fn make_service(name: &str, url: &str) -> ServiceEntry {
        ServiceEntry {
            name: name.to_string(),
            url: url.to_string(),
            status: HealthStatus::Healthy,
            last_checked: Utc::now(),
            check_interval_secs: 30,
        }
    }

    // --- Health Check Tests ---

    #[tokio::test]
    async fn test_register_health_check() {
        let mgr = make_manager();
        mgr.register_health_check("db").await;
        let health = mgr.get_system_health().await;
        assert_eq!(health.checks.len(), 1);
        assert_eq!(health.checks[0].name, "db");
        assert_eq!(health.checks[0].status, HealthStatus::Healthy);
    }

    #[tokio::test]
    async fn test_register_multiple_health_checks() {
        let mgr = make_manager();
        mgr.register_health_check("db").await;
        mgr.register_health_check("cache").await;
        mgr.register_health_check("llm").await;
        let health = mgr.get_system_health().await;
        assert_eq!(health.checks.len(), 3);
    }

    #[tokio::test]
    async fn test_update_health_check_found() {
        let mgr = make_manager();
        mgr.register_health_check("db").await;
        let updated = mgr
            .update_health_check("db", HealthStatus::Degraded("slow".to_string()), Some(150))
            .await;
        assert!(updated);
        let health = mgr.get_system_health().await;
        assert_eq!(
            health.checks[0].status,
            HealthStatus::Degraded("slow".to_string())
        );
        assert_eq!(health.checks[0].response_time_ms, Some(150));
    }

    #[tokio::test]
    async fn test_update_health_check_not_found() {
        let mgr = make_manager();
        let updated = mgr
            .update_health_check("ghost", HealthStatus::Healthy, None)
            .await;
        assert!(!updated);
    }

    #[tokio::test]
    async fn test_overall_all_healthy() {
        let mgr = make_manager();
        mgr.register_health_check("db").await;
        mgr.register_health_check("cache").await;
        let health = mgr.get_system_health().await;
        assert_eq!(health.overall, HealthStatus::Healthy);
    }

    #[tokio::test]
    async fn test_overall_one_degraded() {
        let mgr = make_manager();
        mgr.register_health_check("db").await;
        mgr.register_health_check("cache").await;
        mgr.update_health_check(
            "cache",
            HealthStatus::Degraded("high latency".to_string()),
            None,
        )
        .await;
        let health = mgr.get_system_health().await;
        assert!(matches!(health.overall, HealthStatus::Degraded(_)));
    }

    #[tokio::test]
    async fn test_overall_one_unhealthy() {
        let mgr = make_manager();
        mgr.register_health_check("db").await;
        mgr.register_health_check("cache").await;
        mgr.update_health_check(
            "db",
            HealthStatus::Unhealthy("connection refused".to_string()),
            None,
        )
        .await;
        let health = mgr.get_system_health().await;
        assert!(matches!(health.overall, HealthStatus::Unhealthy(_)));
    }

    #[tokio::test]
    async fn test_overall_unhealthy_trumps_degraded() {
        let mgr = make_manager();
        mgr.register_health_check("db").await;
        mgr.register_health_check("cache").await;
        mgr.register_health_check("llm").await;
        mgr.update_health_check("cache", HealthStatus::Degraded("slow".to_string()), None)
            .await;
        mgr.update_health_check("db", HealthStatus::Unhealthy("down".to_string()), None)
            .await;
        let health = mgr.get_system_health().await;
        assert!(matches!(health.overall, HealthStatus::Unhealthy(_)));
    }

    #[tokio::test]
    async fn test_is_healthy_true() {
        let mgr = make_manager();
        mgr.register_health_check("db").await;
        assert!(mgr.is_healthy().await);
    }

    #[tokio::test]
    async fn test_is_healthy_false_when_degraded() {
        let mgr = make_manager();
        mgr.register_health_check("db").await;
        mgr.update_health_check("db", HealthStatus::Degraded("slow".to_string()), None)
            .await;
        assert!(!mgr.is_healthy().await);
    }

    #[tokio::test]
    async fn test_is_healthy_empty_checks() {
        let mgr = make_manager();
        assert!(mgr.is_healthy().await);
    }

    #[tokio::test]
    async fn test_system_health_contains_version() {
        let mgr = make_manager();
        let health = mgr.get_system_health().await;
        assert_eq!(health.version, "1.0.0");
    }

    // --- Deployment Tests ---

    #[tokio::test]
    async fn test_deployment_info_defaults() {
        let mgr = make_manager();
        let info = mgr.get_deployment_info().await;
        assert_eq!(info.version, "1.0.0");
        assert_eq!(info.environment, Environment::Production);
        assert!(info.commit_hash.is_none());
        assert!(info.features.is_empty());
    }

    #[tokio::test]
    async fn test_set_commit_hash() {
        let mgr = make_manager();
        mgr.set_commit_hash("abc123").await;
        let info = mgr.get_deployment_info().await;
        assert_eq!(info.commit_hash, Some("abc123".to_string()));
    }

    #[tokio::test]
    async fn test_add_feature_flag() {
        let mgr = make_manager();
        mgr.add_feature_flag("voice").await;
        assert!(mgr.has_feature("voice").await);
    }

    #[tokio::test]
    async fn test_add_feature_flag_no_duplicates() {
        let mgr = make_manager();
        mgr.add_feature_flag("voice").await;
        mgr.add_feature_flag("voice").await;
        let info = mgr.get_deployment_info().await;
        assert_eq!(info.features.len(), 1);
    }

    #[tokio::test]
    async fn test_remove_feature_flag_found() {
        let mgr = make_manager();
        mgr.add_feature_flag("voice").await;
        assert!(mgr.remove_feature_flag("voice").await);
        assert!(!mgr.has_feature("voice").await);
    }

    #[tokio::test]
    async fn test_remove_feature_flag_not_found() {
        let mgr = make_manager();
        assert!(!mgr.remove_feature_flag("ghost").await);
    }

    #[tokio::test]
    async fn test_has_feature_false() {
        let mgr = make_manager();
        assert!(!mgr.has_feature("nonexistent").await);
    }

    #[tokio::test]
    async fn test_multiple_feature_flags() {
        let mgr = make_manager();
        mgr.add_feature_flag("voice").await;
        mgr.add_feature_flag("vision").await;
        mgr.add_feature_flag("brain").await;
        assert!(mgr.has_feature("voice").await);
        assert!(mgr.has_feature("vision").await);
        assert!(mgr.has_feature("brain").await);
        let info = mgr.get_deployment_info().await;
        assert_eq!(info.features.len(), 3);
    }

    // --- Metrics Tests ---

    #[tokio::test]
    async fn test_record_request() {
        let mgr = make_manager();
        mgr.record_request().await;
        mgr.record_request().await;
        mgr.record_request().await;
        let m = mgr.get_metrics().await;
        assert_eq!(m.requests_total, 3);
    }

    #[tokio::test]
    async fn test_record_error() {
        let mgr = make_manager();
        mgr.record_error().await;
        mgr.record_error().await;
        let m = mgr.get_metrics().await;
        assert_eq!(m.errors_total, 2);
    }

    #[tokio::test]
    async fn test_set_connections() {
        let mgr = make_manager();
        mgr.set_connections(42).await;
        let m = mgr.get_metrics().await;
        assert_eq!(m.active_connections, 42);
    }

    #[tokio::test]
    async fn test_metrics_initial_state() {
        let mgr = make_manager();
        let m = mgr.get_metrics().await;
        assert_eq!(m.requests_total, 0);
        assert_eq!(m.errors_total, 0);
        assert_eq!(m.active_connections, 0);
        assert_eq!(m.memory_usage_bytes, 0);
        assert_eq!(m.cpu_usage_percent, 0.0);
        assert_eq!(m.disk_usage_bytes, 0);
    }

    #[tokio::test]
    async fn test_uptime_is_non_negative() {
        let mgr = make_manager();
        let uptime = mgr.uptime_secs().await;
        // Just created, uptime should be 0 or very small
        assert!(uptime < 5);
    }

    #[tokio::test]
    async fn test_metrics_uptime_matches() {
        let mgr = make_manager();
        let m = mgr.get_metrics().await;
        let uptime = mgr.uptime_secs().await;
        // Both should be very close (within 1s of each other)
        assert!(m.uptime_secs.abs_diff(uptime) <= 1);
    }

    // --- Service Registry Tests ---

    #[tokio::test]
    async fn test_register_service() {
        let mgr = make_manager();
        mgr.register_service(make_service("llm", "http://localhost:8080"))
            .await;
        let svc = mgr.get_service("llm").await;
        assert!(svc.is_some());
        assert_eq!(svc.unwrap().url, "http://localhost:8080");
    }

    #[tokio::test]
    async fn test_unregister_service_found() {
        let mgr = make_manager();
        mgr.register_service(make_service("llm", "http://localhost:8080"))
            .await;
        assert!(mgr.unregister_service("llm").await);
        assert!(mgr.get_service("llm").await.is_none());
    }

    #[tokio::test]
    async fn test_unregister_service_not_found() {
        let mgr = make_manager();
        assert!(!mgr.unregister_service("ghost").await);
    }

    #[tokio::test]
    async fn test_get_service_not_found() {
        let mgr = make_manager();
        assert!(mgr.get_service("ghost").await.is_none());
    }

    #[tokio::test]
    async fn test_list_services_empty() {
        let mgr = make_manager();
        assert!(mgr.list_services().await.is_empty());
    }

    #[tokio::test]
    async fn test_list_services_multiple() {
        let mgr = make_manager();
        mgr.register_service(make_service("llm", "http://localhost:8080"))
            .await;
        mgr.register_service(make_service("searxng", "http://192.0.2.20:8080"))
            .await;
        let services = mgr.list_services().await;
        assert_eq!(services.len(), 2);
    }

    #[tokio::test]
    async fn test_update_service_status_found() {
        let mgr = make_manager();
        mgr.register_service(make_service("llm", "http://localhost:8080"))
            .await;
        let updated = mgr
            .update_service_status("llm", HealthStatus::Unhealthy("timeout".to_string()))
            .await;
        assert!(updated);
        let svc = mgr.get_service("llm").await.unwrap();
        assert_eq!(svc.status, HealthStatus::Unhealthy("timeout".to_string()));
    }

    #[tokio::test]
    async fn test_update_service_status_not_found() {
        let mgr = make_manager();
        assert!(
            !mgr.update_service_status("ghost", HealthStatus::Healthy)
                .await
        );
    }

    #[tokio::test]
    async fn test_register_service_overwrites() {
        let mgr = make_manager();
        mgr.register_service(make_service("llm", "http://old:8080"))
            .await;
        mgr.register_service(make_service("llm", "http://new:8080"))
            .await;
        let svc = mgr.get_service("llm").await.unwrap();
        assert_eq!(svc.url, "http://new:8080");
        assert_eq!(mgr.list_services().await.len(), 1);
    }

    // --- Serialization Roundtrip Tests ---

    #[tokio::test]
    async fn test_health_status_serialization_roundtrip() {
        let statuses = vec![
            HealthStatus::Healthy,
            HealthStatus::Degraded("slow".to_string()),
            HealthStatus::Unhealthy("down".to_string()),
        ];
        for status in statuses {
            let json = serde_json::to_string(&status).unwrap();
            let deserialized: HealthStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(status, deserialized);
        }
    }

    #[tokio::test]
    async fn test_environment_serialization_roundtrip() {
        let envs = vec![
            Environment::Development,
            Environment::Staging,
            Environment::Production,
        ];
        for env in envs {
            let json = serde_json::to_string(&env).unwrap();
            let deserialized: Environment = serde_json::from_str(&json).unwrap();
            assert_eq!(env, deserialized);
        }
    }

    #[tokio::test]
    async fn test_deployment_info_serialization_roundtrip() {
        let mgr = make_manager();
        mgr.set_commit_hash("abc123").await;
        mgr.add_feature_flag("voice").await;
        let info = mgr.get_deployment_info().await;
        let json = serde_json::to_string(&info).unwrap();
        let deserialized: DeploymentInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.version, "1.0.0");
        assert_eq!(deserialized.commit_hash, Some("abc123".to_string()));
        assert_eq!(deserialized.features, vec!["voice"]);
    }

    #[tokio::test]
    async fn test_system_health_serialization_roundtrip() {
        let mgr = make_manager();
        mgr.register_health_check("db").await;
        let health = mgr.get_system_health().await;
        let json = serde_json::to_string(&health).unwrap();
        let deserialized: SystemHealth = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.version, "1.0.0");
        assert_eq!(deserialized.checks.len(), 1);
    }

    #[tokio::test]
    async fn test_ops_metrics_serialization_roundtrip() {
        let mgr = make_manager();
        mgr.record_request().await;
        mgr.record_error().await;
        mgr.set_connections(5).await;
        let metrics = mgr.get_metrics().await;
        let json = serde_json::to_string(&metrics).unwrap();
        let deserialized: OpsMetrics = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.requests_total, 1);
        assert_eq!(deserialized.errors_total, 1);
        assert_eq!(deserialized.active_connections, 5);
    }

    #[tokio::test]
    async fn test_service_entry_serialization_roundtrip() {
        let entry = make_service("llm", "http://localhost:8080");
        let json = serde_json::to_string(&entry).unwrap();
        let deserialized: ServiceEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.name, "llm");
        assert_eq!(deserialized.url, "http://localhost:8080");
    }

    // --- Edge Case Tests ---

    #[tokio::test]
    async fn test_development_environment() {
        let mgr = OpsManager::new("0.1.0", Environment::Development);
        let info = mgr.get_deployment_info().await;
        assert_eq!(info.environment, Environment::Development);
    }

    #[tokio::test]
    async fn test_staging_environment() {
        let mgr = OpsManager::new("0.1.0", Environment::Staging);
        let info = mgr.get_deployment_info().await;
        assert_eq!(info.environment, Environment::Staging);
    }
}
