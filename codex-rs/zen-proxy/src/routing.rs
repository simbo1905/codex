/// Model family classification for upstream routing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ModelFamily {
    Gpt,
    Claude,
    Unknown,
}

/// Classify a model name into a family for routing decisions.
pub(crate) fn classify_model(model: &str) -> ModelFamily {
    if model.starts_with("gpt-") || model.starts_with("o1") || model.starts_with("o3") {
        ModelFamily::Gpt
    } else if model.starts_with("claude-") {
        ModelFamily::Claude
    } else {
        ModelFamily::Unknown
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn test_classify_gpt() {
        assert_eq!(classify_model("gpt-4o"), ModelFamily::Gpt);
        assert_eq!(classify_model("gpt-5.4"), ModelFamily::Gpt);
        assert_eq!(classify_model("o1-preview"), ModelFamily::Gpt);
        assert_eq!(classify_model("o3-mini"), ModelFamily::Gpt);
    }

    #[test]
    fn test_classify_claude() {
        assert_eq!(
            classify_model("claude-sonnet-4-20250514"),
            ModelFamily::Claude
        );
        assert_eq!(classify_model("claude-3.5-sonnet"), ModelFamily::Claude);
    }

    #[test]
    fn test_classify_unknown() {
        assert_eq!(classify_model("gemini-pro"), ModelFamily::Unknown);
        assert_eq!(classify_model("llama-3"), ModelFamily::Unknown);
    }
}
