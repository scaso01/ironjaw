/// Static cost calculation for Claude API usage.
pub struct CostCalculator;

impl CostCalculator {
    /// Returns (input_cost_per_mtok, output_cost_per_mtok) for a given model.
    ///
    /// Costs are in USD per million tokens. Local/unknown models return (0.0, 0.0).
    pub fn cost_per_mtok(model: &str) -> (f64, f64) {
        match model {
            m if m.contains("opus") => (15.0, 75.0),
            m if m.contains("sonnet") => (3.0, 15.0),
            m if m.contains("haiku") => (0.25, 1.25),
            _ => (0.0, 0.0), // local models are free
        }
    }

    /// Calculate the total cost in USD for a given model and token counts.
    pub fn calculate(model: &str, input_tokens: u64, output_tokens: u64) -> f64 {
        let (input_rate, output_rate) = Self::cost_per_mtok(model);
        (input_tokens as f64 * input_rate + output_tokens as f64 * output_rate) / 1_000_000.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_opus_cost_per_mtok() {
        let (input, output) = CostCalculator::cost_per_mtok("claude-3-opus-20240229");
        assert_eq!(input, 15.0);
        assert_eq!(output, 75.0);
    }

    #[test]
    fn test_sonnet_cost_per_mtok() {
        let (input, output) = CostCalculator::cost_per_mtok("claude-3-5-sonnet-20241022");
        assert_eq!(input, 3.0);
        assert_eq!(output, 15.0);
    }

    #[test]
    fn test_haiku_cost_per_mtok() {
        let (input, output) = CostCalculator::cost_per_mtok("claude-3-haiku-20240307");
        assert_eq!(input, 0.25);
        assert_eq!(output, 1.25);
    }

    #[test]
    fn test_local_model_free() {
        let (input, output) = CostCalculator::cost_per_mtok("qwen3-coder-abliterated");
        assert_eq!(input, 0.0);
        assert_eq!(output, 0.0);
    }

    #[test]
    fn test_unknown_model_free() {
        let (input, output) = CostCalculator::cost_per_mtok("gpt-4o");
        assert_eq!(input, 0.0);
        assert_eq!(output, 0.0);
    }

    #[test]
    fn test_calculate_opus_cost() {
        // 1M input + 500K output at opus rates
        let cost = CostCalculator::calculate("claude-3-opus", 1_000_000, 500_000);
        // 1M * 15 / 1M + 500K * 75 / 1M = 15 + 37.5 = 52.5
        assert!((cost - 52.5).abs() < f64::EPSILON);
    }

    #[test]
    fn test_calculate_sonnet_cost() {
        let cost = CostCalculator::calculate("claude-3-5-sonnet", 1_000_000, 1_000_000);
        // 1M * 3 / 1M + 1M * 15 / 1M = 3 + 15 = 18
        assert!((cost - 18.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_calculate_haiku_cost() {
        let cost = CostCalculator::calculate("claude-3-haiku", 2_000_000, 1_000_000);
        // 2M * 0.25 / 1M + 1M * 1.25 / 1M = 0.5 + 1.25 = 1.75
        assert!((cost - 1.75).abs() < f64::EPSILON);
    }

    #[test]
    fn test_calculate_zero_tokens() {
        let cost = CostCalculator::calculate("claude-3-opus", 0, 0);
        assert_eq!(cost, 0.0);
    }

    #[test]
    fn test_calculate_local_model_always_free() {
        let cost = CostCalculator::calculate("llama-server/qwen3-coder", 10_000_000, 5_000_000);
        assert_eq!(cost, 0.0);
    }

    #[test]
    fn test_calculate_small_token_count() {
        // 1000 input tokens at opus rate = 1000 * 15 / 1M = 0.015
        let cost = CostCalculator::calculate("claude-3-opus", 1_000, 0);
        assert!((cost - 0.015).abs() < f64::EPSILON);
    }

    #[test]
    fn test_opus_substring_match() {
        // Ensure model name containing "opus" anywhere matches
        let (input, output) = CostCalculator::cost_per_mtok("my-custom-opus-model");
        assert_eq!(input, 15.0);
        assert_eq!(output, 75.0);
    }

    #[test]
    fn test_model_matching_priority() {
        // "opus" check comes first, so a model with both "opus" and "sonnet" gets opus rates
        let (input, _) = CostCalculator::cost_per_mtok("opus-sonnet-hybrid");
        assert_eq!(input, 15.0);
    }
}
