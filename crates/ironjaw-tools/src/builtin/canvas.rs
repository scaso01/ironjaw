use crate::types::{Tool, ToolContext, ToolResult};
use ironjaw_security::PermissionTier;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

/// Template types for canvas rendering.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum CanvasTemplate {
    Table,
    Summary,
    Report,
    Code,
}

/// Request for canvas rendering.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CanvasRequest {
    pub template: CanvasTemplate,
    pub title: String,
    pub data: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub style: Option<String>,
}

/// Canvas presentation tool that renders structured data as standalone HTML.
pub struct CanvasTool;

impl Default for CanvasTool {
    fn default() -> Self {
        Self
    }
}

impl CanvasTool {
    pub fn new() -> Self {
        Self
    }

    /// Render a canvas request to a standalone HTML string.
    pub fn render(&self, request: CanvasRequest) -> Result<String, ToolError> {
        let title = html_escape(&request.title);
        let custom_css = request
            .style
            .as_deref()
            .map(html_escape)
            .unwrap_or_default();

        let body = match request.template {
            CanvasTemplate::Table => self.render_table(&request.data)?,
            CanvasTemplate::Summary => self.render_summary(&request.data)?,
            CanvasTemplate::Report => self.render_report(&request.data)?,
            CanvasTemplate::Code => self.render_code(&request.data)?,
        };

        Ok(format!(
            r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>{title}</title>
<style>
{BASE_CSS}
{custom_css}
</style>
</head>
<body>
<h1>{title}</h1>
{body}
</body>
</html>"#
        ))
    }

    /// Render JSON array as an HTML table.
    fn render_table(&self, data: &serde_json::Value) -> Result<String, ToolError> {
        let rows = data
            .as_array()
            .ok_or_else(|| ToolError::InvalidData("Table template requires a JSON array".into()))?;

        if rows.is_empty() {
            return Ok("<p>No data.</p>".to_string());
        }

        // Extract headers from first object
        let headers: Vec<String> = match rows[0].as_object() {
            Some(obj) => obj.keys().cloned().collect(),
            None => {
                return Err(ToolError::InvalidData(
                    "Table rows must be JSON objects".into(),
                ))
            }
        };

        let mut html = String::from("<table>\n<thead><tr>");
        for h in &headers {
            html.push_str(&format!("<th>{}</th>", html_escape(h)));
        }
        html.push_str("</tr></thead>\n<tbody>\n");

        for row in rows {
            html.push_str("<tr>");
            if let Some(obj) = row.as_object() {
                for h in &headers {
                    let val = obj.get(h).map(value_to_string).unwrap_or_default();
                    html.push_str(&format!("<td>{}</td>", html_escape(&val)));
                }
            }
            html.push_str("</tr>\n");
        }
        html.push_str("</tbody>\n</table>");
        Ok(html)
    }

    /// Render JSON object as key-value summary card.
    fn render_summary(&self, data: &serde_json::Value) -> Result<String, ToolError> {
        let obj = data.as_object().ok_or_else(|| {
            ToolError::InvalidData("Summary template requires a JSON object".into())
        })?;

        if obj.is_empty() {
            return Ok("<p>No data.</p>".to_string());
        }

        let mut html = String::from("<div class=\"card\">\n<dl>\n");
        for (key, val) in obj {
            html.push_str(&format!(
                "<dt>{}</dt><dd>{}</dd>\n",
                html_escape(key),
                html_escape(&value_to_string(val))
            ));
        }
        html.push_str("</dl>\n</div>");
        Ok(html)
    }

    /// Render report with title and sections array: [{heading, content}].
    fn render_report(&self, data: &serde_json::Value) -> Result<String, ToolError> {
        let sections = data.as_array().ok_or_else(|| {
            ToolError::InvalidData(
                "Report template requires a JSON array of {heading, content}".into(),
            )
        })?;

        if sections.is_empty() {
            return Ok("<p>No sections.</p>".to_string());
        }

        let mut html = String::new();
        for section in sections {
            let heading = section
                .get("heading")
                .and_then(|v| v.as_str())
                .unwrap_or("Untitled");
            let content = section
                .get("content")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            html.push_str(&format!(
                "<section>\n<h2>{}</h2>\n<p>{}</p>\n</section>\n",
                html_escape(heading),
                html_escape(content)
            ));
        }
        Ok(html)
    }

    /// Render code block with syntax highlighting CSS.
    fn render_code(&self, data: &serde_json::Value) -> Result<String, ToolError> {
        let code = match data.as_str() {
            Some(s) => s.to_string(),
            None => match data.get("code").and_then(|v| v.as_str()) {
                Some(s) => s.to_string(),
                None => value_to_string(data),
            },
        };

        let language = data
            .get("language")
            .and_then(|v| v.as_str())
            .unwrap_or("text");

        Ok(format!(
            "<pre><code class=\"language-{}\">{}</code></pre>",
            html_escape(language),
            html_escape(&code)
        ))
    }
}

/// Escape HTML special characters to prevent XSS.
fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#x27;")
}

/// Convert a serde_json Value to a display string.
fn value_to_string(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// Errors specific to canvas rendering.
#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error("Invalid data: {0}")]
    InvalidData(String),
}

const BASE_CSS: &str = r#"
* { margin: 0; padding: 0; box-sizing: border-box; }
body { font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, sans-serif;
       max-width: 900px; margin: 0 auto; padding: 2rem; color: #1a1a2e; background: #fafafa; }
h1 { margin-bottom: 1.5rem; font-size: 1.75rem; }
h2 { margin-bottom: 0.75rem; font-size: 1.25rem; color: #16213e; }
table { width: 100%; border-collapse: collapse; margin: 1rem 0; }
th, td { padding: 0.5rem 0.75rem; text-align: left; border: 1px solid #ddd; }
th { background: #16213e; color: #fff; }
tr:nth-child(even) { background: #f2f2f2; }
.card { background: #fff; border-radius: 8px; padding: 1.5rem; box-shadow: 0 2px 4px rgba(0,0,0,0.1); }
dl { display: grid; grid-template-columns: auto 1fr; gap: 0.5rem 1rem; }
dt { font-weight: 600; color: #16213e; }
dd { color: #555; }
section { margin-bottom: 1.5rem; }
section p { line-height: 1.6; color: #333; }
pre { background: #1a1a2e; color: #e8e8e8; padding: 1rem; border-radius: 6px; overflow-x: auto; }
code { font-family: 'Cascadia Code', 'Fira Code', monospace; font-size: 0.9rem; }
"#;

#[async_trait::async_trait]
impl Tool for CanvasTool {
    fn name(&self) -> &str {
        "canvas"
    }

    fn description(&self) -> &str {
        "Render structured data as standalone HTML presentations"
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "template": {
                    "type": "string",
                    "enum": ["table", "summary", "report", "code"],
                    "description": "Template type to use"
                },
                "title": {
                    "type": "string",
                    "description": "Title of the presentation"
                },
                "data": {
                    "description": "Data to render (structure depends on template)"
                },
                "style": {
                    "type": "string",
                    "description": "Optional custom CSS to inject"
                }
            },
            "required": ["template", "title", "data"]
        })
    }

    fn tier(&self) -> PermissionTier {
        PermissionTier::Auto
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        _ctx: &ToolContext,
        _cancel: CancellationToken,
    ) -> ToolResult {
        let template_str = match params.get("template").and_then(|v| v.as_str()) {
            Some(t) => t,
            None => {
                return ToolResult::error("INVALID_PARAMS", "Missing required parameter: template")
            }
        };

        let template: CanvasTemplate = match serde_json::from_value(
            serde_json::Value::String(template_str.to_string()),
        ) {
            Ok(t) => t,
            Err(_) => {
                return ToolResult::error(
                    "INVALID_PARAMS",
                    format!("Invalid template: '{template_str}'. Must be one of: table, summary, report, code"),
                )
            }
        };

        let title = match params.get("title").and_then(|v| v.as_str()) {
            Some(t) => t.to_string(),
            None => {
                return ToolResult::error("INVALID_PARAMS", "Missing required parameter: title")
            }
        };

        let data = match params.get("data") {
            Some(d) => d.clone(),
            None => return ToolResult::error("INVALID_PARAMS", "Missing required parameter: data"),
        };

        let style = params
            .get("style")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        let request = CanvasRequest {
            template,
            title,
            data,
            style,
        };

        match self.render(request) {
            Ok(html) => ToolResult::text(html),
            Err(e) => ToolResult::error("CANVAS_RENDER_FAILED", e.to_string()),
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
    fn test_table_template_basic() {
        let tool = CanvasTool::new();
        let req = CanvasRequest {
            template: CanvasTemplate::Table,
            title: "Test Table".to_string(),
            data: serde_json::json!([
                {"name": "Alice", "age": 30},
                {"name": "Bob", "age": 25}
            ]),
            style: None,
        };
        let html = tool.render(req).unwrap();
        assert!(html.contains("<table>"));
        assert!(html.contains("<th>name</th>"));
        assert!(html.contains("<th>age</th>"));
        assert!(html.contains("<td>Alice</td>"));
        assert!(html.contains("<td>Bob</td>"));
        assert!(html.contains("<!DOCTYPE html>"));
    }

    #[test]
    fn test_summary_template() {
        let tool = CanvasTool::new();
        let req = CanvasRequest {
            template: CanvasTemplate::Summary,
            title: "Status".to_string(),
            data: serde_json::json!({"status": "healthy", "uptime": "3 days"}),
            style: None,
        };
        let html = tool.render(req).unwrap();
        assert!(html.contains("<div class=\"card\">"));
        assert!(html.contains("<dt>status</dt>"));
        assert!(html.contains("<dd>healthy</dd>"));
    }

    #[test]
    fn test_report_template() {
        let tool = CanvasTool::new();
        let req = CanvasRequest {
            template: CanvasTemplate::Report,
            title: "Weekly Report".to_string(),
            data: serde_json::json!([
                {"heading": "Overview", "content": "All systems go."},
                {"heading": "Issues", "content": "None found."}
            ]),
            style: None,
        };
        let html = tool.render(req).unwrap();
        assert!(html.contains("<h2>Overview</h2>"));
        assert!(html.contains("<p>All systems go.</p>"));
        assert!(html.contains("<h2>Issues</h2>"));
    }

    #[test]
    fn test_code_template_string() {
        let tool = CanvasTool::new();
        let req = CanvasRequest {
            template: CanvasTemplate::Code,
            title: "Example".to_string(),
            data: serde_json::json!("fn main() {}"),
            style: None,
        };
        let html = tool.render(req).unwrap();
        assert!(html.contains("<pre><code"));
        assert!(html.contains("fn main() {}"));
    }

    #[test]
    fn test_code_template_object() {
        let tool = CanvasTool::new();
        let req = CanvasRequest {
            template: CanvasTemplate::Code,
            title: "Rust Code".to_string(),
            data: serde_json::json!({"code": "let x = 42;", "language": "rust"}),
            style: None,
        };
        let html = tool.render(req).unwrap();
        assert!(html.contains("language-rust"));
        assert!(html.contains("let x = 42;"));
    }

    #[test]
    fn test_html_escaping_xss_prevention() {
        let tool = CanvasTool::new();
        let req = CanvasRequest {
            template: CanvasTemplate::Summary,
            title: "<script>alert('xss')</script>".to_string(),
            data: serde_json::json!({"key": "<img onerror=alert(1) src=x>"}),
            style: None,
        };
        let html = tool.render(req).unwrap();
        assert!(!html.contains("<script>"));
        assert!(html.contains("&lt;script&gt;"));
        assert!(!html.contains("<img"));
        assert!(html.contains("&lt;img"));
    }

    #[test]
    fn test_html_escaping_in_table() {
        let tool = CanvasTool::new();
        let req = CanvasRequest {
            template: CanvasTemplate::Table,
            title: "Safe".to_string(),
            data: serde_json::json!([{"val": "a&b<c>d\"e'f"}]),
            style: None,
        };
        let html = tool.render(req).unwrap();
        assert!(html.contains("a&amp;b&lt;c&gt;d&quot;e&#x27;f"));
    }

    #[test]
    fn test_empty_table_data() {
        let tool = CanvasTool::new();
        let req = CanvasRequest {
            template: CanvasTemplate::Table,
            title: "Empty".to_string(),
            data: serde_json::json!([]),
            style: None,
        };
        let html = tool.render(req).unwrap();
        assert!(html.contains("No data."));
    }

    #[test]
    fn test_empty_summary_data() {
        let tool = CanvasTool::new();
        let req = CanvasRequest {
            template: CanvasTemplate::Summary,
            title: "Empty".to_string(),
            data: serde_json::json!({}),
            style: None,
        };
        let html = tool.render(req).unwrap();
        assert!(html.contains("No data."));
    }

    #[test]
    fn test_empty_report_data() {
        let tool = CanvasTool::new();
        let req = CanvasRequest {
            template: CanvasTemplate::Report,
            title: "Empty".to_string(),
            data: serde_json::json!([]),
            style: None,
        };
        let html = tool.render(req).unwrap();
        assert!(html.contains("No sections."));
    }

    #[test]
    fn test_title_in_output() {
        let tool = CanvasTool::new();
        let req = CanvasRequest {
            template: CanvasTemplate::Summary,
            title: "My Dashboard".to_string(),
            data: serde_json::json!({"key": "val"}),
            style: None,
        };
        let html = tool.render(req).unwrap();
        assert!(html.contains("<title>My Dashboard</title>"));
        assert!(html.contains("<h1>My Dashboard</h1>"));
    }

    #[test]
    fn test_custom_style_injection() {
        let tool = CanvasTool::new();
        let req = CanvasRequest {
            template: CanvasTemplate::Summary,
            title: "Styled".to_string(),
            data: serde_json::json!({"key": "val"}),
            style: Some("body { background: red; }".to_string()),
        };
        let html = tool.render(req).unwrap();
        assert!(html.contains("body { background: red; }"));
    }

    #[test]
    fn test_invalid_table_data_type() {
        let tool = CanvasTool::new();
        let req = CanvasRequest {
            template: CanvasTemplate::Table,
            title: "Bad".to_string(),
            data: serde_json::json!("not an array"),
            style: None,
        };
        assert!(tool.render(req).is_err());
    }

    #[test]
    fn test_invalid_summary_data_type() {
        let tool = CanvasTool::new();
        let req = CanvasRequest {
            template: CanvasTemplate::Summary,
            title: "Bad".to_string(),
            data: serde_json::json!([1, 2, 3]),
            style: None,
        };
        assert!(tool.render(req).is_err());
    }

    #[test]
    fn test_template_enum_serde() {
        let json = serde_json::json!("table");
        let t: CanvasTemplate = serde_json::from_value(json).unwrap();
        assert_eq!(t, CanvasTemplate::Table);

        let json = serde_json::json!("report");
        let t: CanvasTemplate = serde_json::from_value(json).unwrap();
        assert_eq!(t, CanvasTemplate::Report);
    }

    #[test]
    fn test_tool_metadata() {
        let tool = CanvasTool::new();
        assert_eq!(tool.name(), "canvas");
        assert_eq!(tool.tier(), PermissionTier::Auto);
        assert!(!tool.description().is_empty());
    }

    #[tokio::test]
    async fn test_execute_missing_template() {
        let tool = CanvasTool::new();
        let result = tool
            .execute(
                serde_json::json!({"title": "x", "data": {}}),
                &test_ctx(),
                CancellationToken::new(),
            )
            .await;
        assert!(result.is_error());
    }

    #[tokio::test]
    async fn test_execute_invalid_template() {
        let tool = CanvasTool::new();
        let result = tool
            .execute(
                serde_json::json!({"template": "invalid", "title": "x", "data": {}}),
                &test_ctx(),
                CancellationToken::new(),
            )
            .await;
        assert!(result.is_error());
    }

    #[tokio::test]
    async fn test_execute_table_success() {
        let tool = CanvasTool::new();
        let result = tool
            .execute(
                serde_json::json!({
                    "template": "table",
                    "title": "People",
                    "data": [{"name": "Alice"}]
                }),
                &test_ctx(),
                CancellationToken::new(),
            )
            .await;
        assert!(!result.is_error());
        match result {
            ToolResult::Text { content } => {
                assert!(content.contains("Alice"));
                assert!(content.contains("<!DOCTYPE html>"));
            }
            _ => panic!("Expected Text result"),
        }
    }
}
