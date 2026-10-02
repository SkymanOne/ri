//! Request cost from token usage.

use ri_types::message::Usage;
use ri_types::model::Model;

/// Prices `usage` with the model's rates and stores the result in `usage.cost`.
///
/// The highest tier whose `inputTokensAbove` is below the request's input tokens
/// (fresh, cached and written) prices the whole request. One-hour cache writes cost
/// twice the base input rate. Arithmetic follows pi operation by operation, so costs
/// print identically.
pub fn calculate_cost(model: &Model, usage: &mut Usage) {
    let input_tokens = usage.input + usage.cache_read + usage.cache_write;
    let pricing = &model.cost;
    let (mut input, mut output, mut cache_read, mut cache_write) = (
        pricing.input,
        pricing.output,
        pricing.cache_read,
        pricing.cache_write,
    );
    let mut matched: Option<u64> = None;
    for tier in pricing.tiers.iter().flatten() {
        if input_tokens > tier.input_tokens_above
            && matched.is_none_or(|threshold| tier.input_tokens_above > threshold)
        {
            (input, output, cache_read, cache_write) =
                (tier.input, tier.output, tier.cache_read, tier.cache_write);
            matched = Some(tier.input_tokens_above);
        }
    }

    let long_write = usage.cache_write_1h.unwrap_or(0);
    let short_write = usage.cache_write.saturating_sub(long_write);
    let cost = &mut usage.cost;
    cost.input = (input / 1_000_000.0) * usage.input as f64;
    cost.output = (output / 1_000_000.0) * usage.output as f64;
    cost.cache_read = (cache_read / 1_000_000.0) * usage.cache_read as f64;
    cost.cache_write =
        (cache_write * short_write as f64 + input * 2.0 * long_write as f64) / 1_000_000.0;
    cost.total = cost.input + cost.output + cost.cache_read + cost.cache_write;
}

#[cfg(test)]
mod tests {
    use super::*;
    use ri_types::json;

    fn model(cost: &str) -> Model {
        serde_json::from_str(&format!(
            r#"{{"id":"m","name":"M","api":"a","provider":"p","baseUrl":"u","reasoning":false,
                "input":["text"],"cost":{cost},"contextWindow":1,"maxTokens":1}}"#
        ))
        .unwrap()
    }

    #[test]
    fn matches_pi_numbers() {
        // From pi's JSON output for claude-sonnet-4-5 on the mock cassette.
        let model = model(r#"{"input":3,"output":15,"cacheRead":0.3,"cacheWrite":3.75}"#);
        let mut usage = Usage {
            input: 12,
            output: 6,
            ..Usage::default()
        };
        calculate_cost(&model, &mut usage);
        assert_eq!(
            json::to_string(&usage.cost).unwrap(),
            r#"{"input":0.000036,"output":0.00009,"cacheRead":0,"cacheWrite":0,"total":0.000126}"#
        );
    }

    #[test]
    fn highest_matching_tier_prices_the_request() {
        let model = model(
            r#"{"input":1,"output":1,"cacheRead":1,"cacheWrite":1,"tiers":[
                {"inputTokensAbove":100,"input":2,"output":2,"cacheRead":2,"cacheWrite":2},
                {"inputTokensAbove":10,"input":3,"output":3,"cacheRead":3,"cacheWrite":3}]}"#,
        );
        let mut usage = Usage {
            input: 1_000_000,
            ..Usage::default()
        };
        calculate_cost(&model, &mut usage);
        assert_eq!(usage.cost.input, 2.0);

        let mut usage = Usage {
            input: 50,
            ..Usage::default()
        };
        calculate_cost(&model, &mut usage);
        assert_eq!(usage.cost.input, 3.0 / 1_000_000.0 * 50.0);
    }

    #[test]
    fn long_cache_writes_cost_twice_the_input_rate() {
        let model = model(r#"{"input":3,"output":0,"cacheRead":0,"cacheWrite":3.75}"#);
        let mut usage = Usage {
            cache_write: 300,
            cache_write_1h: Some(100),
            ..Usage::default()
        };
        calculate_cost(&model, &mut usage);
        assert_eq!(
            usage.cost.cache_write,
            (3.75 * 200.0 + 3.0 * 2.0 * 100.0) / 1e6
        );
    }
}
