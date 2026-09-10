use crate::domain::{ModelDescriptor, ThinkingCapability, ThinkingLevel, ThinkingMode};
use serde_json::Value;
use std::collections::HashSet;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ApiKind {
    Responses,
    ChatCompletions,
}

impl ApiKind {
    pub(super) fn path(self) -> &'static str {
        match self {
            Self::Responses => "/responses",
            Self::ChatCompletions => "/chat/completions",
        }
    }
}

#[derive(Clone)]
pub(super) struct ModelInfo {
    pub(super) descriptor: ModelDescriptor,
    pub(super) api: ApiKind,
    max_completion_tokens: Option<u32>,
    efforts: HashSet<String>,
}

impl ModelInfo {
    pub(super) fn max_output(&self, context: u32) -> u32 {
        let reserved = crate::agent::reserved_output_tokens(context);
        self.max_completion_tokens
            .map_or(reserved, |maximum| maximum.min(reserved))
            .max(1)
    }

    pub(super) fn reasoning(&self, mode: ThinkingMode) -> Result<Option<Value>, String> {
        let effort = match mode {
            ThinkingMode::Default => return Ok(None),
            ThinkingMode::Level(ThinkingLevel::Low) => "low",
            ThinkingMode::Level(ThinkingLevel::Medium) => "medium",
            ThinkingMode::Level(ThinkingLevel::High) => "high",
            ThinkingMode::Off | ThinkingMode::On => {
                return Err("This Grok model does not advertise a boolean Thinking control. Use the model default.".into());
            }
        };
        if !self.efforts.contains(effort) {
            return Err(
                "The selected Thinking level is not confirmed by this Grok model's metadata."
                    .into(),
            );
        }
        Ok(Some(serde_json::json!({"effort": effort})))
    }
}

pub(super) fn parse_models(value: &Value) -> Result<Vec<ModelInfo>, String> {
    let models = value
        .get("data")
        .and_then(Value::as_array)
        .ok_or("Grok returned an invalid model list.")?;
    let mut seen = HashSet::new();
    let mut result = Vec::new();
    for value in models {
        let Some(object) = value.as_object() else {
            continue;
        };
        let meta = object.get("_meta").and_then(Value::as_object);
        let field = |keys: &[&str]| -> Option<&Value> {
            keys.iter()
                .find_map(|key| object.get(*key))
                .or_else(|| meta.and_then(|object| keys.iter().find_map(|key| object.get(*key))))
        };
        if field(&["hidden"]).and_then(Value::as_bool) == Some(true)
            || field(&["supportedInApi", "supported_in_api"]).and_then(Value::as_bool)
                == Some(false)
        {
            continue;
        }
        // This is the public Grok Build catalog default, not a retry fallback:
        // xai-grok-sampling-types/src/types.rs:979-986 at commit 37949780.
        let api = match field(&["apiBackend", "api_backend"]).and_then(Value::as_str) {
            Some("responses") => ApiKind::Responses,
            None | Some("chat_completions") => ApiKind::ChatCompletions,
            Some(_) => continue,
        };
        let Some(name) = field(&["model", "modelId", "id"])
            .and_then(Value::as_str)
            .filter(|name| {
                !name.trim().is_empty() && name.len() <= 256 && !name.chars().any(char::is_control)
            })
        else {
            continue;
        };
        if !seen.insert(name.to_string()) {
            continue;
        }
        let context_length = field(&["contextWindow", "context_window", "totalContextTokens"])
            .and_then(Value::as_u64)
            .and_then(|size| u32::try_from(size).ok())
            .filter(|size| *size > 0);
        let max_completion_tokens = field(&["maxCompletionTokens", "max_completion_tokens"])
            .and_then(Value::as_u64)
            .and_then(|size| u32::try_from(size).ok())
            .filter(|size| *size > 0);
        let supports_effort = field(&["supportsReasoningEffort", "supports_reasoning_effort"])
            .and_then(Value::as_bool)
            == Some(true);
        let efforts: HashSet<String> = if supports_effort {
            field(&["reasoningEfforts", "reasoning_efforts"])
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|effort| {
                    effort
                        .as_str()
                        .or_else(|| effort.get("value").and_then(Value::as_str))
                })
                .filter(|effort| matches!(*effort, "low" | "medium" | "high"))
                .map(str::to_owned)
                .collect()
        } else {
            HashSet::new()
        };
        let thinking = if ["low", "medium", "high"]
            .iter()
            .all(|effort| efforts.contains(*effort))
        {
            ThinkingCapability::Levels
        } else {
            ThinkingCapability::Unverified
        };
        let vision = field(&["supportsVision", "supports_vision"]).and_then(Value::as_bool)
            == Some(true)
            || field(&["inputModalities", "input_modalities"])
                .and_then(Value::as_array)
                .is_some_and(|modalities| {
                    modalities
                        .iter()
                        .any(|value| value.as_str() == Some("image"))
                });
        // Function calls are the documented inference contract, also used by
        // Grok Build's sampler without a separate per-model tools flag:
        // https://docs.x.ai/developers/tools/function-calling
        // Grok Build 37949780, xai-grok-shell/src/remote/client.rs:634-746
        // parses this catalog without a required supports-tools property.
        // Explicit model-level refusal still takes precedence.
        let tools = field(&[
            "supportsTools",
            "supports_tools",
            "supportsFunctionCalling",
            "supports_function_calling",
        ])
        .and_then(Value::as_bool)
        .unwrap_or(true);
        result.push(ModelInfo {
            descriptor: ModelDescriptor {
                name: name.into(),
                size: 0,
                thinking,
                vision,
                tools,
                context_length,
            },
            api,
            max_completion_tokens,
            efforts,
        });
    }
    if result.is_empty() {
        return Err("Grok did not provide any supported inference models for this account.".into());
    }
    Ok(result)
}
