//! GitHub webhook triggers: a shared HTTP server that turns signed webhook POSTs into
//! [`TriggerEvent`]s feeding the daemon's pull-based [`Trigger`] loop.
//!
//! [`WebhookServer`] is push-side: it owns the HTTP listener and one
//! [`mpsc::Sender`](tokio::sync::mpsc) per [subscription](GithubWebhookDecl). Each
//! [`GithubWebhookTrigger`] is pull-side: it holds the matching receiver and yields events
//! from `next_event()`, so a webhook fits the same `Trigger` shape as cron — the daemon's
//! supervisor loop drives both identically.
//!
//! Wiring: `new()` → `subscribe()` (once per declared webhook trigger, returns the
//! [`GithubWebhookTrigger`] to register with the [`Daemon`](crate::Daemon)) → `bind()` →
//! `serve(shutdown)`. The server and the daemon share one [`CancellationToken`].
//!
//! ## Hardening
//!
//! - **Signatures**: HMAC-SHA256 over the raw body, fail-closed when a secret is configured.
//! - **Idempotency**: GitHub re-delivers on a non-2xx/timeout; recent `X-GitHub-Delivery`
//!   ids are remembered ([`DeliveryDedup`]) so a retry doesn't start a duplicate run.
//! - **Resource bounds**: a 25 MiB body cap, a bounded per-subscription queue (excess
//!   dropped + logged), and the daemon's `max_concurrent_runs` together bound the work a
//!   flood of *valid* deliveries can spawn — so HTTP-edge rate limiting is left to a fronting
//!   reverse proxy rather than reimplemented here.
//! - **TLS**: not built in — run behind a TLS-terminating reverse proxy (the server warns
//!   when bound to a non-loopback address over plain HTTP).

use std::collections::{HashSet, VecDeque};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use anyhow::Context as _;
use async_trait::async_trait;
use axum::Router;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use hmac::{Hmac, Mac};
use indexmap::IndexMap;
use odin_core::ir::GithubWebhookDecl;
use odin_core::traits::{Trigger, TriggerEvent};
use odin_core::{RunInput, TriggerError, WorkflowId};
use sha2::Sha256;
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

type HmacSha256 = Hmac<Sha256>;

/// Largest webhook body buffered. GitHub caps payloads at 25 MiB; most are far smaller.
const MAX_BODY_BYTES: usize = 25 * 1024 * 1024;
/// Per-subscription queue depth. Events beyond this (a slow run during a burst) are dropped
/// with a warning rather than applying unbounded back-pressure to the HTTP handler.
const QUEUE_DEPTH: usize = 64;
/// How many recent `X-GitHub-Delivery` ids to remember for retry de-duplication. GitHub
/// re-delivers on a non-2xx/timeout; redelivery happens within minutes, so a bounded recent
/// set is enough (it intentionally does not survive a daemon restart).
const DEDUP_CAPACITY: usize = 2048;

/// A bounded most-recent set of delivery ids (FIFO eviction) for idempotent webhook handling.
struct DeliveryDedup {
    seen: HashSet<String>,
    order: VecDeque<String>,
}

impl DeliveryDedup {
    fn new() -> Self {
        Self {
            seen: HashSet::new(),
            order: VecDeque::new(),
        }
    }

    /// Records `id` and returns `true` if it is new, `false` if it was already seen.
    fn check_and_record(&mut self, id: &str) -> bool {
        if self.seen.contains(id) {
            return false;
        }
        if self.order.len() >= DEDUP_CAPACITY {
            if let Some(evicted) = self.order.pop_front() {
                self.seen.remove(&evicted);
            }
        }
        self.seen.insert(id.to_owned());
        self.order.push_back(id.to_owned());
        true
    }
}

/// A parsed event matcher, e.g. `"issues.labeled"` → type `issues`, action `labeled`; a
/// bare `"issues"` → type `issues`, action `None` (matches any action on that type).
#[derive(Clone, Debug)]
struct EventSpec {
    event_type: String,
    action: Option<String>,
}

impl EventSpec {
    fn parse(spec: &str) -> Self {
        match spec.split_once('.') {
            Some((event_type, action)) => Self {
                event_type: event_type.to_owned(),
                action: Some(action.to_owned()),
            },
            None => Self {
                event_type: spec.to_owned(),
                action: None,
            },
        }
    }

    fn matches(&self, event_type: &str, action: Option<&str>) -> bool {
        if !self.event_type.eq_ignore_ascii_case(event_type) {
            return false;
        }
        match &self.action {
            None => true,
            Some(want) => action.is_some_and(|got| want.eq_ignore_ascii_case(got)),
        }
    }
}

/// One declared webhook trigger bound to a workflow, plus the channel feeding its
/// [`GithubWebhookTrigger`].
struct Subscription {
    workflow: WorkflowId,
    specs: Vec<EventSpec>,
    /// Lowercased `owner/repo` filter, if any.
    repo: Option<String>,
    /// Declared param → dot-path into the event payload.
    params: IndexMap<String, String>,
    tx: mpsc::Sender<TriggerEvent>,
}

impl Subscription {
    fn matches(
        &self,
        event_type: &str,
        action: Option<&str>,
        repo_full_name: Option<&str>,
    ) -> bool {
        if let Some(want) = &self.repo {
            // A repo-scoped subscription requires a payload repo that matches it.
            if !repo_full_name.is_some_and(|got| got.eq_ignore_ascii_case(want)) {
                return false;
            }
        }
        self.specs.iter().any(|s| s.matches(event_type, action))
    }
}

/// The pull-side handle for a webhook subscription: a [`Trigger`] that yields events the
/// [`WebhookServer`] routes to it.
pub struct GithubWebhookTrigger {
    rx: mpsc::Receiver<TriggerEvent>,
}

#[async_trait]
impl Trigger for GithubWebhookTrigger {
    // The trait fixes the return type to `&str`; the literal cannot be `&'static str`.
    #[allow(clippy::unnecessary_literal_bound)]
    fn kind(&self) -> &str {
        "github_webhook"
    }

    async fn next_event(&mut self) -> Result<Option<TriggerEvent>, TriggerError> {
        // `None` once every `Sender` is dropped — i.e. the server has shut down.
        Ok(self.rx.recv().await)
    }
}

/// Accumulates webhook subscriptions, then binds an HTTP server that routes signed GitHub
/// events to them. See the module-level documentation for the wiring sequence.
pub struct WebhookServer {
    addr: SocketAddr,
    secret: Option<String>,
    subscriptions: Vec<Subscription>,
}

impl WebhookServer {
    /// A server that will listen on `addr`. If `secret` is `Some`, every request must carry
    /// a valid `X-Hub-Signature-256`; if `None`, the server runs in **dev mode** and accepts
    /// unsigned requests (a warning is logged at [`serve`](BoundWebhookServer::serve)).
    #[must_use]
    pub fn new(addr: SocketAddr, secret: Option<String>) -> Self {
        Self {
            addr,
            secret,
            subscriptions: Vec::new(),
        }
    }

    /// Registers one declared webhook trigger and returns the [`GithubWebhookTrigger`] to
    /// hand to the [`Daemon`](crate::Daemon). Call once per `github_webhook` decl.
    pub fn subscribe(
        &mut self,
        decl: &GithubWebhookDecl,
        workflow: WorkflowId,
    ) -> GithubWebhookTrigger {
        let (tx, rx) = mpsc::channel(QUEUE_DEPTH);
        self.subscriptions.push(Subscription {
            workflow,
            specs: decl.events.iter().map(|e| EventSpec::parse(e)).collect(),
            repo: decl.repo.as_ref().map(|r| r.to_lowercase()),
            params: decl
                .params
                .iter()
                .map(|(k, v)| (k.as_str().to_owned(), v.clone()))
                .collect(),
            tx,
        });
        GithubWebhookTrigger { rx }
    }

    /// Number of registered subscriptions (one per `github_webhook` decl).
    #[must_use]
    pub fn subscription_count(&self) -> usize {
        self.subscriptions.len()
    }

    /// Whether any subscriptions were registered. The daemon skips starting the server when
    /// no workflow declares a webhook trigger.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.subscriptions.is_empty()
    }

    /// Binds the TCP listener (so the actual port is known before serving — useful for the
    /// startup log and for tests using an ephemeral `:0` port).
    ///
    /// # Errors
    /// Returns an error if the address cannot be bound (e.g. the port is in use).
    pub async fn bind(self) -> anyhow::Result<BoundWebhookServer> {
        let listener = TcpListener::bind(self.addr)
            .await
            .with_context(|| format!("binding webhook server to {}", self.addr))?;
        let local_addr = listener
            .local_addr()
            .context("reading webhook server local address")?;
        Ok(BoundWebhookServer {
            listener,
            local_addr,
            state: Arc::new(AppState {
                secret: self.secret,
                subscriptions: self.subscriptions,
                dedup: Mutex::new(DeliveryDedup::new()),
            }),
        })
    }
}

/// A [`WebhookServer`] with its listener bound; call [`serve`](Self::serve) to run it.
pub struct BoundWebhookServer {
    listener: TcpListener,
    local_addr: SocketAddr,
    state: Arc<AppState>,
}

impl BoundWebhookServer {
    /// The actual bound address (resolves an ephemeral `:0` port).
    #[must_use]
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Serves `POST /webhook` and `GET /health` until `shutdown` fires, draining in-flight
    /// requests on the way out.
    ///
    /// # Errors
    /// Returns an error if the server task fails (not from individual bad requests, which
    /// are answered with a 4xx and logged).
    pub async fn serve(self, shutdown: CancellationToken) -> anyhow::Result<()> {
        if self.state.secret.is_none() && !self.state.subscriptions.is_empty() {
            eprintln!(
                "odind: WARNING no webhook secret set (ODIN_WEBHOOK_SECRET / --webhook-secret); \
                 accepting UNSIGNED webhook requests — dev mode only"
            );
        }
        let app = Router::new()
            .route("/webhook", post(handle_webhook))
            .route("/health", get(handle_health))
            .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
            .with_state(self.state);
        axum::serve(self.listener, app)
            .with_graceful_shutdown(async move { shutdown.cancelled().await })
            .await
            .context("webhook server error")?;
        Ok(())
    }
}

/// Shared handler state.
struct AppState {
    secret: Option<String>,
    subscriptions: Vec<Subscription>,
    /// Recent delivery ids, to drop GitHub's retries of an already-handled delivery.
    dedup: Mutex<DeliveryDedup>,
}

#[allow(clippy::unused_async)] // axum route handlers must be async.
async fn handle_health() -> impl IntoResponse {
    (StatusCode::OK, "ok")
}

/// Verifies the signature, parses the event, and routes it to every matching subscription.
/// Always answers `2xx` once accepted (GitHub treats non-2xx as a delivery failure and
/// retries); routing/queue problems are logged, not surfaced to GitHub.
async fn handle_webhook(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Some(rejection) = verify_signature(state.secret.as_deref(), &headers, &body) {
        return rejection;
    }

    let payload = match serde_json::from_slice::<serde_json::Value>(&body) {
        Ok(value) => value,
        Err(e) => {
            eprintln!("odind: webhook: rejecting invalid JSON body: {e}");
            return (StatusCode::BAD_REQUEST, "invalid JSON body").into_response();
        }
    };

    let Some(event_type) = header_str(&headers, "x-github-event") else {
        return (StatusCode::BAD_REQUEST, "missing X-GitHub-Event").into_response();
    };
    let action = payload.get("action").and_then(serde_json::Value::as_str);
    let repo_full_name = payload
        .get("repository")
        .and_then(|r| r.get("full_name"))
        .and_then(serde_json::Value::as_str);
    let delivery = header_str(&headers, "x-github-delivery").unwrap_or("?");
    let label = event_label(event_type, action);

    // Idempotency: GitHub re-delivers on timeout/error. Drop a delivery we've already
    // handled (a verified, identified one) so a retry doesn't start a duplicate run.
    if delivery != "?" && !state.dedup.lock().unwrap().check_and_record(delivery) {
        eprintln!("odind: webhook: duplicate delivery {delivery} ignored");
        return (StatusCode::OK, "duplicate delivery ignored").into_response();
    }

    let mut matched = 0_usize;
    for sub in &state.subscriptions {
        if sub.matches(event_type, action, repo_full_name) {
            matched += 1;
            let input = build_input(&payload, &sub.params);
            let event = TriggerEvent::new(
                format!("github_webhook:{label}"),
                sub.workflow.clone(),
                input,
            );
            if let Err(e) = sub.tx.try_send(event) {
                eprintln!(
                    "odind: webhook: dropping {label} event for {} ({e})",
                    sub.workflow.as_str()
                );
            }
        }
    }
    eprintln!("odind: webhook: {label} delivery={delivery} → matched {matched} subscription(s)");
    (StatusCode::ACCEPTED, format!("accepted; matched {matched}")).into_response()
}

/// Verifies `X-Hub-Signature-256` against the raw body. Returns `None` to accept (a
/// configured secret matched, or none is configured — dev mode) or `Some(response)` with the
/// rejection. (Returning an `Option` rather than a `Result<(), Response>` keeps the large
/// axum `Response` out of an `Err` variant — `clippy::result_large_err`.)
fn verify_signature(secret: Option<&str>, headers: &HeaderMap, body: &[u8]) -> Option<Response> {
    let secret = secret?; // dev mode: accept unsigned (warned about at startup).
    let Some(header) = header_str(headers, "x-hub-signature-256") else {
        eprintln!("odind: webhook: rejecting unsigned request (a secret is configured)");
        return Some((StatusCode::BAD_REQUEST, "missing signature").into_response());
    };
    let Some(hex_sig) = header.strip_prefix("sha256=") else {
        return Some((StatusCode::BAD_REQUEST, "malformed signature").into_response());
    };
    let Ok(expected) = hex::decode(hex_sig) else {
        return Some((StatusCode::BAD_REQUEST, "malformed signature").into_response());
    };
    // HMAC accepts a key of any length, so `new_from_slice` cannot fail here.
    let mut mac =
        HmacSha256::new_from_slice(secret.as_bytes()).expect("HMAC key length is flexible");
    mac.update(body);
    if mac.verify_slice(&expected).is_err() {
        eprintln!("odind: webhook: rejecting request with invalid signature");
        return Some((StatusCode::UNAUTHORIZED, "invalid signature").into_response());
    }
    None
}

/// Builds the run input: the full event as `trigger_payload` (reachable as `trigger.*`),
/// plus any declared params extracted from the payload by dot-path.
fn build_input(payload: &serde_json::Value, params: &IndexMap<String, String>) -> RunInput {
    let mut input = RunInput::manual().with_trigger("github_webhook", payload.clone());
    for (name, path) in params {
        if let Some(value) = extract_path(payload, path) {
            input.params.insert(name.clone(), value);
        }
    }
    input
}

/// Resolves a dot-path (`issue.html_url`) into a JSON object tree. Object keys only — array
/// indices are not supported in v1.
fn extract_path(value: &serde_json::Value, path: &str) -> Option<serde_json::Value> {
    let mut current = value;
    for segment in path.split('.') {
        current = current.get(segment)?;
    }
    Some(current.clone())
}

fn header_str<'h>(headers: &'h HeaderMap, name: &str) -> Option<&'h str> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.is_empty())
}

fn event_label(event_type: &str, action: Option<&str>) -> String {
    match action {
        Some(action) => format!("{event_type}.{action}"),
        None => event_type.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::{EventSpec, HmacSha256, Subscription, build_input, extract_path, verify_signature};
    use axum::http::{HeaderMap, HeaderValue};
    use hmac::Mac as _;
    use indexmap::IndexMap;
    use odin_core::WorkflowId;
    use tokio::sync::mpsc;

    fn sub(events: &[&str], repo: Option<&str>) -> Subscription {
        let (tx, _rx) = mpsc::channel(4);
        Subscription {
            workflow: WorkflowId::new("w"),
            specs: events.iter().map(|e| EventSpec::parse(e)).collect(),
            repo: repo.map(str::to_lowercase),
            params: IndexMap::new(),
            tx,
        }
    }

    #[test]
    fn event_spec_exact_and_wildcard() {
        let labeled = EventSpec::parse("issues.labeled");
        assert!(labeled.matches("issues", Some("labeled")));
        assert!(!labeled.matches("issues", Some("opened")));
        assert!(!labeled.matches("issues", None));
        assert!(!labeled.matches("pull_request", Some("labeled")));

        let any_issue = EventSpec::parse("issues");
        assert!(any_issue.matches("issues", Some("labeled")));
        assert!(any_issue.matches("issues", Some("opened")));
        assert!(any_issue.matches("issues", None)); // e.g. an actionless event
        assert!(!any_issue.matches("push", None));
    }

    #[test]
    fn event_matching_is_case_insensitive() {
        let spec = EventSpec::parse("Issues.Labeled");
        assert!(spec.matches("issues", Some("labeled")));
    }

    #[test]
    fn subscription_repo_filter() {
        let scoped = sub(&["issues.labeled"], Some("marlboro-red/Odin"));
        // Case-insensitive match on the repo.
        assert!(scoped.matches("issues", Some("labeled"), Some("Marlboro-Red/odin")));
        // Wrong repo is filtered out.
        assert!(!scoped.matches("issues", Some("labeled"), Some("someone/else")));
        // A repo-scoped subscription requires a repo in the payload.
        assert!(!scoped.matches("issues", Some("labeled"), None));

        // An unscoped subscription accepts any repo (or none).
        let any = sub(&["issues.labeled"], None);
        assert!(any.matches("issues", Some("labeled"), Some("any/repo")));
        assert!(any.matches("issues", Some("labeled"), None));
    }

    #[test]
    fn extract_path_walks_objects() {
        let payload = serde_json::json!({
            "issue": { "html_url": "https://x/1", "user": { "login": "octocat" } },
            "action": "labeled",
        });
        assert_eq!(
            extract_path(&payload, "issue.html_url"),
            Some(serde_json::json!("https://x/1"))
        );
        assert_eq!(
            extract_path(&payload, "issue.user.login"),
            Some(serde_json::json!("octocat"))
        );
        assert_eq!(extract_path(&payload, "issue.missing"), None);
        assert_eq!(extract_path(&payload, "nope.at.all"), None);
    }

    #[test]
    fn build_input_maps_declared_params_and_keeps_payload() {
        let payload = serde_json::json!({ "issue": { "html_url": "https://x/7" } });
        let mut params = IndexMap::new();
        params.insert("issue_url".to_owned(), "issue.html_url".to_owned());
        params.insert("missing".to_owned(), "nope".to_owned());

        let input = build_input(&payload, &params);
        assert_eq!(input.trigger, "github_webhook");
        assert_eq!(input.trigger_payload, payload);
        assert_eq!(input.params["issue_url"], serde_json::json!("https://x/7"));
        // An unresolvable path is simply skipped (the run later fails validation if required).
        assert!(!input.params.contains_key("missing"));
    }

    fn sign(secret: &str, body: &[u8]) -> String {
        let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(body);
        format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
    }

    #[test]
    fn signature_required_and_verified_when_secret_is_set() {
        let body = br#"{"action":"labeled"}"#;
        let secret = "s3cr3t";

        // Valid signature passes (None == accepted).
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-hub-signature-256",
            HeaderValue::from_str(&sign(secret, body)).unwrap(),
        );
        assert!(verify_signature(Some(secret), &headers, body).is_none());

        // Wrong signature is rejected (Some == a rejection response).
        let mut bad = HeaderMap::new();
        bad.insert(
            "x-hub-signature-256",
            HeaderValue::from_str(&sign("wrong", body)).unwrap(),
        );
        assert!(verify_signature(Some(secret), &bad, body).is_some());

        // Missing header is rejected when a secret is configured.
        assert!(verify_signature(Some(secret), &HeaderMap::new(), body).is_some());

        // Tampered body fails against a signature for the original body.
        assert!(verify_signature(Some(secret), &headers, b"tampered").is_some());
    }

    #[test]
    fn dev_mode_accepts_unsigned() {
        // No secret configured → unsigned requests are accepted.
        assert!(verify_signature(None, &HeaderMap::new(), b"anything").is_none());
    }
}
