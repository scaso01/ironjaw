//! Prompt cache: separates static system prompt (identity, rules, tool descriptions)
//! from dynamic context (session memories, active tools, conversation state).
//! Static portion is hashed via Blake2b — identical hashes skip re-encoding.

use blake2::{Blake2b512, Digest};
use std::collections::HashMap;

const PROMPT_BOUNDARY: &str = "__PROMPT_BOUNDARY__";

/// Splits a full system prompt at the boundary marker.
/// Content above is static (cacheable), content below is dynamic (per-session).
pub fn split_prompt(full_prompt: &str) -> (&str, &str) {
    if let Some(pos) = full_prompt.find(PROMPT_BOUNDARY) {
        let static_part = full_prompt[..pos].trim_end();
        let dynamic_start = pos + PROMPT_BOUNDARY.len();
        let dynamic_part = if dynamic_start < full_prompt.len() {
            full_prompt[dynamic_start..].trim_start()
        } else {
            ""
        };
        (static_part, dynamic_part)
    } else {
        (full_prompt, "")
    }
}

/// LRU cache for static prompt portions, keyed by Blake2b hash.
pub struct PromptCache {
    cache: HashMap<String, Vec<String>>,
    order: Vec<String>,
    max_entries: usize,
}

impl PromptCache {
    pub fn new(max_entries: usize) -> Self {
        Self {
            cache: HashMap::new(),
            order: Vec::new(),
            max_entries,
        }
    }

    /// Hash static content with Blake2b-256. Returns hex string.
    pub fn hash_static(&self, content: &str) -> String {
        let mut hasher = Blake2b512::new();
        hasher.update(content.as_bytes());
        let result = hasher.finalize();
        hex::encode(result)
    }

    pub fn store(&mut self, hash: String, encoded_messages: Vec<String>) {
        if self.cache.len() >= self.max_entries && !self.cache.contains_key(&hash) {
            if let Some(oldest) = self.order.first().cloned() {
                self.cache.remove(&oldest);
                self.order.remove(0);
            }
        }
        self.order.retain(|h| h != &hash);
        self.order.push(hash.clone());
        self.cache.insert(hash, encoded_messages);
    }

    pub fn get(&self, hash: &str) -> Option<&Vec<String>> {
        self.cache.get(hash)
    }

    pub fn len(&self) -> usize {
        self.cache.len()
    }

    pub fn is_empty(&self) -> bool {
        self.cache.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_prompt_cache_hit_on_identical_static() {
        let mut cache = PromptCache::new(10);

        let static_content = "You are Ironjaw, an AI assistant.";
        let hash1 = cache.hash_static(static_content);
        cache.store(hash1.clone(), vec!["cached message".to_string()]);

        let hash2 = cache.hash_static(static_content);
        assert_eq!(hash1, hash2);
        assert!(cache.get(&hash2).is_some());
    }

    #[test]
    fn test_prompt_cache_miss_on_different_static() {
        let mut cache = PromptCache::new(10);

        let hash1 = cache.hash_static("version 1");
        cache.store(hash1, vec!["v1".to_string()]);

        let hash2 = cache.hash_static("version 2");
        assert!(cache.get(&hash2).is_none());
    }

    #[test]
    fn test_prompt_split_produces_static_and_dynamic() {
        let full_prompt = "# Identity\nYou are Ironjaw.\n\n__PROMPT_BOUNDARY__\n\n# Session\nCurrent session: abc123";
        let (static_part, dynamic_part) = split_prompt(full_prompt);
        assert!(static_part.contains("You are Ironjaw"));
        assert!(!static_part.contains("abc123"));
        assert!(dynamic_part.contains("abc123"));
        assert!(!dynamic_part.contains("You are Ironjaw"));
    }
}
