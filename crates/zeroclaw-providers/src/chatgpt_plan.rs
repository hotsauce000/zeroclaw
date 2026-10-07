//! Restricted, explicitly bound ChatGPT plan usage over public Responses.
//! This first slice supports text without tools. No API-key fallback.
use crate::auth::AuthService;
use crate::traits::{ChatMessage, ChatRequest, ChatResponse, ModelProvider};
use anyhow::{Context, Result};
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::time::Duration;
use zeroclaw_config::schema::{ModelProviderConfig, WireApi};

const RESPONSES_URL: &str = "https://api.openai.com/v1/responses";
const MODELS_URL: &str = "https://api.openai.com/v1/models";

#[cfg(test)]
tokio::task_local! { static FIXTURE_REQUEST_TIMEOUT: Duration; }

fn request_timeout() -> Duration {
    #[cfg(test)]
    if let Ok(timeout) = FIXTURE_REQUEST_TIMEOUT.try_with(|timeout| *timeout) {
        return timeout;
    }
    Duration::from_secs(300)
}

fn protocol_error(message: &'static str) -> anyhow::Error {
    ::zeroclaw_log::record!(
        WARN,
        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
            .with_outcome(::zeroclaw_log::EventOutcome::Failure)
            .with_attrs(serde_json::json!({"reason":message})),
        "ChatGPT plan transport failure"
    );
    anyhow::Error::msg(message)
}

pub struct ChatGptPlanProvider {
    alias: String,
    registration: String,
    auth: AuthService,
    client: reqwest::Client,
    responses_url: String,
    models_url: String,
}

impl ChatGptPlanProvider {
    pub(crate) fn new(
        alias: &str,
        config: &ModelProviderConfig,
        key: Option<&str>,
        uri: Option<&str>,
        opts: &crate::ModelProviderRuntimeOptions,
    ) -> Result<Self> {
        anyhow::ensure!(
            cfg!(unix),
            "ChatGPT plan usage is unsupported on native Windows in this slice"
        );
        let binding = config
            .chatgpt_plan_auth
            .as_ref()
            .context("Explicit ChatGPT plan auth reference required")?;
        anyhow::ensure!(
            binding.registration.starts_with("chatgpt-plan:")
                && binding.registration.len() > "chatgpt-plan:".len(),
            "Invalid ChatGPT plan registration reference"
        );
        anyhow::ensure!(
            !config.requires_openai_auth
                && config.api_key.is_none()
                && key.is_none()
                && opts.auth_profile_override.is_none(),
            "ChatGPT plan auth cannot be combined with Codex, API-key or global-profile credentials"
        );
        anyhow::ensure!(
            config.kind.as_deref() == Some("chatgpt-plan")
                && opts
                    .provider_kind
                    .as_deref()
                    .is_none_or(|kind| kind == "chatgpt-plan"),
            "ChatGPT plan auth requires kind = chatgpt-plan"
        );
        for endpoint in [uri, config.uri.as_deref(), opts.provider_api_url.as_deref()]
            .into_iter()
            .flatten()
        {
            anyhow::ensure!(
                matches!(endpoint, "https://api.openai.com/v1" | RESPONSES_URL),
                "ChatGPT plan usage requires the public OpenAI v1 endpoint"
            );
        }
        anyhow::ensure!(
            config
                .wire_api
                .is_none_or(|wire| wire == WireApi::Responses)
                && opts
                    .wire_api
                    .as_deref()
                    .is_none_or(|wire| wire == "responses"),
            "ChatGPT plan usage requires Responses"
        );
        anyhow::ensure!(
            config.extra_headers.is_empty()
                && opts.extra_headers.is_empty()
                && config.tls_ca_cert_path.is_none()
                && opts.tls_ca_cert_path.is_none()
                && opts.api_path.is_none(),
            "ChatGPT plan transport overrides are unsupported"
        );
        anyhow::ensure!(
            config.temperature.is_none()
                && config.max_tokens.is_none()
                && opts.provider_max_tokens.is_none()
                && config.provider_extra.is_none()
                && opts.provider_extra.is_none()
                && opts.chat_template_kwargs.is_none()
                && !config.merge_system_into_user
                && !opts.merge_system_into_user,
            "ChatGPT plan preview request parameters are unsupported"
        );
        anyhow::ensure!(
            config.fallback.is_empty(),
            "ChatGPT plan provider fallbacks are unsupported; no metered fallback is enabled"
        );
        anyhow::ensure!(
            config.native_tools != Some(true) && opts.native_tools != Some(true),
            "ChatGPT plan tools are unsupported in this slice"
        );
        let root = opts
            .zeroclaw_dir
            .as_deref()
            .context("ChatGPT plan usage requires an explicit instance directory")?;
        let client = reqwest::Client::builder()
            .timeout(request_timeout())
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(30))
            .build()
            .context("Unable to create ChatGPT plan inference client")?;
        let (responses_url, models_url) = (RESPONSES_URL.to_string(), MODELS_URL.to_string());
        #[cfg(any(test, feature = "test-helpers"))]
        let (responses_url, models_url) = crate::plan_test_transport::ENDPOINTS
            .try_with(|endpoints| (endpoints.responses.clone(), endpoints.models.clone()))
            .unwrap_or((responses_url, models_url));
        Ok(Self {
            alias: alias.into(),
            registration: binding.registration.clone(),
            auth: AuthService::new(root, opts.secrets_encrypt),
            client,
            responses_url,
            models_url,
        })
    }

    async fn complete(
        &self,
        messages: &[ChatMessage],
        model: &str,
        temperature: Option<f64>,
    ) -> Result<String> {
        let body = request_body(messages, model, temperature)?;
        let access = self
            .auth
            .get_valid_chatgpt_plan_access_token(&self.registration)
            .await?;
        let response = tokio::time::timeout(
            Duration::from_secs(30),
            self.client
                .post(&self.responses_url)
                .bearer_auth(access)
                .header("Accept", "text/event-stream")
                .json(&body)
                .send(),
        )
        .await
        .map_err(|_| protocol_error("ChatGPT plan response headers timed out"))?
        .map_err(|_| protocol_error("ChatGPT plan request failed"))?;
        anyhow::ensure!(
            response.status() == reqwest::StatusCode::OK,
            "ChatGPT plan inference rejected the request (HTTP {}); no metered fallback",
            response.status().as_u16()
        );
        anyhow::ensure!(
            response
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| v.starts_with("text/event-stream")),
            "ChatGPT plan response is not an event stream"
        );
        let mut stream = response.bytes_stream();
        let mut pending = Vec::new();
        let mut sse = PlanSse::default();
        while let Some(chunk) = tokio::time::timeout(Duration::from_secs(300), stream.next())
            .await
            .map_err(|_| protocol_error("ChatGPT plan stream interrupted"))?
        {
            let bytes = chunk.map_err(|_| protocol_error("ChatGPT plan stream interrupted"))?;
            pending.extend_from_slice(&bytes);
            anyhow::ensure!(
                pending.len() <= 8 * 1024 * 1024,
                "ChatGPT plan stream event exceeds size limit"
            );
            loop {
                let end = pending
                    .windows(2)
                    .position(|w| w == b"\n\n")
                    .map(|end| (end, 2))
                    .or_else(|| {
                        pending
                            .windows(4)
                            .position(|w| w == b"\r\n\r\n")
                            .map(|end| (end, 4))
                    });
                let Some((end, width)) = end else {
                    break;
                };
                let event = std::str::from_utf8(&pending[..end])
                    .map_err(|_| protocol_error("Invalid ChatGPT stream encoding"))?;
                sse.event(event)?;
                pending.drain(..end + width);
            }
            // Process every event already in the terminal read, including a
            // late failure. Do not wait for a server to close after completion.
            if sse.completed {
                anyhow::ensure!(
                    pending.iter().all(u8::is_ascii_whitespace),
                    "ChatGPT plan stream has an incomplete event after completion"
                );
                return sse.finish();
            }
        }
        if !pending.is_empty() {
            anyhow::bail!("ChatGPT plan stream ended with an incomplete event");
        }
        sse.finish()
    }
}

fn request_body(messages: &[ChatMessage], model: &str, temperature: Option<f64>) -> Result<Value> {
    anyhow::ensure!(
        temperature.is_none(),
        "ChatGPT plan preview does not support temperature"
    );
    anyhow::ensure!(!model.trim().is_empty(), "ChatGPT plan model is required");
    let mut input = Vec::new();
    let mut instructions = Vec::new();
    for message in messages {
        match message.role.as_str() {
            "system" | "developer" => instructions.push(message.content.clone()),
            "user" | "assistant" => {
                input.push(json!({"role":message.role,"content":message.content}))
            }
            _ => anyhow::bail!("ChatGPT plan tools and tool history are unsupported in this slice"),
        }
    }
    anyhow::ensure!(!input.is_empty(), "ChatGPT plan input is required");
    let mut body = json!({"model":model,"input":input,"store":false,"stream":true});
    if !instructions.is_empty() {
        body["instructions"] = instructions.join("\n\n").into();
    }
    Ok(body)
}

#[derive(Default)]
struct PlanSse {
    completed: bool,
    text: String,
}
impl PlanSse {
    fn event(&mut self, event: &str) -> Result<()> {
        let data = event
            .lines()
            .filter_map(|line| line.strip_prefix("data:").map(str::trim_start))
            .collect::<Vec<_>>()
            .join("\n");
        if data.is_empty() {
            return Ok(());
        }
        anyhow::ensure!(
            data != "[DONE]",
            "ChatGPT plan stream omitted response.completed"
        );
        let event: Value = serde_json::from_str(&data)
            .map_err(|_| protocol_error("Malformed ChatGPT plan stream event"))?;
        match event["type"]
            .as_str()
            .context("ChatGPT stream event missing type")?
        {
            "response.failed" | "response.incomplete" | "error" => {
                let code = event["response"]["error"]["code"]
                    .as_str()
                    .or_else(|| event["code"].as_str());
                match code {
                    Some("subscription_sharing_usage_limit_exceeded") => anyhow::bail!(
                        "ChatGPT plan usage limit reached; review ChatGPT usage settings"
                    ),
                    Some("subscription_sharing_usage_unavailable") => anyhow::bail!(
                        "ChatGPT plan usage unavailable; review ChatGPT app permissions"
                    ),
                    _ => anyhow::bail!("ChatGPT plan response failed or was incomplete"),
                }
            }
            "response.output_text.delta" => {
                anyhow::ensure!(!self.completed, "ChatGPT text arrived after completion");
                self.text.push_str(
                    event["delta"]
                        .as_str()
                        .context("ChatGPT stream delta missing text")?,
                );
                anyhow::ensure!(
                    self.text.len() <= 16 * 1024 * 1024,
                    "ChatGPT response exceeds size limit"
                );
            }
            "response.completed" => {
                anyhow::ensure!(
                    !self.completed && event["response"]["status"] == "completed",
                    "Invalid ChatGPT response completion"
                );
                if let Some(output) = event["response"]["output"].as_array() {
                    anyhow::ensure!(
                        !output.iter().any(|item| matches!(
                            item["type"].as_str(),
                            Some("function_call" | "custom_tool_call")
                        )),
                        "ChatGPT plan tools are unsupported in this slice"
                    );
                }
                if self.text.is_empty() {
                    if let Some(text) = event["response"]["output_text"].as_str() {
                        self.text = text.into();
                    } else if let Some(output) = event["response"]["output"].as_array() {
                        for item in output {
                            if item["type"] == "message"
                                && let Some(parts) = item["content"].as_array()
                            {
                                for part in parts {
                                    if part["type"] == "output_text"
                                        && let Some(text) = part["text"].as_str()
                                    {
                                        self.text.push_str(text);
                                    }
                                }
                            }
                        }
                    }
                }
                self.completed = true;
            }
            "response.output_item.added" | "response.output_item.done"
                if matches!(
                    event["item"]["type"].as_str(),
                    Some("function_call" | "custom_tool_call")
                ) =>
            {
                anyhow::bail!("ChatGPT plan tools are unsupported in this slice")
            }
            _ => {}
        }
        Ok(())
    }
    fn finish(&self) -> Result<String> {
        anyhow::ensure!(
            self.completed,
            "ChatGPT plan stream ended before response.completed"
        );
        anyhow::ensure!(
            !self.text.trim().is_empty(),
            "ChatGPT plan response completed without text"
        );
        Ok(self.text.clone())
    }
}

#[async_trait::async_trait]
impl ModelProvider for ChatGptPlanProvider {
    fn default_base_url(&self) -> Option<&str> {
        Some(RESPONSES_URL)
    }
    fn default_wire_api(&self) -> &str {
        "responses"
    }
    async fn chat_with_system(
        &self,
        system: Option<&str>,
        message: &str,
        model: &str,
        temperature: Option<f64>,
    ) -> Result<String> {
        let mut messages = Vec::new();
        if let Some(system) = system {
            messages.push(ChatMessage::system(system));
        }
        messages.push(ChatMessage::user(message));
        self.complete(&messages, model, temperature).await
    }
    async fn chat_with_history(
        &self,
        messages: &[ChatMessage],
        model: &str,
        temperature: Option<f64>,
    ) -> Result<String> {
        self.complete(messages, model, temperature).await
    }
    async fn chat(
        &self,
        request: ChatRequest<'_>,
        model: &str,
        temperature: Option<f64>,
    ) -> Result<ChatResponse> {
        anyhow::ensure!(
            request.tools.is_none_or(|tools| tools.is_empty()),
            "ChatGPT plan tools are unsupported in this slice"
        );
        Ok(ChatResponse {
            text: Some(self.complete(request.messages, model, temperature).await?),
            tool_calls: Vec::new(),
            usage: None,
            reasoning_content: None,
        })
    }
    async fn chat_with_tools(
        &self,
        messages: &[ChatMessage],
        tools: &[Value],
        model: &str,
        temperature: Option<f64>,
    ) -> Result<ChatResponse> {
        anyhow::ensure!(
            tools.is_empty(),
            "ChatGPT plan tools are unsupported in this slice"
        );
        Ok(ChatResponse {
            text: Some(self.complete(messages, model, temperature).await?),
            tool_calls: Vec::new(),
            usage: None,
            reasoning_content: None,
        })
    }
    async fn list_models(&self) -> Result<Vec<String>> {
        let access = self
            .auth
            .get_valid_chatgpt_plan_access_token(&self.registration)
            .await?;
        let response = tokio::time::timeout(
            Duration::from_secs(30),
            self.client.get(&self.models_url).bearer_auth(access).send(),
        )
        .await
        .map_err(|_| protocol_error("ChatGPT model catalog timed out"))?
        .map_err(|_| protocol_error("ChatGPT model catalog request failed"))?;
        anyhow::ensure!(
            response.status() == reqwest::StatusCode::OK,
            "ChatGPT model catalog unavailable (HTTP {})",
            response.status().as_u16()
        );
        let bytes = tokio::time::timeout(
            Duration::from_secs(30),
            crate::compatible::read_body_capped(response, 1024 * 1024),
        )
        .await
        .map_err(|_| protocol_error("ChatGPT model catalog timed out"))??;
        let catalog: Value = serde_json::from_slice(&bytes)
            .map_err(|_| protocol_error("Invalid ChatGPT model catalog"))?;
        let models = catalog["models"]
            .as_array()
            .context("ChatGPT model catalog missing models")?;
        Ok(models
            .iter()
            .filter(|model| model["visibility"] == "list")
            .filter_map(|model| model["slug"].as_str().map(str::to_owned))
            .collect())
    }
}
impl zeroclaw_api::attribution::Attributable for ChatGptPlanProvider {
    fn role(&self) -> zeroclaw_api::attribution::Role {
        zeroclaw_api::attribution::Role::Provider(zeroclaw_api::attribution::ProviderKind::Model(
            zeroclaw_api::attribution::ModelProviderKind::OpenAi,
        ))
    }
    fn alias(&self) -> &str {
        &self.alias
    }
}

#[cfg(all(test, unix))]
#[path = "chatgpt_plan_tests.rs"]
mod tests;
