use crate::types::{Tool, ToolContext, ToolResult};
use ironjaw_security::PermissionTier;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

const DEFAULT_ENDPOINT: &str = "http://127.0.0.1:7860/sdapi/v1/txt2img";
const DEFAULT_WIDTH: u32 = 512;
const DEFAULT_HEIGHT: u32 = 512;
const DEFAULT_STEPS: u32 = 20;
const MAX_DIMENSION: u32 = 2048;
const MIN_DIMENSION: u32 = 64;
const MAX_STEPS: u32 = 150;

/// Request payload for image generation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageGenRequest {
    pub prompt: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub negative_prompt: Option<String>,
    #[serde(default = "default_width")]
    pub width: u32,
    #[serde(default = "default_height")]
    pub height: u32,
    #[serde(default = "default_steps")]
    pub steps: u32,
}

fn default_width() -> u32 {
    DEFAULT_WIDTH
}
fn default_height() -> u32 {
    DEFAULT_HEIGHT
}
fn default_steps() -> u32 {
    DEFAULT_STEPS
}

/// Response from image generation endpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageGenResponse {
    pub images: Vec<String>,
    #[serde(default)]
    pub seed: i64,
}

/// Image generation tool backed by a Stable Diffusion-compatible API.
pub struct ImageGenTool {
    endpoint: String,
}

impl Default for ImageGenTool {
    fn default() -> Self {
        Self {
            endpoint: DEFAULT_ENDPOINT.to_string(),
        }
    }
}

impl ImageGenTool {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_endpoint(endpoint: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into(),
        }
    }

    /// Validate the generation request.
    fn validate(request: &ImageGenRequest) -> Result<(), String> {
        if request.prompt.is_empty() {
            return Err("Prompt must not be empty".to_string());
        }
        if request.width < MIN_DIMENSION || request.width > MAX_DIMENSION {
            return Err(format!(
                "Width must be between {MIN_DIMENSION} and {MAX_DIMENSION}"
            ));
        }
        if request.height < MIN_DIMENSION || request.height > MAX_DIMENSION {
            return Err(format!(
                "Height must be between {MIN_DIMENSION} and {MAX_DIMENSION}"
            ));
        }
        if request.steps == 0 || request.steps > MAX_STEPS {
            return Err(format!("Steps must be between 1 and {MAX_STEPS}"));
        }
        Ok(())
    }

    /// Send generation request to the endpoint.
    pub async fn generate(&self, request: ImageGenRequest) -> Result<ImageGenResponse, ToolError> {
        Self::validate(&request).map_err(ToolError::Validation)?;

        let client = reqwest::Client::new();
        let resp = client
            .post(&self.endpoint)
            .json(&request)
            .send()
            .await
            .map_err(|e| ToolError::Network(format!("Request failed: {e}")))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(ToolError::Api(format!("API returned {status}: {body}")));
        }

        let gen_resp: ImageGenResponse = resp
            .json()
            .await
            .map_err(|e| ToolError::Parse(format!("Failed to parse response: {e}")))?;

        Ok(gen_resp)
    }
}

/// Errors specific to image generation.
#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error("Validation error: {0}")]
    Validation(String),
    #[error("Network error: {0}")]
    Network(String),
    #[error("API error: {0}")]
    Api(String),
    #[error("Parse error: {0}")]
    Parse(String),
}

#[async_trait::async_trait]
impl Tool for ImageGenTool {
    fn name(&self) -> &str {
        "image_gen"
    }

    fn description(&self) -> &str {
        "Generate images using a Stable Diffusion-compatible API"
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "prompt": {
                    "type": "string",
                    "description": "Text prompt describing the image to generate"
                },
                "negative_prompt": {
                    "type": "string",
                    "description": "Negative prompt (things to avoid)"
                },
                "width": {
                    "type": "integer",
                    "description": "Image width in pixels (default 512)",
                    "minimum": MIN_DIMENSION,
                    "maximum": MAX_DIMENSION
                },
                "height": {
                    "type": "integer",
                    "description": "Image height in pixels (default 512)",
                    "minimum": MIN_DIMENSION,
                    "maximum": MAX_DIMENSION
                },
                "steps": {
                    "type": "integer",
                    "description": "Number of diffusion steps (default 20)",
                    "minimum": 1,
                    "maximum": MAX_STEPS
                }
            },
            "required": ["prompt"]
        })
    }

    fn tier(&self) -> PermissionTier {
        PermissionTier::Notify
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        _ctx: &ToolContext,
        _cancel: CancellationToken,
    ) -> ToolResult {
        let prompt = match params.get("prompt").and_then(|v| v.as_str()) {
            Some(p) => p.to_string(),
            None => {
                return ToolResult::error("INVALID_PARAMS", "Missing required parameter: prompt")
            }
        };

        let negative_prompt = params
            .get("negative_prompt")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        let width = params
            .get("width")
            .and_then(|v| v.as_u64())
            .map(|v| v as u32)
            .unwrap_or(DEFAULT_WIDTH);

        let height = params
            .get("height")
            .and_then(|v| v.as_u64())
            .map(|v| v as u32)
            .unwrap_or(DEFAULT_HEIGHT);

        let steps = params
            .get("steps")
            .and_then(|v| v.as_u64())
            .map(|v| v as u32)
            .unwrap_or(DEFAULT_STEPS);

        let request = ImageGenRequest {
            prompt,
            negative_prompt,
            width,
            height,
            steps,
        };

        match self.generate(request).await {
            Ok(resp) => ToolResult::json(serde_json::json!({
                "images": resp.images,
                "seed": resp.seed,
                "count": resp.images.len(),
            })),
            Err(e) => ToolResult::error("IMAGE_GEN_FAILED", e.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn test_ctx() -> ToolContext {
        ToolContext::new("test", "test", PathBuf::from("/tmp"))
    }

    #[test]
    fn test_request_serialization() {
        let req = ImageGenRequest {
            prompt: "a cat".to_string(),
            negative_prompt: Some("blurry".to_string()),
            width: 512,
            height: 512,
            steps: 20,
        };
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["prompt"], "a cat");
        assert_eq!(json["negative_prompt"], "blurry");
        assert_eq!(json["width"], 512);
        assert_eq!(json["height"], 512);
        assert_eq!(json["steps"], 20);
    }

    #[test]
    fn test_request_serialization_no_negative() {
        let req = ImageGenRequest {
            prompt: "a dog".to_string(),
            negative_prompt: None,
            width: 256,
            height: 256,
            steps: 10,
        };
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["prompt"], "a dog");
        assert!(json.get("negative_prompt").is_none());
    }

    #[test]
    fn test_request_deserialization_defaults() {
        let json = serde_json::json!({"prompt": "hello"});
        let req: ImageGenRequest = serde_json::from_value(json).unwrap();
        assert_eq!(req.prompt, "hello");
        assert!(req.negative_prompt.is_none());
        assert_eq!(req.width, DEFAULT_WIDTH);
        assert_eq!(req.height, DEFAULT_HEIGHT);
        assert_eq!(req.steps, DEFAULT_STEPS);
    }

    #[test]
    fn test_response_parsing() {
        let json = serde_json::json!({
            "images": ["base64data1", "base64data2"],
            "seed": 42
        });
        let resp: ImageGenResponse = serde_json::from_value(json).unwrap();
        assert_eq!(resp.images.len(), 2);
        assert_eq!(resp.images[0], "base64data1");
        assert_eq!(resp.seed, 42);
    }

    #[test]
    fn test_response_parsing_no_seed() {
        let json = serde_json::json!({
            "images": ["img1"]
        });
        let resp: ImageGenResponse = serde_json::from_value(json).unwrap();
        assert_eq!(resp.images.len(), 1);
        assert_eq!(resp.seed, 0); // default
    }

    #[test]
    fn test_validate_empty_prompt() {
        let req = ImageGenRequest {
            prompt: "".to_string(),
            negative_prompt: None,
            width: 512,
            height: 512,
            steps: 20,
        };
        assert!(ImageGenTool::validate(&req).is_err());
    }

    #[test]
    fn test_validate_width_too_large() {
        let req = ImageGenRequest {
            prompt: "test".to_string(),
            negative_prompt: None,
            width: 4096,
            height: 512,
            steps: 20,
        };
        let err = ImageGenTool::validate(&req).unwrap_err();
        assert!(err.contains("Width"));
    }

    #[test]
    fn test_validate_height_too_small() {
        let req = ImageGenRequest {
            prompt: "test".to_string(),
            negative_prompt: None,
            width: 512,
            height: 32,
            steps: 20,
        };
        let err = ImageGenTool::validate(&req).unwrap_err();
        assert!(err.contains("Height"));
    }

    #[test]
    fn test_validate_steps_zero() {
        let req = ImageGenRequest {
            prompt: "test".to_string(),
            negative_prompt: None,
            width: 512,
            height: 512,
            steps: 0,
        };
        assert!(ImageGenTool::validate(&req).is_err());
    }

    #[test]
    fn test_validate_steps_too_high() {
        let req = ImageGenRequest {
            prompt: "test".to_string(),
            negative_prompt: None,
            width: 512,
            height: 512,
            steps: 200,
        };
        assert!(ImageGenTool::validate(&req).is_err());
    }

    #[test]
    fn test_validate_success() {
        let req = ImageGenRequest {
            prompt: "a beautiful landscape".to_string(),
            negative_prompt: Some("ugly".to_string()),
            width: 768,
            height: 768,
            steps: 30,
        };
        assert!(ImageGenTool::validate(&req).is_ok());
    }

    #[tokio::test]
    async fn test_error_on_bad_endpoint() {
        let tool = ImageGenTool::with_endpoint("http://127.0.0.1:1/bad");
        let req = ImageGenRequest {
            prompt: "test".to_string(),
            negative_prompt: None,
            width: 512,
            height: 512,
            steps: 20,
        };
        let result = tool.generate(req).await;
        assert!(result.is_err());
        match result.unwrap_err() {
            ToolError::Network(_) => {}
            other => panic!("Expected Network error, got: {other}"),
        }
    }

    #[test]
    fn test_default_values() {
        let tool = ImageGenTool::new();
        assert_eq!(tool.endpoint, DEFAULT_ENDPOINT);
    }

    #[test]
    fn test_with_custom_endpoint() {
        let tool = ImageGenTool::with_endpoint("http://custom:1234/api");
        assert_eq!(tool.endpoint, "http://custom:1234/api");
    }

    #[test]
    fn test_tool_tier() {
        let tool = ImageGenTool::new();
        assert_eq!(tool.tier(), PermissionTier::Notify);
    }

    #[test]
    fn test_tool_name_and_desc() {
        let tool = ImageGenTool::new();
        assert_eq!(tool.name(), "image_gen");
        assert!(!tool.description().is_empty());
    }

    #[tokio::test]
    async fn test_execute_missing_prompt() {
        let tool = ImageGenTool::new();
        let result = tool
            .execute(serde_json::json!({}), &test_ctx(), CancellationToken::new())
            .await;
        assert!(result.is_error());
        match result {
            ToolResult::Error { code, .. } => assert_eq!(code, "INVALID_PARAMS"),
            _ => panic!("Expected Error result"),
        }
    }

    #[tokio::test]
    async fn test_execute_bad_endpoint() {
        let tool = ImageGenTool::with_endpoint("http://127.0.0.1:1/bad");
        let result = tool
            .execute(
                serde_json::json!({"prompt": "a cat"}),
                &test_ctx(),
                CancellationToken::new(),
            )
            .await;
        assert!(result.is_error());
        match result {
            ToolResult::Error { code, .. } => assert_eq!(code, "IMAGE_GEN_FAILED"),
            _ => panic!("Expected Error result"),
        }
    }
}
