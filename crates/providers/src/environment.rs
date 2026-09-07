//! Environment profiles retain credential variable names, never credential values.
use crate::{Protocol, ProviderConfig};

pub fn nonempty(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

pub fn discover() -> Vec<(String, ProviderConfig)> {
    discover_with(nonempty)
}

/// Explicit TOML profiles are merged by the caller and always take precedence.
pub fn discover_with(env: impl Fn(&str) -> Option<String>) -> Vec<(String, ProviderConfig)> {
    let get = |name: &str| env(name).filter(|v| !v.trim().is_empty());
    let mut profiles = Vec::new();
    for (name, protocol, key, model_var, model, base_var, base) in [
        ("openai", Protocol::Openai, "OPENAI_API_KEY", "OPENAI_MODEL", "gpt-4.1", "OPENAI_BASE_URL", "https://api.openai.com/v1"),
        ("anthropic", Protocol::Anthropic, "ANTHROPIC_API_KEY", "ANTHROPIC_MODEL", "claude-sonnet-4-5", "ANTHROPIC_BASE_URL", "https://api.anthropic.com/v1"),
        ("ollama", Protocol::Compatible, "OLLAMA_API_KEY", "OLLAMA_MODEL", "qwen3:8b", "OLLAMA_BASE_URL", "http://localhost:11434/v1"),
    ] {
        let credential = get(key).is_some();
        if !credential && name != "ollama" {
            continue;
        }
        let mut base_url = get(base_var).unwrap_or_else(|| {
            if name == "ollama" {
                get("OLLAMA_HOST").unwrap_or_else(|| base.into())
            } else {
                base.into()
            }
        });
        if name == "ollama" {
            if !base_url.contains("://") {
                base_url = format!("http://{base_url}");
            }
            base_url = base_url.trim_end_matches('/').to_string();
            if !base_url.ends_with("/v1") {
                base_url.push_str("/v1");
            }
        }
        profiles.push((name.into(), ProviderConfig {
            protocol,
            model: get(model_var).unwrap_or_else(|| model.into()),
            base_url,
            api_key_env: credential.then(|| key.into()),
            input_price_per_million: None,
            output_price_per_million: None,
        }));
    }
    profiles
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn keys_are_references_and_empty_values_are_missing() {
        let profiles = discover_with(|k| match k {
            "OPENAI_API_KEY" => Some("secret-sentinel".into()),
            "ANTHROPIC_API_KEY" => Some("  ".into()),
            "OPENAI_MODEL" => Some("custom-model".into()),
            "OLLAMA_HOST" => Some("localhost:12000/".into()),
            _ => None,
        });
        assert_eq!(profiles.len(), 2);
        assert_eq!(profiles[0].1.model, "custom-model");
        assert_eq!(profiles[0].1.api_key_env.as_deref(), Some("OPENAI_API_KEY"));
        assert_eq!(profiles[1].1.base_url, "http://localhost:12000/v1");
        assert!(profiles[1].1.api_key_env.is_none());
        assert!(!serde_json::to_string(&profiles).unwrap().contains("secret-sentinel"));
    }
    #[test]
    fn ollama_key_and_base_override() {
        let profiles = discover_with(|k| match k {
            "OLLAMA_API_KEY" => Some("secret-sentinel".into()),
            "OLLAMA_BASE_URL" => Some("https://example.com/v1/".into()),
            "OLLAMA_HOST" => Some("ignored:1234".into()),
            _ => None,
        });
        assert_eq!(profiles[0].1.base_url, "https://example.com/v1");
        assert_eq!(profiles[0].1.api_key_env.as_deref(), Some("OLLAMA_API_KEY"));
    }
}
