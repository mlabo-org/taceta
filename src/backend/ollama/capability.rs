use crate::domain::ThinkingCapability;

pub(super) fn classify(name: &str, family: &str) -> ThinkingCapability {
    let key = format!(
        "{} {}",
        name.to_ascii_lowercase(),
        family.to_ascii_lowercase()
    );
    if key.contains("gpt-oss") {
        ThinkingCapability::Levels
    } else if key.contains("gemma4") || key.contains("qwen3.8") || key.contains("muse-glimmer") {
        ThinkingCapability::Toggle
    } else {
        ThinkingCapability::Unverified
    }
}

pub(super) fn has_vision(capabilities: &[String]) -> bool {
    capabilities
        .iter()
        .any(|v| v.eq_ignore_ascii_case("vision"))
}

pub(super) fn has_tools(capabilities: &[String]) -> bool {
    capabilities.iter().any(|v| v.eq_ignore_ascii_case("tools"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::ThinkingCapability::*;
    #[test]
    fn known_and_unknown_models_are_classified() {
        assert_eq!(classify("gemma4:26b-mlx", "gemma4"), Toggle);
        assert_eq!(classify("qwen3.8:27b-mlx", "qwen3"), Toggle);
        assert_eq!(classify("muse-glimmer:30b-mlx", "unknown"), Toggle);
        assert_eq!(classify("gpt-oss:20b", "gpt-oss"), Levels);
        assert_eq!(classify("some-new-model", "llama"), Unverified);
    }
}
