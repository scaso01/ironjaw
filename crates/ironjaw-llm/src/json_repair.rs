//! JSON repair utilities for malformed LLM tool-call output.
//!
//! LLMs sometimes produce JSON with code fences, trailing commas,
//! single-quoted strings, unquoted keys, or unbalanced braces.
//! This module attempts to repair such output before parsing.

use thiserror::Error;

/// Errors that can occur during JSON repair.
#[derive(Debug, Clone, Error)]
pub enum JsonRepairError {
    /// The input could not be repaired into valid JSON.
    #[error("JSON could not be repaired: {reason}")]
    Unfixable { reason: String },
}

/// Attempt to repair malformed JSON produced by an LLM.
///
/// Applies the following strategies in order:
/// 1. Strip code fences (```` ```json ... ``` ````)
/// 2. Strip comments (`//` line comments and `/* */` block comments)
/// 3. Fix trailing commas before `]` or `}`
/// 4. Balance unmatched braces/brackets
/// 5. Fix single-quoted strings to double-quoted
/// 6. Fix unquoted object keys
///
/// After all repairs, validates via `serde_json::from_str`.
/// Returns `JsonRepairError::Unfixable` if the result is still invalid.
pub fn repair_json(input: &str) -> Result<String, JsonRepairError> {
    let mut s = input.to_string();

    // 1. Strip code fences
    s = strip_code_fences(&s);

    // 2. Strip comments
    s = strip_comments(&s);

    // 3. Fix trailing commas
    s = fix_trailing_commas(&s);

    // 4. Balance braces
    s = balance_braces(&s);

    // 5. Fix single quotes
    s = fix_single_quotes(&s);

    // 6. Fix unquoted keys
    s = fix_unquoted_keys(&s);

    // Validate
    let trimmed = s.trim();
    match serde_json::from_str::<serde_json::Value>(trimmed) {
        Ok(_) => Ok(trimmed.to_string()),
        Err(e) => Err(JsonRepairError::Unfixable {
            reason: e.to_string(),
        }),
    }
}

/// Thin public alias for [`repair_json`], intended for repairing LLM-produced
/// tool-call argument strings.
///
/// Applies the same repair pipeline as [`repair_json`].  Exists as a distinct
/// entry point so call sites are semantically clear about what is being
/// repaired.
pub fn repair_json_args(s: &str) -> Result<String, JsonRepairError> {
    repair_json(s)
}

/// Extract the first complete JSON object or array from a string
/// that may contain surrounding text.
///
/// Finds the first `{` or `[`, then locates the matching closing
/// bracket while accounting for nesting and string escapes.
pub fn extract_json(input: &str) -> Option<String> {
    let bytes = input.as_bytes();
    let len = bytes.len();

    // Find the first '{' or '['
    let start = bytes.iter().position(|&b| b == b'{' || b == b'[')?;
    let open = bytes[start];
    let close = if open == b'{' { b'}' } else { b']' };

    let mut depth = 0i32;
    let mut in_string = false;
    let mut escape_next = false;
    let mut i = start;

    while i < len {
        let ch = bytes[i];

        if escape_next {
            escape_next = false;
            i += 1;
            continue;
        }

        if ch == b'\\' && in_string {
            escape_next = true;
            i += 1;
            continue;
        }

        if ch == b'"' {
            in_string = !in_string;
            i += 1;
            continue;
        }

        if !in_string {
            if ch == open {
                depth += 1;
            } else if ch == close {
                depth -= 1;
                if depth == 0 {
                    return Some(input[start..=i].to_string());
                }
            } else if ch == if open == b'{' { b'[' } else { b'{' } {
                // Handle nested opposite bracket type
                depth += 1;
            } else if ch == if open == b'{' { b']' } else { b'}' } {
                depth -= 1;
            }
        }

        i += 1;
    }

    None
}

/// Remove markdown code fences wrapping JSON.
fn strip_code_fences(input: &str) -> String {
    let trimmed = input.trim();

    // Match ```json or ``` at start, and ``` at end
    let without_start = if let Some(rest) = trimmed.strip_prefix("```json") {
        rest
    } else if let Some(rest) = trimmed.strip_prefix("```") {
        rest
    } else {
        return trimmed.to_string();
    };

    // Strip trailing ```
    let without_end = if let Some(rest) = without_start.trim().strip_suffix("```") {
        rest
    } else {
        without_start
    };

    without_end.trim().to_string()
}

/// Remove `//` line comments and `/* */` block comments, but not inside strings.
fn strip_comments(input: &str) -> String {
    let bytes = input.as_bytes();
    let len = bytes.len();
    let mut result = Vec::with_capacity(len);
    let mut i = 0;
    let mut in_string = false;
    let mut escape_next = false;

    while i < len {
        if escape_next {
            result.push(bytes[i]);
            escape_next = false;
            i += 1;
            continue;
        }

        if in_string {
            if bytes[i] == b'\\' {
                escape_next = true;
                result.push(bytes[i]);
            } else if bytes[i] == b'"' {
                in_string = false;
                result.push(bytes[i]);
            } else {
                result.push(bytes[i]);
            }
            i += 1;
            continue;
        }

        // Not in a string
        if bytes[i] == b'"' {
            in_string = true;
            result.push(bytes[i]);
            i += 1;
        } else if i + 1 < len && bytes[i] == b'/' && bytes[i + 1] == b'/' {
            // Line comment -- skip to end of line
            i += 2;
            while i < len && bytes[i] != b'\n' {
                i += 1;
            }
        } else if i + 1 < len && bytes[i] == b'/' && bytes[i + 1] == b'*' {
            // Block comment -- skip to */
            i += 2;
            while i + 1 < len && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                i += 1;
            }
            if i + 1 < len {
                i += 2; // skip */
            }
        } else {
            result.push(bytes[i]);
            i += 1;
        }
    }

    // SAFETY: we only pass through or skip ASCII bytes from valid UTF-8 input
    String::from_utf8(result).unwrap_or_else(|_| input.to_string())
}

/// Remove trailing commas before `]` or `}` (with optional whitespace).
fn fix_trailing_commas(input: &str) -> String {
    let mut result = String::with_capacity(input.len());
    let chars: Vec<char> = input.chars().collect();
    let len = chars.len();
    let mut i = 0;
    let mut in_string = false;
    let mut escape_next = false;

    while i < len {
        if escape_next {
            result.push(chars[i]);
            escape_next = false;
            i += 1;
            continue;
        }

        if in_string {
            if chars[i] == '\\' {
                escape_next = true;
            } else if chars[i] == '"' {
                in_string = false;
            }
            result.push(chars[i]);
            i += 1;
            continue;
        }

        if chars[i] == '"' {
            in_string = true;
            result.push(chars[i]);
            i += 1;
            continue;
        }

        if chars[i] == ',' {
            // Look ahead past whitespace for ] or }
            let mut j = i + 1;
            while j < len && chars[j].is_whitespace() {
                j += 1;
            }
            if j < len && (chars[j] == ']' || chars[j] == '}') {
                // Skip the comma (trailing comma)
                i += 1;
                continue;
            }
        }

        result.push(chars[i]);
        i += 1;
    }

    result
}

/// If there are more opening braces/brackets than closing ones, append
/// the missing closers in reverse nesting order.
fn balance_braces(input: &str) -> String {
    let mut stack: Vec<char> = Vec::new();
    let mut in_string = false;
    let mut escape_next = false;

    for ch in input.chars() {
        if escape_next {
            escape_next = false;
            continue;
        }
        if in_string {
            if ch == '\\' {
                escape_next = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }
        match ch {
            '"' => in_string = true,
            '{' => stack.push('}'),
            '[' => stack.push(']'),
            '}' | ']' => {
                // Pop matching opener if present
                if let Some(&expected) = stack.last() {
                    if expected == ch {
                        stack.pop();
                    }
                }
            }
            _ => {}
        }
    }

    if stack.is_empty() {
        input.to_string()
    } else {
        let mut result = input.to_string();
        for closer in stack.into_iter().rev() {
            result.push(closer);
        }
        result
    }
}

/// Replace single-quoted strings with double-quoted strings.
/// Only applies outside of existing double-quoted strings.
fn fix_single_quotes(input: &str) -> String {
    let chars: Vec<char> = input.chars().collect();
    let len = chars.len();
    let mut result = String::with_capacity(len);
    let mut i = 0;
    let mut in_double = false;
    let mut escape_next = false;

    while i < len {
        if escape_next {
            result.push(chars[i]);
            escape_next = false;
            i += 1;
            continue;
        }

        if in_double {
            if chars[i] == '\\' {
                escape_next = true;
            } else if chars[i] == '"' {
                in_double = false;
            }
            result.push(chars[i]);
            i += 1;
            continue;
        }

        if chars[i] == '"' {
            in_double = true;
            result.push(chars[i]);
            i += 1;
            continue;
        }

        if chars[i] == '\'' {
            // Start of a single-quoted string -- convert to double
            result.push('"');
            i += 1;
            // Consume until matching single quote
            while i < len {
                if chars[i] == '\\' && i + 1 < len {
                    // Pass through escape sequence
                    result.push(chars[i]);
                    result.push(chars[i + 1]);
                    i += 2;
                    continue;
                }
                if chars[i] == '\'' {
                    result.push('"');
                    i += 1;
                    break;
                }
                // Escape any double quotes inside
                if chars[i] == '"' {
                    result.push('\\');
                }
                result.push(chars[i]);
                i += 1;
            }
            continue;
        }

        result.push(chars[i]);
        i += 1;
    }

    result
}

/// Fix unquoted object keys: `{key:` -> `{"key":`
/// Handles keys after `{` or `,`.
fn fix_unquoted_keys(input: &str) -> String {
    let chars: Vec<char> = input.chars().collect();
    let len = chars.len();
    let mut result = String::with_capacity(len);
    let mut i = 0;
    let mut in_string = false;
    let mut escape_next = false;

    while i < len {
        if escape_next {
            result.push(chars[i]);
            escape_next = false;
            i += 1;
            continue;
        }

        if in_string {
            if chars[i] == '\\' {
                escape_next = true;
            } else if chars[i] == '"' {
                in_string = false;
            }
            result.push(chars[i]);
            i += 1;
            continue;
        }

        if chars[i] == '"' {
            in_string = true;
            result.push(chars[i]);
            i += 1;
            continue;
        }

        // After { or , look for unquoted key
        if chars[i] == '{' || chars[i] == ',' {
            result.push(chars[i]);
            i += 1;

            // Skip whitespace
            while i < len && chars[i].is_whitespace() {
                result.push(chars[i]);
                i += 1;
            }

            // Check for unquoted key (identifier followed by colon)
            if i < len && is_ident_start(chars[i]) {
                let key_start = i;
                while i < len && is_ident_char(chars[i]) {
                    i += 1;
                }
                let key_end = i;

                // Skip whitespace between key and colon
                let mut j = i;
                while j < len && chars[j].is_whitespace() {
                    j += 1;
                }

                if j < len && chars[j] == ':' {
                    // This is an unquoted key -- quote it
                    let key: String = chars[key_start..key_end].iter().collect();
                    result.push('"');
                    result.push_str(&key);
                    result.push('"');
                    // Consume whitespace between key and colon
                    i = j;
                } else {
                    // Not a key:value pattern, output as-is
                    for ch in &chars[key_start..key_end] {
                        result.push(*ch);
                    }
                }
            }
            continue;
        }

        result.push(chars[i]);
        i += 1;
    }

    result
}

fn is_ident_start(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_' || c == '$'
}

fn is_ident_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '$'
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- repair_json tests ----

    #[test]
    fn test_valid_json_unchanged() {
        let input = r#"{"key": "value"}"#;
        let result = repair_json(input).unwrap();
        assert_eq!(result, input);
    }

    #[test]
    fn test_strip_code_fences_json() {
        let input = "```json\n{\"key\": \"value\"}\n```";
        let result = repair_json(input).unwrap();
        assert_eq!(result, r#"{"key": "value"}"#);
    }

    #[test]
    fn test_strip_code_fences_bare() {
        let input = "```\n{\"key\": \"value\"}\n```";
        let result = repair_json(input).unwrap();
        assert_eq!(result, r#"{"key": "value"}"#);
    }

    #[test]
    fn test_strip_line_comments() {
        let input = r#"{
            "key": "value" // this is a comment
        }"#;
        let result = repair_json(input).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["key"], "value");
    }

    #[test]
    fn test_strip_block_comments() {
        let input = r#"{
            /* comment */
            "key": "value"
        }"#;
        let result = repair_json(input).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["key"], "value");
    }

    #[test]
    fn test_comments_not_stripped_inside_strings() {
        let input = r#"{"url": "http://example.com"}"#;
        let result = repair_json(input).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["url"], "http://example.com");
    }

    #[test]
    fn test_trailing_comma_object() {
        let input = r#"{"a": 1, "b": 2,}"#;
        let result = repair_json(input).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["a"], 1);
        assert_eq!(parsed["b"], 2);
    }

    #[test]
    fn test_trailing_comma_array() {
        let input = r#"[1, 2, 3,]"#;
        let result = repair_json(input).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed.as_array().unwrap().len(), 3);
    }

    #[test]
    fn test_trailing_comma_with_whitespace() {
        let input = "{\n  \"a\": 1,\n  \"b\": 2,  \n}";
        let result = repair_json(input).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["a"], 1);
    }

    #[test]
    fn test_balance_missing_closing_brace() {
        let input = r#"{"key": "value""#;
        let result = repair_json(input).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["key"], "value");
    }

    #[test]
    fn test_balance_missing_closing_bracket() {
        let input = r#"[1, 2, 3"#;
        let result = repair_json(input).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed.as_array().unwrap().len(), 3);
    }

    #[test]
    fn test_balance_nested_missing() {
        let input = r#"{"arr": [1, 2"#;
        let result = repair_json(input).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["arr"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn test_single_quotes() {
        let input = "{'key': 'value'}";
        let result = repair_json(input).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["key"], "value");
    }

    #[test]
    fn test_single_quotes_with_inner_double() {
        let input = r#"{'key': 'say "hello"'}"#;
        let result = repair_json(input).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["key"], r#"say "hello""#);
    }

    #[test]
    fn test_unquoted_keys() {
        let input = r#"{name: "Alice", age: 30}"#;
        let result = repair_json(input).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["name"], "Alice");
        assert_eq!(parsed["age"], 30);
    }

    #[test]
    fn test_unquoted_keys_with_underscore() {
        let input = r#"{first_name: "Bob"}"#;
        let result = repair_json(input).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["first_name"], "Bob");
    }

    #[test]
    fn test_multiple_repairs_combined() {
        let input = "```json\n{name: 'Alice', items: [1, 2,],}\n```";
        let result = repair_json(input).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["name"], "Alice");
        assert_eq!(parsed["items"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn test_unfixable() {
        let input = "this is not json at all and has no structure";
        let result = repair_json(input);
        assert!(result.is_err());
    }

    #[test]
    fn test_empty_object() {
        let result = repair_json("{}").unwrap();
        assert_eq!(result, "{}");
    }

    #[test]
    fn test_empty_array() {
        let result = repair_json("[]").unwrap();
        assert_eq!(result, "[]");
    }

    #[test]
    fn test_code_fence_with_extra_text() {
        let input = "```json\n{\"tool\": \"read_file\", \"args\": {\"path\": \"/tmp/test\"}}\n```";
        let result = repair_json(input).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["tool"], "read_file");
    }

    // ---- repair_json_args tests ----

    #[test]
    fn test_repair_json_args_unterminated_object() {
        // Missing closing brace — must be repaired
        let raw = r#"{"city": "London""#;
        let result = repair_json_args(raw).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["city"], "London");
    }

    #[test]
    fn test_repair_json_args_trailing_comma() {
        let raw = r#"{"a": 1, "b": 2,}"#;
        let result = repair_json_args(raw).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["a"], 1);
        assert_eq!(parsed["b"], 2);
    }

    #[test]
    fn test_repair_json_args_missing_bracket() {
        let raw = r#"{"items": [1, 2, 3"#;
        let result = repair_json_args(raw).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["items"].as_array().unwrap().len(), 3);
    }

    #[test]
    fn test_repair_json_args_valid_passthrough() {
        let raw = r#"{"key": "value", "num": 42}"#;
        let result = repair_json_args(raw).unwrap();
        // Valid JSON passes through unchanged
        assert_eq!(result, raw);
    }

    // ---- extract_json tests ----

    #[test]
    fn test_extract_json_from_text() {
        let input = "Here is the result: {\"key\": \"value\"} and some more text";
        let extracted = extract_json(input).unwrap();
        assert_eq!(extracted, r#"{"key": "value"}"#);
    }

    #[test]
    fn test_extract_json_array() {
        let input = "Result: [1, 2, 3] done";
        let extracted = extract_json(input).unwrap();
        assert_eq!(extracted, "[1, 2, 3]");
    }

    #[test]
    fn test_extract_json_nested() {
        let input = "Prefix {\"a\": {\"b\": [1, 2]}} suffix";
        let extracted = extract_json(input).unwrap();
        assert_eq!(extracted, r#"{"a": {"b": [1, 2]}}"#);
    }

    #[test]
    fn test_extract_json_with_strings_containing_braces() {
        let input = r#"Found: {"msg": "use { and } carefully"} end"#;
        let extracted = extract_json(input).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&extracted).unwrap();
        assert_eq!(parsed["msg"], "use { and } carefully");
    }

    #[test]
    fn test_extract_json_no_json() {
        let input = "no json here";
        assert!(extract_json(input).is_none());
    }

    #[test]
    fn test_extract_json_unbalanced() {
        let input = "start {\"key\": \"value\"";
        // Unbalanced -- should return None
        assert!(extract_json(input).is_none());
    }

    #[test]
    fn test_extract_json_escaped_quotes() {
        let input = r#"x {"msg": "he said \"hi\""} y"#;
        let extracted = extract_json(input).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&extracted).unwrap();
        assert_eq!(parsed["msg"], r#"he said "hi""#);
    }
}
