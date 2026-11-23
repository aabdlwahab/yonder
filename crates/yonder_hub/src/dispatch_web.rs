use std::fs::File;
use std::io::{BufReader, ErrorKind};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use axum::body::Body;
use axum::extract::ws::rejection::WebSocketUpgradeRejection;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{DefaultBodyLimit, Path as RequestPath, State};
use axum::http::{HeaderMap, HeaderValue, Request, StatusCode, Uri, header};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse as _, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::StreamExt as _;
use futures::channel::{mpsc, oneshot};
use rustls::ServerConfig;
use serde::Deserialize;
use serde_json::{Value, json};
use subtle::ConstantTimeEq as _;
use tokio::net::TcpListener;
use tokio::time::{Instant, sleep, timeout};
use tokio_rustls::TlsAcceptor;

use crate::dispatch::VoiceCall;
use crate::push::{Kinds, Push, Subscription};
use crate::server::{Event, WebRequest};

const MAX_OFFER_BYTES: usize = 32 * 1024;
const CALL_START_TIMEOUT: Duration = Duration::from_secs(120);
const CALL_IDLE_TIMEOUT: Duration = Duration::from_secs(120);
const COOKIE: &str = "__Host-yonder-dispatch";
const PAGE: &str = include_str!("dispatch_web/index.html");
const STYLE: &str = include_str!("dispatch_web/dispatch.css");
const SCRIPT: &str = include_str!("dispatch_web/dispatch.js");
const MANIFEST: &str = include_str!("dispatch_web/manifest.webmanifest");
const ICON_180: &[u8] = include_bytes!("dispatch_web/icon-180.png");
const ICON_192: &[u8] = include_bytes!("dispatch_web/icon-192.png");
const ICON_512: &[u8] = include_bytes!("dispatch_web/icon-512.png");
const SERVICE_WORKER: &str = include_str!("dispatch_web/sw.js");
/// Long enough for a reply to wait out a thread that's starting.
const WEB_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(clap::Args)]
pub struct Options {
    /// Bind phone Dispatch to the hub's Tailscale IP and an HTTPS port.
    #[arg(long, requires_all = ["dispatch_codex", "dispatch_web_origin", "dispatch_web_directory"])]
    dispatch_web_listen: Option<SocketAddr>,
    /// The HTTPS origin for phone Dispatch, including the port if nonstandard.
    #[arg(long, requires = "dispatch_web_listen")]
    dispatch_web_origin: Option<String>,
    /// Private directory containing cert.pem, key.pem, and pairing-code.
    #[arg(long, requires = "dispatch_web_listen")]
    dispatch_web_directory: Option<PathBuf>,
}

impl Options {
    pub fn enabled(&self) -> bool {
        self.dispatch_web_listen.is_some()
    }

    /// Phone notifications, which keep their key next to the TLS files.
    pub fn load_push(&self) -> Option<Result<Push>> {
        let directory = self.dispatch_web_directory.as_ref()?;
        let origin = self.dispatch_web_origin.clone()?;
        Some(Push::load(directory, origin))
    }
}

#[derive(Clone)]
struct WebState {
    pairing_code: Arc<str>,
    origin: String,
    authority: String,
    content_security_policy: HeaderValue,
    events: mpsc::UnboundedSender<Event>,
}

pub async fn serve(options: Options, events: mpsc::UnboundedSender<Event>) -> Result<()> {
    let address = options
        .dispatch_web_listen
        .context("missing Dispatch listen address")?;
    let origin = options
        .dispatch_web_origin
        .context("missing Dispatch origin")?;
    let directory = options
        .dispatch_web_directory
        .context("missing Dispatch TLS directory")?;
    let authority = https_authority(&origin)?;
    if address.ip().is_unspecified() || authority.port_u16().unwrap_or(443) != address.port() {
        bail!("Dispatch needs an explicit listen IP and matching HTTPS port");
    }
    let code_path = directory.join("pairing-code");
    let pairing_code = std::fs::read_to_string(&code_path)
        .with_context(|| format!("reading {}", code_path.display()))?;
    let pairing_code = pairing_code.trim();
    if pairing_code.len() != 64 || !pairing_code.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("Dispatch pairing-code must contain 64 hexadecimal digits");
    }
    let content_security_policy = HeaderValue::from_str(&format!(
        concat!(
            "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self'; ",
            "manifest-src 'self'; worker-src 'self'; media-src 'self' blob:; ",
            "connect-src 'self' wss://{}; frame-ancestors 'none'; base-uri 'none'; form-action 'none'"
        ),
        authority
    ))?;
    let state = WebState {
        pairing_code: Arc::from(pairing_code),
        origin,
        authority: authority.to_string(),
        content_security_policy,
        events,
    };
    let acceptor = tls_acceptor(&directory)?;
    let listener = loop {
        match TcpListener::bind(address).await {
            Ok(listener) => break listener,
            // A user service can start at boot before Tailscale assigns its IP.
            Err(error) if error.kind() == ErrorKind::AddrNotAvailable => {
                log::debug!("waiting for {address} to listen for Dispatch: {error}");
                sleep(Duration::from_secs(5)).await;
            }
            Err(error) => {
                return Err(error).with_context(|| format!("listening for Dispatch on {address}"));
            }
        }
    };
    log::info!("Phone Dispatch listening at {}/dispatch", state.origin);
    let router = router(state);
    loop {
        let stream = match listener.accept().await {
            Ok((stream, _)) => stream,
            Err(error) => {
                log::warn!("accepting a Dispatch connection: {error}");
                sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        let acceptor = acceptor.clone();
        let router = router.clone();
        tokio::spawn(async move {
            let Ok(Ok(stream)) = timeout(Duration::from_secs(10), acceptor.accept(stream)).await
            else {
                return;
            };
            if let Err(error) = hyper::server::conn::Http::new()
                .http1_only(true)
                .serve_connection(stream, router)
                .with_upgrades()
                .await
            {
                log::debug!("Dispatch web connection: {error}");
            }
        });
    }
}

fn tls_acceptor(directory: &Path) -> Result<TlsAcceptor> {
    let open = |name| -> Result<_> {
        let path = directory.join(name);
        let file = File::open(&path).with_context(|| format!("opening {}", path.display()))?;
        Ok(BufReader::new(file))
    };
    let certificates =
        rustls_pemfile::certs(&mut open("cert.pem")?).collect::<std::io::Result<_>>()?;
    let key = rustls_pemfile::private_key(&mut open("key.pem")?)?
        .context("no private key in Dispatch key.pem")?;
    let tls =
        ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()?
            .with_no_client_auth()
            .with_single_cert(certificates, key)?;
    Ok(TlsAcceptor::from(Arc::new(tls)))
}

fn https_authority(origin: &str) -> Result<axum::http::uri::Authority> {
    let uri: Uri = origin.parse().context("invalid Dispatch web origin")?;
    let authority = uri.authority().context("Dispatch origin needs a host")?;
    if uri.scheme_str() != Some("https")
        || origin != format!("https://{authority}")
        || authority.as_str().contains('@')
    {
        bail!("Dispatch web origin must be an HTTPS origin without a path");
    }
    Ok(authority.clone())
}

fn router(state: WebState) -> Router {
    let protected = Router::new()
        .route("/dispatch/sessions", get(sessions))
        .route("/dispatch/sessions/:id", get(history))
        .route("/dispatch/call", get(call))
        .route("/dispatch/push/key", get(push_key))
        .route("/dispatch/push/subscribe", post(push_subscribe))
        .route("/dispatch/push/unsubscribe", post(push_unsubscribe))
        .route("/dispatch/push/test", post(push_test))
        .route("/dispatch/sessions/:id/approval", post(answer_approval))
        .route("/dispatch/sessions/:id/reply", post(reply))
        .layer(DefaultBodyLimit::max(16 * 1024))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            require_pairing,
        ));
    Router::new()
        .route("/dispatch", get(|| async { Html(PAGE) }))
        .route("/dispatch/", get(|| async { Html(PAGE) }))
        .route(
            "/dispatch/dispatch.css",
            get(|| async { asset("text/css; charset=utf-8", STYLE) }),
        )
        .route(
            "/dispatch/dispatch.js",
            get(|| async { asset("text/javascript; charset=utf-8", SCRIPT) }),
        )
        .route(
            "/dispatch/manifest.webmanifest",
            get(|| async { asset("application/manifest+json", MANIFEST) }),
        )
        .route("/dispatch/icon-180.png", get(|| async { image(ICON_180) }))
        .route("/dispatch/icon-192.png", get(|| async { image(ICON_192) }))
        .route("/dispatch/icon-512.png", get(|| async { image(ICON_512) }))
        .route("/dispatch/sw.js", get(service_worker))
        .route(
            "/dispatch/pair",
            post(pair).layer(DefaultBodyLimit::max(256)),
        )
        .merge(protected)
        .layer(middleware::from_fn_with_state(
            state.clone(),
            secure_headers,
        ))
        .with_state(state)
}

fn same_origin(headers: &HeaderMap, state: &WebState) -> bool {
    headers
        .get(header::ORIGIN)
        .and_then(|origin| origin.to_str().ok())
        == Some(&state.origin)
}

fn matches_code(input: &str, state: &WebState) -> bool {
    bool::from(input.as_bytes().ct_eq(state.pairing_code.as_bytes()))
}

async fn require_pairing(
    State(state): State<WebState>,
    request: Request<Body>,
    next: Next<Body>,
) -> Response {
    let paired = request
        .headers()
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|header| header.to_str().ok())
        .flat_map(|cookies| cookies.split(';'))
        .filter_map(|cookie| cookie.trim().split_once('='))
        .any(|(name, value)| name == COOKIE && matches_code(value, &state));
    if !paired {
        return (
            StatusCode::UNAUTHORIZED,
            "Pair this browser with the hub to use Dispatch.",
        )
            .into_response();
    }
    next.run(request).await
}

async fn pair(State(state): State<WebState>, headers: HeaderMap, body: String) -> Response {
    if !same_origin(&headers, &state) {
        return (
            StatusCode::FORBIDDEN,
            "Open Dispatch from its configured HTTPS address",
        )
            .into_response();
    }
    if !matches_code(body.trim(), &state) {
        return (
            StatusCode::UNAUTHORIZED,
            "That pairing code does not match this hub.",
        )
            .into_response();
    }
    // A session cookie would unpair the Home Screen app every time iOS closes it
    // in the background. 400 days is the longest lifetime browsers accept.
    let cookie = format!(
        "{COOKIE}={}; Path=/; Max-Age=34560000; Secure; HttpOnly; SameSite=Strict",
        state.pairing_code
    );
    (StatusCode::NO_CONTENT, [(header::SET_COOKIE, cookie)]).into_response()
}

async fn secure_headers(
    State(state): State<WebState>,
    request: Request<Body>,
    next: Next<Body>,
) -> Response {
    if request
        .headers()
        .get(header::HOST)
        .and_then(|host| host.to_str().ok())
        != Some(&state.authority)
    {
        return StatusCode::MISDIRECTED_REQUEST.into_response();
    }
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        state.content_security_policy,
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(
        "permissions-policy",
        HeaderValue::from_static("microphone=(self)"),
    );
    response
}

fn asset(content_type: &'static str, text: &'static str) -> Response {
    ([(header::CONTENT_TYPE, content_type)], text).into_response()
}

fn image(bytes: &'static [u8]) -> Response {
    ([(header::CONTENT_TYPE, "image/png")], bytes).into_response()
}

/// The service worker shows notifications. It's allowed to cover `/dispatch`
/// as well as `/dispatch/`, since the Home Screen app starts at the former.
async fn service_worker() -> Response {
    (
        [
            (header::CONTENT_TYPE, "text/javascript; charset=utf-8"),
            (
                header::HeaderName::from_static("service-worker-allowed"),
                "/dispatch",
            ),
        ],
        SERVICE_WORKER,
    )
        .into_response()
}

#[derive(Deserialize)]
struct BrowserSubscription {
    endpoint: String,
    keys: BrowserKeys,
}

#[derive(Deserialize)]
struct BrowserKeys {
    p256dh: String,
    auth: String,
}

#[derive(Deserialize)]
struct SubscribeBody {
    subscription: BrowserSubscription,
    #[serde(default)]
    kinds: Kinds,
}

#[derive(Deserialize)]
struct EndpointBody {
    endpoint: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ApprovalBody {
    approval_id: String,
    decision: String,
}

#[derive(Deserialize)]
struct ReplyBody {
    text: String,
}

async fn push_key(State(state): State<WebState>) -> Response {
    web(&state, WebRequest::PushKey).await
}

async fn push_subscribe(
    State(state): State<WebState>,
    headers: HeaderMap,
    Json(body): Json<SubscribeBody>,
) -> Response {
    if !same_origin(&headers, &state) {
        return forbidden();
    }
    let subscription = Subscription {
        endpoint: body.subscription.endpoint,
        p256dh: body.subscription.keys.p256dh,
        auth: body.subscription.keys.auth,
        kinds: body.kinds,
    };
    web(&state, WebRequest::Subscribe(subscription)).await
}

async fn push_unsubscribe(
    State(state): State<WebState>,
    headers: HeaderMap,
    Json(body): Json<EndpointBody>,
) -> Response {
    if !same_origin(&headers, &state) {
        return forbidden();
    }
    let endpoint = body.endpoint;
    web(&state, WebRequest::Unsubscribe { endpoint }).await
}

async fn push_test(
    State(state): State<WebState>,
    headers: HeaderMap,
    Json(body): Json<EndpointBody>,
) -> Response {
    if !same_origin(&headers, &state) {
        return forbidden();
    }
    let endpoint = body.endpoint;
    web(&state, WebRequest::TestPush { endpoint }).await
}

async fn answer_approval(
    State(state): State<WebState>,
    RequestPath(session): RequestPath<String>,
    headers: HeaderMap,
    Json(body): Json<ApprovalBody>,
) -> Response {
    if !same_origin(&headers, &state) {
        return forbidden();
    }
    let approve = match body.decision.as_str() {
        "approve" => true,
        "reject" => false,
        _ => return (StatusCode::BAD_REQUEST, "Answer approve or reject").into_response(),
    };
    let approval = body.approval_id;
    web(
        &state,
        WebRequest::Answer {
            session,
            approval,
            approve,
        },
    )
    .await
}

async fn reply(
    State(state): State<WebState>,
    RequestPath(session): RequestPath<String>,
    headers: HeaderMap,
    Json(body): Json<ReplyBody>,
) -> Response {
    if !same_origin(&headers, &state) {
        return forbidden();
    }
    let text = body.text;
    web(&state, WebRequest::Reply { session, text }).await
}

fn forbidden() -> Response {
    (
        StatusCode::FORBIDDEN,
        "Open Dispatch from its configured HTTPS address",
    )
        .into_response()
}

/// Hands a request to the hub and returns its answer.
async fn web(state: &WebState, request: WebRequest) -> Response {
    let (reply, response) = oneshot::channel();
    if state
        .events
        .unbounded_send(Event::Web { request, reply })
        .is_ok()
        && let Ok(Ok(result)) = timeout(WEB_REQUEST_TIMEOUT, response).await
    {
        return match result {
            Ok(data) => Json(data).into_response(),
            Err(message) => (StatusCode::UNPROCESSABLE_ENTITY, message).into_response(),
        };
    }
    (
        StatusCode::SERVICE_UNAVAILABLE,
        "The Yonder hub is unavailable",
    )
        .into_response()
}

async fn sessions(State(state): State<WebState>) -> Response {
    session_data(state, None).await
}

async fn history(State(state): State<WebState>, RequestPath(id): RequestPath<String>) -> Response {
    session_data(state, Some(id)).await
}

async fn session_data(state: WebState, id: Option<String>) -> Response {
    let (reply, response) = oneshot::channel();
    if state
        .events
        .unbounded_send(Event::DispatchSessions { id, reply })
        .is_ok()
        && let Ok(Ok(data)) = timeout(Duration::from_secs(5), response).await
    {
        if data.is_null() {
            return (StatusCode::NOT_FOUND, "Thread unavailable").into_response();
        }
        return Json(data).into_response();
    }
    (
        StatusCode::SERVICE_UNAVAILABLE,
        "The Yonder hub is unavailable",
    )
        .into_response()
}

async fn call(
    State(state): State<WebState>,
    headers: HeaderMap,
    upgrade: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
) -> Response {
    // Reject cross-site sockets before the upgrade extractor can answer first.
    if !same_origin(&headers, &state) {
        return (
            StatusCode::FORBIDDEN,
            "Open Dispatch from its configured HTTPS address",
        )
            .into_response();
    }
    let upgrade = match upgrade {
        Ok(upgrade) => upgrade,
        Err(rejection) => return rejection.into_response(),
    };
    upgrade
        .max_message_size(MAX_OFFER_BYTES)
        .max_frame_size(MAX_OFFER_BYTES)
        .on_upgrade(|socket| voice_socket(socket, state.events))
}

#[derive(Deserialize)]
struct Offer {
    sdp: String,
}

async fn voice_socket(mut socket: WebSocket, events: mpsc::UnboundedSender<Event>) {
    let Ok(Some(Ok(Message::Text(message)))) =
        timeout(Duration::from_secs(15), socket.recv()).await
    else {
        return;
    };
    let Ok(Offer { sdp }) = serde_json::from_str::<Offer>(&message) else {
        send_update(
            &mut socket,
            json!({"type": "error", "message": "Expected a WebRTC offer"}),
        )
        .await;
        return;
    };
    if !sdp.starts_with("v=0\r\n") {
        send_update(
            &mut socket,
            json!({"type": "error", "message": "Invalid WebRTC offer"}),
        )
        .await;
        return;
    }
    let (updates, mut messages) = mpsc::channel(32);
    let (hangup, closed) = oneshot::channel();
    if events
        .unbounded_send(Event::Voice(VoiceCall {
            offer: sdp,
            updates,
            hangup: closed,
        }))
        .is_err()
    {
        send_update(
            &mut socket,
            json!({"type": "error", "message": "The Yonder hub is unavailable"}),
        )
        .await;
        return;
    }
    log::info!("Dispatch voice call started from the phone");
    let deadline = sleep(CALL_START_TIMEOUT);
    tokio::pin!(deadline);
    let mut answered = false;
    let ended = loop {
        tokio::select! {
            incoming = socket.recv() => match incoming {
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => continue,
                Some(Ok(Message::Close(_))) | None => break "the phone hung up",
                Some(Ok(_)) => break "the phone sent an unexpected message",
                Some(Err(error)) => {
                    log::info!("Dispatch call socket error: {error}");
                    break "the phone's connection dropped";
                }
            },
            update = messages.next() => {
                let Some(update) = update else { break "Dispatch ended the call"; };
                if update["type"] == "answer" {
                    answered = true;
                }
                if answered && matches!(update["type"].as_str(), Some("answer" | "caption" | "transcript")) {
                    deadline.as_mut().reset(Instant::now() + CALL_IDLE_TIMEOUT);
                }
                if !send_update(&mut socket, update).await {
                    break "the phone stopped receiving updates";
                }
            },
            () = &mut deadline => {
                let update = if answered {
                    json!({"type": "idle"})
                } else {
                    json!({"type": "error", "message": "Dispatch took too long to connect"})
                };
                send_update(&mut socket, update).await;
                break if answered { "the call was quiet for two minutes" } else { "Dispatch took too long to connect" };
            },
        }
    };
    log::info!("Dispatch voice call ended: {ended}");
    if hangup.send(()).is_err() {
        log::trace!("Dispatch voice already ended");
    }
    // A close frame lets the page tell an ended call from a dropped connection.
    if !matches!(
        timeout(Duration::from_secs(2), socket.send(Message::Close(None))).await,
        Ok(Ok(()))
    ) {
        log::debug!("the phone's call socket was already closed");
    }
}

async fn send_update(socket: &mut WebSocket, update: Value) -> bool {
    matches!(
        timeout(
            Duration::from_secs(5),
            socket.send(Message::Text(update.to_string()))
        )
        .await,
        Ok(Ok(()))
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use hyper::service::Service as _;

    #[tokio::test]
    async fn pairing_protects_thread_data_and_voice_signaling() -> Result<()> {
        let (events, mut hub) = mpsc::unbounded();
        let origin = "https://hub.example.test:8443";
        let host = "hub.example.test:8443";
        let code = "a".repeat(64);
        let mut app = router(WebState {
            pairing_code: Arc::from(code.as_str()),
            origin: origin.into(),
            authority: host.into(),
            content_security_policy: HeaderValue::from_static("default-src 'none'"),
            events,
        });
        let request = |path| Request::builder().uri(path).header(header::HOST, host);
        let response = app
            .call(
                request("/dispatch/sessions")
                    .header("tailscale-user-login", "ignored@example.test")
                    .body(Body::empty())?,
            )
            .await?;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

        for (path, content_type) in [
            (
                "/dispatch/manifest.webmanifest",
                "application/manifest+json",
            ),
            ("/dispatch/icon-180.png", "image/png"),
        ] {
            let response = app.call(request(path).body(Body::empty())?).await?;
            assert_eq!(response.status(), StatusCode::OK, "{path}");
            assert_eq!(response.headers()[header::CONTENT_TYPE], content_type);
        }

        let pair_request = |request_origin: &str| {
            request("/dispatch/pair")
                .method("POST")
                .header(header::ORIGIN, request_origin)
                .body(Body::from(code.clone()))
        };
        let response = app
            .call(pair_request("https://other.example.test")?)
            .await?;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let response = app.call(pair_request(origin)?).await?;
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        let cookie = response.headers()[header::SET_COOKIE].to_str()?;
        for attribute in ["Secure", "HttpOnly", "SameSite=Strict", "Max-Age="] {
            assert!(cookie.contains(attribute));
        }
        let pair_cookie = cookie.split(';').next().context("missing pairing cookie")?;

        let get_sessions = app.call(
            request("/dispatch/sessions")
                .header(header::COOKIE, pair_cookie)
                .body(Body::empty())?,
        );
        let answer_sessions = async {
            let Some(Event::DispatchSessions { id: None, reply }) = hub.next().await else {
                bail!("expected a catalog request");
            };
            reply
                .send(json!({ "sessions": [], "total": 0 }))
                .map_err(|_| anyhow::anyhow!("catalog request was cancelled"))
        };
        let (response, answered) = tokio::join!(get_sessions, answer_sessions);
        answered?;
        assert_eq!(response?.status(), StatusCode::OK);

        let websocket_request = request("/dispatch/call")
            .header(header::COOKIE, pair_cookie)
            .header(header::CONNECTION, "Upgrade")
            .header(header::UPGRADE, "websocket")
            .header(header::SEC_WEBSOCKET_VERSION, "13")
            .header(header::SEC_WEBSOCKET_KEY, "dGhlIHNhbXBsZSBub25jZQ==")
            .header(header::ORIGIN, "https://other.example.test")
            .body(Body::empty())?;
        assert_eq!(
            app.call(websocket_request).await?.status(),
            StatusCode::FORBIDDEN
        );
        Ok(())
    }
}
