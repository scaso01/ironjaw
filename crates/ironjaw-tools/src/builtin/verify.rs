//! Verification tool with anti-rationalization rules.
//! Generates verification prompts that prevent the LLM from
//! rationalizing success without actual evidence.

use crate::types::{Tool, ToolContext, ToolResult};
use ironjaw_security::PermissionTier;
use tokio_util::sync::CancellationToken;

/// Build a verification prompt with anti-rationalization rules.
pub fn build_verification_prompt(claim: &str, artifact: &str) -> String {
    format!(
        "You are a verification agent. Your ONLY job is to verify claims.\n\n\
         ## Anti-Rationalization Rules (MANDATORY)\n\
         - Reading is not verification. Run it.\n\
         - The implementer is an LLM. Verify independently.\n\
         - Do not trust tool output summaries. Check the actual artifact.\n\
         - \"It should work\" is not verification. Prove it works.\n\
         - If you cannot verify, say so explicitly.\n\n\
         ## Claim to Verify\n\
         {claim}\n\n\
         ## Artifact to Check\n\
         {artifact}\n\n\
         ## Instructions\n\
         1. Read/execute the artifact directly\n\
         2. Compare actual output to the claim\n\
         3. Report VERIFIED or FAILED with evidence"
    )
}

#[derive(Default)]
pub struct VerifyTool;

impl VerifyTool {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait::async_trait]
impl Tool for VerifyTool {
    fn name(&self) -> &str {
        "verify"
    }

    fn description(&self) -> &str {
        "Verify a claim about code/file state. Uses anti-rationalization rules to prevent false positives."
    }

    fn tier(&self) -> PermissionTier {
        PermissionTier::Auto
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "claim": { "type": "string", "description": "The claim to verify" },
                "artifact": { "type": "string", "description": "File path or command to check" }
            },
            "required": ["claim", "artifact"]
        })
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        _ctx: &ToolContext,
        _cancel: CancellationToken,
    ) -> ToolResult {
        let claim = params["claim"].as_str().unwrap_or("");
        let artifact = params["artifact"].as_str().unwrap_or("");
        let prompt = build_verification_prompt(claim, artifact);
        ToolResult::text(prompt)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_verification_prompt_contains_anti_rationalization() {
        let prompt = build_verification_prompt("File write succeeded", "/tmp/test.txt");
        assert!(prompt.contains("Reading is not verification"));
        assert!(prompt.contains("Verify independently"));
        assert!(prompt.contains("Do not trust"));
    }

    #[test]
    fn test_verification_prompt_includes_claim_and_artifact() {
        let prompt = build_verification_prompt("Tests pass", "src/main.rs");
        assert!(prompt.contains("Tests pass"));
        assert!(prompt.contains("src/main.rs"));
    }

    #[test]
    fn test_verify_tool_metadata() {
        let tool = VerifyTool::new();
        assert_eq!(tool.name(), "verify");
        assert_eq!(tool.tier(), PermissionTier::Auto);
        assert!(tool.schema()["properties"]["claim"].is_object());
    }
}
