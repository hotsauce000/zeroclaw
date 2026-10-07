use super::*;
use zeroclaw_config::schema::{ChatGptPlanAuthConfig, ModelProviderConfig};

async fn tool_fixture_profile(root: &std::path::Path) {
    use crate::auth::profiles::{
        AuthProfile, AuthProfilesStore, ChatGptPlanRegistration, TokenSet,
    };
    let mut profile = AuthProfile::new_oauth(
        "chatgpt-plan",
        "subscriber",
        TokenSet {
            access_token: "synthetic-tool-access".into(),
            refresh_token: Some("synthetic-tool-refresh".into()),
            id_token: None,
            expires_at: Some(chrono::Utc::now() + chrono::Duration::hours(1)),
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
    AuthProfilesStore::new(root, true)
        .upsert_profile(profile, false)
        .await
        .unwrap();
}

#[tokio::test]
async fn function_tools_http_roundtrip_preserves_namespace_reasoning_and_call_ids() {
    use axum::{Json, Router, routing::post};
    use std::sync::{Arc, Mutex};
    use zeroclaw_api::tool::ToolSpec;
    let requests = Arc::new(Mutex::new(Vec::new()));
    let call = json!({"type":"function_call","id":"fc_fixture","call_id":"call_fixture","namespace":"zeroclaw","name":"file_read","arguments":"{\"path\":\"fixture.txt\"}","status":"completed"});
    let reasoning = json!({"type":"reasoning","id":"rs_fixture","summary":[],"encrypted_content":"opaque-fixture-reasoning"});
    let app = Router::new().route("/responses", post({
        let requests = requests.clone();
        let call = call.clone();
        let reasoning = reasoning.clone();
        move |headers: axum::http::HeaderMap, Json(body): Json<Value>| {
            let requests = requests.clone();
            let call = call.clone();
            let reasoning = reasoning.clone();
            async move {
                assert_eq!(headers["authorization"], "Bearer synthetic-tool-access");
                let followup = body["input"].as_array().unwrap().iter().any(|item| item["type"] == "function_call_output");
                requests.lock().unwrap().push(body);
                let output = if followup {
                    json!([{"type":"message","role":"assistant","content":[{"type":"output_text","text":"READ COMPLETE"}]}])
                } else {
                    json!([reasoning,call])
                };
                ([("content-type","text/event-stream")],format!("data: {}\n\n", json!({"type":"response.completed","response":{"status":"completed","output":output,"usage":{"input_tokens":12,"output_tokens":3}}})))
            }
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = ::zeroclaw_spawn::spawn!(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let root = tempfile::tempdir().unwrap();
    tool_fixture_profile(root.path()).await;
    crate::plan_test_transport::scope(&base, async {
        let config = ModelProviderConfig {
            kind: Some("chatgpt-plan".into()),
            chatgpt_plan_auth: Some(ChatGptPlanAuthConfig { registration: "chatgpt-plan:subscriber".into() }),
            ..Default::default()
        };
        let opts = crate::ModelProviderRuntimeOptions { zeroclaw_dir: Some(root.path().into()), ..Default::default() };
        let provider = ChatGptPlanProvider::new("subscriber", &config, None, None, &opts).unwrap();
        assert!(provider.capabilities_for_model("fixture").native_tool_calling);
        let tools = [ToolSpec::new("file_read", "Read a local file", json!({"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}))];
        let mut history = vec![ChatMessage::system("local instructions"), ChatMessage::user("Read fixture")];
        let first = provider.chat(ChatRequest { messages:&history, tools:Some(&tools), thinking:None }, "fixture", None).await.unwrap();
        assert_eq!(first.tool_calls.len(), 1);
        assert_eq!(first.tool_calls[0].id, "call_fixture");
        assert_eq!(first.tool_calls[0].name, "file_read");
        assert_eq!(first.tool_calls[0].arguments, "{\"path\":\"fixture.txt\"}");
        assert_eq!(first.usage.as_ref().unwrap().input_tokens, Some(12));
        history.push(ChatMessage::assistant(json!({"content":first.text,"tool_calls":first.tool_calls,"reasoning_content":first.reasoning_content}).to_string()));
        history.push(ChatMessage::tool(json!({"tool_call_id":"call_fixture","content":"fixture contents","attachments":[]}).to_string()));
        let final_response = provider.chat(ChatRequest { messages:&history, tools:Some(&tools), thinking:None }, "fixture", None).await.unwrap();
        assert_eq!(final_response.text.as_deref(), Some("READ COMPLETE"));
    }).await;
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    for body in requests.iter() {
        assert_eq!(body["store"], false);
        assert_eq!(body["stream"], true);
        assert_eq!(body["tools"][0]["type"], "namespace");
        assert_eq!(body["tools"][0]["name"], "zeroclaw");
        assert_eq!(body["tools"][0]["tools"][0]["type"], "function");
        assert_eq!(body["tools"][0]["tools"][0]["strict"], false);
        assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
        assert!(body.get("previous_response_id").is_none());
        assert!(body.get("temperature").is_none());
        assert_eq!(body["instructions"], "local instructions");
    }
    assert_eq!(requests[1]["input"][1], reasoning);
    assert_eq!(requests[1]["input"][2], call);
    assert_eq!(
        requests[1]["input"][3],
        json!({"type":"function_call_output","call_id":"call_fixture","output":"fixture contents"})
    );
    server.abort();
}

#[tokio::test]
async fn function_tools_raw_payloads_reject_hosted_and_unknown_fields_before_auth() {
    let root = tempfile::tempdir().unwrap();
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
    for tool in [
        json!({"type":"tool_search","execution":"client"}),
        json!({"type":"mcp","server_url":"https://synthetic-secret.example"}),
        json!({"type":"programmatic_tool_calling"}),
        json!({"type":"custom","name":"fixture","format":{"type":"text"}}),
        json!({"type":"function","name":"fixture","parameters":{"type":"object"},"defer_loading":true}),
        json!({"type":"function","function":{"name":"fixture","description":"fixture","parameters":{"type":"object"},"defer_loading":true}}),
    ] {
        let error = provider
            .chat_with_tools(&[ChatMessage::user("fixture")], &[tool], "fixture", None)
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("client-side function"),
            "{error}"
        );
        assert!(!format!("{error:?}").contains("synthetic-secret"));
    }
    assert!(!root.path().join("auth-profiles.json").exists());
}

#[tokio::test]
async fn function_tools_disabled_provider_rejects_specs_before_auth() {
    let root = tempfile::tempdir().unwrap();
    let config = ModelProviderConfig {
        kind: Some("chatgpt-plan".into()),
        native_tools: Some(false),
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
    assert!(
        !provider
            .capabilities_for_model("fixture")
            .native_tool_calling
    );
    let tools = [zeroclaw_api::tool::ToolSpec::new(
        "shell",
        "fixture",
        json!({"type":"object"}),
    )];
    let error = provider
        .chat(
            ChatRequest {
                messages: &[ChatMessage::user("fixture")],
                tools: Some(&tools),
                thinking: None,
            },
            "fixture",
            None,
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("tools are disabled"), "{error}");
    assert!(!root.path().join("auth-profiles.json").exists());
}

#[tokio::test]
async fn function_tools_terminal_admission_rejects_unsafe_calls() {
    use axum::{Json, Router, routing::post};
    use zeroclaw_api::tool::ToolSpec;
    let app = Router::new().route("/responses",post(|Json(body):Json<Value>| async move {
        let model = body["model"].as_str().unwrap();
        let mut call = json!({"type":"function_call","namespace":"zeroclaw","name":"shell","call_id":"call_fixture","arguments":"{}","status":"completed"});
        match model {
            "namespace" => call["namespace"] = "hosted".into(),
            "unoffered" => call["name"] = "synthetic-secret-unoffered".into(),
            "id" => call["call_id"] = "".into(),
            "arguments" => call["arguments"] = "synthetic-secret-malformed".into(),
            "status" => call["status"] = "incomplete".into(),
            "caller" => call["caller"] = json!({"type":"program","caller_id":"pc_fixture"}),
            "hosted" => call = json!({"type":"mcp_call","name":"synthetic-secret-hosted"}),
            "duplicate" | "no-completion" | "late-error" | "empty" => {},
            _ => panic!("unexpected fixture"),
        }
        let output = if model == "empty" {json!([])} else if model == "duplicate" {json!([call.clone(),call.clone()])} else {json!([call.clone()])};
        let mut events = format!("data: {}\n\n",json!({"type":"response.output_item.done","item":call}));
            if model != "no-completion" {events.push_str(&format!("data: {}\n\n",json!({"type":"response.completed","response":{"status":"completed","output":output,"output_text":if model=="empty" {""} else {"fixture terminal"}}})));}
        if model == "late-error" { events.push_str("data: {\"type\":\"error\",\"message\":\"synthetic-secret-error\"}\n\n"); }
        ([("content-type","text/event-stream")],events)
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = ::zeroclaw_spawn::spawn!(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let root = tempfile::tempdir().unwrap();
    tool_fixture_profile(root.path()).await;
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
        let tools = [ToolSpec::new("shell", "fixture", json!({"type":"object"}))];
        for (model, reason) in [
            ("namespace", "namespace"),
            ("unoffered", "unoffered"),
            ("id", "call ID"),
            ("arguments", "arguments"),
            ("status", "call status"),
            ("caller", "client-side"),
            ("hosted", "output type"),
            ("duplicate", "Duplicate"),
            ("no-completion", "response.completed"),
            ("late-error", "failed"),
            ("empty", "without text or tool calls"),
        ] {
            let error = provider
                .chat(
                    ChatRequest {
                        messages: &[ChatMessage::user("fixture")],
                        tools: Some(&tools),
                        thinking: None,
                    },
                    model,
                    None,
                )
                .await
                .unwrap_err();
            assert!(error.to_string().contains(reason), "{model}: {error}");
            assert!(!format!("{error:?}").contains("synthetic-secret"));
        }
    })
    .await;
    server.abort();
}

#[tokio::test]
async fn function_tools_request_admission_never_sends_invalid_history_or_schemas() {
    use axum::{Router, routing::post};
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use zeroclaw_api::tool::ToolSpec;
    let sends = Arc::new(AtomicUsize::new(0));
    let app = Router::new().route("/responses",post({let sends=sends.clone(); move || {
        let sends=sends.clone(); async move {
            sends.fetch_add(1,Ordering::SeqCst);
            ([("content-type","text/event-stream")],"data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"output_text\":\"UNEXPECTED REQUEST\"}}\n\n")
        }
    }}));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = ::zeroclaw_spawn::spawn!(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let root = tempfile::tempdir().unwrap();
    tool_fixture_profile(root.path()).await;
    crate::plan_test_transport::scope(&base,async {
        let config=ModelProviderConfig {kind:Some("chatgpt-plan".into()),chatgpt_plan_auth:Some(ChatGptPlanAuthConfig {registration:"chatgpt-plan:subscriber".into()}),..Default::default()};
        let opts=crate::ModelProviderRuntimeOptions {zeroclaw_dir:Some(root.path().into()),..Default::default()};
        let provider=ChatGptPlanProvider::new("subscriber",&config,None,None,&opts).unwrap();
        let call=json!({"id":"call_fixture","name":"shell","arguments":"{}"});
        let item=json!({"type":"function_call","namespace":"zeroclaw","call_id":"call_fixture","name":"shell","arguments":"{}"});
        let assistant=|registration:&str,call:Value,items:Value| ChatMessage::assistant(json!({"content":null,"tool_calls":[call],"reasoning_content":json!({"provider":"chatgpt-plan","registration":registration,"items":items}).to_string()}).to_string());
        let result=|| ChatMessage::tool(json!({"tool_call_id":"call_fixture","content":"fixture result"}).to_string());
        let histories=vec![
            ("different registration",vec![ChatMessage::user("fixture"),assistant("chatgpt-plan:other",call.clone(),json!([item.clone()])),result()]),
            ("does not match",vec![ChatMessage::user("fixture"),assistant("chatgpt-plan:subscriber",json!({"id":"call_fixture","name":"shell","arguments":"{\"different\":true}"}),json!([item.clone()])),result()]),
            ("Duplicate",vec![ChatMessage::user("fixture"),assistant("chatgpt-plan:subscriber",call.clone(),json!([item.clone()])),assistant("chatgpt-plan:subscriber",call.clone(),json!([item.clone()])),result()]),
            ("matching call ID",vec![ChatMessage::user("fixture"),result()]),
            ("missing results",vec![ChatMessage::user("fixture"),assistant("chatgpt-plan:subscriber",call.clone(),json!([item.clone()]))]),
            ("function name",vec![ChatMessage::user("fixture"),assistant("chatgpt-plan:subscriber",json!({"id":"call_fixture","name":"bad.name","arguments":"{}"}),json!([{"type":"function_call","namespace":"zeroclaw","call_id":"call_fixture","name":"bad.name","arguments":"{}"}])),result()]),
            ("output type",vec![ChatMessage::user("fixture"),assistant("chatgpt-plan:subscriber",call,json!([{"type":"tool_search_output"},item])),result()]),
        ];
        let tools=[ToolSpec::new("shell","fixture",json!({"type":"object"}))];
        for (reason,history) in histories {
            let error=provider.chat(ChatRequest {messages:&history,tools:Some(&tools),thinking:None},"fixture",None).await.unwrap_err();
            assert!(error.to_string().contains(reason),"{reason}: {error}");
        }
        for (reason,tools) in [
            ("function name",vec![ToolSpec::new("bad.name","fixture",json!({"type":"object"}))]),
            ("function name",vec![ToolSpec::new("shell","fixture",json!({"type":"object"})),ToolSpec::new("shell","fixture",json!({"type":"object"}))]),
            ("schema object",vec![ToolSpec::new("shell","fixture",json!(true))]),
        ] {
            let error=provider.chat(ChatRequest {messages:&[ChatMessage::user("fixture")],tools:Some(&tools),thinking:None},"fixture",None).await.unwrap_err();
            assert!(error.to_string().contains(reason),"{reason}: {error}");
        }
        for raw in [
            json!({"type":"mcp","name":"shell","description":"fixture","parameters":{"type":"object"}}),
            json!({"type":"function","function":{"name":"shell","description":"fixture","parameters":{"type":"object"}},"server_url":"https://synthetic-secret.example"}),
            json!({"type":"function","name":"shell","description":"fixture","parameters":{"type":"object"},"defer_loading":true}),
            json!({"type":"function","name":"shell","description":"fixture","parameters":{"type":"object"},"strict":true}),
        ] {
            let error=provider.chat_with_tools(&[ChatMessage::user("fixture")],&[raw],"fixture",None).await.unwrap_err();
            assert!(error.to_string().contains("client-side function"),"{error}");
            assert!(!format!("{error:?}").contains("synthetic-secret"));
        }
        let mut disabled=config;
        disabled.native_tools=Some(false);
        let provider=ChatGptPlanProvider::new("subscriber",&disabled,None,None,&opts).unwrap();
        let error=provider.chat(ChatRequest {messages:&[ChatMessage::user("fixture")],tools:Some(&tools),thinking:None},"fixture",None).await.unwrap_err();
        assert!(error.to_string().contains("tools are disabled"));
    }).await;
    assert_eq!(
        sends.load(Ordering::SeqCst),
        0,
        "invalid requests must fail before egress even with a valid grant"
    );
    server.abort();
}

#[test]
fn preview_request_is_text_only_and_has_only_supported_fields() {
    let request = request_body(
        &[
            ChatMessage::system("instructions"),
            ChatMessage::user("fixture"),
            ChatMessage::assistant("history"),
        ],
        &[],
        "model-fixture",
        None,
        "chatgpt-plan:subscriber",
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
    assert!(
        request_body(
            &[ChatMessage::tool("fixture")],
            &[],
            "model-fixture",
            None,
            "chatgpt-plan:subscriber"
        )
        .is_err()
    );
    assert!(
        request_body(
            &[ChatMessage::user("fixture")],
            &[],
            "model-fixture",
            Some(0.5),
            "chatgpt-plan:subscriber"
        )
        .is_err()
    );
}

#[test]
fn sse_requires_completed_and_rejects_incomplete_done_and_late_errors() {
    let mut sse = PlanSse::default();
    sse.event("data: {\"type\":\"response.output_text.delta\",\"delta\":\"partial\"}")
        .unwrap();
    assert!(sse.finish(&[], "chatgpt-plan:subscriber").is_err());
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
    assert_eq!(
        sse.finish(&[], "chatgpt-plan:subscriber")
            .unwrap()
            .text
            .as_deref(),
        Some("READY")
    );
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
    assert!(ChatGptPlanProvider::new("subscriber", &incompatible, None, None, &opts).is_ok());
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
                "eof",
                None,
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("response.completed"));
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
