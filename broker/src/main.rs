//! # sRemote Session Broker  (broker.exe)
//!
//! Secure WebSocket signaling server.  Validates JWT on every connection,
//! manages concurrent session "rooms," relays SDP Offer/Answer and ICE
//! candidates between pairs of peers, and injects Coturn TURN credentials.
//!
//! ## Run
//! ```
//! broker.exe          # reads .env from cwd, binds to BROKER_ADDR (default 0.0.0.0:5695)
//! ```

use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Path, Query, State,
    },
    http::StatusCode,
    response::Response,
    routing::get,
    Router,
};
use dotenvy::dotenv;
use futures::{SinkExt, StreamExt};
use jsonwebtoken::{decode, Algorithm, DecodingKey, Validation};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, env, net::SocketAddr, sync::Arc};
use tokio::sync::{broadcast, RwLock};
use tracing::{error, info, warn};

// ─── Shared state ────────────────────────────────────────────────────────────

#[derive(Clone)]
struct AppState {
    rooms:       Arc<RwLock<HashMap<String, RoomHandle>>>,
    jwt_secret:  String,
    turn_url:    String,
    turn_user:   String,
    turn_secret: String,
}

struct RoomHandle {
    /// Every peer in the room subscribes to this channel.
    tx:         broadcast::Sender<RelayMessage>,
    peer_count: usize,
}

// ─── JWT ─────────────────────────────────────────────────────────────────────

/// Claims embedded inside every JWT token.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Claims {
    sub:  String, // unique peer ID
    role: String, // "agent" | "technician"
    room: String, // room the token grants access to
    exp:  usize,  // unix timestamp expiry
}

#[derive(Deserialize)]
struct ConnectQuery {
    token: String,
}

fn validate_jwt(token: &str, secret: &str) -> Result<Claims, StatusCode> {
    let key = DecodingKey::from_secret(secret.as_bytes());
    let mut v = Validation::new(Algorithm::HS256);
    v.validate_exp = true;
    decode::<Claims>(token, &key, &v)
        .map(|d| d.claims)
        .map_err(|e| {
            warn!("JWT rejected: {}", e);
            StatusCode::UNAUTHORIZED
        })
}

// ─── ICE / TURN config ───────────────────────────────────────────────────────

#[derive(Serialize)]
struct IceServer {
    urls: Vec<String>,
    #[serde(skip_serializing_if = "String::is_empty")]
    username: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    credential: String,
}

/// Sent to every peer immediately after authentication so the WebRTC
/// PeerConnection can be initialised with the correct TURN relay.
#[derive(Serialize)]
struct ConfigMessage {
    #[serde(rename = "type")]
    kind:        String,
    ice_servers: Vec<IceServer>,
}

fn build_ice_servers(state: &AppState) -> Vec<IceServer> {
    let mut ice_servers = vec![IceServer {
        urls: vec!["stun:stun.l.google.com:19302".into()],
        username: String::new(),
        credential: String::new(),
    }];

    let turn_is_placeholder = state.turn_url.eq_ignore_ascii_case("turn:localhost:3478")
        && state.turn_user == "sremote"
        && state.turn_secret == "coturn-static-secret";

    if !state.turn_url.trim().is_empty()
        && !state.turn_secret.trim().is_empty()
        && !turn_is_placeholder
    {
        ice_servers.insert(
            0,
            IceServer {
                urls: vec![state.turn_url.clone()],
                username: state.turn_user.clone(),
                credential: state.turn_secret.clone(),
            },
        );
    } else {
        info!("TURN not configured; sending STUN-only ICE config");
    }

    ice_servers
}

// ─── Relay payload ───────────────────────────────────────────────────────────

/// Fast-path signaling relay: `(sender_id, raw_json_text)`.
/// The broker stays JSON-agnostic and simply forwards text frames.
type RelayMessage = (String, String);

// ─── Entry point ─────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() {
    dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("broker=info".parse().unwrap()),
        )
        .init();

    let jwt_secret  = env::var("JWT_SECRET").expect("JWT_SECRET missing in .env");
    let turn_url    = env::var("TURN_URL").unwrap_or_default();
    let turn_user   = env::var("TURN_USER").unwrap_or_else(|_| "sremote".into());
    let turn_secret = env::var("TURN_SECRET").unwrap_or_default();
    let addr: SocketAddr = env::var("BROKER_ADDR")
        .unwrap_or_else(|_| "0.0.0.0:5695".into())
        .parse()
        .expect("BROKER_ADDR is not a valid socket address");

    let state = AppState {
        rooms: Arc::new(RwLock::new(HashMap::new())),
        jwt_secret,
        turn_url,
        turn_user,
        turn_secret,
    };

    let app = Router::new()
        .route("/ws/:room_id", get(ws_handler))
        .route("/health",      get(|| async { "OK" }))
        .with_state(state);

    info!("Session Broker listening on {}", addr);
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}

// ─── WebSocket upgrade handler ────────────────────────────────────────────────

async fn ws_handler(
    ws:               WebSocketUpgrade,
    Path(room_id):    Path<String>,
    Query(params):    Query<ConnectQuery>,
    State(state):     State<AppState>,
) -> Result<Response, StatusCode> {
    let claims = validate_jwt(&params.token, &state.jwt_secret)?;

    if claims.room != room_id {
        warn!("Token room '{}' ≠ URL room '{}'", claims.room, room_id);
        return Err(StatusCode::FORBIDDEN);
    }

    info!("Peer '{}' ({}) joining room '{}'", claims.sub, claims.role, room_id);
    Ok(ws.on_upgrade(move |sock| handle_socket(sock, claims, room_id, state)))
}

// ─── Per-connection relay loop ────────────────────────────────────────────────

async fn handle_socket(socket: WebSocket, claims: Claims, room_id: String, state: AppState) {
    // Acquire or create the room.
    let (tx, mut rx) = {
        let mut rooms = state.rooms.write().await;
        let room = rooms.entry(room_id.clone()).or_insert_with(|| {
            let (tx, _) = broadcast::channel(1024);
            RoomHandle { tx, peer_count: 0 }
        });
        room.peer_count += 1;
        let tx = room.tx.clone();
        let rx = tx.subscribe();
        (tx, rx)
    };

    let (mut ws_tx, mut ws_rx) = socket.split();

    // ── Send ICE/TURN config immediately ────────────────────────────────────
    let cfg = ConfigMessage {
        kind: "config".into(),
        ice_servers: build_ice_servers(&state),
    };
    if ws_tx
        .send(Message::Text(serde_json::to_string(&cfg).unwrap()))
        .await
        .is_err()
    {
        cleanup(&state.rooms, &room_id).await;
        return;
    }

    let my_id = claims.sub.clone();
    info!("Peer '{}' ready – room '{}'", my_id, room_id);

    // ── Main relay loop ──────────────────────────────────────────────────────
    loop {
        tokio::select! {
            // Inbound from this peer → broadcast to room
            msg = ws_rx.next() => match msg {
                Some(Ok(Message::Text(text))) => {
                    let _ = tx.send((my_id.clone(), text.to_string()));
                }
                Some(Ok(Message::Ping(b))) => { let _ = ws_tx.send(Message::Pong(b)).await; }
                Some(Ok(Message::Close(_))) | None => {
                    info!("Peer '{}' left room '{}'", my_id, room_id);
                    break;
                }
                Some(Err(e)) => { error!("WS error for '{}': {}", my_id, e); break; }
                _ => {}
            },

            // Outbound from room → forward to this peer (skip our own)
            bcast = rx.recv() => match bcast {
                Ok((from, raw)) => {
                    if from == my_id { continue; }
                    if ws_tx.send(Message::Text(raw.into())).await.is_err() { break; }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    warn!("Peer '{}' lagged {} messages", my_id, n);
                }
                Err(broadcast::error::RecvError::Closed) => break,
            },
        }
    }

    cleanup(&state.rooms, &room_id).await;
}

async fn cleanup(rooms: &Arc<RwLock<HashMap<String, RoomHandle>>>, room_id: &str) {
    let mut map = rooms.write().await;
    if let Some(r) = map.get_mut(room_id) {
        r.peer_count = r.peer_count.saturating_sub(1);
        if r.peer_count == 0 {
            map.remove(room_id);
            info!("Room '{}' removed (empty)", room_id);
        }
    }
}
