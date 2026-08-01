use axum::{
    extract::{Path, State},
    http::StatusCode,
    routing::{delete, get, post},
    Json, Router,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::info;

// === Types ===

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PushRegistration {
    pub device_id: String,
    pub push_token: String,
    pub platform: Platform,
    pub app_version: String,
    pub registered_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Platform {
    Android,
    Ios,
    Web,
}

#[derive(Debug, Deserialize)]
pub struct RegisterPushRequest {
    pub device_id: String,
    pub push_token: String,
    pub platform: Platform,
    pub app_version: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct RegisterPushResponse {
    pub ok: bool,
    pub device_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DevicePreferences {
    pub device_id: String,
    pub notifications_enabled: bool,
    pub theme: String,
    pub language: String,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Deserialize)]
pub struct UpdatePreferencesRequest {
    pub notifications_enabled: Option<bool>,
    pub theme: Option<String>,
    pub language: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SyncStatus {
    pub device_id: String,
    pub pending_messages: u64,
    pub last_sync: Option<DateTime<Utc>>,
    pub server_version: String,
}

// === State ===

#[derive(Clone)]
pub struct MobileApiState {
    registrations: Arc<RwLock<HashMap<String, PushRegistration>>>,
    preferences: Arc<RwLock<HashMap<String, DevicePreferences>>>,
    sync_timestamps: Arc<RwLock<HashMap<String, DateTime<Utc>>>>,
}

impl Default for MobileApiState {
    fn default() -> Self {
        Self::new()
    }
}

impl MobileApiState {
    pub fn new() -> Self {
        Self {
            registrations: Arc::new(RwLock::new(HashMap::new())),
            preferences: Arc::new(RwLock::new(HashMap::new())),
            sync_timestamps: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub async fn register_push(&self, req: RegisterPushRequest) -> RegisterPushResponse {
        let device_id = req.device_id.clone();
        let registration = PushRegistration {
            device_id: req.device_id,
            push_token: req.push_token,
            platform: req.platform,
            app_version: req.app_version,
            registered_at: Utc::now(),
        };
        self.registrations
            .write()
            .await
            .insert(device_id.clone(), registration);
        info!(device_id = %device_id, "Push registration saved");
        RegisterPushResponse {
            ok: true,
            device_id,
        }
    }

    pub async fn unregister_push(&self, device_id: &str) -> bool {
        let removed = self.registrations.write().await.remove(device_id).is_some();
        if removed {
            info!(device_id = %device_id, "Push registration removed");
        }
        removed
    }

    pub async fn get_registration(&self, device_id: &str) -> Option<PushRegistration> {
        self.registrations.read().await.get(device_id).cloned()
    }

    pub async fn list_registrations(&self) -> Vec<PushRegistration> {
        self.registrations.read().await.values().cloned().collect()
    }

    pub async fn get_preferences(&self, device_id: &str) -> Option<DevicePreferences> {
        self.preferences.read().await.get(device_id).cloned()
    }

    pub async fn update_preferences(
        &self,
        device_id: &str,
        req: UpdatePreferencesRequest,
    ) -> DevicePreferences {
        let mut prefs_map = self.preferences.write().await;
        let prefs = prefs_map
            .entry(device_id.to_string())
            .or_insert_with(|| DevicePreferences {
                device_id: device_id.to_string(),
                notifications_enabled: true,
                theme: "system".to_string(),
                language: "en".to_string(),
                updated_at: Utc::now(),
            });

        if let Some(enabled) = req.notifications_enabled {
            prefs.notifications_enabled = enabled;
        }
        if let Some(theme) = req.theme {
            prefs.theme = theme;
        }
        if let Some(language) = req.language {
            prefs.language = language;
        }
        prefs.updated_at = Utc::now();
        info!(device_id = %device_id, "Preferences updated");
        prefs.clone()
    }

    pub async fn get_sync_status(&self, device_id: &str) -> SyncStatus {
        let last_sync = self.sync_timestamps.read().await.get(device_id).copied();
        SyncStatus {
            device_id: device_id.to_string(),
            pending_messages: 0,
            last_sync,
            server_version: env!("CARGO_PKG_VERSION").to_string(),
        }
    }

    pub async fn mark_synced(&self, device_id: &str) {
        self.sync_timestamps
            .write()
            .await
            .insert(device_id.to_string(), Utc::now());
        info!(device_id = %device_id, "Device marked as synced");
    }
}

// === Route handlers ===

async fn register_push_handler(
    State(state): State<MobileApiState>,
    Json(req): Json<RegisterPushRequest>,
) -> (StatusCode, Json<RegisterPushResponse>) {
    let resp = state.register_push(req).await;
    (StatusCode::CREATED, Json(resp))
}

async fn unregister_push_handler(
    State(state): State<MobileApiState>,
    Path(device_id): Path<String>,
) -> StatusCode {
    if state.unregister_push(&device_id).await {
        StatusCode::NO_CONTENT
    } else {
        StatusCode::NOT_FOUND
    }
}

async fn list_registrations_handler(
    State(state): State<MobileApiState>,
) -> Json<Vec<PushRegistration>> {
    Json(state.list_registrations().await)
}

async fn get_preferences_handler(
    State(state): State<MobileApiState>,
    Path(device_id): Path<String>,
) -> Result<Json<DevicePreferences>, StatusCode> {
    state
        .get_preferences(&device_id)
        .await
        .map(Json)
        .ok_or(StatusCode::NOT_FOUND)
}

async fn update_preferences_handler(
    State(state): State<MobileApiState>,
    Path(device_id): Path<String>,
    Json(req): Json<UpdatePreferencesRequest>,
) -> Json<DevicePreferences> {
    Json(state.update_preferences(&device_id, req).await)
}

async fn get_sync_status_handler(
    State(state): State<MobileApiState>,
    Path(device_id): Path<String>,
) -> Json<SyncStatus> {
    Json(state.get_sync_status(&device_id).await)
}

async fn mark_synced_handler(
    State(state): State<MobileApiState>,
    Path(device_id): Path<String>,
) -> StatusCode {
    state.mark_synced(&device_id).await;
    StatusCode::NO_CONTENT
}

// === Router ===

pub fn mobile_api_routes(state: MobileApiState) -> Router {
    Router::new()
        .route("/api/mobile/push/register", post(register_push_handler))
        .route(
            "/api/mobile/push/{device_id}",
            delete(unregister_push_handler),
        )
        .route("/api/mobile/push", get(list_registrations_handler))
        .route(
            "/api/mobile/devices/{device_id}/preferences",
            get(get_preferences_handler).post(update_preferences_handler),
        )
        .route(
            "/api/mobile/devices/{device_id}/sync",
            get(get_sync_status_handler).post(mark_synced_handler),
        )
        .with_state(state)
}

// === Tests ===

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    fn test_state() -> MobileApiState {
        MobileApiState::new()
    }

    fn test_app() -> Router {
        mobile_api_routes(test_state())
    }

    fn app_with_state(state: MobileApiState) -> Router {
        mobile_api_routes(state)
    }

    // --- Push Registration CRUD ---

    #[tokio::test]
    async fn test_register_push() {
        let app = test_app();
        let body = serde_json::json!({
            "device_id": "dev-1",
            "push_token": "fcm-token-abc",
            "platform": "android",
            "app_version": "0.9.3"
        });

        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/mobile/push/register")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::CREATED);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let parsed: RegisterPushResponse = serde_json::from_slice(&body).unwrap();
        assert!(parsed.ok);
        assert_eq!(parsed.device_id, "dev-1");
    }

    #[tokio::test]
    async fn test_register_push_ios() {
        let app = test_app();
        let body = serde_json::json!({
            "device_id": "ios-1",
            "push_token": "apns-token-xyz",
            "platform": "ios",
            "app_version": "1.0.0"
        });

        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/mobile/push/register")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::CREATED);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let parsed: RegisterPushResponse = serde_json::from_slice(&body).unwrap();
        assert!(parsed.ok);
        assert_eq!(parsed.device_id, "ios-1");
    }

    #[tokio::test]
    async fn test_list_registrations_empty() {
        let app = test_app();

        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/mobile/push")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let parsed: Vec<PushRegistration> = serde_json::from_slice(&body).unwrap();
        assert!(parsed.is_empty());
    }

    #[tokio::test]
    async fn test_list_registrations_with_data() {
        let state = test_state();
        state
            .register_push(RegisterPushRequest {
                device_id: "dev-1".to_string(),
                push_token: "token-1".to_string(),
                platform: Platform::Android,
                app_version: "1.0".to_string(),
            })
            .await;
        state
            .register_push(RegisterPushRequest {
                device_id: "dev-2".to_string(),
                push_token: "token-2".to_string(),
                platform: Platform::Web,
                app_version: "2.0".to_string(),
            })
            .await;

        let app = app_with_state(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/mobile/push")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let parsed: Vec<PushRegistration> = serde_json::from_slice(&body).unwrap();
        assert_eq!(parsed.len(), 2);
    }

    #[tokio::test]
    async fn test_unregister_push_existing() {
        let state = test_state();
        state
            .register_push(RegisterPushRequest {
                device_id: "dev-1".to_string(),
                push_token: "token-1".to_string(),
                platform: Platform::Android,
                app_version: "1.0".to_string(),
            })
            .await;

        let app = app_with_state(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/api/mobile/push/dev-1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn test_unregister_push_nonexistent() {
        let app = test_app();

        let resp = app
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/api/mobile/push/nonexistent")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_register_push_overwrites() {
        let state = test_state();
        state
            .register_push(RegisterPushRequest {
                device_id: "dev-1".to_string(),
                push_token: "old-token".to_string(),
                platform: Platform::Android,
                app_version: "1.0".to_string(),
            })
            .await;
        state
            .register_push(RegisterPushRequest {
                device_id: "dev-1".to_string(),
                push_token: "new-token".to_string(),
                platform: Platform::Android,
                app_version: "1.1".to_string(),
            })
            .await;

        let regs = state.list_registrations().await;
        assert_eq!(regs.len(), 1);
        assert_eq!(regs[0].push_token, "new-token");
        assert_eq!(regs[0].app_version, "1.1");
    }

    // --- Preferences ---

    #[tokio::test]
    async fn test_get_preferences_not_found() {
        let app = test_app();

        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/mobile/devices/dev-1/preferences")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_update_preferences_creates_defaults() {
        let state = test_state();
        let app = app_with_state(state.clone());

        let body = serde_json::json!({
            "theme": "dark"
        });

        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/mobile/devices/dev-1/preferences")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let parsed: DevicePreferences = serde_json::from_slice(&body).unwrap();
        assert_eq!(parsed.device_id, "dev-1");
        assert_eq!(parsed.theme, "dark");
        // Defaults for unset fields
        assert!(parsed.notifications_enabled);
        assert_eq!(parsed.language, "en");
    }

    #[tokio::test]
    async fn test_update_preferences_partial_update() {
        let state = test_state();

        // First, set all fields
        state
            .update_preferences(
                "dev-1",
                UpdatePreferencesRequest {
                    notifications_enabled: Some(true),
                    theme: Some("dark".to_string()),
                    language: Some("fr".to_string()),
                },
            )
            .await;

        // Now update only one field
        let app = app_with_state(state.clone());
        let body = serde_json::json!({
            "notifications_enabled": false
        });

        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/mobile/devices/dev-1/preferences")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let parsed: DevicePreferences = serde_json::from_slice(&body).unwrap();
        assert!(!parsed.notifications_enabled);
        // Other fields unchanged
        assert_eq!(parsed.theme, "dark");
        assert_eq!(parsed.language, "fr");
    }

    #[tokio::test]
    async fn test_get_preferences_after_update() {
        let state = test_state();
        state
            .update_preferences(
                "dev-1",
                UpdatePreferencesRequest {
                    notifications_enabled: Some(false),
                    theme: Some("light".to_string()),
                    language: None,
                },
            )
            .await;

        let app = app_with_state(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/mobile/devices/dev-1/preferences")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let parsed: DevicePreferences = serde_json::from_slice(&body).unwrap();
        assert!(!parsed.notifications_enabled);
        assert_eq!(parsed.theme, "light");
        assert_eq!(parsed.language, "en"); // default
    }

    #[tokio::test]
    async fn test_preferences_empty_update_preserves_defaults() {
        let state = test_state();

        // Update with all None fields -- creates defaults, changes nothing
        let prefs = state
            .update_preferences(
                "dev-1",
                UpdatePreferencesRequest {
                    notifications_enabled: None,
                    theme: None,
                    language: None,
                },
            )
            .await;

        assert!(prefs.notifications_enabled);
        assert_eq!(prefs.theme, "system");
        assert_eq!(prefs.language, "en");
    }

    // --- Sync Status ---

    #[tokio::test]
    async fn test_get_sync_status_no_prior_sync() {
        let app = test_app();

        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/mobile/devices/dev-1/sync")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let parsed: SyncStatus = serde_json::from_slice(&body).unwrap();
        assert_eq!(parsed.device_id, "dev-1");
        assert!(parsed.last_sync.is_none());
        assert_eq!(parsed.pending_messages, 0);
        assert_eq!(parsed.server_version, env!("CARGO_PKG_VERSION"));
    }

    #[tokio::test]
    async fn test_mark_synced_then_get_status() {
        let state = test_state();

        // Mark synced
        let app = app_with_state(state.clone());
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/mobile/devices/dev-1/sync")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);

        // Check status
        let app = app_with_state(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/mobile/devices/dev-1/sync")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let parsed: SyncStatus = serde_json::from_slice(&body).unwrap();
        assert!(parsed.last_sync.is_some());
    }

    #[tokio::test]
    async fn test_mark_synced_updates_timestamp() {
        let state = test_state();
        state.mark_synced("dev-1").await;
        let status1 = state.get_sync_status("dev-1").await;
        let ts1 = status1.last_sync.unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        state.mark_synced("dev-1").await;
        let status2 = state.get_sync_status("dev-1").await;
        let ts2 = status2.last_sync.unwrap();

        assert!(ts2 > ts1);
    }

    // --- Platform Enum ---

    #[test]
    fn test_platform_serialization() {
        assert_eq!(
            serde_json::to_string(&Platform::Android).unwrap(),
            "\"android\""
        );
        assert_eq!(serde_json::to_string(&Platform::Ios).unwrap(), "\"ios\"");
        assert_eq!(serde_json::to_string(&Platform::Web).unwrap(), "\"web\"");
    }

    #[test]
    fn test_platform_deserialization() {
        let android: Platform = serde_json::from_str("\"android\"").unwrap();
        assert_eq!(android, Platform::Android);

        let ios: Platform = serde_json::from_str("\"ios\"").unwrap();
        assert_eq!(ios, Platform::Ios);

        let web: Platform = serde_json::from_str("\"web\"").unwrap();
        assert_eq!(web, Platform::Web);
    }

    #[test]
    fn test_platform_roundtrip() {
        for platform in [Platform::Android, Platform::Ios, Platform::Web] {
            let json = serde_json::to_string(&platform).unwrap();
            let parsed: Platform = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, platform);
        }
    }

    // --- JSON Roundtrips ---

    #[test]
    fn test_push_registration_roundtrip() {
        let reg = PushRegistration {
            device_id: "dev-1".to_string(),
            push_token: "fcm-abc".to_string(),
            platform: Platform::Android,
            app_version: "0.9.3".to_string(),
            registered_at: Utc::now(),
        };
        let json = serde_json::to_string(&reg).unwrap();
        let parsed: PushRegistration = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.device_id, reg.device_id);
        assert_eq!(parsed.push_token, reg.push_token);
        assert_eq!(parsed.platform, reg.platform);
        assert_eq!(parsed.app_version, reg.app_version);
    }

    #[test]
    fn test_device_preferences_roundtrip() {
        let prefs = DevicePreferences {
            device_id: "dev-1".to_string(),
            notifications_enabled: false,
            theme: "dark".to_string(),
            language: "ja".to_string(),
            updated_at: Utc::now(),
        };
        let json = serde_json::to_string(&prefs).unwrap();
        let parsed: DevicePreferences = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.device_id, prefs.device_id);
        assert!(!parsed.notifications_enabled);
        assert_eq!(parsed.theme, "dark");
        assert_eq!(parsed.language, "ja");
    }

    #[test]
    fn test_sync_status_roundtrip() {
        let status = SyncStatus {
            device_id: "dev-1".to_string(),
            pending_messages: 42,
            last_sync: Some(Utc::now()),
            server_version: "0.1.0".to_string(),
        };
        let json = serde_json::to_string(&status).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["device_id"], "dev-1");
        assert_eq!(parsed["pending_messages"], 42);
        assert!(parsed["last_sync"].is_string());
    }

    #[test]
    fn test_sync_status_null_last_sync() {
        let status = SyncStatus {
            device_id: "dev-1".to_string(),
            pending_messages: 0,
            last_sync: None,
            server_version: "0.1.0".to_string(),
        };
        let json = serde_json::to_string(&status).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(parsed["last_sync"].is_null());
    }

    // --- State Unit Tests ---

    #[tokio::test]
    async fn test_state_get_registration_nonexistent() {
        let state = test_state();
        assert!(state.get_registration("nope").await.is_none());
    }

    #[tokio::test]
    async fn test_state_get_registration_exists() {
        let state = test_state();
        state
            .register_push(RegisterPushRequest {
                device_id: "dev-1".to_string(),
                push_token: "token".to_string(),
                platform: Platform::Android,
                app_version: "1.0".to_string(),
            })
            .await;

        let reg = state.get_registration("dev-1").await;
        assert!(reg.is_some());
        let reg = reg.unwrap();
        assert_eq!(reg.push_token, "token");
        assert_eq!(reg.platform, Platform::Android);
    }

    #[tokio::test]
    async fn test_state_multiple_devices() {
        let state = test_state();
        for i in 0..5 {
            state
                .register_push(RegisterPushRequest {
                    device_id: format!("dev-{i}"),
                    push_token: format!("token-{i}"),
                    platform: Platform::Android,
                    app_version: "1.0".to_string(),
                })
                .await;
        }

        let regs = state.list_registrations().await;
        assert_eq!(regs.len(), 5);
    }

    #[tokio::test]
    async fn test_state_default() {
        let state = MobileApiState::default();
        assert!(state.list_registrations().await.is_empty());
    }

    // --- Route: Invalid JSON ---

    #[tokio::test]
    async fn test_register_push_invalid_json() {
        let app = test_app();

        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/mobile/push/register")
                    .header("content-type", "application/json")
                    .body(Body::from(b"not json".to_vec()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_register_push_missing_field() {
        let app = test_app();
        let body = serde_json::json!({
            "device_id": "dev-1",
            "push_token": "token"
            // missing platform and app_version
        });

        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/mobile/push/register")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[tokio::test]
    async fn test_update_preferences_invalid_json() {
        let app = test_app();

        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/mobile/devices/dev-1/preferences")
                    .header("content-type", "application/json")
                    .body(Body::from(b"{bad".to_vec()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }
}
