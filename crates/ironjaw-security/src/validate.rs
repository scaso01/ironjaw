use serde_json::Value;

/// Error returned when JSON Schema validation fails.
#[derive(Debug, thiserror::Error)]
pub enum ValidationError {
    #[error("Schema compilation failed: {0}")]
    SchemaError(String),
    #[error("Validation failed: {errors}")]
    Invalid { errors: String },
}

/// Validate tool parameters against a JSON Schema.
///
/// Returns Ok(()) if valid, or a ValidationError with details.
pub fn validate_params(schema: &Value, params: &Value) -> Result<(), ValidationError> {
    let validator = jsonschema::validator_for(schema)
        .map_err(|e| ValidationError::SchemaError(e.to_string()))?;

    let errors: Vec<String> = validator
        .iter_errors(params)
        .map(|e| e.to_string())
        .collect();

    if errors.is_empty() {
        Ok(())
    } else {
        Err(ValidationError::Invalid {
            errors: errors.join("; "),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tool_schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": { "type": "string" },
                "timeout_secs": { "type": "number" }
            },
            "required": ["command"]
        })
    }

    #[test]
    fn test_valid_params() {
        let schema = tool_schema();
        let params = json!({ "command": "ls -la" });
        assert!(validate_params(&schema, &params).is_ok());
    }

    #[test]
    fn test_valid_params_with_optional() {
        let schema = tool_schema();
        let params = json!({ "command": "ls", "timeout_secs": 30 });
        assert!(validate_params(&schema, &params).is_ok());
    }

    #[test]
    fn test_missing_required_field() {
        let schema = tool_schema();
        let params = json!({ "timeout_secs": 30 });
        let err = validate_params(&schema, &params).unwrap_err();
        assert!(matches!(err, ValidationError::Invalid { .. }));
        let msg = err.to_string();
        assert!(
            msg.contains("command"),
            "Error should mention 'command': {msg}"
        );
    }

    #[test]
    fn test_wrong_type() {
        let schema = tool_schema();
        let params = json!({ "command": 123 });
        let err = validate_params(&schema, &params).unwrap_err();
        assert!(matches!(err, ValidationError::Invalid { .. }));
    }
}
