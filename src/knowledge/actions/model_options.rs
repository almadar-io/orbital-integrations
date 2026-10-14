//! The models a learner can choose between (`modelOptions.ts`).

pub const MODEL_KEYS: [&str; 2] = ["qwen3-30b-a3b", "deepseek-v4-flash"];

pub const DEFAULT_EXPAND_MODEL: &str = "qwen3-30b-a3b";
pub const DEFAULT_EXPLAIN_MODEL: &str = "deepseek-v4-flash";

pub fn is_model_key(key: &str) -> bool {
    MODEL_KEYS.contains(&key)
}
