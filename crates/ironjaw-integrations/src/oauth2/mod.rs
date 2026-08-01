pub mod pkce;
pub mod providers;

use pkce::PkceChallenge;
use providers::OAuthProvider;
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum OAuth2Error {
    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("token endpoint returned error: {error} — {description}")]
    TokenError { error: String, description: String },
    #[error("PKCE error: {0}")]
    Pkce(#[from] pkce::PkceError),
}

/// Token response from an OAuth2 token endpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenResponse {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub expires_in: u64,
    #[serde(default = "default_token_type")]
    pub token_type: String,
}

fn default_token_type() -> String {
    "Bearer".to_string()
}

/// Error response from an OAuth2 token endpoint.
#[derive(Debug, Deserialize)]
struct TokenErrorResponse {
    error: String,
    #[serde(default)]
    error_description: String,
}

/// OAuth2 client with PKCE support.
pub struct OAuth2Client {
    http: reqwest::Client,
}

impl OAuth2Client {
    pub fn new() -> Self {
        Self {
            http: reqwest::Client::new(),
        }
    }

    /// Build the full authorization URL with PKCE parameters.
    pub fn build_auth_url(&self, provider: &OAuthProvider, challenge: &PkceChallenge) -> String {
        let scopes = provider.scopes.join(" ");
        let mut url = format!(
            "{}?response_type=code&client_id={}&redirect_uri={}&code_challenge={}&code_challenge_method={}",
            provider.auth_url,
            urlencoded(&provider.client_id),
            urlencoded(&provider.redirect_uri),
            urlencoded(&challenge.challenge),
            challenge.method,
        );
        if !scopes.is_empty() {
            url.push_str(&format!("&scope={}", urlencoded(&scopes)));
        }
        url
    }

    /// Exchange an authorization code for tokens using the PKCE verifier.
    pub async fn exchange_code(
        &self,
        provider: &OAuthProvider,
        code: &str,
        verifier: &str,
    ) -> Result<TokenResponse, OAuth2Error> {
        let params = [
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", &provider.redirect_uri),
            ("client_id", &provider.client_id),
            ("code_verifier", verifier),
        ];

        let resp = self
            .http
            .post(&provider.token_url)
            .form(&params)
            .send()
            .await?;

        if resp.status().is_success() {
            Ok(resp.json::<TokenResponse>().await?)
        } else {
            let err = resp
                .json::<TokenErrorResponse>()
                .await
                .unwrap_or(TokenErrorResponse {
                    error: "unknown_error".to_string(),
                    error_description: "No error details".to_string(),
                });
            Err(OAuth2Error::TokenError {
                error: err.error,
                description: err.error_description,
            })
        }
    }

    /// Refresh an access token using a refresh token.
    pub async fn refresh_token(
        &self,
        provider: &OAuthProvider,
        refresh_token: &str,
    ) -> Result<TokenResponse, OAuth2Error> {
        let params = [
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", &provider.client_id),
        ];

        let resp = self
            .http
            .post(&provider.token_url)
            .form(&params)
            .send()
            .await?;

        if resp.status().is_success() {
            Ok(resp.json::<TokenResponse>().await?)
        } else {
            let err = resp
                .json::<TokenErrorResponse>()
                .await
                .unwrap_or(TokenErrorResponse {
                    error: "unknown_error".to_string(),
                    error_description: "No error details".to_string(),
                });
            Err(OAuth2Error::TokenError {
                error: err.error,
                description: err.error_description,
            })
        }
    }
}

impl Default for OAuth2Client {
    fn default() -> Self {
        Self::new()
    }
}

/// Minimal percent-encoding for URL query parameters.
fn urlencoded(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                result.push(b as char);
            }
            _ => {
                result.push_str(&format!("%{b:02X}"));
            }
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_auth_url_contains_pkce_params() {
        let client = OAuth2Client::new();
        let provider = providers::gmail();
        let challenge = PkceChallenge {
            verifier: "test_verifier".to_string(),
            challenge: "test_challenge".to_string(),
            method: "S256",
        };

        let url = client.build_auth_url(&provider, &challenge);
        assert!(url.starts_with("https://accounts.google.com"));
        assert!(url.contains("code_challenge=test_challenge"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("response_type=code"));
        assert!(url.contains("client_id="));
        assert!(url.contains("redirect_uri="));
    }

    #[test]
    fn test_build_auth_url_includes_scopes() {
        let client = OAuth2Client::new();
        let provider = providers::gmail();
        let challenge = PkceChallenge {
            verifier: "v".to_string(),
            challenge: "c".to_string(),
            method: "S256",
        };

        let url = client.build_auth_url(&provider, &challenge);
        assert!(
            url.contains("scope="),
            "URL should contain scopes for Gmail"
        );
    }

    #[test]
    fn test_build_auth_url_no_scopes() {
        let client = OAuth2Client::new();
        let provider = providers::notion(); // Notion has empty scopes
        let challenge = PkceChallenge {
            verifier: "v".to_string(),
            challenge: "c".to_string(),
            method: "S256",
        };

        let url = client.build_auth_url(&provider, &challenge);
        assert!(
            !url.contains("scope="),
            "Notion URL should not have scope param when scopes are empty"
        );
    }

    #[test]
    fn test_token_response_deserialization() {
        let json = r#"{"access_token":"abc","refresh_token":"def","expires_in":3600,"token_type":"Bearer"}"#;
        let token: TokenResponse = serde_json::from_str(json).unwrap();
        assert_eq!(token.access_token, "abc");
        assert_eq!(token.refresh_token, Some("def".to_string()));
        assert_eq!(token.expires_in, 3600);
        assert_eq!(token.token_type, "Bearer");
    }

    #[test]
    fn test_token_response_minimal() {
        let json = r#"{"access_token":"xyz"}"#;
        let token: TokenResponse = serde_json::from_str(json).unwrap();
        assert_eq!(token.access_token, "xyz");
        assert_eq!(token.refresh_token, None);
        assert_eq!(token.expires_in, 0);
        assert_eq!(token.token_type, "Bearer");
    }

    #[test]
    fn test_oauth2_client_default() {
        let _client = OAuth2Client::default();
    }

    #[test]
    fn test_urlencoded_safe_chars() {
        assert_eq!(urlencoded("abc-._~"), "abc-._~");
    }

    #[test]
    fn test_urlencoded_special_chars() {
        assert_eq!(urlencoded("a b"), "a%20b");
        assert_eq!(urlencoded("a&b"), "a%26b");
        assert_eq!(urlencoded("a=b"), "a%3Db");
    }

    #[test]
    fn test_urlencoded_url() {
        let encoded = urlencoded("http://localhost:18794/callback");
        assert!(encoded.contains("%3A")); // colon
        assert!(encoded.contains("%2F")); // slash
    }
}
