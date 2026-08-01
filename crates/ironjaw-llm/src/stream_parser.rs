//! SSE tool-call delta accumulator for llama-server streaming.
//!
//! When llama-server streams a response with tool calls, the function arguments
//! arrive as fragmented JSON strings across multiple `tool_calls` delta chunks.
//! This module accumulates those deltas into complete [`ToolCall`] values.
//!
//! # Protocol
//!
//! Each SSE chunk's `delta.tool_calls` is a JSON array.  Every entry has an
//! `index` field identifying the slot it belongs to.  The first delta for a
//! slot carries the `id`, `type`, and `function.name`; subsequent deltas
//! carry only `function.arguments` fragments that must be concatenated.
//!
//! When `finish_reason == "tool_calls"` the caller should call
//! [`ToolCallAccumulator::complete_all`] to finalize all slots.

use crate::json_repair;
use crate::types::{ToolCall, ToolCallDelta, ToolCallFunction};
use std::collections::BTreeMap;
use thiserror::Error;

// ──────────────────────────────────────────────────────────
// Error type
// ──────────────────────────────────────────────────────────

/// Errors produced when finalizing accumulated tool-call deltas.
#[derive(Debug, Clone, Error)]
pub enum StreamParserError {
    /// A slot was missing a call-id field (first delta must supply it).
    #[error("tool_call at index {index} is missing an id")]
    MissingId { index: usize },

    /// A slot was missing a function name (first delta must supply it).
    #[error("tool_call at index {index} is missing a function name")]
    MissingName { index: usize },

    /// The accumulated argument string could not be parsed as JSON even after
    /// JSON repair.
    #[error("tool_call at index {index} has invalid arguments: {reason}")]
    BadArguments { index: usize, reason: String },
}

// ──────────────────────────────────────────────────────────
// Internal slot
// ──────────────────────────────────────────────────────────

/// Per-slot state while accumulating deltas for a single tool call.
#[derive(Debug, Default, Clone)]
pub struct ToolCallSlot {
    /// The call identifier, e.g. `"call_abc123"`.
    pub id: Option<String>,
    /// Always `"function"` per the OpenAI spec.
    pub type_: Option<String>,
    /// Function name (arrives once in the first delta).
    pub name: Option<String>,
    /// Argument JSON fragments concatenated in order.
    pub arguments: String,
}

// ──────────────────────────────────────────────────────────
// Accumulator
// ──────────────────────────────────────────────────────────

/// Accumulates `tool_calls` SSE deltas and produces complete [`ToolCall`]s.
///
/// Internally indexed by the delta's `index` field.  [`BTreeMap`] keeps slots
/// ordered so the final flush emits calls in the same order as the model
/// intended.
#[derive(Debug, Default)]
pub struct ToolCallAccumulator {
    slots: BTreeMap<usize, ToolCallSlot>,
}

impl ToolCallAccumulator {
    /// Create a new, empty accumulator.
    pub fn new() -> Self {
        Self::default()
    }

    /// Apply a single [`ToolCallDelta`] to the appropriate slot.
    ///
    /// - If no slot exists for `delta.index`, one is created.
    /// - `id` and `type_` are set once (first delta wins; later deltas that
    ///   supply them are ignored because they carry `None` by that point).
    /// - `function.name` is set once; `function.arguments` is **appended**.
    pub fn apply_delta(&mut self, delta: &ToolCallDelta) {
        let slot = self.slots.entry(delta.index).or_default();

        if let Some(id) = &delta.id {
            if !id.is_empty() && slot.id.is_none() {
                slot.id = Some(id.clone());
            }
        }

        if let Some(t) = &delta.type_ {
            if !t.is_empty() && slot.type_.is_none() {
                slot.type_ = Some(t.clone());
            }
        }

        if let Some(func) = &delta.function {
            if let Some(name) = &func.name {
                if !name.is_empty() && slot.name.is_none() {
                    slot.name = Some(name.clone());
                }
            }
            if let Some(args) = &func.arguments {
                slot.arguments.push_str(args);
            }
        }
    }

    /// Apply a slice of deltas (convenience wrapper).
    pub fn apply_deltas(&mut self, deltas: &[ToolCallDelta]) {
        for delta in deltas {
            self.apply_delta(delta);
        }
    }

    /// Returns `true` if no deltas have been applied yet.
    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    /// Finalize all accumulated slots, returning the complete [`ToolCall`]
    /// list in index order.
    ///
    /// # Errors
    ///
    /// Returns the first [`StreamParserError`] encountered when any slot is
    /// missing a required field or has unparseable arguments.
    pub fn complete_all(&self) -> Result<Vec<ToolCall>, StreamParserError> {
        let mut out = Vec::with_capacity(self.slots.len());
        for (&index, slot) in &self.slots {
            out.push(self.finalize_slot(index, slot)?);
        }
        Ok(out)
    }

    /// Finalize a single slot by index and return the [`ToolCall`], leaving
    /// other slots untouched.
    ///
    /// # Errors
    ///
    /// Same as [`complete_all`].
    pub fn complete_index(&self, index: usize) -> Result<ToolCall, StreamParserError> {
        let slot = self.slots.get(&index).cloned().unwrap_or_default();
        self.finalize_slot(index, &slot)
    }

    // ── private helpers ────────────────────────────────────

    fn finalize_slot(
        &self,
        index: usize,
        slot: &ToolCallSlot,
    ) -> Result<ToolCall, StreamParserError> {
        let id = slot
            .id
            .clone()
            .filter(|s| !s.is_empty())
            .ok_or(StreamParserError::MissingId { index })?;

        let name = slot
            .name
            .clone()
            .filter(|s| !s.is_empty())
            .ok_or(StreamParserError::MissingName { index })?;

        let type_ = slot
            .type_
            .clone()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "function".to_string());

        let arguments = self.parse_arguments(index, &slot.arguments)?;

        Ok(ToolCall {
            id,
            type_,
            function: ToolCallFunction { name, arguments },
        })
    }

    /// Parse the accumulated argument string.
    ///
    /// Empty strings become `"{}"` (valid empty object).
    /// Otherwise tries `serde_json::from_str` first, then falls back to
    /// [`json_repair::repair_json_args`].
    fn parse_arguments(&self, index: usize, raw: &str) -> Result<String, StreamParserError> {
        if raw.trim().is_empty() {
            return Ok("{}".to_string());
        }

        // Fast path: valid JSON already
        if serde_json::from_str::<serde_json::Value>(raw).is_ok() {
            return Ok(raw.to_string());
        }

        // Slow path: attempt repair
        json_repair::repair_json_args(raw).map_err(|e| StreamParserError::BadArguments {
            index,
            reason: e.to_string(),
        })
    }
}

// ──────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ToolCallDelta, ToolCallFunctionDelta};

    fn make_delta(
        index: usize,
        id: Option<&str>,
        type_: Option<&str>,
        name: Option<&str>,
        arguments: Option<&str>,
    ) -> ToolCallDelta {
        ToolCallDelta {
            index,
            id: id.map(|s| s.to_string()),
            type_: type_.map(|s| s.to_string()),
            function: if name.is_some() || arguments.is_some() {
                Some(ToolCallFunctionDelta {
                    name: name.map(|s| s.to_string()),
                    arguments: arguments.map(|s| s.to_string()),
                })
            } else {
                None
            },
        }
    }

    // ── 1. Single tool call, arguments arrive in three fragments ──

    #[test]
    fn test_single_call_fragmented_args() {
        let mut acc = ToolCallAccumulator::new();
        // First delta: id + name, first arg fragment
        acc.apply_delta(&make_delta(
            0,
            Some("call_abc"),
            Some("function"),
            Some("get_weather"),
            Some(r#"{"city":"#),
        ));
        // Second delta: continuation
        acc.apply_delta(&make_delta(0, None, None, None, Some(r#""London""#)));
        // Third delta: closing brace
        acc.apply_delta(&make_delta(0, None, None, None, Some("}")));

        let calls = acc.complete_all().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "call_abc");
        assert_eq!(calls[0].function.name, "get_weather");

        let args: serde_json::Value = serde_json::from_str(&calls[0].function.arguments).unwrap();
        assert_eq!(args["city"], "London");
    }

    // ── 2. Two parallel tool calls, interleaved deltas ──

    #[test]
    fn test_two_parallel_tool_calls() {
        let mut acc = ToolCallAccumulator::new();
        acc.apply_delta(&make_delta(
            0,
            Some("call_1"),
            Some("function"),
            Some("read_file"),
            Some(r#"{"path": "/tmp/a"}"#),
        ));
        acc.apply_delta(&make_delta(
            1,
            Some("call_2"),
            Some("function"),
            Some("write_file"),
            Some(r#"{"path": "/tmp/b", "content": "hi"}"#),
        ));

        let calls = acc.complete_all().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].function.name, "read_file");
        assert_eq!(calls[1].function.name, "write_file");
    }

    // ── 3. Missing id triggers MissingId error ──

    #[test]
    fn test_missing_id_error() {
        let mut acc = ToolCallAccumulator::new();
        acc.apply_delta(&make_delta(
            0,
            None, // no id
            Some("function"),
            Some("get_weather"),
            Some("{}"),
        ));
        let err = acc.complete_all().unwrap_err();
        assert!(matches!(err, StreamParserError::MissingId { index: 0 }));
    }

    // ── 4. Missing name triggers MissingName error ──

    #[test]
    fn test_missing_name_error() {
        let mut acc = ToolCallAccumulator::new();
        acc.apply_delta(&make_delta(
            0,
            Some("call_xyz"),
            Some("function"),
            None, // no name
            Some("{}"),
        ));
        let err = acc.complete_all().unwrap_err();
        assert!(matches!(err, StreamParserError::MissingName { index: 0 }));
    }

    // ── 5. Truncated JSON args repaired by json_repair ──

    #[test]
    fn test_truncated_args_repaired() {
        let mut acc = ToolCallAccumulator::new();
        // Arguments missing closing brace — repair_json_args should fix it
        acc.apply_delta(&make_delta(
            0,
            Some("call_tr"),
            Some("function"),
            Some("do_thing"),
            Some(r#"{"param": "value""#), // missing }
        ));
        let calls = acc.complete_all().unwrap();
        let args: serde_json::Value = serde_json::from_str(&calls[0].function.arguments).unwrap();
        assert_eq!(args["param"], "value");
    }

    // ── 6. Empty arguments become "{}" ──

    #[test]
    fn test_empty_args_become_empty_object() {
        let mut acc = ToolCallAccumulator::new();
        acc.apply_delta(&make_delta(
            0,
            Some("call_empty"),
            Some("function"),
            Some("no_args_fn"),
            None, // no arguments at all
        ));
        let calls = acc.complete_all().unwrap();
        assert_eq!(calls[0].function.arguments, "{}");
    }

    // ── 7. is_empty() and type_ default ──

    #[test]
    fn test_is_empty_and_default_type() {
        let mut acc = ToolCallAccumulator::new();
        assert!(acc.is_empty());

        // type_ not provided — should default to "function"
        acc.apply_delta(&make_delta(
            0,
            Some("call_def"),
            None, // no type_
            Some("fn_name"),
            Some("{}"),
        ));
        assert!(!acc.is_empty());

        let calls = acc.complete_all().unwrap();
        assert_eq!(calls[0].type_, "function");
    }

    // ── 8. apply_deltas (slice) and BTreeMap ordering ──

    #[test]
    fn test_apply_deltas_ordering() {
        let mut acc = ToolCallAccumulator::new();
        let deltas = vec![
            // Deliberately out of logical order by emission, but index keeps order
            make_delta(
                2,
                Some("call_c"),
                Some("function"),
                Some("func_c"),
                Some("{}"),
            ),
            make_delta(
                0,
                Some("call_a"),
                Some("function"),
                Some("func_a"),
                Some("{}"),
            ),
            make_delta(
                1,
                Some("call_b"),
                Some("function"),
                Some("func_b"),
                Some("{}"),
            ),
        ];
        acc.apply_deltas(&deltas);

        let calls = acc.complete_all().unwrap();
        assert_eq!(calls.len(), 3);
        // BTreeMap guarantees index order: 0 → 1 → 2
        assert_eq!(calls[0].function.name, "func_a");
        assert_eq!(calls[1].function.name, "func_b");
        assert_eq!(calls[2].function.name, "func_c");
    }
}
