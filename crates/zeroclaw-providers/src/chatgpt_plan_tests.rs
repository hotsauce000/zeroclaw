use super::*;
use zeroclaw_config::schema::{ChatGptPlanAuthConfig, ModelProviderConfig};

#[test]
fn preview_request_is_text_only_and_has_only_supported_fields() {
    let request = request_body(
        &[
            ChatMessage::system("instructions"),
            ChatMessage::user("fixture"),
            ChatMessage::assistant("history"),
        ],
        "model-fixture",
        None,
    )
    .unwrap();
    assert_eq!(request["store"], false);
    assert_eq!(request["stream"], true);
    assert_eq!(request["instructions"], "instructions");
    let keys: std::collections::BTreeSet<_> = request
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        std::collections::BTreeSet::from(["input", "instructions", "model", "store", "stream"])
    );
    assert!(request_body(&[ChatMessage::tool("fixture")], "model-fixture", None).is_err());
    assert!(request_body(&[ChatMessage::user("fixture")], "model-fixture", Some(0.5)).is_err());
}

#[test]
fn sse_requires_completed_and_rejects_incomplete_done_and_late_errors() {
    let mut sse = PlanSse::default();
    sse.event("data: {\"type\":\"response.output_text.delta\",\"delta\":\"partial\"}")
        .unwrap();
    assert!(sse.finish().is_err());
    for terminal in [
        "data: [DONE]",
        "data: {\"type\":\"response.incomplete\"}",
        "data: {\"type\":\"response.failed\",\"response\":{\"error\":{\"code\":\"subscription_sharing_usage_limit_exceeded\"}}}",
        "data: {\"type\":\"error\",\"message\":\"synthetic-secret\"}",
        "data: malformed",
    ] {
        let mut sse = PlanSse::default();
        sse.event("data: {\"type\":\"response.output_text.delta\",\"delta\":\"partial\"}")
            .unwrap();
        let error = sse.event(terminal).unwrap_err();
        assert!(!format!("{error:?}").contains("synthetic-secret"));
    }
    let mut sse = PlanSse::default();
    sse.event("data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"output_text\":\"READY\"}}").unwrap();
    assert_eq!(sse.finish().unwrap(), "READY");
    assert!(sse.event("data: {\"type\":\"response.failed\"}").is_err());
}

#[test]
fn factory_rejects_conflicting_auth_endpoint_and_unsupported_preview_options() {
    let base = ModelProviderConfig {
        kind: Some("chatgpt-plan".into()),
        chatgpt_plan_auth: Some(ChatGptPlanAuthConfig {
            registration: "chatgpt-plan:subscriber".into(),
        }),
        ..Default::default()
    };
    let root = tempfile::TempDir::new().unwrap();
    let opts = crate::ModelProviderRuntimeOptions {
        zeroclaw_dir: Some(root.path().into()),
        ..Default::default()
    };
    assert!(ChatGptPlanProvider::new("subscriber", &base, None, None, &opts).is_ok());
    assert!(
        ChatGptPlanProvider::new("subscriber", &base, Some("synthetic-key"), None, &opts).is_err()
    );
    for uri in [
        "http://api.openai.com/v1",
        "https://api.openai.com/v1/responses?redirect=fixture",
        "https://chatgpt.com/backend-api/codex",
        "https://api.openai.com.evil.test/v1",
        "http://127.0.0.1:1234/v1",
    ] {
        assert!(
            ChatGptPlanProvider::new("subscriber", &base, None, Some(uri), &opts).is_err(),
            "{uri}"
        );
    }
    let mut incompatible = base.clone();
    incompatible.requires_openai_auth = true;
    assert!(ChatGptPlanProvider::new("subscriber", &incompatible, None, None, &opts).is_err());
    incompatible = base.clone();
    incompatible.api_key = Some("synthetic-key".into());
    assert!(ChatGptPlanProvider::new("subscriber", &incompatible, None, None, &opts).is_err());
    incompatible = base.clone();
    incompatible.temperature = Some(0.5);
    assert!(ChatGptPlanProvider::new("subscriber", &incompatible, None, None, &opts).is_err());
    incompatible = base.clone();
    incompatible.max_tokens = Some(10);
    assert!(ChatGptPlanProvider::new("subscriber", &incompatible, None, None, &opts).is_err());
    incompatible = base.clone();
    incompatible
        .extra_headers
        .insert("Authorization".into(), "synthetic-key".into());
    assert!(ChatGptPlanProvider::new("subscriber", &incompatible, None, None, &opts).is_err());
    incompatible = base.clone();
    incompatible.kind = None;
    assert!(ChatGptPlanProvider::new("subscriber", &incompatible, None, None, &opts).is_err());
    incompatible = base.clone();
    incompatible.wire_api = Some(zeroclaw_config::schema::WireApi::ChatCompletions);
    assert!(ChatGptPlanProvider::new("subscriber", &incompatible, None, None, &opts).is_err());
    incompatible = base.clone();
    incompatible.native_tools = Some(true);
    assert!(ChatGptPlanProvider::new("subscriber", &incompatible, None, None, &opts).is_err());
}

#[test]
fn routing_rejects_plan_fallbacks_and_reliability_key_rotation() {
    use crate::factory::FamilyProviderFactory;
    use zeroclaw_config::schema::{Config, OpenAIModelProviderConfig};
    let root = tempfile::TempDir::new().unwrap();
    let mut config = Config {
        config_path: root.path().join("config.toml"),
        ..Default::default()
    };
    let base = ModelProviderConfig {
        kind: Some("chatgpt-plan".into()),
        chatgpt_plan_auth: Some(ChatGptPlanAuthConfig {
            registration: "chatgpt-plan:subscriber".into(),
        }),
        ..Default::default()
    };
    config
        .providers
        .models
        .openai
        .insert("subscriber".into(), OpenAIModelProviderConfig { base });
    let opts = crate::model_provider_runtime_options_from_model_provider_entry(
        &config,
        config.providers.models.find("openai", "subscriber"),
    );
    let mut reliability = config.reliability.clone();
    assert!(
        !config.providers.models.openai["subscriber"]
            .fallback_auth_ready(Some("synthetic-metered-key"), &opts)
    );
    assert!(!crate::factory::fallback_auth_ready_for_alias(
        &config,
        "openai",
        "subscriber",
        None,
        &opts
    ));
    assert!(!crate::factory::fallback_auth_ready_for_alias(
        &config,
        "openai",
        "subscriber",
        Some("synthetic-metered-key"),
        &opts
    ));
    reliability.api_keys = vec!["synthetic-metered-key".into()];
    assert!(
        crate::create_resilient_model_provider_for_alias(
            &config,
            "openai",
            "subscriber",
            None,
            None,
            &reliability,
            &opts
        )
        .is_err()
    );
    config
        .providers
        .models
        .openai
        .get_mut("subscriber")
        .unwrap()
        .base
        .fallback
        .push(serde_json::from_value(serde_json::json!("openai.metered")).unwrap());
    assert!(
        crate::create_model_provider_for_alias(&config, "openai", "subscriber", None, &opts)
            .is_err()
    );
}

#[tokio::test]
async fn http_sse_eof_incomplete_and_quota_failure_never_complete() {
    use crate::auth::profiles::{
        AuthProfile, AuthProfilesStore, ChatGptPlanRegistration, TokenSet,
    };
    use axum::{Json, Router, routing::post};
    use chrono::Utc;
    let app = Router::new().route("/responses",post(|Json(body):Json<serde_json::Value>| async move {
        let ending = match body["model"].as_str().unwrap() {
            "eof" => "",
            "incomplete" => "data: {\"type\":\"response.incomplete\"}\n\n",
            "late-quota" => "data: {\"type\":\"response.failed\",\"response\":{\"error\":{\"code\":\"subscription_sharing_usage_limit_exceeded\"}}}\n\n",
            "partial-after-completion" => "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n\ndata: {\"type\":\"response.failed\"}",
            "error-after-completion" => "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n\ndata: {\"type\":\"response.failed\"}\n\n",
            _ => panic!("unexpected fixture model"),
        };
        ([("content-type","text/event-stream")],format!("data: {{\"type\":\"response.output_text.delta\",\"delta\":\"partial\"}}\n\n{ending}"))
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = ::zeroclaw_spawn::spawn!(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let root = tempfile::TempDir::new().unwrap();
    let mut profile = AuthProfile::new_oauth(
        "chatgpt-plan",
        "subscriber",
        TokenSet {
            access_token: "synthetic-access".into(),
            refresh_token: Some("synthetic-refresh".into()),
            id_token: None,
            expires_at: Some(Utc::now() + chrono::Duration::hours(1)),
            token_type: Some("Bearer".into()),
            scope: Some("chatgpt.tokens.use.direct".into()),
        },
    );
    profile.plan_registration = Some(ChatGptPlanRegistration {
        client_id: "oaiapp_fixture".into(),
        subject: "subject-fixture".into(),
        earliest_refresh_at: None,
        refresh_started_at: None,
    });
    AuthProfilesStore::new(root.path(), true)
        .upsert_profile(profile, false)
        .await
        .unwrap();
    crate::plan_test_transport::scope(&base, async {
        let config = ModelProviderConfig {
            kind: Some("chatgpt-plan".into()),
            chatgpt_plan_auth: Some(ChatGptPlanAuthConfig {
                registration: "chatgpt-plan:subscriber".into(),
            }),
            ..Default::default()
        };
        let opts = crate::ModelProviderRuntimeOptions {
            zeroclaw_dir: Some(root.path().into()),
            ..Default::default()
        };
        let provider = ChatGptPlanProvider::new("subscriber", &config, None, None, &opts).unwrap();
        for model in [
            "eof",
            "incomplete",
            "late-quota",
            "partial-after-completion",
            "error-after-completion",
        ] {
            assert!(
                provider.simple_chat("fixture", model, None).await.is_err(),
                "{model}"
            );
        }
        let error = provider
            .chat(
                ChatRequest {
                    messages: &[ChatMessage::user("fixture")],
                    tools: Some(&[zeroclaw_api::tool::ToolSpec::new(
                        "tool-fixture",
                        "fixture",
                        serde_json::json!({}),
                    )]),
                    thinking: None,
                },
                "fixture",
                None,
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("tools are unsupported"));
    })
    .await;
    server.abort();
}

#[tokio::test]
async fn total_sse_deadline_stops_continuous_keepalives() {
    use crate::auth::profiles::{
        AuthProfile, AuthProfilesStore, ChatGptPlanRegistration, TokenSet,
    };
    use axum::{Router, body::Body, routing::post};
    use chrono::Utc;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    assert_eq!(request_timeout(), Duration::from_secs(300));
    let reads = Arc::new(AtomicUsize::new(0));
    let app = Router::new().route(
        "/responses",
        post({
            let reads = reads.clone();
            move || {
                let reads = reads.clone();
                async move {
                    let stream = futures_util::stream::unfold(reads, |reads| async move {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                        reads.fetch_add(1, Ordering::SeqCst);
                        Some((Ok::<_, std::convert::Infallible>(": keepalive\n\n"), reads))
                    });
                    (
                        [("content-type", "text/event-stream")],
                        Body::from_stream(stream),
                    )
                }
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = ::zeroclaw_spawn::spawn!(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let root = tempfile::TempDir::new().unwrap();
    let mut profile = AuthProfile::new_oauth(
        "chatgpt-plan",
        "subscriber",
        TokenSet {
            access_token: "synthetic-access".into(),
            refresh_token: Some("synthetic-refresh".into()),
            id_token: None,
            expires_at: Some(Utc::now() + chrono::Duration::hours(1)),
            token_type: Some("Bearer".into()),
            scope: Some("chatgpt.tokens.use.direct".into()),
        },
    );
    profile.plan_registration = Some(ChatGptPlanRegistration {
        client_id: "opaque-client".into(),
        subject: "subject-fixture".into(),
        earliest_refresh_at: None,
        refresh_started_at: None,
    });
    AuthProfilesStore::new(root.path(), true)
        .upsert_profile(profile, false)
        .await
        .unwrap();
    crate::plan_test_transport::scope(&base, async {
        FIXTURE_REQUEST_TIMEOUT
            .scope(Duration::from_millis(250), async {
                let config = ModelProviderConfig {
                    kind: Some("chatgpt-plan".into()),
                    chatgpt_plan_auth: Some(ChatGptPlanAuthConfig {
                        registration: "chatgpt-plan:subscriber".into(),
                    }),
                    ..Default::default()
                };
                let opts = crate::ModelProviderRuntimeOptions {
                    zeroclaw_dir: Some(root.path().into()),
                    ..Default::default()
                };
                let provider =
                    ChatGptPlanProvider::new("subscriber", &config, None, None, &opts).unwrap();
                let result = tokio::time::timeout(
                    Duration::from_secs(2),
                    provider.simple_chat("fixture", "model-fixture", None),
                )
                .await
                .expect("client's total deadline must finish before harness timeout");
                assert!(
                    result
                        .unwrap_err()
                        .to_string()
                        .contains("stream interrupted")
                );
            })
            .await;
    })
    .await;
    assert!(reads.load(Ordering::SeqCst) >= 2);
    server.abort();
}
