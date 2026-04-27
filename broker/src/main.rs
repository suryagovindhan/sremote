//! # sRemote Session Broker  (broker.exe) — v1.1.0
//!
//! Dual-mode WebSocket gateway:
//!
//! **Mode A — WebRTC Signaling (Agent-based sessions)**
//! Validates JWT, manages session "rooms," relays SDP Offer/Answer and ICE
//! candidates, injects Coturn TURN credentials.
//!
//! **Mode B — Native RDP Bridge (Agentless sessions)**
//! Uses `ironrdp` to terminate the RDP protocol server-side:
//!   1. TLS upgrade → optional NLA/CredSSP handshake
//!   2. Active-stage decode loop → encode dirty regions as WebP (lossless, no text blur)
//!   3. Push binary frames over WebSocket → browser Canvas renders them
//!   4. Receive JSON mouse/keyboard events → inject as RDP Fast-Path Input PDUs
//!
//! Binary frame wire format (sent to browser):
//!   [0..1] frame_width  u16 LE
//!   [2..3] frame_height u16 LE
//!   [4..5] x_offset     u16 LE
//!   [6..7] y_offset     u16 LE
//!   [8..]  WebP bytes (lossless)

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
use tokio::io::AsyncReadExt;
use tokio::sync::mpsc;
use dotenvy::dotenv;
use futures::{SinkExt, StreamExt};
use ironrdp::{
    connector::{self, Credentials},
    session::{image::DecodedImage, ActiveStage, ActiveStageOutput},
};
use ironrdp_blocking::Framed as BlockingFramed;
use ironrdp_connector::ConnectionResult;
use ironrdp_pdu::{
    gcc::KeyboardType,
    input::{
        fast_path::{FastPathInput, FastPathInputEvent, KeyboardFlags},
        mouse::{MousePdu, PointerFlags},
    },
    rdp::{
        capability_sets::MajorPlatformType,
        client_info::{PerformanceFlags, TimezoneInfo},
    },
};
use jsonwebtoken::{decode, Algorithm, DecodingKey, Validation};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap, env, net::SocketAddr, sync::Arc, time::Duration,
};
use tokio::sync::{broadcast, Mutex, RwLock};
use tower_http::cors::{Any, CorsLayer};
use tracing::{error, info, warn};

// ─── Shared state ─────────────────────────────────────────────────────────────

type InputRegistry = Arc<Mutex<HashMap<String, mpsc::Sender<RdpInput>>>>;

#[derive(Clone)]
struct AppState {
    rooms:          Arc<RwLock<HashMap<String, RoomHandle>>>,
    jwt_secret:     String,
    turn_url:       String,
    turn_user:      String,
    turn_secret:    String,
    input_registry: InputRegistry,
}

struct RoomHandle {
    tx:         broadcast::Sender<RelayMessage>,
    peer_count: usize,
}

// ─── JWT ──────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Claims {
    sub:        String,
    role:       String,
    room:       String,
    exp:        usize,
    rdp_target: Option<RdpTarget>,
    auth_mode:  Option<String>, // "nla" | "interactive"
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RdpTarget {
    host:     String,
    port:     u16,
    username: String,
    password: String,
    domain:   Option<String>,
}

#[derive(Deserialize)]
struct ConnectQuery {
    token: String,
}

fn validate_jwt(token: &str, secret: &str) -> Result<Claims, StatusCode> {
    let key = DecodingKey::from_secret(secret.as_bytes());
    let mut v = Validation::new(Algorithm::HS256);
    v.validate_exp = true;
    v.validate_aud = false; // Django injects 'aud', but we handle it manually
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

type RelayMessage = (String, String);

// ─── RAII cleanup guard ───────────────────────────────────────────────────────

struct RoomGuard {
    rooms:   Arc<RwLock<HashMap<String, RoomHandle>>>,
    room_id: String,
}

impl Drop for RoomGuard {
    fn drop(&mut self) {
        let rooms   = self.rooms.clone();
        let room_id = self.room_id.clone();
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
        .unwrap_or_else(|_| "127.0.0.1:5695".into())
        .parse()
        .expect("BROKER_ADDR is not a valid socket address");

    let state = AppState {
        rooms: Arc::new(RwLock::new(HashMap::new())),
        jwt_secret,
        turn_url,
        turn_user,
        turn_secret,
        input_registry: Arc::new(Mutex::new(HashMap::new())),
    };

    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_headers(Any)
        .allow_methods(Any);

    let app = Router::new()
        .route("/ws/:room_id", get(ws_handler))
        .route("/health",      get(|| async { "OK" }))
        .layer(cors)
        .with_state(state);

    info!("Session Broker v1.1 listening on {}", addr);
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

    // Branch: agentless RDP session → Native RDP Bridge
    if let Some(target) = claims.rdp_target.clone() {
        let auth_mode = claims.auth_mode.clone().unwrap_or_else(|| "nla".into());
        info!(
            "RDP native bridge: room='{}' target={}:{} auth={}",
            room_id, target.host, target.port, auth_mode
        );
        return Ok(ws.on_upgrade(move |sock| {
            handle_rdp_native(sock, target, auth_mode, room_id, state)
        }));
    }

    // Default: WebRTC signaling relay
    Ok(ws.on_upgrade(move |sock| handle_socket(sock, claims, room_id, state)))
}

// ─── WebRTC signaling relay (Agent-based sessions) ───────────────────────────

async fn handle_socket(socket: WebSocket, claims: Claims, room_id: String, state: AppState) {
    let (tx, mut rx, notify_pair, peer_count) = {
        let mut rooms = state.rooms.write().await;
        let room = rooms.entry(room_id.clone()).or_insert_with(|| {
            let (tx, _) = broadcast::channel(256);
            RoomHandle { tx, peer_count: 0 }
        });
        room.peer_count += 1;
        let notify_pair = room.peer_count >= 2;
        let tx = room.tx.clone();
        let rx = tx.subscribe();
        (tx, rx, notify_pair, room.peer_count)
    };

    let _guard = RoomGuard { rooms: state.rooms.clone(), room_id: room_id.clone() };

    let (mut ws_tx, mut ws_rx) = socket.split();

    let cfg = ConfigMessage {
        kind:        "config".into(),
        ice_servers: build_ice_servers(&state),
    };
    if ws_tx
        .send(Message::Text(serde_json::to_string(&cfg).unwrap().into()))
        .await
        .is_err()
    {
        return;
    }

    if notify_pair {
        let notice = format!(r#"{{"type":"peer_joined","role":"{}"}}"#, claims.role);
        let _ = tx.send(("broker".to_string(), notice));
        info!("Room '{}' now has {} peers — peer_joined (role={}) broadcast sent", room_id, peer_count, claims.role);
    }

    let my_id = claims.sub.clone();
    info!("Peer '{}' ready – room '{}'", my_id, room_id);

    let mut ping_interval = tokio::time::interval(Duration::from_secs(20));
    ping_interval.tick().await;

    loop {
        tokio::select! {
            biased;

            msg = ws_rx.next() => match msg {
                Some(Ok(Message::Text(text))) => {
                    let _ = tx.send((my_id.clone(), text.to_string()));
                }
                Some(Ok(Message::Ping(b))) => {
                    let _ = ws_tx.send(Message::Pong(b)).await;
                }
                Some(Ok(Message::Pong(_))) => {}
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

            bcast = rx.recv() => match bcast {
                Ok((from, raw)) => {
                    if from == my_id { continue; }
                    if ws_tx.send(Message::Text(raw.into())).await.is_err() { break; }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    warn!("Peer '{}' lagged {} messages — disconnecting", my_id, n);
                    break;
                }
                Err(broadcast::error::RecvError::Closed) => break,
            },

            _ = ping_interval.tick() => {
                if ws_tx.send(Message::Ping(b"ka".to_vec())).await.is_err() {
                    break;
                }
            }
        }
    }
}

// ─── Agentless RDP — Native Bridge ────────────────────────────────────────────

/// JSON commands browser → broker
#[derive(Deserialize, Debug)]
#[serde(tag = "type", rename_all = "snake_case")]
enum BrowserCommand {
    NlaCredentials {
        username: String,
        password: String,
        domain:   Option<String>,
    },
    StartInteractive,
    MouseMove   { x: i32, y: i32 },
    MouseButton { x: i32, y: i32, button: String, down: bool },
    Key         { code: String, down: bool },
    SendCtrlAltDel,
    FrameAck,
}

/// RDP input events forwarded from the async WS reader to the blocking RDP thread
enum RdpInput {
    MouseMove   { x: u16, y: u16 },
    MouseButton { x: u16, y: u16, flags: u16 },
    Key         { scancode: u8, extended: bool, down: bool },
    CtrlAltDel,
    FrameAck,
}

async fn rdp_input_handler(
    ws:      WebSocketUpgrade,
    Path(room_id): Path<String>,
    State(state): State<AppState>,
) -> Response {
    ws.on_upgrade(|socket| handle_rdp_input_only(socket, room_id, state))
}

async fn handle_rdp_input_only(mut socket: WebSocket, room_id: String, state: AppState) {
    info!("rdp_input: dedicated channel opened for room '{}'", room_id);
    while let Some(Ok(Message::Text(text))) = socket.next().await {
        let cmd: BrowserCommand = match serde_json::from_str(&text) {
            Ok(c) => c,
            Err(_) => continue,
        };
        
        let registry = state.input_registry.lock().await;
        if let Some(tx) = registry.get(&room_id) {
            let rdpi = match cmd {
                BrowserCommand::FrameAck => RdpInput::FrameAck,
                BrowserCommand::MouseMove { x, y } => RdpInput::MouseMove { x: x as u16, y: y as u16 },
                BrowserCommand::MouseButton { x, y, button, down } => {
                     // ... reuse logic ...
                     let flags = match (button.as_str(), down) {
                         ("left", true)   => 0x01, ("left", false)  => 0x00,
                         ("right", true)  => 0x02, ("right", false) => 0x00,
                         _ => 0,
                     };
                     RdpInput::MouseButton { x: x as u16, y: y as u16, flags }
                }
                BrowserCommand::Key { code, down } => {
                    let (sc, ext) = scancode_from_code(&code).unwrap_or((0, false));
                    RdpInput::Key { scancode: sc, extended: ext, down }
                }
                _ => continue,
            };
            let _ = tx.send(rdpi).await;
        }
    }
}

async fn handle_rdp_native(
    socket:    WebSocket,
    target:    RdpTarget,
    auth_mode: String,
    room_id:   String,
    state:     AppState,
) {
    let (mut ws_tx, mut ws_rx) = socket.split();

    // ── 1. Determine credentials ──────────────────────────────────────────────
    let credentials = if auth_mode == "interactive" {
        Credentials::UsernamePassword {
            username: target.username.clone(),
            password: target.password.clone(),
        }
    } else {
        // NLA: check if pre-stored creds exist; otherwise wait for browser to submit
        if target.username.is_empty() {
            let auth_req = serde_json::json!({
                "type": "auth_required",
                "auth_mode": "nla",
                "message": "Enter credentials for NLA authentication"
            });
            if ws_tx.send(Message::Text(auth_req.to_string().into())).await.is_err() {
                return;
            }
            // Wait for nla_credentials message from the browser modal
            let creds = loop {
                match ws_rx.next().await {
                    Some(Ok(Message::Text(t))) => {
                        if let Ok(BrowserCommand::NlaCredentials { username, password, .. }) =
                            serde_json::from_str::<BrowserCommand>(&t)
                        {
                            break Credentials::UsernamePassword { username, password };
                        }
                    }
                    _ => {
                        info!("rdp_native: WS closed while awaiting NLA creds — room '{}'", room_id);
                        return;
                    }
                }
            };
            creds
        } else {
            Credentials::UsernamePassword {
                username: target.username.clone(),
                password: target.password.clone(),
            }
        }
    };

    // ── 2. Notify browser: connecting ─────────────────────────────────────────
    let _ = ws_tx.send(Message::Text(
        serde_json::json!({"type":"rdp_connecting","message":"Establishing RDP session…"})
            .to_string().into()
    )).await;

    // ── 3. Spawn blocking thread — TLS + NLA handshake ────────────────────────
    let host       = target.host.clone();
    let port       = target.port;
    let domain     = target.domain.clone();
    let enable_nla = auth_mode != "interactive";

    let connect_result = tokio::task::spawn_blocking(move || {
        rdp_connect_blocking(host, port, credentials, domain, enable_nla)
    })
    .await;

    let (conn_result, framed) = match connect_result {
        Ok(Ok(pair)) => pair,
        Ok(Err(e)) => {
            error!("rdp_native: handshake failed: {}", e);
            let _ = ws_tx.send(Message::Text(
                serde_json::json!({"type":"error","message": format!("RDP handshake failed: {}", e)})
                    .to_string().into()
            )).await;
            return;
        }
        Err(e) => {
            error!("rdp_native: spawn_blocking panicked: {}", e);
            return;
        }
    };

    let desktop_width  = conn_result.desktop_size.width;
    let desktop_height = conn_result.desktop_size.height;

    // ── 4. Notify browser: connected ─────────────────────────────────────────
    if ws_tx
        .send(Message::Text(
            serde_json::json!({
                "type":      "rdp_connected",
                "width":     desktop_width,
                "height":    desktop_height,
                "auth_mode": auth_mode
            })
            .to_string()
            .into(),
        ))
        .await
        .is_err()
    {
        return;
    }

    info!(
        "rdp_native: active stage started — {}×{} — room '{}'",
        desktop_width, desktop_height, room_id
    );

    // ── 5. Channels: frame TX (RDP→WS), input RX (WS→RDP) ───────────────────
    let (frame_tx, mut frame_rx) = mpsc::channel::<Vec<u8>>(1); 
    let (input_tx, mut input_rx_async) = mpsc::channel::<RdpInput>(100);
    let (input_tx_std, input_rx_std) = std::sync::mpsc::channel::<RdpInput>();

    // Register this input channel for the room
    {
        let mut registry = state.input_registry.lock().await;
        registry.insert(room_id.clone(), input_tx.clone());
    }

    // Bridge: Forward async inputs to the blocking std channel
    tokio::spawn(async move {
        while let Some(msg) = input_rx_async.recv().await {
            if input_tx_std.send(msg).is_err() { break; }
        }
    });

    // ── 6. Run active-stage loop in its own blocking thread ──────────────────
    tokio::task::spawn_blocking(move || {
        active_stage_thread(conn_result, framed, frame_tx, input_rx_std);
    });

    // ── 7. Async loop: forward frames → WS, receive input ← WS ──────────────
    let mut ping_interval = tokio::time::interval(Duration::from_secs(20));
    ping_interval.tick().await;

    loop {
        tokio::select! {
            biased;

            frame = frame_rx.recv() => {
                match frame {
                    Some(data) => {
                        if ws_tx.send(Message::Binary(data.into())).await.is_err() {
                            break;
                        }
                    }
                    None => {
                        info!("rdp_native: frame channel closed — room '{}'", room_id);
                        break;
                    }
                }
            }

            msg = ws_rx.next() => match msg {
                Some(Ok(Message::Text(text))) => {
                    let cmd: BrowserCommand = match serde_json::from_str(&text) {
                        Ok(c) => c,
                        Err(_) => continue,
                    };
                    match cmd {
                        BrowserCommand::FrameAck => {
                            let _ = input_tx.send(RdpInput::FrameAck);
                        }
                        BrowserCommand::MouseMove { x, y } => {
                            let _ = input_tx.send(RdpInput::MouseMove { x: x as u16, y: y as u16 });
                        }
                        BrowserCommand::MouseButton { x, y, button, down } => {
                            let flags = match (button.as_str(), down) {
                                ("left", true)   => 0x01,
                                ("left", false)  => 0x00,
                                ("right", true)  => 0x02,
                                ("right", false) => 0x00,
                                _ => 0,
                            };
                            let _ = input_tx.send(RdpInput::MouseButton { x: x as u16, y: y as u16, flags });
                        }
                        _ => {}
                    }
                }
                Some(Ok(Message::Ping(b))) => {
                    let _ = ws_tx.send(Message::Pong(b)).await;
                }
                Some(Ok(Message::Close(_))) | None => {
                    info!("rdp_native: browser disconnected — room '{}'", room_id);
                    break;
                }
                _ => {}
            },

            _ = ping_interval.tick() => {
                if ws_tx.send(Message::Ping(b"ka".to_vec())).await.is_err() {
                    break;
                }
            }
        }
    }

    info!("rdp_native: session closed — room '{}'", room_id);
}

// ─── Blocking: TLS type alias ─────────────────────────────────────────────────

type TlsStream = rustls::StreamOwned<rustls::ClientConnection, std::net::TcpStream>;
type TlsFramed = BlockingFramed<TlsStream>;

// ─── Blocking: RDP connect (TLS + optional NLA/CredSSP) ──────────────────────

fn rdp_connect_blocking(
    host:       String,
    port:       u16,
    creds:      Credentials,
    domain:     Option<String>,
    enable_nla: bool,
) -> anyhow::Result<(ConnectionResult, TlsFramed)> {
    use std::net::TcpStream;

    let addr = {
        use std::net::ToSocketAddrs;
        (host.as_str(), port)
            .to_socket_addrs()?
            .next()
            .ok_or_else(|| anyhow::anyhow!("Could not resolve {}", host))?
    };

    let tcp = TcpStream::connect(addr)?;
    tcp.set_nodelay(true).ok(); 
    tcp.set_read_timeout(Some(Duration::from_secs(5))).ok(); // Handshake needs more than 2ms!
    let client_addr = tcp.local_addr()?;
    let mut framed = BlockingFramed::new(tcp);

    // connector::Config — only fields present in ironrdp-connector 0.8
    let config = connector::Config {
        credentials: creds,
        domain,
        enable_tls:              false, // We do our own TLS upgrade below
        enable_credssp:          enable_nla,
        keyboard_type:           KeyboardType::IbmEnhanced,
        keyboard_subtype:        0,
        keyboard_layout:         0,
        keyboard_functional_keys_count: 12,
        ime_file_name:           String::new(),
        dig_product_id:          String::new(),
        desktop_size:            connector::DesktopSize { width: 1280, height: 1024 },
        bitmap:                  None,
        client_build:            0,
        client_name:             "sRemote-Broker".to_owned(),
        client_dir:              "C:\\Windows\\System32\\mstscax.dll".to_owned(),
        platform:                MajorPlatformType::WINDOWS,
        hardware_id:             None,
        request_data:            None,
        autologon:               false,
        enable_audio_playback:   false,
        performance_flags:       PerformanceFlags::default(),
        license_cache:           None,
        timezone_info:           TimezoneInfo::default(),
        enable_server_pointer:   true,
        pointer_software_rendering: false,
        desktop_scale_factor:    0,
    };

    let mut rdp_connector = connector::ClientConnector::new(config, client_addr);

    // Phase 1: pre-TLS negotiation
    let should_upgrade = ironrdp_blocking::connect_begin(&mut framed, &mut rdp_connector)?;

    // Phase 2: TLS upgrade (synchronous rustls)
    let initial_stream = framed.into_inner_no_leftover();
    let (tls_stream, server_public_key) = tls_upgrade(initial_stream, host.clone())?;
    let upgraded = ironrdp_blocking::mark_as_upgraded(should_upgrade, &mut rdp_connector);
    let mut tls_framed = BlockingFramed::new(tls_stream);

    // Phase 3: CredSSP/NLA + capability exchange
    let mut network_client = sspi::network_client::reqwest_network_client::ReqwestNetworkClient;
    let connection_result = ironrdp_blocking::connect_finalize(
        upgraded,
        rdp_connector,
        &mut tls_framed,
        &mut network_client,
        host.into(),
        server_public_key,
        None,
    )?;

    Ok((connection_result, tls_framed))
}

// ─── Blocking: TLS upgrade using rustls directly ─────────────────────────────

fn tls_upgrade(
    stream:      std::net::TcpStream,
    server_name: String,
) -> anyhow::Result<(TlsStream, Vec<u8>)> {
    use anyhow::Context as _;
    use std::io::Write as _;
    use x509_cert::der::Decode as _;

    let config = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(std::sync::Arc::new(NoCertVerification))
        .with_no_client_auth();

    let mut config = config;
    config.resumption = rustls::client::Resumption::disabled();
    let config = std::sync::Arc::new(config);

    let server_name_ref: rustls::pki_types::ServerName<'static> = server_name
        .clone()
        .try_into()
        .map_err(|_| anyhow::anyhow!("Invalid TLS server name: {}", server_name))?;

    let client = rustls::ClientConnection::new(config, server_name_ref)?;
    let mut tls = rustls::StreamOwned::new(client, stream);
    tls.flush()?;

    let cert = tls
        .conn
        .peer_certificates()
        .and_then(|c| c.first())
        .context("No peer certificate from RDP server")?;

    let parsed  = x509_cert::Certificate::from_der(cert.as_ref())?;
    let pub_key = parsed
        .tbs_certificate
        .subject_public_key_info
        .subject_public_key
        .as_bytes()
        .context("subject_public_key missing")?
        .to_owned();

    Ok((tls, pub_key))
}

// ─── Blocking: Active-stage decode loop ──────────────────────────────────────

fn active_stage_thread(
    connection_result: ConnectionResult,
    mut framed:        TlsFramed,
    frame_tx:          mpsc::Sender<Vec<u8>>,
    input_rx:          std::sync::mpsc::Receiver<RdpInput>,
) {
    let w = connection_result.desktop_size.width;
    let h = connection_result.desktop_size.height;

    let mut active_stage = ActiveStage::new(connection_result);

    let mut image = DecodedImage::new(
        ironrdp_graphics::image_processing::PixelFormat::RgbA32,
        w, h,
    );

    // ─── FFmpeg Video Engine Context ──────────────────────────────────────────
    // Moving video encoding to a separate async task to keep the RDP loop responsive
    let (video_tx, video_rx) = std::sync::mpsc::channel::<Vec<u8>>();
    let frame_tx_for_video = frame_tx.clone();

    tokio::spawn(async move {
        let mut ffmpeg_tx: Option<tokio::process::ChildStdin> = None;
        let mut current_w = 0;
        let mut current_h = 0;

        while let Ok(frame_data) = video_rx.recv() {
             // Logic to (re)spawn ffmpeg if resolution changes
             if w != current_w || h != current_h {
                 if let Ok((tx, mut rx)) = spawn_ffmpeg(w, h) {
                     ffmpeg_tx = Some(tx);
                     current_w = w;
                     current_h = h;
                     
                     let frame_tx_inner = frame_tx_for_video.clone();
                     tokio::spawn(async move {
                         let mut buf = vec![0u8; 65536];
                         while let Ok(n) = rx.read(&mut buf).await {
                             if n == 0 { break; }
                             let mut out = Vec::with_capacity(n + 1);
                             out.push(4u8); // Type 4 = H.264
                             out.extend_from_slice(&buf[..n]);
                             if frame_tx_inner.send(out).await.is_err() { break; }
                         }
                     });
                 }
             }

             if let Some(tx) = &mut ffmpeg_tx {
                 use tokio::io::AsyncWriteExt;
                 if tx.write_all(&frame_data).await.is_err() {
                     ffmpeg_tx = None; 
                 }
             }
        }
    });

    let mut last_flush = std::time::Instant::now();
    let mut dirty_rect: Option<ironrdp_pdu::geometry::InclusiveRectangle> = None;
    let mut awaiting_ack = false;
    const FRAME_MS: u128 = 16; 
    const ACK_TIMEOUT_MS: u128 = 200;

    'outer: loop {
        // Use a short timeout for polling during the active session
        let _ = framed.get_inner_mut().0.get_mut().set_read_timeout(Some(Duration::from_millis(5)));

        // Drain all pending input events first (non-blocking)
        while let Ok(ev) = input_rx.try_recv() {
            match ev {
                RdpInput::FrameAck => {
                    awaiting_ack = false;
                }
                _ => {
                    match build_fast_path_input(ev) {
                        Ok(Some(pdu_bytes)) => {
                            if let Err(e) = framed.write_all(&pdu_bytes) {
                                error!("rdp_native: write input PDU: {}", e);
                                break 'outer;
                            }
                        }
                        Ok(None) => {}
                        Err(e) => error!("rdp_native: encode input: {}", e),
                    }
                }
            }
        }

        // Safety timeout to prevent stalls if an ACK is lost
        if awaiting_ack && last_flush.elapsed().as_millis() > ACK_TIMEOUT_MS {
            awaiting_ack = false;
        }

        // Read one PDU from the RDP connection
        let (action, payload) = match framed.read_pdu() {
            Ok(pair) => pair,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock || e.kind() == std::io::ErrorKind::TimedOut => {
                // No data yet — maybe flush a pending frame
                if let Some(rect) = dirty_rect.as_ref() {
                    if last_flush.elapsed().as_millis() >= FRAME_MS {
                        if let Some(frame) = encode_frame(&image, rect.clone()) {
                            if frame_tx.blocking_send(frame).is_err() {
                                break;
                            }
                        }
                        dirty_rect = None;
                        last_flush = std::time::Instant::now();
                    }
                }
                std::thread::sleep(Duration::from_millis(5));
                continue;
            }
            Err(e) => {
                error!("rdp_native: read PDU: {}", e);
                break;
            }
        };

        let outputs = match active_stage.process(&mut image, action, &payload) {
            Ok(o)  => o,
            Err(e) => { error!("rdp_native: active stage process: {}", e); break; }
        };

        for out in outputs {
            match out {
                ActiveStageOutput::ResponseFrame(bytes) => {
                    if framed.write_all(&bytes).is_err() {
                        break 'outer;
                    }
                }
                ActiveStageOutput::GraphicsUpdate(rect) => {
                    if let Some(existing) = dirty_rect.as_ref() {
                        dirty_rect = Some(ironrdp_pdu::geometry::InclusiveRectangle {
                            left: existing.left.min(rect.left),
                            top: existing.top.min(rect.top),
                            right: existing.right.max(rect.right),
                            bottom: existing.bottom.max(rect.bottom),
                        });
                    } else {
                        dirty_rect = Some(rect);
                    }
                }
                ActiveStageOutput::PointerBitmap(ptr) => {
                    if let Some(frame) = encode_pointer(ptr.width, ptr.height, ptr.hotspot_x, ptr.hotspot_y, ptr.bitmap_data.as_slice()) {
                        if frame_tx.blocking_send(frame).is_err() {
                            break 'outer;
                        }
                    }
                }
                ActiveStageOutput::Terminate(_) => break 'outer,
                _ => {}
            }
        }

        // Flush throttled frame to FFmpeg if time budget is met
        if let Some(_) = dirty_rect.as_ref() {
            if !awaiting_ack && last_flush.elapsed().as_millis() >= FRAME_MS {
                // Send the FULL frame to the async video task
                let _ = video_tx.send(image.data().to_vec());
                awaiting_ack = true;
                dirty_rect = None;
                last_flush = std::time::Instant::now();
            }
        }
    }

    info!("rdp_native: active stage thread exited");
}

fn spawn_ffmpeg(w: u16, h: u16) -> anyhow::Result<(tokio::process::ChildStdin, tokio::process::ChildStdout)> {
    use tokio::process::Command;
    use std::process::Stdio;

    let mut child = Command::new("ffmpeg")
        .args([
            "-f", "rawvideo",
            "-pixel_format", "rgba",
            "-video_size", &format!("{}x{}", w, h),
            "-i", "-",
            "-c:v", "libx264",
            "-preset", "ultrafast",
            "-tune", "zerolatency",
            "-f", "h264",
            "-an",
            "-"
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;

    let stdin = child.stdin.take().ok_or_else(|| anyhow::anyhow!("No stdin"))?;
    let stdout = child.stdout.take().ok_or_else(|| anyhow::anyhow!("No stdout"))?;
    Ok((stdin, stdout))
}

// ─── Frame encoding: WebP fallback (kept for legacy or pointer frames) ───────

fn encode_frame(image: &DecodedImage, rect: ironrdp_pdu::geometry::InclusiveRectangle) -> Option<Vec<u8>> {
    let full_w = image.width() as usize;
    let full_h = image.height() as usize;
    let rgba   = image.data();

    // Clamp and calculate dimensions
    let x1 = (rect.left as usize).min(full_w - 1);
    let y1 = (rect.top as usize).min(full_h - 1);
    let x2 = (rect.right as usize).min(full_w - 1);
    let y2 = (rect.bottom as usize).min(full_h - 1);

    if x2 < x1 || y2 < y1 { return None; }

    let width  = (x2 - x1 + 1) as u32;
    let height = (y2 - y1 + 1) as u32;

    // Slice the sub-rectangle
    let mut sub_rgba = Vec::with_capacity((width * height * 4) as usize);
    for y in y1..=y2 {
        let start = (y * full_w + x1) * 4;
        let end   = (y * full_w + x1 + width as usize) * 4;
        if end <= rgba.len() {
            sub_rgba.extend_from_slice(&rgba[start..end]);
        }
    }

    let mut out = Vec::with_capacity(9 + sub_rgba.len() / 2);
    // Type 0 = Graphics Frame
    out.push(0u8);
    // Header: width, height, x-offset, y-offset (all u16 LE)
    out.extend_from_slice(&(width as u16).to_le_bytes());
    out.extend_from_slice(&(height as u16).to_le_bytes());
    out.extend_from_slice(&(x1 as u16).to_le_bytes());
    out.extend_from_slice(&(y1 as u16).to_le_bytes());

    let enc  = webp::Encoder::from_rgba(&sub_rgba, width, height);
    let webp = enc.encode(30.0); // TURBO: Quality 30 for sub-20kb frames
    out.extend_from_slice(&*webp);
    Some(out)
}

fn encode_pointer(w: u16, h: u16, hx: u16, hy: u16, rgba: &[u8]) -> Option<Vec<u8>> {
    if w == 0 || h == 0 || rgba.is_empty() {
        return None;
    }
    
    let mut out = Vec::with_capacity(5 + rgba.len() / 2);
    // Type 1 = Pointer Event
    out.push(1u8);
    // Header: hotspot_x, hotspot_y (u16 LE)
    out.extend_from_slice(&hx.to_le_bytes());
    out.extend_from_slice(&hy.to_le_bytes());

    let enc = webp::Encoder::from_rgba(rgba, w as u32, h as u32);
    let webp = enc.encode(30.0);
    out.extend_from_slice(&*webp);
    Some(out)
}

// ─── Fast-Path Input PDU builder ─────────────────────────────────────────────

fn build_fast_path_input(ev: RdpInput) -> anyhow::Result<Option<Vec<u8>>> {
    let event: FastPathInputEvent = match ev {
        RdpInput::MouseMove { x, y } => {
            FastPathInputEvent::MouseEvent(MousePdu {
                flags: PointerFlags::MOVE,
                number_of_wheel_rotation_units: 0,
                x_position: x,
                y_position: y,
            })
        }
        RdpInput::MouseButton { x, y, flags } => {
            FastPathInputEvent::MouseEvent(MousePdu {
                flags: PointerFlags::from_bits_retain(flags),
                number_of_wheel_rotation_units: 0,
                x_position: x,
                y_position: y,
            })
        }
        RdpInput::FrameAck => return Ok(None),
        RdpInput::Key { scancode, extended, down } => {
            let mut kflags = KeyboardFlags::empty();
            if !down    { kflags |= KeyboardFlags::RELEASE; }
            if extended { kflags |= KeyboardFlags::EXTENDED; }
            FastPathInputEvent::KeyboardEvent(kflags, scancode)
        }
        RdpInput::CtrlAltDel => {
            // Build multiple events via individual single-event PDUs
            let events = [
                kev(0x1D, false, true),  // Ctrl ↓
                kev(0x38, false, true),  // Alt  ↓
                kev(0x53, true,  true),  // Del  ↓ (extended)
                kev(0x53, true,  false), // Del  ↑
                kev(0x38, false, false), // Alt  ↑
                kev(0x1D, false, false), // Ctrl ↑
            ];
            // Encode each as a single-event PDU and concatenate
            let mut all_bytes = Vec::new();
            for e in events {
                let pdu = FastPathInput::single(e);
                let mut buf = Vec::new();
                write_pdu(&pdu, &mut buf)?;
                all_bytes.extend(buf);
            }
            return Ok(Some(all_bytes));
        }
    };

    let pdu = FastPathInput::single(event);
    let mut buf = Vec::new();
    write_pdu(&pdu, &mut buf)?;
    Ok(Some(buf))
}

/// Encode a PDU that impl Encode into a Vec<u8>
fn write_pdu<P: ironrdp_pdu::Encode>(pdu: &P, out: &mut Vec<u8>) -> anyhow::Result<()> {
    let size = pdu.size();
    out.resize(size, 0u8);
    let mut cursor = ironrdp_pdu::WriteCursor::new(out);
    pdu.encode(&mut cursor)
        .map_err(|e| anyhow::anyhow!("PDU encode error: {:?}", e))
}

fn kev(sc: u8, extended: bool, down: bool) -> FastPathInputEvent {
    let mut f = KeyboardFlags::empty();
    if !down    { f |= KeyboardFlags::RELEASE; }
    if extended { f |= KeyboardFlags::EXTENDED; }
    FastPathInputEvent::KeyboardEvent(f, sc)
}

// ─── Browser command → RDP input translation ─────────────────────────────────

fn browser_cmd_to_rdp_input(cmd: BrowserCommand) -> Option<RdpInput> {
    match cmd {
        BrowserCommand::MouseMove { x, y } => Some(RdpInput::MouseMove {
            x: x.max(0) as u16,
            y: y.max(0) as u16,
        }),
        BrowserCommand::MouseButton { x, y, button, down } => {
            let flag: u16 = match (button.as_str(), down) {
                ("left",   true)  => (PointerFlags::DOWN | PointerFlags::LEFT_BUTTON).bits(),
                ("left",   false) => PointerFlags::LEFT_BUTTON.bits(),
                ("right",  true)  => (PointerFlags::DOWN | PointerFlags::RIGHT_BUTTON).bits(),
                ("right",  false) => PointerFlags::RIGHT_BUTTON.bits(),
                ("middle", true)  => (PointerFlags::DOWN | PointerFlags::MIDDLE_BUTTON_OR_WHEEL).bits(),
                ("middle", false) => PointerFlags::MIDDLE_BUTTON_OR_WHEEL.bits(),
                _ => return None,
            };
            Some(RdpInput::MouseButton { x: x.max(0) as u16, y: y.max(0) as u16, flags: flag })
        }
        BrowserCommand::Key { code, down } => {
            scancode_from_code(&code).map(|(sc, ext)| RdpInput::Key { scancode: sc, extended: ext, down })
        }
        BrowserCommand::SendCtrlAltDel => Some(RdpInput::CtrlAltDel),
        _ => None,
    }
}

/// Map `KeyboardEvent.code` → (Windows scancode, is_extended)
fn scancode_from_code(code: &str) -> Option<(u8, bool)> {
    // Format: (code_string, scancode, is_extended)
    const MAP: &[(&str, u8, bool)] = &[
        // Escape + F-keys
        ("Escape",0x01,false),("F1",0x3B,false),("F2",0x3C,false),("F3",0x3D,false),
        ("F4",0x3E,false),("F5",0x3F,false),("F6",0x40,false),("F7",0x41,false),
        ("F8",0x42,false),("F9",0x43,false),("F10",0x44,false),("F11",0x57,false),
        ("F12",0x58,false),
        // Number row
        ("Backquote",0x29,false),("Digit1",0x02,false),("Digit2",0x03,false),
        ("Digit3",0x04,false),("Digit4",0x05,false),("Digit5",0x06,false),
        ("Digit6",0x07,false),("Digit7",0x08,false),("Digit8",0x09,false),
        ("Digit9",0x0A,false),("Digit0",0x0B,false),("Minus",0x0C,false),
        ("Equal",0x0D,false),("Backspace",0x0E,false),
        // Tab + QWERTY row
        ("Tab",0x0F,false),("KeyQ",0x10,false),("KeyW",0x11,false),
        ("KeyE",0x12,false),("KeyR",0x13,false),("KeyT",0x14,false),
        ("KeyY",0x15,false),("KeyU",0x16,false),("KeyI",0x17,false),
        ("KeyO",0x18,false),("KeyP",0x19,false),("BracketLeft",0x1A,false),
        ("BracketRight",0x1B,false),("Enter",0x1C,false),
        // CapsLock + ASDF row
        ("CapsLock",0x3A,false),("KeyA",0x1E,false),("KeyS",0x1F,false),
        ("KeyD",0x20,false),("KeyF",0x21,false),("KeyG",0x22,false),
        ("KeyH",0x23,false),("KeyJ",0x24,false),("KeyK",0x25,false),
        ("KeyL",0x26,false),("Semicolon",0x27,false),("Quote",0x28,false),
        ("Backslash",0x2B,false),
        // Shift + ZXCV row
        ("ShiftLeft",0x2A,false),("KeyZ",0x2C,false),("KeyX",0x2D,false),
        ("KeyC",0x2E,false),("KeyV",0x2F,false),("KeyB",0x30,false),
        ("KeyN",0x31,false),("KeyM",0x32,false),("Comma",0x33,false),
        ("Period",0x34,false),("Slash",0x35,false),("ShiftRight",0x36,false),
        // Bottom row
        ("ControlLeft",0x1D,false),("MetaLeft",0x5B,true),("AltLeft",0x38,false),
        ("Space",0x39,false),("AltRight",0x38,true),("MetaRight",0x5C,true),
        ("ContextMenu",0x5D,true),("ControlRight",0x1D,true),
        // Navigation cluster
        ("Insert",0x52,true),("Home",0x47,true),("PageUp",0x49,true),
        ("Delete",0x53,true),("End",0x4F,true),("PageDown",0x51,true),
        ("ArrowUp",0x48,true),("ArrowLeft",0x4B,true),
        ("ArrowDown",0x50,true),("ArrowRight",0x4D,true),
        // Numpad
        ("NumLock",0x45,false),("NumpadDivide",0x35,true),
        ("NumpadMultiply",0x37,false),("NumpadSubtract",0x4A,false),
        ("Numpad7",0x47,false),("Numpad8",0x48,false),("Numpad9",0x49,false),
        ("NumpadAdd",0x4E,false),("Numpad4",0x4B,false),("Numpad5",0x4C,false),
        ("Numpad6",0x4D,false),("Numpad1",0x4F,false),("Numpad2",0x50,false),
        ("Numpad3",0x51,false),("Numpad0",0x52,false),("NumpadDecimal",0x53,false),
        ("NumpadEnter",0x1C,true),
    ];

    MAP.iter()
        .find(|(c, _, _)| *c == code)
        .map(|(_, sc, ext)| (*sc, *ext))
}

// ─── TLS: accept all certificates (enterprise internal PKI) ──────────────────

#[derive(Debug)]
struct NoCertVerification;

impl rustls::client::danger::ServerCertVerifier for NoCertVerification {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self, _: &[u8],
        _: &rustls::pki_types::CertificateDer<'_>,
        _: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self, _: &[u8],
        _: &rustls::pki_types::CertificateDer<'_>,
        _: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        use rustls::SignatureScheme;
        vec![
            SignatureScheme::RSA_PKCS1_SHA1, SignatureScheme::ECDSA_SHA1_Legacy,
            SignatureScheme::RSA_PKCS1_SHA256, SignatureScheme::ECDSA_NISTP256_SHA256,
            SignatureScheme::RSA_PKCS1_SHA384, SignatureScheme::ECDSA_NISTP384_SHA384,
            SignatureScheme::RSA_PKCS1_SHA512, SignatureScheme::ECDSA_NISTP521_SHA512,
            SignatureScheme::RSA_PSS_SHA256, SignatureScheme::RSA_PSS_SHA384,
            SignatureScheme::RSA_PSS_SHA512, SignatureScheme::ED25519, SignatureScheme::ED448,
        ]
    }
}
