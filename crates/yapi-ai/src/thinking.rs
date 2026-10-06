//! Thinking levels and output token budgets.

use yapi_types::message::{Message, ThinkingLevel};
use yapi_types::model::Model;

use crate::stream::{StreamOptions, ThinkingBudgets};
use crate::transcript::estimate_context_tokens;

const CONTEXT_SAFETY_TOKENS: u64 = 4096;
/// Output tokens kept for the answer when a thinking budget would consume all.
pub const MIN_ANSWER_TOKENS: u64 = 1024;

/// The levels a model accepts: only `off` without reasoning; `xhigh` and `max` only
/// when its thinking level map names them; none the map marks unsupported.
pub fn supported_levels(model: &Model) -> Vec<ThinkingLevel> {
    if !model.reasoning {
        return vec![ThinkingLevel::Off];
    }
    ThinkingLevel::ALL
        .into_iter()
        .filter(|level| match model.thinking_level_value(*level) {
            Some(None) => false,
            mapped if matches!(level, ThinkingLevel::Xhigh | ThinkingLevel::Max) => {
                mapped.is_some()
            }
            _ => true,
        })
        .collect()
}

/// The nearest supported level: the requested one, else the next higher, else the
/// next lower.
pub fn clamp_level(model: &Model, level: ThinkingLevel) -> ThinkingLevel {
    let supported = supported_levels(model);
    if supported.contains(&level) {
        return level;
    }
    ThinkingLevel::ALL
        .into_iter()
        .filter(|candidate| *candidate > level)
        .chain(
            ThinkingLevel::ALL
                .into_iter()
                .rev()
                .filter(|candidate| *candidate < level),
        )
        .find(|candidate| supported.contains(candidate))
        .unwrap_or(ThinkingLevel::Off)
}

/// Caps `max_tokens` so the estimated context plus output fits the window.
pub fn clamp_max_tokens_to_context(model: &Model, messages: &[Message], max_tokens: u64) -> u64 {
    if model.context_window == 0 {
        return max_tokens.max(1);
    }
    let used = estimate_context_tokens(messages) + CONTEXT_SAFETY_TOKENS;
    let available = model.context_window.saturating_sub(used).max(1);
    max_tokens.min(available)
}

/// pi's `streamSimple` output limit: the requested limit, else the model's, capped
/// to the context window.
pub fn requested_max_tokens(model: &Model, messages: &[Message], options: &StreamOptions) -> u64 {
    clamp_max_tokens_to_context(
        model,
        messages,
        options.max_tokens.unwrap_or(model.max_tokens),
    )
}

/// The requested level clamped to the model; `None` when it is off.
pub fn effort(model: &Model, options: &StreamOptions) -> Option<ThinkingLevel> {
    options
        .reasoning
        .map(|level| clamp_level(model, level))
        .filter(|level| *level != ThinkingLevel::Off)
}

/// Budget-based thinking at `level` on top of the output limit `base`: the output
/// limit capped to the context, and the budget leaving room to answer under it.
/// Returns `(max_tokens, budget)`.
pub fn budgeted(
    model: &Model,
    messages: &[Message],
    base: u64,
    level: ThinkingLevel,
    budgets: &ThinkingBudgets,
) -> (u64, u64) {
    let (adjusted, budget) = adjust_max_tokens_for_thinking(base, model.max_tokens, level, budgets);
    let max_tokens = clamp_max_tokens_to_context(model, messages, adjusted);
    (
        max_tokens,
        budget.min(max_tokens.saturating_sub(MIN_ANSWER_TOKENS)),
    )
}

/// The budget for budget-based thinking at `level`; `xhigh` and `max` use `high`.
pub fn budget_for_level(level: ThinkingLevel, budgets: &ThinkingBudgets) -> u64 {
    match level {
        ThinkingLevel::Off | ThinkingLevel::Minimal => budgets.minimal.unwrap_or(1024),
        ThinkingLevel::Low => budgets.low.unwrap_or(2048),
        ThinkingLevel::Medium => budgets.medium.unwrap_or(8192),
        ThinkingLevel::High | ThinkingLevel::Xhigh | ThinkingLevel::Max => {
            budgets.high.unwrap_or(16384)
        }
    }
}

/// Raises `base` by the thinking budget, capped at the model's output limit; shrinks
/// the budget when it would leave no room to answer. Returns `(max_tokens, budget)`.
fn adjust_max_tokens_for_thinking(
    base: u64,
    model_max_tokens: u64,
    level: ThinkingLevel,
    budgets: &ThinkingBudgets,
) -> (u64, u64) {
    let mut budget = budget_for_level(level, budgets);
    let max_tokens = (base + budget).min(model_max_tokens);
    if max_tokens <= budget {
        budget = budget.min(max_tokens.saturating_sub(MIN_ANSWER_TOKENS));
    }
    (max_tokens, budget)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn model(map: serde_json::Value) -> Model {
        serde_json::from_value(json!({
            "id":"m","name":"M","api":"a","provider":"p","baseUrl":"u","reasoning":true,"input":["text"],
            "cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0},"contextWindow":1000,"maxTokens":64000,
            "thinkingLevelMap": map
        }))
        .unwrap()
    }

    #[test]
    fn clamps_to_supported_levels() {
        let model = model(json!({"minimal": null, "medium": null, "max": "max"}));
        assert_eq!(
            supported_levels(&model),
            [
                ThinkingLevel::Off,
                ThinkingLevel::Low,
                ThinkingLevel::High,
                ThinkingLevel::Max
            ]
        );
        assert_eq!(
            clamp_level(&model, ThinkingLevel::Medium),
            ThinkingLevel::High
        );
        assert_eq!(
            clamp_level(&model, ThinkingLevel::Xhigh),
            ThinkingLevel::Max
        );
        assert_eq!(
            clamp_level(&model, ThinkingLevel::Minimal),
            ThinkingLevel::Low
        );
    }

    #[test]
    fn budgets_match_pi() {
        let budgets = ThinkingBudgets::default();
        assert_eq!(
            adjust_max_tokens_for_thinking(64000, 64000, ThinkingLevel::Medium, &budgets),
            (64000, 8192)
        );
        assert_eq!(
            adjust_max_tokens_for_thinking(1000, 2000, ThinkingLevel::High, &budgets),
            (2000, 976)
        );
    }
}
