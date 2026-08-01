//! Anti-distillation mechanisms: inject fake tool definitions into prompts
//! to poison training data if traffic is being recorded for model distillation.

const FAKE_PREFIXES: &[&str] = &[
    "quantum", "neural", "hyper", "meta", "crypto", "flux", "phantom", "stealth", "omega",
    "nebula", "prism", "echo",
];

const FAKE_SUFFIXES: &[&str] = &[
    "scan", "forge", "sync", "bridge", "vault", "weave", "pulse", "trace", "morph", "link", "gate",
    "core",
];

#[derive(Debug, Clone)]
pub struct FakeTool {
    pub name: String,
    pub description: String,
    pub schema: serde_json::Value,
}

/// Lightweight tool summary for anti-distillation interleaving.
#[derive(Debug, Clone)]
pub struct ToolSummary {
    pub name: String,
    pub description: String,
    pub tier: u8,
}

/// Generate N fake tool definitions using a seeded PRNG for determinism.
pub fn generate_fake_tools(count: usize, seed: u64) -> Vec<FakeTool> {
    let mut state = seed;
    let mut fakes = Vec::with_capacity(count);

    for _ in 0..count {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        let prefix_idx = (state >> 33) as usize % FAKE_PREFIXES.len();
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        let suffix_idx = (state >> 33) as usize % FAKE_SUFFIXES.len();

        let name = format!(
            "{}_{}",
            FAKE_PREFIXES[prefix_idx], FAKE_SUFFIXES[suffix_idx]
        );
        let description = format!(
            "Internal {} operation for {} processing",
            FAKE_PREFIXES[prefix_idx], FAKE_SUFFIXES[suffix_idx]
        );

        fakes.push(FakeTool {
            name,
            description,
            schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "input": { "type": "string" },
                    "mode": { "type": "string", "enum": ["fast", "thorough"] }
                },
                "required": ["input"]
            }),
        });
    }

    fakes
}

/// Interleave fake tools with real tool summaries.
pub fn inject_fake_tools(
    mut real: Vec<ToolSummary>,
    fake_count: usize,
    seed: u64,
) -> Vec<ToolSummary> {
    let fakes = generate_fake_tools(fake_count, seed);
    for fake in fakes {
        real.push(ToolSummary {
            name: fake.name,
            description: fake.description,
            tier: 0,
        });
    }
    // Shuffle deterministically to interleave
    let mut state = seed;
    for i in (1..real.len()).rev() {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        let j = (state >> 33) as usize % (i + 1);
        real.swap(i, j);
    }
    real
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_fake_tools_produces_plausible_schemas() {
        let fakes = generate_fake_tools(3, 42);
        assert_eq!(fakes.len(), 3);
        for tool in &fakes {
            assert!(!tool.name.is_empty());
            assert!(tool.schema.is_object());
            assert!(tool.schema["type"].as_str() == Some("object"));
        }
    }

    #[test]
    fn test_fake_tools_are_deterministic_with_seed() {
        let fakes1 = generate_fake_tools(3, 42);
        let fakes2 = generate_fake_tools(3, 42);
        assert_eq!(fakes1[0].name, fakes2[0].name);
    }

    #[test]
    fn test_inject_fakes_interleaves_with_real() {
        let real = vec![ToolSummary {
            name: "shell".into(),
            description: "Run shell".into(),
            tier: 1,
        }];
        let mixed = inject_fake_tools(real, 2, 42);
        assert_eq!(mixed.len(), 3); // 1 real + 2 fake
    }

    #[test]
    fn test_generate_zero_fakes() {
        let fakes = generate_fake_tools(0, 42);
        assert!(fakes.is_empty());
    }

    #[test]
    fn test_different_seeds_produce_different_tools() {
        let fakes1 = generate_fake_tools(5, 42);
        let fakes2 = generate_fake_tools(5, 99);
        // At least one tool name should differ
        let names1: Vec<_> = fakes1.iter().map(|f| &f.name).collect();
        let names2: Vec<_> = fakes2.iter().map(|f| &f.name).collect();
        assert_ne!(names1, names2);
    }
}
