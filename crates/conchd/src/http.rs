use std::{net::SocketAddr, str::FromStr};

use axum::{
    body::Body,
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        ConnectInfo, Path, Query, State,
    },
    http::{header, HeaderMap, HeaderValue, Response, StatusCode},
    response::IntoResponse,
    routing::get,
    Json, Router,
};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::{
    io::{duplex, split, AsyncWriteExt},
    net::TcpListener,
    sync::mpsc,
    task::{self, JoinHandle},
};
use tokio_tungstenite::{
    connect_async, tungstenite::Message as TungsteniteMessage, MaybeTlsStream, WebSocketStream,
};

use conch_core::{
    client::ClientReply,
    frame,
    ticket::Ticket,
    types::{Hash32, RoomId},
};

use crate::tcp::{read_frame, ConnectionProtocol, Daemon, DaemonError};

const BRIDGE_CAPACITY: usize = 64 * 1024;

pub struct RunningHttpServer {
    addr: SocketAddr,
    task: JoinHandle<Result<(), std::io::Error>>,
}

impl RunningHttpServer {
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn abort(&self) {
        self.task.abort();
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
        let router = router(self.clone());
        let task = tokio::spawn(async move {
            axum::serve(
                listener,
                router.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
        });
        Ok(RunningHttpServer { addr, task })
    }

    pub async fn serve_http(&self, addr: SocketAddr) -> Result<(), DaemonError> {
        let listener = TcpListener::bind(addr).await?;
        self.remember_http_addr(listener.local_addr()?)?;
        axum::serve(
            listener,
            router(self.clone()).into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await?;
        Ok(())
    }

    pub async fn sync_room_from_ws(
        &self,
        endpoint: &str,
        room: RoomId,
        token: Option<conch_core::types::Hash32>,
        expected_genesis: Option<conch_core::types::Hash32>,
    ) -> Result<conch_core::types::ChainState, DaemonError> {
        let (socket, _) = connect_async(endpoint)
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

fn router(daemon: Daemon) -> Router {
    Router::new()
        .route("/ticket/{id}", get(get_ticket))
        .route("/history/{id}", get(get_history))
        .route("/swarm", get(ws_swarm))
        .route("/client", get(ws_client))
        .route("/", get(index))
        .route("/ui/", get(index))
        .route("/ui/app.js", get(app_js))
        .route("/ui/app.css", get(app_css))
        .with_state(daemon)
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

async fn get_ticket(
    State(daemon): State<Daemon>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<Ticket>, HttpError> {
    let room = parse_room(&id)?;
    authorize(&daemon, room, &headers)?;
    let ticket = task::spawn_blocking(move || daemon.served_ticket(room)).await??;
    Ok(Json(ticket))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct HistoryQuery {
    #[serde(default)]
    from: u64,
}

async fn get_history(
    State(daemon): State<Daemon>,
    Path(id): Path<String>,
    Query(query): Query<HistoryQuery>,
    headers: HeaderMap,
) -> Result<Json<Value>, HttpError> {
    let room = parse_room(&id)?;
    authorize(&daemon, room, &headers)?;
    Ok(Json(serde_json::to_value(
        daemon.history_from(room, query.from)?,
    )?))
}

async fn ws_swarm(State(daemon): State<Daemon>, upgrade: WebSocketUpgrade) -> impl IntoResponse {
    upgrade
        .on_upgrade(move |socket| websocket_bridge(socket, daemon, ConnectionProtocol::Swarm, None))
}

#[derive(Debug, Default, Deserialize)]
struct ClientAccess {
    room: Option<RoomId>,
    token: Option<Hash32>,
}

async fn ws_client(
    State(daemon): State<Daemon>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Query(access): Query<ClientAccess>,
    upgrade: WebSocketUpgrade,
) -> Response<Body> {
    let allowed_room = if peer.ip().is_loopback() {
        None
    } else {
        let Some(room) = access.room else {
            return StatusCode::FORBIDDEN.into_response();
        };
        match daemon.token_sha256(room) {
            Ok(None) => Some(room),
            Ok(Some(_))
                if access
                    .token
                    .is_some_and(|token| daemon.authenticate(room, token).unwrap_or(false)) =>
            {
                Some(room)
            }
            _ => return StatusCode::FORBIDDEN.into_response(),
        }
    };
    upgrade
        .on_upgrade(move |socket| {
            websocket_bridge(socket, daemon, ConnectionProtocol::Client, allowed_room)
        })
        .into_response()
}

async fn websocket_bridge(
    socket: WebSocket,
    daemon: Daemon,
    protocol: ConnectionProtocol,
    allowed_room: Option<RoomId>,
) {
    let (bridge, daemon_stream) = duplex(BRIDGE_CAPACITY);
    let (mut bridge_reader, mut bridge_writer) = split(bridge);
    let (mut socket_writer, mut socket_reader) = socket.split();
    let (outgoing_tx, mut outgoing_rx) = mpsc::unbounded_channel();
    let daemon_task = tokio::spawn(async move {
        let _ = daemon.handle_transport(daemon_stream, protocol).await;
    });
    let control_tx = outgoing_tx.clone();
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
                            if control_tx.send(Message::Text(text.into())).is_err() {
                                break;
                            }
                            continue;
                        }
                    }
                    let Ok(encoded) = frame::encode(&value) else {
                        break;
                    };
                    if bridge_writer.write_all(&encoded).await.is_err() {
                        break;
                    }
                }
                Message::Ping(bytes) => {
                    if control_tx.send(Message::Pong(bytes)).is_err() {
                        break;
                    }
                }
                Message::Close(_) => break,
                Message::Binary(_) | Message::Pong(_) => {}
            }
        }
    });
    let mut egress = tokio::spawn(async move {
        while let Ok(Some(value)) = read_frame::<_, Value>(&mut bridge_reader).await {
            let Ok(text) = serde_json::to_string(&value) else {
                break;
            };
            if outgoing_tx.send(Message::Text(text.into())).is_err() {
                break;
            }
        }
    });
    let mut sender = tokio::spawn(async move {
        while let Some(message) = outgoing_rx.recv().await {
            if socket_writer.send(message).await.is_err() {
                break;
            }
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
    let (outgoing_tx, mut outgoing_rx) = mpsc::unbounded_channel();
    let control_tx = outgoing_tx.clone();
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
                    if bridge_writer.write_all(&encoded).await.is_err() {
                        break;
                    }
                }
                TungsteniteMessage::Ping(bytes) => {
                    if control_tx.send(TungsteniteMessage::Pong(bytes)).is_err() {
                        break;
                    }
                }
                TungsteniteMessage::Close(_) => break,
                TungsteniteMessage::Binary(_)
                | TungsteniteMessage::Pong(_)
                | TungsteniteMessage::Frame(_) => {}
            }
        }
    });
    let mut egress = tokio::spawn(async move {
        while let Ok(Some(value)) = read_frame::<_, Value>(&mut bridge_reader).await {
            let Ok(text) = serde_json::to_string(&value) else {
                break;
            };
            if outgoing_tx
                .send(TungsteniteMessage::Text(text.into()))
                .is_err()
            {
                break;
            }
        }
    });
    let mut sender = tokio::spawn(async move {
        while let Some(message) = outgoing_rx.recv().await {
            if socket_writer.send(message).await.is_err() {
                break;
            }
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

fn parse_room(id: &str) -> Result<RoomId, HttpError> {
    RoomId::from_str(id).map_err(|_| HttpError::BadRequest("invalid room id"))
}

fn authorize(daemon: &Daemon, room: RoomId, headers: &HeaderMap) -> Result<(), HttpError> {
    if daemon.token_sha256(room)?.is_none() {
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

#[derive(Debug)]
enum HttpError {
    BadRequest(&'static str),
    Unauthorized,
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
