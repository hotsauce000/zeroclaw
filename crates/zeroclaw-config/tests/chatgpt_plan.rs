use zeroclaw_config::schema::Config;

#[test]
fn chatgpt_plan_config_reference_roundtrips_without_credentials_or_legacy_auth() {
    let raw = r#"
[providers.models.openai.subscriber]
kind = "chatgpt-plan"
model = "model-fixture"
wire_api = "responses"
[providers.models.openai.subscriber.chatgpt_plan_auth]
registration = "chatgpt-plan:subscriber"
"#;
    let config: Config = toml::from_str(raw).unwrap();
    let entry = config
        .providers
        .models
        .find("openai", "subscriber")
        .unwrap();
    assert_eq!(
        entry.chatgpt_plan_auth.as_ref().unwrap().registration,
        "chatgpt-plan:subscriber"
    );
    assert!(!entry.requires_openai_auth);
    assert!(entry.api_key.is_none());
    let saved = toml::to_string(&config).unwrap();
    let restored: Config = toml::from_str(&saved).unwrap();
    assert_eq!(
        restored
            .providers
            .models
            .find("openai", "subscriber")
            .unwrap()
            .chatgpt_plan_auth
            .as_ref()
            .unwrap()
            .registration,
        "chatgpt-plan:subscriber"
    );
    for key in [
        "access_token",
        "refresh_token",
        "id_token",
        "subject",
        "client_id",
    ] {
        assert!(!saved.contains(key));
    }
}

#[test]
fn legacy_codex_config_keeps_its_existing_auth_semantics() {
    let config: Config = toml::from_str(
        r#"
[providers.models.openai.codex]
requires_openai_auth = true
wire_api = "responses"
"#,
    )
    .unwrap();
    let entry = config.providers.models.find("openai", "codex").unwrap();
    assert!(entry.requires_openai_auth);
    assert!(entry.chatgpt_plan_auth.is_none());
}
