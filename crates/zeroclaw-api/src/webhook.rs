//! Wasmtime-free wiring for channel-plugin webhook ingress.
//!
//! The core plugin webhook ingress, the gateway, and the channel supervisor
//! meet in this API crate so neither the transport nor the ingress needs the
//! plugin runtime. A running channel publishes one bounded route, the ingress
//! forwards exact request bytes to it, and a one-shot carries only the
//! outcome.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use tokio::sync::{mpsc, oneshot, watch};

/// Cancellation signal for one plugin webhook request owned by the core ingress.
pub type WebhookCancellation = tokio_util::sync::CancellationToken;

/// The state shared with a duplicate while the first delivery owns a stable
/// message ID.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebhookReservationStatus {
    /// The owning request has not completed delivery yet.
    InFlight,
    /// The owning request delivered the message successfully.
    Committed,
    /// The owning request failed before delivery and released the ID.
    RolledBack,
}

/// Opaque ownership proof for one in-flight idempotency reservation.
///
/// The generation prevents a delayed owner from committing or rolling back a
/// later request that acquired the same key after the first owner failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebhookReservationToken {
    key: String,
    generation: u64,
}

impl WebhookReservationToken {
    /// Create a token at the core ingress idempotency boundary.
    #[must_use]
    pub fn new(key: String, generation: u64) -> Self {
        Self { key, generation }
    }

    /// Storage key owned by the core ingress idempotency store.
    #[must_use]
    pub fn key(&self) -> &str {
        &self.key
    }

    /// Monotonic ownership generation for this storage key.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation
    }
}

/// Wait handle returned to a request that meets an in-flight owner.
pub struct WebhookReservationWaiter {
    status: watch::Receiver<WebhookReservationStatus>,
}

impl WebhookReservationWaiter {
    /// Wrap the core ingress store's reservation watch channel.
    #[must_use]
    pub fn new(status: watch::Receiver<WebhookReservationStatus>) -> Self {
        Self { status }
    }

    /// Wait until the owner commits or rolls back.
    ///
    /// A dropped sender is treated as rollback: no successful owner remains to
    /// justify acknowledging the duplicate.
    pub async fn wait(&mut self) -> WebhookReservationStatus {
        loop {
            let status = *self.status.borrow_and_update();
            if status != WebhookReservationStatus::InFlight {
                return status;
            }
            if self.status.changed().await.is_err() {
                return WebhookReservationStatus::RolledBack;
            }
        }
    }
}

/// Result of trying to reserve one authenticated, host-authorized message ID.
pub enum WebhookReservation {
    /// This request owns delivery and must commit or roll back with the token.
    Owner(WebhookReservationToken),
    /// A prior owner already delivered the message.
    Committed,
    /// Another request owns delivery; wait for its actual outcome.
    InFlight(WebhookReservationWaiter),
    /// The bounded core ingress store cannot safely admit another owner.
    Unavailable,
}

/// Live callbacks into the core ingress's canonical idempotency store.
///
/// The plugin worker invokes this only after guest authentication and live
/// host sender authorization. Ownership tokens make rollback conditional, and
/// an in-flight duplicate receives a waiter rather than a premature success.
#[derive(Clone)]
pub struct WebhookIdempotency {
    begin: Arc<dyn Fn(&str) -> WebhookReservation + Send + Sync>,
    commit: Arc<dyn Fn(&WebhookReservationToken) -> bool + Send + Sync>,
    rollback: Arc<dyn Fn(&WebhookReservationToken) -> bool + Send + Sync>,
}

impl WebhookIdempotency {
    /// Build a bridge to a host-owned reservation store.
    #[must_use]
    pub fn new(
        begin: impl Fn(&str) -> WebhookReservation + Send + Sync + 'static,
        commit: impl Fn(&WebhookReservationToken) -> bool + Send + Sync + 'static,
        rollback: impl Fn(&WebhookReservationToken) -> bool + Send + Sync + 'static,
    ) -> Self {
        Self {
            begin: Arc::new(begin),
            commit: Arc::new(commit),
            rollback: Arc::new(rollback),
        }
    }

    /// Begin or observe delivery for one stable message ID.
    #[must_use]
    pub fn begin(&self, message_id: &str) -> WebhookReservation {
        (self.begin)(message_id)
    }

    /// Commit only if `token` still owns the exact reservation generation.
    #[must_use]
    pub fn commit(&self, token: &WebhookReservationToken) -> bool {
        (self.commit)(token)
    }

    /// Roll back only if `token` still owns the exact reservation generation.
    #[must_use]
    pub fn rollback(&self, token: &WebhookReservationToken) -> bool {
        (self.rollback)(token)
    }
}

/// A raw inbound request received on `/plugin/{path}`.
pub struct RawWebhook {
    /// HTTP method supplied by the gateway request, never by a header.
    pub method: String,
    /// Raw query string, without the leading `?`.
    pub query: String,
    /// Header names normalized to lowercase and their UTF-8 values.
    pub headers: Vec<(String, String)>,
    /// Exact received body bytes.
    pub body: Vec<u8>,
    /// Request-lifetime cancellation shared with the disposable guest parser.
    pub cancellation: WebhookCancellation,
    /// Access to the core ingress's canonical idempotency store.
    pub idempotency: Option<WebhookIdempotency>,
    /// Outcome used to select the public HTTP response.
    pub reply: oneshot::Sender<Result<WebhookOutcome, WebhookReject>>,
}

/// Maximum UTF-8 byte length of a plugin's HTTP challenge response.
pub const MAX_WEBHOOK_RESPONSE_BODY_BYTES: usize = 4 * 1024;
/// Requests one published route may hold before admission reports it full.
pub const PLUGIN_WEBHOOK_QUEUE_DEPTH: usize = 64;
/// Lifetime of one request, counted from successful enqueue.
pub const PLUGIN_WEBHOOK_DEADLINE: Duration = Duration::from_secs(10);
/// Largest request body; a transport's own body ceiling must not exceed it.
pub const MAX_PLUGIN_WEBHOOK_BODY_BYTES: usize = 64 * 1024;
/// Most headers one request may carry, above what the HTTP listener admits.
pub const MAX_PLUGIN_WEBHOOK_HEADERS: usize = 512;
/// Longest header name, the most the `http` crate's `HeaderName` can hold.
pub const MAX_PLUGIN_WEBHOOK_HEADER_NAME_BYTES: usize = 65_535;
/// Sum of name and value bytes over all headers.
pub const MAX_PLUGIN_WEBHOOK_HEADER_BYTES: usize = 512 * 1024;
/// Above the HTTP URI limit (65 534 bytes), so HTTP never reaches it.
pub const MAX_PLUGIN_WEBHOOK_QUERY_BYTES: usize = 64 * 1024;
/// Longest route path segment.
pub const MAX_PLUGIN_WEBHOOK_PATH_BYTES: usize = 64;

/// One route path segment: 1 to 64 ASCII letters, digits, `-` or `_`.
#[must_use]
pub fn is_valid_plugin_webhook_path(path: &str) -> bool {
    let bytes = path.as_bytes();
    (1..=MAX_PLUGIN_WEBHOOK_PATH_BYTES).contains(&bytes.len())
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

/// Acknowledgement after message delivery, or a response with no agent work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WebhookOutcome {
    Ack,
    Body(String),
}

/// Why a plugin webhook request could not be accepted.
#[derive(Debug, Clone)]
pub enum WebhookReject {
    /// Guest authentication failed. The detail is for host logs only.
    Unauthorized(String),
    /// The authenticated payload was malformed. Detail is host-only.
    BadRequest(String),
    /// Guest execution or downstream delivery was unavailable. Detail is
    /// host-only; the public response is always an opaque 503.
    Unavailable(String),
    /// The plugin's response exceeds the host-owned body limit (opaque 502).
    InvalidResponse,
    /// The request deadline cancelled guest parsing or delivery.
    Timeout,
}

/// The plugin instance that owns a published route.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PluginWebhookOwner {
    plugin: String,
    channel_alias: String,
}

impl PluginWebhookOwner {
    /// Identify a route by its plugin package name and channel alias.
    #[must_use]
    pub fn new(plugin: impl Into<String>, channel_alias: impl Into<String>) -> Self {
        Self {
            plugin: plugin.into(),
            channel_alias: channel_alias.into(),
        }
    }

    /// Plugin package name.
    #[must_use]
    pub fn plugin(&self) -> &str {
        &self.plugin
    }

    /// Configured channel alias of the owning instance.
    #[must_use]
    pub fn channel_alias(&self) -> &str {
        &self.channel_alias
    }
}

/// One published route: its owning instance and the bounded queue its worker
/// drains.
#[derive(Debug, Clone)]
pub struct PluginWebhookRoute {
    owner: PluginWebhookOwner,
    sink: mpsc::Sender<RawWebhook>,
}

impl PluginWebhookRoute {
    /// Pair a route owner with its worker queue.
    #[must_use]
    pub fn new(owner: PluginWebhookOwner, sink: mpsc::Sender<RawWebhook>) -> Self {
        Self { owner, sink }
    }

    /// The plugin instance that claimed this route.
    #[must_use]
    pub fn owner(&self) -> &PluginWebhookOwner {
        &self.owner
    }

    /// The bounded queue of the owning worker.
    #[must_use]
    pub fn sink(&self) -> &mpsc::Sender<RawWebhook> {
        &self.sink
    }
}

/// Why a transport request cannot become a [`PluginWebhookRequest`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PluginWebhookRequestError {
    #[error("unsupported plugin webhook method")]
    UnsupportedMethod,
    #[error("too many plugin webhook headers")]
    TooManyHeaders,
    #[error("invalid plugin webhook header")]
    InvalidHeader,
    #[error("plugin webhook headers exceed the byte limit")]
    HeadersTooLarge,
    #[error("plugin webhook query exceeds the byte limit")]
    QueryTooLarge,
    #[error("plugin webhook body exceeds the byte limit")]
    BodyTooLarge,
}

impl PluginWebhookRequestError {
    /// Stable log key.
    #[must_use]
    pub fn reason(self) -> &'static str {
        match self {
            Self::UnsupportedMethod => "unsupported_method",
            Self::TooManyHeaders => "too_many_headers",
            Self::InvalidHeader => "invalid_header",
            Self::HeadersTooLarge => "headers_too_large",
            Self::QueryTooLarge => "query_too_large",
            Self::BodyTooLarge => "body_too_large",
        }
    }
}

/// One webhook request, validated against the ingress bounds.
///
/// The path is not validated here: an unknown or malformed path is a
/// [`PluginWebhookOutcome::NotFound`] outcome at dispatch.
pub struct PluginWebhookRequest {
    path: String,
    method: &'static str,
    query: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl PluginWebhookRequest {
    /// Validate a request in a fixed order: the method is exactly `GET` or
    /// `POST`; the header count; each header; the header bytes; the query
    /// bytes; the body bytes. Header order and repeated names are kept.
    ///
    /// A header name is lowercased and must then be a name the `http` crate's
    /// `HeaderName` can hold: 1 to [`MAX_PLUGIN_WEBHOOK_HEADER_NAME_BYTES`]
    /// bytes, each an RFC 9110 `tchar` or `"`, which HTTP/2 and HTTP/3
    /// decoders also admit. A header value may hold only visible ASCII,
    /// space, and tab.
    pub fn new(
        path: impl Into<String>,
        method: &str,
        query: impl Into<String>,
        mut headers: Vec<(String, String)>,
        body: Vec<u8>,
    ) -> Result<Self, PluginWebhookRequestError> {
        let method = match method {
            "GET" => "GET",
            "POST" => "POST",
            _ => return Err(PluginWebhookRequestError::UnsupportedMethod),
        };
        if headers.len() > MAX_PLUGIN_WEBHOOK_HEADERS {
            return Err(PluginWebhookRequestError::TooManyHeaders);
        }
        let mut header_bytes = 0_usize;
        for (name, value) in &mut headers {
            name.make_ascii_lowercase();
            if !(1..=MAX_PLUGIN_WEBHOOK_HEADER_NAME_BYTES).contains(&name.len())
                || !name.bytes().all(is_header_name_byte)
                || !value.bytes().all(is_header_value_byte)
            {
                return Err(PluginWebhookRequestError::InvalidHeader);
            }
            header_bytes = header_bytes
                .saturating_add(name.len())
                .saturating_add(value.len());
        }
        if header_bytes > MAX_PLUGIN_WEBHOOK_HEADER_BYTES {
            return Err(PluginWebhookRequestError::HeadersTooLarge);
        }
        let query = query.into();
        if query.len() > MAX_PLUGIN_WEBHOOK_QUERY_BYTES {
            return Err(PluginWebhookRequestError::QueryTooLarge);
        }
        if body.len() > MAX_PLUGIN_WEBHOOK_BODY_BYTES {
            return Err(PluginWebhookRequestError::BodyTooLarge);
        }
        Ok(Self {
            path: path.into(),
            method,
            query,
            headers,
            body,
        })
    }

    /// Route path segment, unvalidated.
    #[must_use]
    pub fn path(&self) -> &str {
        &self.path
    }

    /// `GET` or `POST`.
    #[must_use]
    pub fn method(&self) -> &str {
        self.method
    }

    /// Raw query string, without the leading `?`.
    #[must_use]
    pub fn query(&self) -> &str {
        &self.query
    }

    /// Lowercased header names and their values, in request order.
    #[must_use]
    pub fn headers(&self) -> &[(String, String)] {
        &self.headers
    }

    /// Exact body bytes.
    #[must_use]
    pub fn body(&self) -> &[u8] {
        &self.body
    }

    /// Hand the request to a route worker. The path stays behind: the worker
    /// is already the route's owner.
    #[must_use]
    pub fn into_raw_webhook(
        self,
        cancellation: WebhookCancellation,
        idempotency: Option<WebhookIdempotency>,
        reply: oneshot::Sender<Result<WebhookOutcome, WebhookReject>>,
    ) -> RawWebhook {
        RawWebhook {
            method: self.method.to_string(),
            query: self.query,
            headers: self.headers,
            body: self.body,
            cancellation,
            idempotency,
            reply,
        }
    }
}

/// Header values and the body may carry signatures, and the query may carry
/// tokens, so only their sizes are shown.
impl fmt::Debug for PluginWebhookRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PluginWebhookRequest")
            .field("path", &self.path)
            .field("method", &self.method)
            .field("query_bytes", &self.query.len())
            .field("headers", &self.headers.len())
            .field("body_bytes", &self.body.len())
            .finish()
    }
}

/// A byte of a lowercased name that `HeaderName` can hold: a lowercase
/// RFC 9110 `tchar`, or the `"` that its HTTP/2 decoding path also admits.
fn is_header_name_byte(byte: u8) -> bool {
    byte.is_ascii_lowercase()
        || byte.is_ascii_digit()
        || matches!(
            byte,
            b'!' | b'"'
                | b'#'
                | b'$'
                | b'%'
                | b'&'
                | b'\''
                | b'*'
                | b'+'
                | b'-'
                | b'.'
                | b'^'
                | b'_'
                | b'`'
                | b'|'
                | b'~'
        )
}

/// Visible ASCII, space, or tab: exactly what an HTTP header value must hold
/// for the gateway to forward it.
fn is_header_value_byte(byte: u8) -> bool {
    byte == b'\t' || (0x20..=0x7E).contains(&byte)
}

/// The result of dispatching one request through the core ingress.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginWebhookOutcome {
    /// The route's worker accepted the request.
    Ack,
    /// The worker answered with a reply of at most
    /// [`MAX_WEBHOOK_RESPONSE_BODY_BYTES`] bytes.
    Reply(String),
    /// No live route owns the path, or the path is malformed.
    NotFound,
    /// The route's queue is full.
    QueueFull,
    /// The route or its worker is unavailable.
    Unavailable,
    /// The guest rejected the request's authentication.
    Unauthorized,
    /// The guest rejected the authenticated payload.
    BadRequest,
    /// The worker's response broke the reply contract.
    InvalidResponse,
    /// No outcome arrived within [`PLUGIN_WEBHOOK_DEADLINE`] of enqueue.
    Timeout,
    /// The caller cancelled the request.
    Cancelled,
}

/// Path-to-route table owned by the core ingress and published by the
/// channel supervisor.
///
/// The map is replaced as one unit after every claimant has been validated, so
/// a partial rebuild or duplicate path cannot leave a mixed-generation route
/// set effective.
#[derive(Default, Clone)]
pub struct PluginWebhookRegistry {
    state: Arc<Mutex<PluginWebhookRegistryState>>,
}

#[derive(Default)]
struct PluginWebhookRegistryState {
    generation: u64,
    routes: HashMap<String, PluginWebhookRoute>,
}

/// Ownership proof for one channel-supervisor route generation.
///
/// Publishing and retirement are conditional on this generation still being
/// current, so a delayed older supervisor cannot erase newer routes.
pub struct PluginWebhookRegistryLease {
    registry: PluginWebhookRegistry,
    generation: u64,
}

impl PluginWebhookRegistry {
    /// Create an empty runtime routing view.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Begin a new empty route generation owned by the returned lease.
    #[must_use]
    pub fn start_generation(&self) -> PluginWebhookRegistryLease {
        let mut state = self.lock_state();
        state.generation = state.generation.wrapping_add(1);
        state.routes.clear();
        PluginWebhookRegistryLease {
            registry: self.clone(),
            generation: state.generation,
        }
    }

    /// Clone the route for `path`, if a live channel owns it.
    #[must_use]
    pub fn get(&self, path: &str) -> Option<PluginWebhookRoute> {
        self.lock_state().routes.get(path).cloned()
    }

    fn lock_state(&self) -> MutexGuard<'_, PluginWebhookRegistryState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl PluginWebhookRegistryLease {
    /// Atomically publish all validated routes if this generation still owns
    /// the registry. Returns `false` when a newer generation already started.
    #[must_use]
    pub fn replace(&self, routes: HashMap<String, PluginWebhookRoute>) -> bool {
        let mut state = self.registry.lock_state();
        if state.generation != self.generation {
            return false;
        }
        state.routes = routes;
        true
    }
}

impl Drop for PluginWebhookRegistryLease {
    fn drop(&mut self) {
        let mut state = self.registry.lock_state();
        if state.generation == self.generation {
            state.routes.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn route(sink: mpsc::Sender<RawWebhook>) -> PluginWebhookRoute {
        PluginWebhookRoute::new(PluginWebhookOwner::new("fixture-plugin", "fixture"), sink)
    }

    fn request(
        method: &str,
        headers: Vec<(String, String)>,
    ) -> Result<PluginWebhookRequest, PluginWebhookRequestError> {
        PluginWebhookRequest::new("fixture", method, "", headers, Vec::new())
    }

    fn header(name: &str, value: &str) -> (String, String) {
        (name.to_string(), value.to_string())
    }

    #[test]
    fn registry_recovers_after_a_poisoned_lock() {
        let registry = PluginWebhookRegistry::new();
        let poison_target = registry.clone();
        let poisoner = std::thread::spawn(move || {
            let state = poison_target
                .state
                .lock()
                .expect("test obtains registry lock");
            assert!(state.routes.is_empty());
            panic!("poison registry for recovery test");
        });
        assert!(poisoner.join().is_err());

        let (sink, receiver) = mpsc::channel(1);
        let lease = registry.start_generation();
        assert!(lease.replace(HashMap::from([("fixture".to_string(), route(sink))])));
        assert!(registry.get("fixture").is_some());
        drop(receiver);
    }

    #[test]
    fn replace_publishes_one_complete_generation() {
        let registry = PluginWebhookRegistry::new();
        let (old, old_rx) = mpsc::channel(1);
        let lease = registry.start_generation();
        assert!(lease.replace(HashMap::from([("old".to_string(), route(old))])));

        let (new, new_rx) = mpsc::channel(1);
        assert!(lease.replace(HashMap::from([("new".to_string(), route(new))])));

        assert!(registry.get("old").is_none());
        assert!(registry.get("new").is_some());
        drop((old_rx, new_rx));
    }

    #[test]
    fn stale_generation_cannot_publish_or_clear_newer_routes() {
        let registry = PluginWebhookRegistry::new();
        let stale = registry.start_generation();
        let (stale_sink, stale_rx) = mpsc::channel(1);
        assert!(stale.replace(HashMap::from([("stale".to_string(), route(stale_sink))])));

        let current = registry.start_generation();
        let (current_sink, current_rx) = mpsc::channel(1);
        assert!(current.replace(HashMap::from([(
            "current".to_string(),
            route(current_sink),
        )])));
        let (late_sink, late_rx) = mpsc::channel(1);
        assert!(!stale.replace(HashMap::from([("late".to_string(), route(late_sink))])));
        drop(stale);

        assert!(registry.get("stale").is_none());
        assert!(registry.get("late").is_none());
        assert!(registry.get("current").is_some());
        drop(current);
        assert!(registry.get("current").is_none());
        drop((stale_rx, current_rx, late_rx));
    }

    #[test]
    fn registry_get_returns_the_route_with_its_owner() {
        let registry = PluginWebhookRegistry::new();
        let (sink, _receiver) = mpsc::channel(1);
        let lease = registry.start_generation();
        assert!(lease.replace(HashMap::from([("fixture".to_string(), route(sink))])));

        let published = registry.get("fixture").expect("route is published");
        assert_eq!(
            published.owner(),
            &PluginWebhookOwner::new("fixture-plugin", "fixture")
        );
        assert_eq!(published.owner().plugin(), "fixture-plugin");
        assert_eq!(published.owner().channel_alias(), "fixture");
        assert!(!published.sink().is_closed());
    }

    #[test]
    fn plugin_webhook_path_grammar_is_one_bounded_segment() {
        for path in [
            "a",
            "Fixture_01",
            "a-b",
            &"x".repeat(MAX_PLUGIN_WEBHOOK_PATH_BYTES),
        ] {
            assert!(
                is_valid_plugin_webhook_path(path),
                "expected valid path: {path:?}"
            );
        }
        for path in [
            String::new(),
            "x".repeat(MAX_PLUGIN_WEBHOOK_PATH_BYTES + 1),
            "has.dot".to_string(),
            "has/slash".to_string(),
            "has space".to_string(),
            "control\n".to_string(),
            "unicode-λ".to_string(),
        ] {
            assert!(
                !is_valid_plugin_webhook_path(&path),
                "expected invalid path: {path:?}"
            );
        }
    }

    #[test]
    fn request_accepts_get_and_post_only() {
        for method in ["GET", "POST"] {
            let accepted = request(method, Vec::new()).expect("supported method");
            assert_eq!(accepted.method(), method);
        }
        for method in ["HEAD", "PUT", "get", ""] {
            assert_eq!(
                request(method, Vec::new()).unwrap_err(),
                PluginWebhookRequestError::UnsupportedMethod,
                "method {method:?}"
            );
        }
    }

    #[test]
    fn request_bounds_accept_the_limit_and_refuse_one_more() {
        let headers = |count: usize| {
            (0..count)
                .map(|index| header(&format!("x-h-{index}"), "v"))
                .collect::<Vec<_>>()
        };
        assert!(request("POST", headers(MAX_PLUGIN_WEBHOOK_HEADERS)).is_ok());
        assert_eq!(
            request("POST", headers(MAX_PLUGIN_WEBHOOK_HEADERS + 1)).unwrap_err(),
            PluginWebhookRequestError::TooManyHeaders
        );

        let sized = |bytes: usize| vec![header("x", &"v".repeat(bytes - 1))];
        assert!(request("POST", sized(MAX_PLUGIN_WEBHOOK_HEADER_BYTES)).is_ok());
        assert_eq!(
            request("POST", sized(MAX_PLUGIN_WEBHOOK_HEADER_BYTES + 1)).unwrap_err(),
            PluginWebhookRequestError::HeadersTooLarge
        );

        let query = |bytes: usize| {
            PluginWebhookRequest::new("fixture", "GET", "q".repeat(bytes), Vec::new(), Vec::new())
        };
        assert!(query(MAX_PLUGIN_WEBHOOK_QUERY_BYTES).is_ok());
        assert_eq!(
            query(MAX_PLUGIN_WEBHOOK_QUERY_BYTES + 1).unwrap_err(),
            PluginWebhookRequestError::QueryTooLarge
        );

        let body = |bytes: usize| {
            PluginWebhookRequest::new("fixture", "POST", "", Vec::new(), vec![b'b'; bytes])
        };
        assert!(body(MAX_PLUGIN_WEBHOOK_BODY_BYTES).is_ok());
        assert_eq!(
            body(MAX_PLUGIN_WEBHOOK_BODY_BYTES + 1).unwrap_err(),
            PluginWebhookRequestError::BodyTooLarge
        );
    }

    #[test]
    fn header_byte_limit_bounds_the_sum_over_all_headers() {
        const COUNT: usize = 4;
        // `COUNT` headers whose sizes add up to `total`, each well under the
        // limit on its own.
        let spread = |total: usize| {
            (0..COUNT)
                .map(|index| {
                    let name = format!("x-{index}");
                    let share = total / COUNT + usize::from(index < total % COUNT);
                    let value = "v".repeat(share - name.len());
                    (name, value)
                })
                .collect::<Vec<_>>()
        };

        let at_limit = spread(MAX_PLUGIN_WEBHOOK_HEADER_BYTES);
        let over_limit = spread(MAX_PLUGIN_WEBHOOK_HEADER_BYTES + 1);
        for headers in [&at_limit, &over_limit] {
            assert!(
                headers
                    .iter()
                    .all(|(name, value)| name.len() + value.len() < MAX_PLUGIN_WEBHOOK_HEADER_BYTES)
            );
        }
        let total = |headers: &[(String, String)]| {
            headers
                .iter()
                .map(|(name, value)| name.len() + value.len())
                .sum::<usize>()
        };
        assert_eq!(total(&at_limit), MAX_PLUGIN_WEBHOOK_HEADER_BYTES);
        assert_eq!(total(&over_limit), MAX_PLUGIN_WEBHOOK_HEADER_BYTES + 1);

        assert!(request("POST", at_limit).is_ok());
        assert_eq!(
            request("POST", over_limit).unwrap_err(),
            PluginWebhookRequestError::HeadersTooLarge
        );
    }

    #[test]
    fn request_normalizes_header_names_and_refuses_what_http_would_not_forward() {
        let accepted = request(
            "POST",
            vec![
                header("X-Sig", "v"),
                header("x-multi", "one"),
                header("X-Multi", "two"),
                header("x-tab", "a\tb"),
                header("x-visible", " !~ "),
                header("x-token!#$%&'*+-.^_`|~", "v"),
                header("X-\"Quoted\"", "v"),
                header(&"n".repeat(MAX_PLUGIN_WEBHOOK_HEADER_NAME_BYTES), "v"),
            ],
        )
        .expect("valid headers");
        assert_eq!(
            accepted.headers(),
            [
                header("x-sig", "v"),
                header("x-multi", "one"),
                header("x-multi", "two"),
                header("x-tab", "a\tb"),
                header("x-visible", " !~ "),
                header("x-token!#$%&'*+-.^_`|~", "v"),
                header("x-\"quoted\"", "v"),
                header(&"n".repeat(MAX_PLUGIN_WEBHOOK_HEADER_NAME_BYTES), "v"),
            ]
        );

        for refused in [
            header("", "v"),
            header(&"n".repeat(MAX_PLUGIN_WEBHOOK_HEADER_NAME_BYTES + 1), "v"),
            header("x(", "v"),
            header("x/", "v"),
            header("x\r", "v"),
            header("x\0", "v"),
            header("x y", "v"),
            header("x:y", "v"),
            header("caf\u{e9}", "v"),
            header("x", "a\nb"),
            header("x", "a\rb"),
            header("x", "a\0b"),
            header("x", "a\u{7f}b"),
            header("x", "caf\u{e9}"),
        ] {
            let shown = format!("{refused:?}");
            assert_eq!(
                request("POST", vec![header("x-ok", "v"), refused]).unwrap_err(),
                PluginWebhookRequestError::InvalidHeader,
                "header {shown}"
            );
        }
    }

    #[test]
    fn request_does_not_validate_the_path() {
        for path in ["not.a.route".to_string(), String::new(), "x".repeat(200)] {
            let accepted =
                PluginWebhookRequest::new(path.clone(), "POST", "", Vec::new(), Vec::new())
                    .expect("the path is resolved at dispatch");
            assert_eq!(accepted.path(), path);
        }
    }

    #[test]
    fn request_debug_shows_sizes_but_never_values() {
        let accepted = PluginWebhookRequest::new(
            "fixture",
            "POST",
            "token=query-secret",
            vec![header("x-signature", "header-secret")],
            b"body-secret".to_vec(),
        )
        .expect("valid request");
        let shown = format!("{accepted:?}");
        assert!(shown.contains("fixture"), "{shown}");
        assert!(shown.contains("POST"), "{shown}");
        for secret in [
            "query-secret",
            "header-secret",
            "x-signature",
            "body-secret",
        ] {
            assert!(!shown.contains(secret), "{shown} leaks {secret}");
        }
    }

    #[test]
    fn into_raw_webhook_moves_the_exact_request() {
        let accepted = PluginWebhookRequest::new(
            "fixture",
            "POST",
            "a=1&a=2",
            vec![header("X-A", "1"), header("x-a", "2")],
            b"\x00\xffraw".to_vec(),
        )
        .expect("valid request");
        let cancellation = WebhookCancellation::new();
        let (reply, _outcome) = oneshot::channel();
        let raw = accepted.into_raw_webhook(cancellation.clone(), None, reply);

        assert_eq!(raw.method, "POST");
        assert_eq!(raw.query, "a=1&a=2");
        assert_eq!(raw.headers, [header("x-a", "1"), header("x-a", "2")]);
        assert_eq!(raw.body, b"\x00\xffraw");
        assert!(raw.idempotency.is_none());
        assert!(!raw.cancellation.is_cancelled());
        cancellation.cancel();
        assert!(raw.cancellation.is_cancelled());
    }

    #[test]
    fn request_error_reasons_are_stable_log_keys() {
        for (error, reason) in [
            (
                PluginWebhookRequestError::UnsupportedMethod,
                "unsupported_method",
            ),
            (
                PluginWebhookRequestError::TooManyHeaders,
                "too_many_headers",
            ),
            (PluginWebhookRequestError::InvalidHeader, "invalid_header"),
            (
                PluginWebhookRequestError::HeadersTooLarge,
                "headers_too_large",
            ),
            (PluginWebhookRequestError::QueryTooLarge, "query_too_large"),
            (PluginWebhookRequestError::BodyTooLarge, "body_too_large"),
        ] {
            assert_eq!(error.reason(), reason);
        }
    }
}
