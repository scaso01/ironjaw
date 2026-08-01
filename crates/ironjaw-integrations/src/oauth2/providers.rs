use serde::{Deserialize, Serialize};

/// Configuration for an OAuth2 provider.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OAuthProvider {
    pub name: String,
    pub auth_url: String,
    pub token_url: String,
    pub scopes: Vec<String>,
    pub client_id: String,
    pub redirect_uri: String,
}

/// Gmail OAuth2 provider (Google).
pub fn gmail() -> OAuthProvider {
    OAuthProvider {
        name: "gmail".to_string(),
        auth_url: "https://accounts.google.com/o/oauth2/v2/auth".to_string(),
        token_url: "https://oauth2.googleapis.com/token".to_string(),
        scopes: vec![
            "https://www.googleapis.com/auth/gmail.readonly".to_string(),
            "https://www.googleapis.com/auth/gmail.send".to_string(),
        ],
        client_id: "CONFIGURE_YOUR_CLIENT_ID".to_string(),
        redirect_uri: "http://localhost:18794/oauth2/callback".to_string(),
    }
}

/// Google Calendar OAuth2 provider.
pub fn calendar() -> OAuthProvider {
    OAuthProvider {
        name: "calendar".to_string(),
        auth_url: "https://accounts.google.com/o/oauth2/v2/auth".to_string(),
        token_url: "https://oauth2.googleapis.com/token".to_string(),
        scopes: vec![
            "https://www.googleapis.com/auth/calendar.readonly".to_string(),
            "https://www.googleapis.com/auth/calendar.events".to_string(),
        ],
        client_id: "CONFIGURE_YOUR_CLIENT_ID".to_string(),
        redirect_uri: "http://localhost:18794/oauth2/callback".to_string(),
    }
}

/// Notion OAuth2 provider.
pub fn notion() -> OAuthProvider {
    OAuthProvider {
        name: "notion".to_string(),
        auth_url: "https://api.notion.com/v1/oauth/authorize".to_string(),
        token_url: "https://api.notion.com/v1/oauth/token".to_string(),
        scopes: vec![],
        client_id: "CONFIGURE_YOUR_CLIENT_ID".to_string(),
        redirect_uri: "http://localhost:18794/oauth2/callback".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_gmail_provider() {
        let p = gmail();
        assert_eq!(p.name, "gmail");
        assert!(p.auth_url.contains("google"));
        assert!(p.token_url.contains("google"));
        assert!(!p.scopes.is_empty());
        assert!(p.scopes.iter().any(|s| s.contains("gmail")));
    }

    #[test]
    fn test_calendar_provider() {
        let p = calendar();
        assert_eq!(p.name, "calendar");
        assert!(p.auth_url.contains("google"));
        assert!(p.scopes.iter().any(|s| s.contains("calendar")));
    }

    #[test]
    fn test_notion_provider() {
        let p = notion();
        assert_eq!(p.name, "notion");
        assert!(p.auth_url.contains("notion.com"));
        assert!(p.token_url.contains("notion.com"));
    }

    #[test]
    fn test_providers_have_placeholder_client_id() {
        for p in [gmail(), calendar(), notion()] {
            assert_eq!(
                p.client_id, "CONFIGURE_YOUR_CLIENT_ID",
                "Provider {} should have placeholder client_id",
                p.name
            );
        }
    }

    #[test]
    fn test_providers_have_redirect_uri() {
        for p in [gmail(), calendar(), notion()] {
            assert!(
                p.redirect_uri.contains("18794"),
                "Provider {} redirect should use Ironjaw port",
                p.name
            );
        }
    }

    #[test]
    fn test_provider_serialization() {
        let p = gmail();
        let json = serde_json::to_string(&p).unwrap();
        let deserialized: OAuthProvider = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.name, p.name);
        assert_eq!(deserialized.auth_url, p.auth_url);
    }
}
