use super::*;

#[test]
fn ollama_provider_entry_enables_llama_cpp_cache_compat() {
    let entry = build_provider_entry(&ProviderKind::Ollama);

    assert!(entry.compat.supports_llama_cpp_cache);
    assert_eq!(entry.auth.as_deref(), Some("none"));
    assert_eq!(entry.api.as_deref(), Some("openai-completions"));
}
