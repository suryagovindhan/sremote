//! # sRemote Session Broker  (broker.exe)
//!
//! Secure WebSocket signaling server. Validates JWT on every connection,
//! manages concurrent session "rooms," relays SDP Offer/Answer and ICE
//! candidates between pairs of peers, injects Coturn TURN credentials,
//! and notifies peers when both sides of a session are present.
//!
//! ## Run
//! ```
//! broker.exe     # reads .env from cwd, binds to BROKER_ADDR (default 0.0.0.0:5695)
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
use std::{collections::HashMap, env, net::SocketAddr, sync::Arc, time::Duration};
use tokio::sync::{broadcast, RwLock};
use tower_http::cors::{Any, CorsLayer};
use tracing::{error, info, warn};

// ─── Shared state ─────────────────────────────────────────────────────────────

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
    tx: broadcast::Sender<RelayMessage>,
    peer_count: usize,
}

// ─── JWT ──────────────────────────────────────────────────────────────────────

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

// ─── ICE / TURN config ────────────────────────────────────────────────────────

#[derive(Serialize)]
struct IceServer {
    urls: Vec<String>,
    #[serde(skip_serializing_if = "String::is_empty")]
    username: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    credential: String,
}

/// Sent to every peer immediately after authentication.
#[derive(Serialize)]
struct ConfigMessage {
    #[serde(rename = "type")]
    kind:        String,
    ice_servers: Vec<IceServer>,
}

fn build_ice_servers(state: &AppState) -> Vec<IceServer> {
    let mut ice_servers = vec![IceServer {
        urls:       vec!["stun:stun.l.google.com:19302".into()],
        username:   String::new(),
        credential: String::new(),
    }];

    let turn_is_placeholder = state.turn_url.eq_ignore_ascii_case("turn:localhost:3478")
        && state.turn_user == "sremote"
        && state.turn_secret == "coturn-static-secret";

    if !state.turn_url.trim().is_empty()
        && !state.turn_secret.trim().is_empty()
        && !turn_is_placeholder
    {
        ice_servers.insert(0, IceServer {
            urls:       vec![state.turn_url.clone()],
            username:   state.turn_user.clone(),
            credential: state.turn_secret.clone(),
        });
    } else {
        info!("TURN not configured; sending STUN-only ICE config");
    }

    ice_servers
}

// ─── Relay payload ────────────────────────────────────────────────────────────

/// `(sender_id, raw_json_text)` — broker stays JSON-agnostic for relay.
type RelayMessage = (String, String);

// ─── RAII cleanup guard ───────────────────────────────────────────────────────

/// Decrements peer_count and removes the room when the last peer leaves,
/// even if the connection drops without a clean WebSocket Close frame.
struct RoomGuard {
    rooms:   Arc<RwLock<HashMap<String, RoomHandle>>>,
    room_id: String,
}

impl Drop for RoomGuard {
    fn drop(&mut self) {
        let rooms   = self.rooms.clone();
        let room_id = self.room_id.clone();
        // Spawn a cleanup task — safe because we always have a Tokio runtime.
        tokio::spawn(async move {
            let mut map = rooms.write().await;
            if let Some(r) = map.get_mut(&room_id) {
                r.peer_count = r.peer_count.saturating_sub(1);
                if r.peer_count == 0 {
                    map.remove(&room_id);
                    info!("Room '{}' removed (empty)", room_id);
                }
            }
        });
    }
}

// ─── Entry point ──────────────────────────────────────────────────────────────

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

    // Allow any origin so the viewer can be served from a different host/port.
    // Tighten this to specific origins in a production deployment.
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_headers(Any)
        .allow_methods(Any);

    let app = Router::new()
        .route("/ws/:room_id", get(ws_handler))
        .route("/health",      get(|| async { "OK" }))
        .layer(cors)
        .with_state(state);

    info!("Session Broker listening on {}", addr);
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}

// ─── WebSocket upgrade handler ────────────────────────────────────────────────

async fn ws_handler(
    ws:            WebSocketUpgrade,
    Path(room_id): Path<String>,
    Query(params): Query<ConnectQuery>,
    State(state):  State<AppState>,
) -> Result<Response, StatusCode> {
    let claims = validate_jwt(&params.token, &state.jwt_secret)?;

    if claims.room != room_id {
        warn!("Token room '{}' ≠ URL room '{}' — rejecting", claims.room, room_id);
        return Err(StatusCode::FORBIDDEN);
    }

    info!("Peer '{}' ({}) joining room '{}'", claims.sub, claims.role, room_id);
    Ok(ws.on_upgrade(move |sock| handle_socket(sock, claims, room_id, state)))
}

// ─── Per-connection relay loop ────────────────────────────────────────────────

async fn handle_socket(socket: WebSocket, claims: Claims, room_id: String, state: AppState) {
    // Acquire or create the room and increment peer count.
    let (tx, mut rx, notify_pair) = {
        let mut rooms = state.rooms.write().await;
        let room = rooms.entry(room_id.clone()).or_insert_with(|| {
            let (tx, _) = broadcast::channel(256);
            RoomHandle { tx, peer_count: 0 }
        });
        room.peer_count += 1;
        let notify_pair = room.peer_count >= 2;
        let tx = room.tx.clone();
        let rx = tx.subscribe();
        (tx, rx, notify_pair)
    };

    // RAII guard: cleanup runs even if this task panics or is dropped mid-loop.
    let _guard = RoomGuard { rooms: state.rooms.clone(), room_id: room_id.clone() };

    let (mut ws_tx, mut ws_rx) = socket.split();

    // ── Send ICE/TURN config immediately ──────────────────────────────────────
    let cfg = ConfigMessage {
        kind:        "config".into(),
        ice_servers: build_ice_servers(&state),
    };
    if ws_tx
        .send(Message::Text(serde_json::to_string(&cfg).unwrap()))
        .await
        .is_err()
    {
        return; // guard's Drop handles cleanup
    }

    // ── If a pair is now complete, tell both peers so the daemon creates its offer ─
    // This prevents the race where the daemon sends an offer before the viewer joins.
    if notify_pair {
        let notice = r#"{"type":"peer_joined"}"#.to_string();
        let _ = tx.send(("broker".to_string(), notice));
        info!("Room '{}' now has 2 peers — peer_joined broadcast sent", room_id);
    }

    let my_id = claims.sub.clone();
    info!("Peer '{}' ready – room '{}'", my_id, room_id);

    // ── Main relay loop with server-initiated keepalive ───────────────────────
    let mut ping_interval = tokio::time::interval(Duration::from_secs(20));
    ping_interval.tick().await; // discard the instant first tick

    loop {
        tokio::select! {
            biased; // check ws_rx first to prioritise inbound messages

            // Inbound from this peer → broadcast to room
            msg = ws_rx.next() => match msg {
                Some(Ok(Message::Text(text))) => {
                    let _ = tx.send((my_id.clone(), text.to_string()));
                }
                Some(Ok(Message::Ping(b))) => {
                    let _ = ws_tx.send(Message::Pong(b)).await;
                }
                Some(Ok(Message::Pong(_))) => {} // response to our keepalive pings
                Some(Ok(Message::Close(_))) | None => {
                    info!("Peer '{}' left room '{}'", my_id, room_id);
                    break;
                }
                Some(Err(e)) => {
                    error!("WS error for '{}': {}", my_id, e);
                    break;
                }
                _ => {}
            },

            // Outbound from room → forward to this peer (skip own messages)
            bcast = rx.recv() => match bcast {
                Ok((from, raw)) => {
                    if from == my_id { continue; }
                    if ws_tx.send(Message::Text(raw.into())).await.is_err() { break; }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    // Peer missed critical signaling messages (ICE candidates, etc.)
                    // — it cannot recover a valid session. Disconnect cleanly.
                    warn!("Peer '{}' lagged {} messages — disconnecting", my_id, n);
                    break;
                }
                Err(broadcast::error::RecvError::Closed) => break,
            },

            // Server-initiated keepalive to prevent NAT/LB idle-timeout drops
            _ = ping_interval.tick() => {
                if ws_tx.send(Message::Ping(b"ka".to_vec())).await.is_err() {
                    break;
                }
            }
        }
    }
    // _guard dropped here → cleanup runs automatically
}
