use std::{
    future::{ready, Future, Ready},
    net::SocketAddr,
    pin::Pin,
    str::FromStr,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    task::{Context, Poll},
};

use axum::{
    body::Body,
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        ConnectInfo, Path, Query, State,
    },
    http::{header, HeaderMap, HeaderValue, Response, StatusCode},
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use futures_util::{SinkExt, StreamExt};
use rand::random;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::{
    io::{duplex, split, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf},
    net::TcpListener,
    sync::mpsc,
    task::{self, JoinHandle},
    time::{timeout, Duration, Instant, Sleep},
};
use tokio_rustls::rustls::ServerConfig;
use tokio_tungstenite::{
    connect_async_tls_with_config, tungstenite::Message as TungsteniteMessage, Connector,
    MaybeTlsStream, WebSocketStream,
};
use url::{Host, Url};

use conch_core::{
    client::ClientReply,
    frame,
    ticket::{JoinRole, Ticket},
    types::{FloorConfig, Hash32, RoomId, StakePolicy},
};

use crate::tcp::{
    read_frame, ConnectionGuard, ConnectionProtocol, Daemon, DaemonError, TransportMode,
};

const BRIDGE_CAPACITY: usize = 64 * 1024;
const PREAUTH_MESSAGE_BYTES: usize = 64 * 1024;
const MAX_QUEUE_BYTES: usize = 64 * 1024 * 1024;
const MAX_QUEUE_FRAMES: usize = 64;
const HTTP_IO_TIMEOUT: Duration = Duration::from_secs(30);
const HTTP_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone)]
struct HttpState {
    daemon: Daemon,
    secure: bool,
    operator: bool,
}

pub struct RunningHttpServer {
    addr: SocketAddr,
    task: JoinHandle<Result<(), std::io::Error>>,
}

struct GuardedIo<S> {
    io: S,
    _guard: ConnectionGuard,
    read_deadline: Pin<Box<Sleep>>,
    write_deadline: Pin<Box<Sleep>>,
}

impl<S> GuardedIo<S> {
    fn new(io: S, guard: ConnectionGuard) -> Self {
        Self {
            io,
            _guard: guard,
            read_deadline: Box::pin(tokio::time::sleep(HTTP_IO_TIMEOUT)),
            write_deadline: Box::pin(tokio::time::sleep(HTTP_IO_TIMEOUT)),
        }
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for GuardedIo<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        if self.read_deadline.as_mut().poll(context).is_ready() {
            return Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "HTTP read idle timeout",
            )));
        }
        let before = buffer.filled().len();
        let result = Pin::new(&mut self.io).poll_read(context, buffer);
        if matches!(&result, Poll::Ready(Ok(()))) && buffer.filled().len() > before {
            self.read_deadline
                .as_mut()
                .reset(Instant::now() + HTTP_IO_TIMEOUT);
        }
        result
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for GuardedIo<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        if self.write_deadline.as_mut().poll(context).is_ready() {
            return Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "HTTP write timeout",
            )));
        }
        let result = Pin::new(&mut self.io).poll_write(context, buffer);
        if matches!(&result, Poll::Ready(Ok(written)) if *written > 0) {
            self.write_deadline
                .as_mut()
                .reset(Instant::now() + HTTP_IO_TIMEOUT);
        }
        result
    }

    fn poll_flush(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.io).poll_flush(context)
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.io).poll_shutdown(context)
    }
}

struct GuardedListener {
    listener: TcpListener,
    daemon: Daemon,
}

#[derive(Clone, Copy)]
struct HttpPeer(SocketAddr);

impl HttpPeer {
    fn ip(self) -> std::net::IpAddr {
        self.0.ip()
    }
}

impl axum::extract::connect_info::Connected<axum::serve::IncomingStream<'_, GuardedListener>>
    for HttpPeer
{
    fn connect_info(stream: axum::serve::IncomingStream<'_, GuardedListener>) -> Self {
        Self(*stream.remote_addr())
    }
}

impl axum::extract::connect_info::Connected<SocketAddr> for HttpPeer {
    fn connect_info(address: SocketAddr) -> Self {
        Self(address)
    }
}

impl axum::serve::Listener for GuardedListener {
    type Io = GuardedIo<tokio::net::TcpStream>;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            let (io, address) =
                <TcpListener as axum::serve::Listener>::accept(&mut self.listener).await;
            if let Some(guard) = self.daemon.connection_guard(address.ip()) {
                return (GuardedIo::new(io, guard), address);
            }
        }
    }

    fn local_addr(&self) -> std::io::Result<Self::Addr> {
        self.listener.local_addr()
    }
}

#[derive(Clone)]
struct GuardAcceptor {
    daemon: Daemon,
}

impl<S> axum_server::accept::Accept<tokio::net::TcpStream, S> for GuardAcceptor {
    type Stream = GuardedIo<tokio::net::TcpStream>;
    type Service = S;
    type Future = Ready<std::io::Result<(Self::Stream, Self::Service)>>;

    fn accept(&self, io: tokio::net::TcpStream, service: S) -> Self::Future {
        let accepted = io
            .peer_addr()
            .ok()
            .and_then(|address| self.daemon.connection_guard(address.ip()))
            .map(|guard| (GuardedIo::new(io, guard), service))
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::ConnectionRefused,
                    "HTTP connection limit reached",
                )
            });
        ready(accepted)
    }
}

impl RunningHttpServer {
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn abort(&self) {
        self.task.abort();
    }

    /// Run the HTTP server to completion. Lets a caller bind first — so nothing
    /// externally visible is claimed until the listener is up — and serve after.
    pub async fn wait(&mut self) -> Result<(), DaemonError> {
        Ok((&mut self.task).await??)
    }
}

impl Drop for RunningHttpServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Daemon {
    pub async fn start_http(&self, addr: SocketAddr) -> Result<RunningHttpServer, DaemonError> {
        let listener = TcpListener::bind(addr).await?;
        let addr = listener.local_addr()?;
        self.remember_http_addr(addr)?;
        let operator = self.transport_mode() == TransportMode::Local && addr.ip().is_loopback();
        let router = router(self.clone(), false, operator);
        let listener = GuardedListener {
            listener,
            daemon: self.clone(),
        };
        let task = tokio::spawn(async move {
            axum::serve(
                listener,
                router.into_make_service_with_connect_info::<HttpPeer>(),
            )
            .await
        });
        Ok(RunningHttpServer { addr, task })
    }

    pub async fn serve_http(&self, addr: SocketAddr) -> Result<(), DaemonError> {
        let listener = TcpListener::bind(addr).await?;
        let addr = listener.local_addr()?;
        self.remember_http_addr(addr)?;
        let operator = self.transport_mode() == TransportMode::Local && addr.ip().is_loopback();
        let listener = GuardedListener {
            listener,
            daemon: self.clone(),
        };
        axum::serve(
            listener,
            router(self.clone(), false, operator).into_make_service_with_connect_info::<HttpPeer>(),
        )
        .await?;
        Ok(())
    }

    pub async fn serve_http_tls(
        &self,
        addr: SocketAddr,
        config: Arc<ServerConfig>,
    ) -> Result<(), DaemonError> {
        self.remember_secure_http_addr(addr)?;
        let config = axum_server::tls_rustls::RustlsConfig::from_config(config);
        axum_server::bind_rustls(addr, config)
            .map(|acceptor| {
                acceptor
                    .handshake_timeout(HTTP_HANDSHAKE_TIMEOUT)
                    .acceptor(GuardAcceptor {
                        daemon: self.clone(),
                    })
            })
            .serve(
                router(self.clone(), true, false).into_make_service_with_connect_info::<HttpPeer>(),
            )
            .await
            .map_err(std::io::Error::other)?;
        Ok(())
    }

    pub async fn sync_room_from_ws(
        &self,
        endpoint: &str,
        room: RoomId,
        token: Option<conch_core::types::Hash32>,
        expected_genesis: Option<conch_core::types::Hash32>,
    ) -> Result<conch_core::types::ChainState, DaemonError> {
        let _dial_guard = self
            .dial_guard(room)
            .ok_or(DaemonError::MutationUnavailable)?;
        let endpoint_url =
            Url::parse(endpoint).map_err(|_| DaemonError::InvalidEndpoint(endpoint.to_owned()))?;
        let connector = match (self.transport_mode(), endpoint_url.scheme()) {
            (TransportMode::Local, "ws") => {
                let host = endpoint_url
                    .host_str()
                    .and_then(|host| host.parse::<std::net::IpAddr>().ok());
                if !host.is_some_and(|host| host.is_loopback()) {
                    return Err(DaemonError::InvalidEndpoint(endpoint.to_owned()));
                }
                Some(Connector::Plain)
            }
            (TransportMode::Lan, "ws") => Some(Connector::Plain),
            (TransportMode::Local | TransportMode::Lan | TransportMode::Public, "wss") => {
                Some(Connector::Rustls(self.tls_client_config().ok_or(
                    DaemonError::Protocol("public mode requires TLS trust"),
                )?))
            }
            _ => return Err(DaemonError::InvalidEndpoint(endpoint.to_owned())),
        };
        let (socket, _) = connect_async_tls_with_config(endpoint, None, false, connector)
            .await
            .map_err(|error| DaemonError::WebSocket(error.to_string()))?;
        let (bridge, daemon_stream) = duplex(BRIDGE_CAPACITY);
        let bridge_task = tokio::spawn(tungstenite_bridge(socket, bridge));
        let result = self
            .sync_room_stream(daemon_stream, room, token, expected_genesis)
            .await;
        bridge_task.abort();
        result
    }
}

fn router(daemon: Daemon, secure: bool, operator: bool) -> Router {
    Router::new()
        .route("/ticket/{id}", get(get_ticket))
        .route("/history/{id}", get(get_history))
        .route("/room/{id}", get(get_room_detail))
        .route("/swarm", get(ws_swarm))
        .route("/client", get(ws_client))
        .route("/session/{id}", post(create_session).delete(delete_session))
        .route(
            "/operator/session",
            post(create_operator_session).delete(delete_operator_session),
        )
        .route(
            "/operator/rooms",
            get(operator_rooms).post(operator_create_room),
        )
        .route("/operator/rooms/join", post(operator_join_room))
        .route("/operator/rooms/{id}", get(operator_room))
        .route("/operator/rooms/{id}/history", get(operator_room_history))
        .route("/operator/client/{id}", get(ws_operator_client))
        .route("/", get(index))
        .route("/rooms/{id}", get(index))
        .route("/ui/", get(index))
        .route("/ui/app.js", get(app_js))
        .route("/ui/app.css", get(app_css))
        .with_state(HttpState {
            daemon,
            secure,
            operator,
        })
}

async fn index() -> Response<Body> {
    static_asset(
        include_str!("../../../ui/index.html"),
        "text/html; charset=utf-8",
    )
}

async fn app_js() -> Response<Body> {
    static_asset(
        include_str!("../../../ui/app.js"),
        "text/javascript; charset=utf-8",
    )
}

async fn app_css() -> Response<Body> {
    static_asset(
        include_str!("../../../ui/app.css"),
        "text/css; charset=utf-8",
    )
}

fn static_asset(contents: &'static str, content_type: &'static str) -> Response<Body> {
    let mut response = Response::new(Body::from(contents));
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    response
}

async fn create_operator_session(
    State(state): State<HttpState>,
    ConnectInfo(peer): ConnectInfo<HttpPeer>,
    headers: HeaderMap,
) -> Result<Response<Body>, HttpError> {
    require_operator_endpoint(&state, peer)?;
    let origin = operator_origin(&headers, true)?;
    let raw = state.daemon.create_operator_session(origin);
    let cookie = format!("conch_operator={raw}; Path=/; HttpOnly; SameSite=Strict; Max-Age=900");
    let mut response = (StatusCode::CREATED, Json(json!({ "ok": true }))).into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_str(&cookie).map_err(|_| HttpError::BadRequest("invalid cookie"))?,
    );
    Ok(operator_no_store(response))
}

async fn delete_operator_session(
    State(state): State<HttpState>,
    ConnectInfo(peer): ConnectInfo<HttpPeer>,
    headers: HeaderMap,
) -> Result<Response<Body>, HttpError> {
    require_operator_endpoint(&state, peer)?;
    let raw = authorize_operator(&state, &headers, true)?;
    state.daemon.revoke_operator_session(raw);
    let mut response = StatusCode::NO_CONTENT.into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_static("conch_operator=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0"),
    );
    Ok(operator_no_store(response))
}

async fn operator_rooms(
    State(state): State<HttpState>,
    ConnectInfo(peer): ConnectInfo<HttpPeer>,
    headers: HeaderMap,
) -> Result<Response<Body>, HttpError> {
    require_operator_endpoint(&state, peer)?;
    authorize_operator(&state, &headers, false)?;
    Ok(operator_no_store(
        Json(state.daemon.operator_catalog()?).into_response(),
    ))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateRoomRequest {
    name: String,
    #[serde(default)]
    mode: Option<String>,
    #[serde(default = "default_timeout_secs")]
    timeout_secs: u64,
}

fn default_timeout_secs() -> u64 {
    30
}

async fn operator_create_room(
    State(state): State<HttpState>,
    ConnectInfo(peer): ConnectInfo<HttpPeer>,
    headers: HeaderMap,
    Json(request): Json<CreateRoomRequest>,
) -> Result<Response<Body>, HttpError> {
    require_operator_endpoint(&state, peer)?;
    authorize_operator(&state, &headers, true)?;
    let name = request.name.trim();
    if name.is_empty() || name.chars().count() > 128 {
        return Err(HttpError::BadRequest(
            "room name must contain 1-128 characters",
        ));
    }
    if request.mode.as_deref().is_some_and(|mode| mode != "stick") {
        return Err(HttpError::BadRequest(
            "the local console currently creates stick-floor rooms",
        ));
    }
    if request.timeout_secs == 0 {
        return Err(HttpError::BadRequest("timeout_secs must be at least 1"));
    }
    let daemon = state.daemon.clone();
    let name = name.to_owned();
    let timeout_secs = request.timeout_secs;
    let ticket = task::spawn_blocking(move || {
        daemon.create_ticket_with_token(
            &name,
            StakePolicy::default(),
            FloorConfig::stick(timeout_secs),
            Some(Hash32::from_bytes(random::<[u8; 32]>())),
        )
    })
    .await??;
    let room = state.daemon.operator_room_detail(ticket.id)?;
    Ok(operator_no_store(
        (
            StatusCode::CREATED,
            Json(json!({ "ticket": ticket, "room": room })),
        )
            .into_response(),
    ))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct JoinRoomRequest {
    ticket: Ticket,
    #[serde(default)]
    role: JoinRole,
}

async fn operator_join_room(
    State(state): State<HttpState>,
    ConnectInfo(peer): ConnectInfo<HttpPeer>,
    headers: HeaderMap,
    Json(request): Json<JoinRoomRequest>,
) -> Result<Response<Body>, HttpError> {
    require_operator_endpoint(&state, peer)?;
    authorize_operator(&state, &headers, true)?;
    let room = request.ticket.id;
    state
        .daemon
        .join_ticket(request.ticket, request.role)
        .await?;
    Ok(operator_no_store(
        (
            StatusCode::CREATED,
            Json(state.daemon.operator_room_detail(room)?),
        )
            .into_response(),
    ))
}

async fn operator_room(
    State(state): State<HttpState>,
    ConnectInfo(peer): ConnectInfo<HttpPeer>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Response<Body>, HttpError> {
    require_operator_endpoint(&state, peer)?;
    authorize_operator(&state, &headers, false)?;
    let room = parse_room(&id)?;
    Ok(operator_no_store(
        Json(state.daemon.operator_room_detail(room)?).into_response(),
    ))
}

async fn operator_room_history(
    State(state): State<HttpState>,
    ConnectInfo(peer): ConnectInfo<HttpPeer>,
    Path(id): Path<String>,
    Query(query): Query<HistoryQuery>,
    headers: HeaderMap,
) -> Result<Response<Body>, HttpError> {
    require_operator_endpoint(&state, peer)?;
    authorize_operator(&state, &headers, false)?;
    let room = parse_room(&id)?;
    Ok(operator_no_store(
        Json(state.daemon.history_page_from(room, query.from)?).into_response(),
    ))
}

async fn ws_operator_client(
    State(state): State<HttpState>,
    ConnectInfo(peer): ConnectInfo<HttpPeer>,
    Path(id): Path<String>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Response<Body> {
    if require_operator_endpoint(&state, peer).is_err() {
        return StatusCode::NOT_FOUND.into_response();
    }
    if authorize_operator(&state, &headers, true).is_err() {
        return StatusCode::FORBIDDEN.into_response();
    }
    let Ok(allowed_room) = parse_room(&id) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    if state.daemon.replay(allowed_room).is_err() {
        return StatusCode::NOT_FOUND.into_response();
    }
    if !state
        .daemon
        .token_sha256(allowed_room)
        .is_ok_and(|token| token.is_some())
    {
        return StatusCode::FORBIDDEN.into_response();
    }
    upgrade
        .max_message_size(MAX_QUEUE_BYTES)
        .max_frame_size(MAX_QUEUE_BYTES)
        .on_upgrade(move |socket| async move {
            websocket_bridge(
                socket,
                state.daemon,
                ConnectionProtocol::Client { allowed_room },
                Some(allowed_room),
                peer.ip(),
            )
            .await;
        })
        .into_response()
}

fn require_operator_endpoint(state: &HttpState, peer: HttpPeer) -> Result<(), HttpError> {
    if state.operator && !state.secure && peer.ip().is_loopback() {
        Ok(())
    } else {
        Err(HttpError::NotFound)
    }
}

fn authorize_operator<'a>(
    state: &HttpState,
    headers: &'a HeaderMap,
    require_origin: bool,
) -> Result<&'a str, HttpError> {
    let origin = operator_origin(headers, require_origin)?;
    let raw = named_cookie(headers, "conch_operator").ok_or(HttpError::Forbidden)?;
    if state.daemon.validate_operator_session(raw, &origin) {
        Ok(raw)
    } else {
        Err(HttpError::Forbidden)
    }
}

fn operator_origin(headers: &HeaderMap, require_origin: bool) -> Result<String, HttpError> {
    let origin = if require_origin {
        canonical_origin(headers, false).map_err(|_| HttpError::Forbidden)?
    } else {
        request_origin(headers, false)?
    };
    let url = Url::parse(&origin).map_err(|_| HttpError::Forbidden)?;
    let literal_loopback = match url.host() {
        Some(Host::Ipv4(address)) => address.is_loopback(),
        Some(Host::Ipv6(address)) => address.is_loopback(),
        Some(Host::Domain(_)) | None => false,
    };
    literal_loopback
        .then_some(origin)
        .ok_or(HttpError::Forbidden)
}

fn request_origin(headers: &HeaderMap, secure: bool) -> Result<String, HttpError> {
    let hosts = headers.get_all(header::HOST).iter().collect::<Vec<_>>();
    if hosts.len() != 1 {
        return Err(HttpError::Forbidden);
    }
    let host = hosts[0].to_str().map_err(|_| HttpError::Forbidden)?;
    if !host.is_ascii()
        || host.contains([',', '@', '/', '?', '#'])
        || host.trim() != host
        || host.is_empty()
    {
        return Err(HttpError::Forbidden);
    }
    let scheme = if secure { "https" } else { "http" };
    let url = Url::parse(&format!("{scheme}://{host}/")).map_err(|_| HttpError::Forbidden)?;
    origin_tuple(&url).map_err(|_| HttpError::Forbidden)
}

fn named_cookie<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find_map(|(key, value)| (key == name).then_some(value))
}

fn operator_no_store(mut response: Response<Body>) -> Response<Body> {
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response.headers_mut().insert(
        header::HeaderName::from_static("cross-origin-resource-policy"),
        HeaderValue::from_static("same-origin"),
    );
    response.headers_mut().insert(
        header::HeaderName::from_static("x-content-type-options"),
        HeaderValue::from_static("nosniff"),
    );
    response
}

async fn get_ticket(
    State(state): State<HttpState>,
    ConnectInfo(peer): ConnectInfo<HttpPeer>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Response<Body>, HttpError> {
    if !state.daemon.auth_allowed(peer.ip()) {
        return Err(HttpError::Unauthorized);
    }
    let room = parse_room(&id)?;
    if let Err(error) = authorize_read(&state, room, &headers) {
        state.daemon.record_auth_failure(peer.ip());
        return Err(error);
    }
    let daemon = state.daemon;
    let ticket = task::spawn_blocking(move || daemon.served_ticket(room)).await??;
    Ok(no_store(Json(ticket).into_response()))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct HistoryQuery {
    #[serde(default)]
    from: u64,
}

async fn get_history(
    State(state): State<HttpState>,
    ConnectInfo(peer): ConnectInfo<HttpPeer>,
    Path(id): Path<String>,
    Query(query): Query<HistoryQuery>,
    headers: HeaderMap,
) -> Result<Response<Body>, HttpError> {
    if !state.daemon.auth_allowed(peer.ip()) {
        return Err(HttpError::Unauthorized);
    }
    let room = parse_room(&id)?;
    if let Err(error) = authorize_read(&state, room, &headers) {
        state.daemon.record_auth_failure(peer.ip());
        return Err(error);
    }
    Ok(no_store(
        Json(state.daemon.history_page_from(room, query.from)?).into_response(),
    ))
}

async fn get_room_detail(
    State(state): State<HttpState>,
    ConnectInfo(peer): ConnectInfo<HttpPeer>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Response<Body>, HttpError> {
    if !state.daemon.auth_allowed(peer.ip()) {
        return Err(HttpError::Unauthorized);
    }
    let room = parse_room(&id)?;
    if let Err(error) = authorize_read(&state, room, &headers) {
        state.daemon.record_auth_failure(peer.ip());
        return Err(error);
    }
    Ok(no_store(
        Json(state.daemon.operator_room_detail(room)?).into_response(),
    ))
}

fn no_store(mut response: Response<Body>) -> Response<Body> {
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

async fn ws_swarm(
    State(state): State<HttpState>,
    ConnectInfo(peer): ConnectInfo<HttpPeer>,
    upgrade: WebSocketUpgrade,
) -> Response<Body> {
    upgrade
        .max_message_size(PREAUTH_MESSAGE_BYTES)
        .max_frame_size(PREAUTH_MESSAGE_BYTES)
        .on_upgrade(move |socket| async move {
            swarm_websocket_bridge(socket, state.daemon, peer.ip()).await;
        })
        .into_response()
}

async fn ws_client(
    State(state): State<HttpState>,
    ConnectInfo(peer): ConnectInfo<HttpPeer>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Response<Body> {
    if !state.daemon.auth_allowed(peer.ip()) {
        return StatusCode::FORBIDDEN.into_response();
    }
    if !state.secure && !peer.ip().is_loopback() {
        state.daemon.record_auth_failure(peer.ip());
        return StatusCode::FORBIDDEN.into_response();
    }
    let Ok(origin) = canonical_origin(&headers, state.secure) else {
        state.daemon.record_auth_failure(peer.ip());
        return StatusCode::FORBIDDEN.into_response();
    };
    let Some(raw) = session_cookie(&headers, state.secure) else {
        state.daemon.record_auth_failure(peer.ip());
        return StatusCode::FORBIDDEN.into_response();
    };
    let Some(allowed_room) = state.daemon.validate_browser_session(raw, None, &origin) else {
        state.daemon.record_auth_failure(peer.ip());
        return StatusCode::FORBIDDEN.into_response();
    };
    upgrade
        .max_message_size(MAX_QUEUE_BYTES)
        .max_frame_size(MAX_QUEUE_BYTES)
        .on_upgrade(move |socket| async move {
            websocket_bridge(
                socket,
                state.daemon,
                ConnectionProtocol::Client { allowed_room },
                Some(allowed_room),
                peer.ip(),
            )
            .await;
        })
        .into_response()
}

async fn create_session(
    State(state): State<HttpState>,
    ConnectInfo(peer): ConnectInfo<HttpPeer>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Response<Body>, HttpError> {
    if !state.daemon.auth_allowed(peer.ip()) {
        return Err(HttpError::Unauthorized);
    }
    if !state.secure && !peer.ip().is_loopback() {
        state.daemon.record_auth_failure(peer.ip());
        return Err(HttpError::Unauthorized);
    }
    let authorized = (|| {
        let room = parse_room(&id)?;
        let origin = canonical_origin(&headers, state.secure)?;
        let token = bearer_token(&headers).ok_or(HttpError::Unauthorized)?;
        let raw = state
            .daemon
            .create_browser_session(room, origin.clone(), token)
            .map_err(|_| HttpError::Unauthorized)?;
        Ok::<_, HttpError>((room, raw))
    })();
    let (room, raw) = match authorized {
        Ok(authorized) => authorized,
        Err(error) => {
            state.daemon.record_auth_failure(peer.ip());
            return Err(error);
        }
    };
    let cookie = if state.secure {
        format!(
            "__Host-conch_session={raw}; Path=/; HttpOnly; SameSite=Strict; Secure; Max-Age=900"
        )
    } else {
        format!("conch_session={raw}; Path=/; HttpOnly; SameSite=Strict; Max-Age=900")
    };
    let mut response = (StatusCode::CREATED, Json(json!({ "room": room }))).into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_str(&cookie).map_err(|_| HttpError::BadRequest("invalid cookie"))?,
    );
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(response)
}

async fn delete_session(
    State(state): State<HttpState>,
    ConnectInfo(peer): ConnectInfo<HttpPeer>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Response<Body>, HttpError> {
    if !state.daemon.auth_allowed(peer.ip()) {
        return Err(HttpError::Unauthorized);
    }
    if !state.secure && !peer.ip().is_loopback() {
        state.daemon.record_auth_failure(peer.ip());
        return Err(HttpError::Unauthorized);
    }
    let authorized = (|| {
        let room = parse_room(&id)?;
        let origin = canonical_origin(&headers, state.secure)?;
        let raw = session_cookie(&headers, state.secure).ok_or(HttpError::Unauthorized)?;
        state
            .daemon
            .validate_browser_session(raw, Some(room), &origin)
            .ok_or(HttpError::Unauthorized)?;
        Ok::<_, HttpError>(raw)
    })();
    let raw = match authorized {
        Ok(authorized) => authorized,
        Err(error) => {
            state.daemon.record_auth_failure(peer.ip());
            return Err(error);
        }
    };
    state.daemon.revoke_browser_session(raw);
    let name = if state.secure {
        "__Host-conch_session"
    } else {
        "conch_session"
    };
    let secure = if state.secure { "; Secure" } else { "" };
    let cookie = format!("{name}=; Path=/; HttpOnly; SameSite=Strict{secure}; Max-Age=0");
    let mut response = StatusCode::NO_CONTENT.into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_str(&cookie).map_err(|_| HttpError::BadRequest("invalid cookie"))?,
    );
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(response)
}

fn bearer_token(headers: &HeaderMap) -> Option<Hash32> {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .and_then(|value| value.parse().ok())
}

fn session_cookie(headers: &HeaderMap, secure: bool) -> Option<&str> {
    let name = if secure {
        "__Host-conch_session"
    } else {
        "conch_session"
    };
    named_cookie(headers, name)
}

fn canonical_origin(headers: &HeaderMap, secure: bool) -> Result<String, HttpError> {
    let origins = headers.get_all(header::ORIGIN).iter().collect::<Vec<_>>();
    if origins.len() != 1 {
        return Err(HttpError::Unauthorized);
    }
    let raw = origins[0].to_str().map_err(|_| HttpError::Unauthorized)?;
    if !raw.is_ascii() || raw == "null" || raw.contains(',') {
        return Err(HttpError::Unauthorized);
    }
    let authority = raw
        .split_once("://")
        .map(|(_, authority)| authority)
        .ok_or(HttpError::Unauthorized)?;
    if authority.contains(['/', '?', '#']) {
        return Err(HttpError::Unauthorized);
    }
    let origin = Url::parse(raw).map_err(|_| HttpError::Unauthorized)?;
    if !origin.username().is_empty()
        || origin.password().is_some()
        || origin.query().is_some()
        || origin.fragment().is_some()
        || origin.path() != "/"
    {
        return Err(HttpError::Unauthorized);
    }
    let expected_scheme = if secure { "https" } else { "http" };
    if origin.scheme() != expected_scheme {
        return Err(HttpError::Unauthorized);
    }
    let hosts = headers.get_all(header::HOST).iter().collect::<Vec<_>>();
    if hosts.len() != 1 {
        return Err(HttpError::Unauthorized);
    }
    let host = hosts[0].to_str().map_err(|_| HttpError::Unauthorized)?;
    if !host.is_ascii()
        || host.contains([',', '@', '/', '?', '#'])
        || host.trim() != host
        || host.is_empty()
    {
        return Err(HttpError::Unauthorized);
    }
    let expected =
        Url::parse(&format!("{expected_scheme}://{host}/")).map_err(|_| HttpError::Unauthorized)?;
    let actual = origin_tuple(&origin)?;
    if actual != origin_tuple(&expected)? {
        return Err(HttpError::Unauthorized);
    }
    Ok(actual)
}

fn origin_tuple(url: &Url) -> Result<String, HttpError> {
    let host = match url.host().ok_or(HttpError::Unauthorized)? {
        Host::Domain(host) => host.to_ascii_lowercase(),
        Host::Ipv4(host) => host.to_string(),
        Host::Ipv6(host) => format!("[{host}]"),
    };
    let port = url.port_or_known_default().ok_or(HttpError::Unauthorized)?;
    Ok(format!(
        "{}://{}:{port}",
        url.scheme().to_ascii_lowercase(),
        host
    ))
}

async fn swarm_websocket_bridge(socket: WebSocket, daemon: Daemon, source: std::net::IpAddr) {
    let (bridge, daemon_stream) = duplex(BRIDGE_CAPACITY);
    let (mut bridge_reader, mut bridge_writer) = split(bridge);
    let (mut socket_writer, mut socket_reader) = socket.split();
    let (outgoing_tx, mut outgoing_rx) = mpsc::channel(MAX_QUEUE_FRAMES);
    let queued_bytes = Arc::new(AtomicUsize::new(0));
    let daemon_task = tokio::spawn(async move {
        let _ = daemon
            .handle_transport_with_source(daemon_stream, ConnectionProtocol::Swarm, source)
            .await;
    });
    let control_tx = outgoing_tx.clone();
    let control_bytes = Arc::clone(&queued_bytes);
    let mut ingress = tokio::spawn(async move {
        while let Some(Ok(message)) = socket_reader.next().await {
            match message {
                Message::Text(text) => {
                    let Ok(value) = serde_json::from_str::<Value>(text.as_str()) else {
                        break;
                    };
                    let Ok(encoded) = frame::encode(&value) else {
                        break;
                    };
                    if !matches!(
                        timeout(HTTP_IO_TIMEOUT, bridge_writer.write_all(&encoded)).await,
                        Ok(Ok(()))
                    ) {
                        break;
                    }
                }
                Message::Binary(bytes) => {
                    if !matches!(
                        timeout(HTTP_IO_TIMEOUT, bridge_writer.write_all(&bytes)).await,
                        Ok(Ok(()))
                    ) {
                        break;
                    }
                }
                Message::Ping(bytes) => {
                    let length = bytes.len();
                    if !try_queue(&control_tx, &control_bytes, Message::Pong(bytes), length) {
                        break;
                    }
                }
                Message::Close(_) => break,
                Message::Pong(_) => {}
            }
        }
    });
    let egress_bytes = Arc::clone(&queued_bytes);
    let mut egress = tokio::spawn(async move {
        let mut buffer = vec![0_u8; PREAUTH_MESSAGE_BYTES];
        loop {
            let read = match timeout(HTTP_IO_TIMEOUT, bridge_reader.read(&mut buffer)).await {
                Ok(Ok(read)) if read > 0 => read,
                _ => break,
            };
            if !try_queue(
                &outgoing_tx,
                &egress_bytes,
                Message::Binary(buffer[..read].to_vec().into()),
                read,
            ) {
                break;
            }
        }
    });
    let sender_bytes = Arc::clone(&queued_bytes);
    let mut sender = tokio::spawn(async move {
        while let Some((message, bytes)) = outgoing_rx.recv().await {
            if !matches!(
                timeout(HTTP_IO_TIMEOUT, socket_writer.send(message)).await,
                Ok(Ok(()))
            ) {
                break;
            }
            sender_bytes.fetch_sub(bytes, Ordering::AcqRel);
        }
    });
    tokio::select! {
        _ = &mut ingress => {},
        _ = &mut egress => {},
        _ = &mut sender => {},
    }
    ingress.abort();
    egress.abort();
    sender.abort();
    daemon_task.abort();
}

async fn websocket_bridge(
    socket: WebSocket,
    daemon: Daemon,
    protocol: ConnectionProtocol,
    allowed_room: Option<RoomId>,
    source: std::net::IpAddr,
) {
    let (bridge, daemon_stream) = duplex(BRIDGE_CAPACITY);
    let (mut bridge_reader, mut bridge_writer) = split(bridge);
    let (mut socket_writer, mut socket_reader) = socket.split();
    let (outgoing_tx, mut outgoing_rx) = mpsc::channel(MAX_QUEUE_FRAMES);
    let queued_bytes = Arc::new(AtomicUsize::new(0));
    let daemon_task = tokio::spawn(async move {
        let _ = daemon
            .handle_transport_with_source(daemon_stream, protocol, source)
            .await;
    });
    let control_tx = outgoing_tx.clone();
    let control_bytes = Arc::clone(&queued_bytes);
    let mut ingress = tokio::spawn(async move {
        while let Some(Ok(message)) = socket_reader.next().await {
            match message {
                Message::Text(text) => {
                    let Ok(value) = serde_json::from_str::<Value>(text.as_str()) else {
                        break;
                    };
                    if let Some(room) = allowed_room {
                        let expected = room.to_string();
                        if value.get("typ").and_then(Value::as_str) != Some("attach")
                            && value.get("room").and_then(Value::as_str) != Some(expected.as_str())
                        {
                            let error = ClientReply::failure(
                                "unauthorized",
                                "WebSocket is authorized for a different room",
                            );
                            let Ok(text) = serde_json::to_string(&error) else {
                                break;
                            };
                            let length = text.len();
                            if !try_queue(
                                &control_tx,
                                &control_bytes,
                                Message::Text(text.into()),
                                length,
                            ) {
                                break;
                            }
                            continue;
                        }
                    }
                    let Ok(encoded) = frame::encode(&value) else {
                        break;
                    };
                    if !matches!(
                        timeout(HTTP_IO_TIMEOUT, bridge_writer.write_all(&encoded)).await,
                        Ok(Ok(()))
                    ) {
                        break;
                    }
                }
                Message::Ping(bytes) => {
                    let length = bytes.len();
                    if !try_queue(&control_tx, &control_bytes, Message::Pong(bytes), length) {
                        break;
                    }
                }
                Message::Close(_) => break,
                Message::Binary(bytes) => {
                    if !matches!(
                        timeout(HTTP_IO_TIMEOUT, bridge_writer.write_all(&bytes)).await,
                        Ok(Ok(()))
                    ) {
                        break;
                    }
                }
                Message::Pong(_) => {}
            }
        }
    });
    let egress_bytes = Arc::clone(&queued_bytes);
    let mut egress = tokio::spawn(async move {
        while let Ok(Some(value)) = read_frame::<_, Value>(&mut bridge_reader).await {
            let Ok(text) = serde_json::to_string(&value) else {
                break;
            };
            let length = text.len();
            if !try_queue(
                &outgoing_tx,
                &egress_bytes,
                Message::Text(text.into()),
                length,
            ) {
                break;
            }
        }
    });
    let sender_bytes = Arc::clone(&queued_bytes);
    let mut sender = tokio::spawn(async move {
        while let Some((message, bytes)) = outgoing_rx.recv().await {
            if !matches!(
                timeout(HTTP_IO_TIMEOUT, socket_writer.send(message)).await,
                Ok(Ok(()))
            ) {
                break;
            }
            sender_bytes.fetch_sub(bytes, Ordering::AcqRel);
        }
    });
    tokio::select! {
        _ = &mut ingress => {},
        _ = &mut egress => {},
        _ = &mut sender => {},
    }
    ingress.abort();
    egress.abort();
    sender.abort();
    daemon_task.abort();
}

async fn tungstenite_bridge(
    socket: WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>,
    bridge: tokio::io::DuplexStream,
) {
    let (mut bridge_reader, mut bridge_writer) = split(bridge);
    let (mut socket_writer, mut socket_reader) = socket.split();
    let (outgoing_tx, mut outgoing_rx) = mpsc::channel(MAX_QUEUE_FRAMES);
    let queued_bytes = Arc::new(AtomicUsize::new(0));
    let control_tx = outgoing_tx.clone();
    let control_bytes = Arc::clone(&queued_bytes);
    let mut ingress = tokio::spawn(async move {
        while let Some(Ok(message)) = socket_reader.next().await {
            match message {
                TungsteniteMessage::Text(text) => {
                    let Ok(value) = serde_json::from_str::<Value>(text.as_str()) else {
                        break;
                    };
                    let Ok(encoded) = frame::encode(&value) else {
                        break;
                    };
                    if !matches!(
                        timeout(HTTP_IO_TIMEOUT, bridge_writer.write_all(&encoded)).await,
                        Ok(Ok(()))
                    ) {
                        break;
                    }
                }
                TungsteniteMessage::Ping(bytes) => {
                    let length = bytes.len();
                    if !try_queue(
                        &control_tx,
                        &control_bytes,
                        TungsteniteMessage::Pong(bytes),
                        length,
                    ) {
                        break;
                    }
                }
                TungsteniteMessage::Close(_) => break,
                TungsteniteMessage::Binary(bytes) => {
                    if !matches!(
                        timeout(HTTP_IO_TIMEOUT, bridge_writer.write_all(&bytes)).await,
                        Ok(Ok(()))
                    ) {
                        break;
                    }
                }
                TungsteniteMessage::Pong(_) | TungsteniteMessage::Frame(_) => {}
            }
        }
    });
    let egress_bytes = Arc::clone(&queued_bytes);
    let mut egress = tokio::spawn(async move {
        let mut buffer = vec![0_u8; PREAUTH_MESSAGE_BYTES];
        loop {
            let read = match timeout(HTTP_IO_TIMEOUT, bridge_reader.read(&mut buffer)).await {
                Ok(Ok(read)) if read > 0 => read,
                _ => break,
            };
            if !try_queue(
                &outgoing_tx,
                &egress_bytes,
                TungsteniteMessage::Binary(buffer[..read].to_vec().into()),
                read,
            ) {
                break;
            }
        }
    });
    let sender_bytes = Arc::clone(&queued_bytes);
    let mut sender = tokio::spawn(async move {
        while let Some((message, bytes)) = outgoing_rx.recv().await {
            if !matches!(
                timeout(HTTP_IO_TIMEOUT, socket_writer.send(message)).await,
                Ok(Ok(()))
            ) {
                break;
            }
            sender_bytes.fetch_sub(bytes, Ordering::AcqRel);
        }
    });
    tokio::select! {
        _ = &mut ingress => {},
        _ = &mut egress => {},
        _ = &mut sender => {},
    }
    ingress.abort();
    egress.abort();
    sender.abort();
}

fn try_queue<T>(
    sender: &mpsc::Sender<(T, usize)>,
    queued: &AtomicUsize,
    message: T,
    bytes: usize,
) -> bool {
    if bytes > MAX_QUEUE_BYTES {
        return false;
    }
    let reserved = queued.fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
        current
            .checked_add(bytes)
            .filter(|next| *next <= MAX_QUEUE_BYTES)
    });
    if reserved.is_err() {
        return false;
    }
    if sender.try_send((message, bytes)).is_err() {
        queued.fetch_sub(bytes, Ordering::AcqRel);
        return false;
    }
    true
}

fn parse_room(id: &str) -> Result<RoomId, HttpError> {
    RoomId::from_str(id).map_err(|_| HttpError::BadRequest("invalid room id"))
}

fn authorize(daemon: &Daemon, room: RoomId, headers: &HeaderMap) -> Result<(), HttpError> {
    if daemon.token_sha256(room)?.is_none() {
        if daemon.transport_mode() == TransportMode::Public {
            return Err(HttpError::Unauthorized);
        }
        return Ok(());
    }
    let token = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .and_then(|value| value.parse().ok())
        .ok_or(HttpError::Unauthorized)?;
    if daemon.authenticate(room, token)? {
        Ok(())
    } else {
        Err(HttpError::Unauthorized)
    }
}

fn authorize_read(state: &HttpState, room: RoomId, headers: &HeaderMap) -> Result<(), HttpError> {
    if authorize(&state.daemon, room, headers).is_ok() {
        return Ok(());
    }
    let origin = request_origin(headers, state.secure).map_err(|_| HttpError::Unauthorized)?;
    let raw = session_cookie(headers, state.secure).ok_or(HttpError::Unauthorized)?;
    state
        .daemon
        .validate_browser_session(raw, Some(room), &origin)
        .ok_or(HttpError::Unauthorized)?;
    Ok(())
}

#[derive(Debug)]
enum HttpError {
    BadRequest(&'static str),
    Unauthorized,
    Forbidden,
    NotFound,
    Daemon(DaemonError),
    Json(serde_json::Error),
    Join(tokio::task::JoinError),
}

impl From<DaemonError> for HttpError {
    fn from(error: DaemonError) -> Self {
        Self::Daemon(error)
    }
}

impl From<serde_json::Error> for HttpError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}

impl From<tokio::task::JoinError> for HttpError {
    fn from(error: tokio::task::JoinError) -> Self {
        Self::Join(error)
    }
}

impl IntoResponse for HttpError {
    fn into_response(self) -> Response<Body> {
        let (status, message) = match self {
            Self::BadRequest(message) => (StatusCode::BAD_REQUEST, message.to_owned()),
            Self::Unauthorized => (StatusCode::UNAUTHORIZED, "bearer token required".into()),
            Self::Forbidden => (StatusCode::FORBIDDEN, "forbidden".into()),
            Self::NotFound => (StatusCode::NOT_FOUND, "not found".into()),
            Self::Daemon(DaemonError::UnknownRoom(_)) => {
                (StatusCode::NOT_FOUND, "unknown room".into())
            }
            Self::Daemon(error) => (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
            Self::Json(error) => (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
            Self::Join(error) => (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
        };
        let mut response = (status, Json(json!({ "error": message }))).into_response();
        if status == StatusCode::UNAUTHORIZED {
            response.headers_mut().insert(
                header::WWW_AUTHENTICATE,
                header::HeaderValue::from_static("Bearer"),
            );
        }
        response
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(origin: &str, host: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(header::ORIGIN, origin.parse().unwrap());
        headers.insert(header::HOST, host.parse().unwrap());
        headers
    }

    #[test]
    fn canonical_origin_requires_one_exact_tuple_without_url_extras() {
        assert_eq!(
            canonical_origin(&headers("http://LOCALHOST:7420", "localhost:7420"), false).unwrap(),
            "http://localhost:7420"
        );
        assert!(canonical_origin(&headers("http://localhost", "localhost:80"), false).is_ok());
        for invalid in [
            "null",
            "http://localhost/",
            "http://user@localhost",
            "http://localhost?x=1",
            "http://localhost#x",
            "https://localhost:7420",
            "http://evil-localhost:7420",
        ] {
            assert!(
                canonical_origin(&headers(invalid, "localhost:7420"), false).is_err(),
                "accepted invalid origin {invalid}"
            );
        }
        let mut multiple = headers("http://localhost:7420", "localhost:7420");
        multiple.append(header::ORIGIN, "http://localhost:7420".parse().unwrap());
        assert!(canonical_origin(&multiple, false).is_err());

        for invalid_host in [
            "",
            " localhost:7420",
            "localhost:7420 ",
            "localhost:bad",
            "user@localhost:7420",
            "localhost:7420/path",
            "localhost:7420,evil.invalid",
            "[::1",
        ] {
            assert!(
                canonical_origin(&headers("http://localhost:7420", invalid_host), false).is_err(),
                "accepted invalid Host {invalid_host:?}"
            );
        }
        let mut multiple_host = headers("http://localhost:7420", "localhost:7420");
        multiple_host.append(header::HOST, "localhost:7420".parse().unwrap());
        assert!(canonical_origin(&multiple_host, false).is_err());

        assert!(canonical_origin(
            &headers("http://xn--bcher-kva.example", "xn--bcher-kva.example"),
            false
        )
        .is_ok());
        assert!(canonical_origin(&headers("http://example.test.", "example.test."), false).is_ok());
        assert!(canonical_origin(&headers("http://example.test.", "example.test"), false).is_err());
        let mut forwarded = headers("http://localhost:7420", "localhost:7420");
        forwarded.insert(
            "forwarded",
            "host=evil.invalid;proto=https".parse().unwrap(),
        );
        forwarded.insert("x-forwarded-host", "evil.invalid".parse().unwrap());
        forwarded.insert("x-forwarded-proto", "https".parse().unwrap());
        assert_eq!(
            canonical_origin(&forwarded, false).unwrap(),
            "http://localhost:7420"
        );
    }

    #[test]
    fn public_http_authorize_rejects_open_rooms() {
        let data = tempfile::TempDir::new().unwrap();
        let daemon = Daemon::open(data.path()).unwrap();
        let room = daemon.create_genesis("open http").unwrap();
        assert!(authorize(&daemon, room, &HeaderMap::new()).is_ok());
        daemon.set_transport_for_test(TransportMode::Public, None);
        assert!(matches!(
            authorize(&daemon, room, &HeaderMap::new()),
            Err(HttpError::Unauthorized)
        ));
    }

    #[test]
    fn browser_ui_keeps_only_deep_link_state_after_authorization() {
        let app = include_str!("../../../ui/app.js");
        let join = app
            .split_once("async function joinRoom(event)")
            .expect("joinRoom exists")
            .1;
        let authorized = join
            .find("if (!response.ok)")
            .expect("join/session status is checked");
        let opened = join
            .find("await openRoom(ticket.id)")
            .expect("the authorized room is opened");
        assert!(authorized < opened, "authorization precedes navigation");
        assert!(app.contains("history.pushState({}, \"\", `/rooms/${room}`)"));
        assert!(app.contains("function roomFromPath()"));
        assert!(!app.contains("localStorage"));
        assert!(!app.contains("sessionStorage"));
        assert!(!app.contains("scrollIntoView"));
    }

    #[test]
    fn slow_reader_queue_is_bounded_by_frames_and_encoded_bytes() {
        let (sender, _receiver) = mpsc::channel(MAX_QUEUE_FRAMES);
        let queued = AtomicUsize::new(0);
        for frame in 0..MAX_QUEUE_FRAMES {
            assert!(try_queue(&sender, &queued, frame, 1));
        }
        assert!(!try_queue(&sender, &queued, MAX_QUEUE_FRAMES, 1));
        assert_eq!(queued.load(Ordering::Acquire), MAX_QUEUE_FRAMES);

        let (sender, _receiver) = mpsc::channel(MAX_QUEUE_FRAMES);
        let queued = AtomicUsize::new(0);
        assert!(try_queue(&sender, &queued, (), MAX_QUEUE_BYTES));
        assert!(!try_queue(&sender, &queued, (), 1));
        assert!(!try_queue(&sender, &queued, (), MAX_QUEUE_BYTES + 1));
        assert_eq!(queued.load(Ordering::Acquire), MAX_QUEUE_BYTES);
    }
}
