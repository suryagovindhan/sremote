// main.rs - Complete Rewrite
// Fix: Use profile-level-id=42e0XX (Constrained Baseline) for universal browser support.
// Fix: Implemented exponential backoff and persistent reconnect loop.
// Fix: Used Weak references in WebRTC callbacks to prevent leaks.

use anyhow::{Context, Result};
use bytes::Bytes;
use dotenvy::dotenv;
use enigo::{Button, Coordinate, Direction, Enigo, Key, Keyboard, Mouse, Settings};
use futures_util::{SinkExt, StreamExt};
use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
use std::{
    env,
    sync::{atomic::{AtomicBool, Ordering}, Arc},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{error, info, warn};

mod hw_capture;
use webrtc::{
    api::{
        interceptor_registry::register_default_interceptors,
        media_engine::{MediaEngine, MIME_TYPE_H264},
        APIBuilder,
    },
    data_channel::data_channel_message::DataChannelMessage,
    ice_transport::ice_server::RTCIceServer,
    interceptor::registry::Registry,
    media::Sample,
    peer_connection::{
        configuration::RTCConfiguration,
        sdp::session_description::RTCSessionDescription,
    },
    rtp_transceiver::rtp_codec::RTCRtpCodecCapability,
    track::track_local::{track_local_static_sample::TrackLocalStaticSample, TrackLocal},
};

// ─── JWT ─────────────────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize)]
struct Claims {
    sub:  String,
    role: String,
    room: String,
    exp:  usize,
}

fn mint_agent_jwt(secret: &str, sub: &str, room: &str) -> Result<String> {
    let exp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("System time error")?
        .as_secs() as usize
        + 7 * 86_400; // 7 days

    let claims = Claims {
        sub: sub.into(),
        role: "agent".into(),
        room: room.into(),
        exp,
    };
    let key = EncodingKey::from_secret(secret.as_bytes());
    encode(&Header::new(Algorithm::HS256), &claims, &key)
        .map_err(|e| anyhow::anyhow!("JWT encode: {}", e))
}

// ─── Signaling messages ───────────────────────────────────────────────────────

#[derive(Deserialize, Debug, Clone)]
struct IceServerCfg {
    urls:       Vec<String>,
    #[serde(default)]
    username:   String,
    #[serde(default)]
    credential: String,
}

#[derive(Deserialize, Debug)]
struct BrokerConfig {
    #[allow(dead_code)]
    #[serde(rename = "type")]
    kind:        String,
    ice_servers: Vec<IceServerCfg>,
}

// ─── Control command from technician console ─────────────────────────────────

#[derive(Deserialize, Debug)]
struct ControlCmd {
    action:  String,
    #[serde(default)] x:       i32,
    #[serde(default)] y:       i32,
    #[serde(default)] button:  String,
    #[serde(default)] key:     String,
    #[serde(default)] delta_y: i32,
    #[serde(default)] width:   Option<u32>,
    #[serde(default)] height:  Option<u32>,
}

// ─── Windows Service entrypoint ───────────────────────────────────────────────

#[cfg(windows)]
mod service {
    use std::ffi::OsString;
    use std::time::Duration;
    use windows_service::{
        define_windows_service,
        service::{
            ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus,
            ServiceType,
        },
        service_control_handler::{self, ServiceControlHandlerResult},
        service_dispatcher,
    };

    define_windows_service!(ffi_service_main, service_main);

    fn service_main(_args: Vec<OsString>) {
        if let Err(e) = run_service() {
            eprintln!("Service error: {}", e);
        }
    }

    fn run_service() -> windows_service::Result<()> {
        let status_handle = service_control_handler::register(
            "sRemoteDaemon",
            move |ctrl| match ctrl {
                ServiceControl::Stop => ServiceControlHandlerResult::NoError,
                ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
                _ => ServiceControlHandlerResult::NotImplemented,
            },
        )?;

        status_handle.set_service_status(ServiceStatus {
            service_type:      ServiceType::OWN_PROCESS,
            current_state:     ServiceState::Running,
            controls_accepted: ServiceControlAccept::STOP,
            exit_code:         ServiceExitCode::Win32(0),
            checkpoint:        0,
            wait_hint:         Duration::default(),
            process_id:        None,
        })?;

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(super::run_daemon()).ok();

        status_handle.set_service_status(ServiceStatus {
            service_type:      ServiceType::OWN_PROCESS,
            current_state:     ServiceState::Stopped,
            controls_accepted: ServiceControlAccept::empty(),
            exit_code:         ServiceExitCode::Win32(0),
            checkpoint:        0,
            wait_hint:         Duration::default(),
            process_id:        None,
        })?;

        Ok(())
    }

    pub fn start() {
        service_dispatcher::start("sRemoteDaemon", ffi_service_main)
            .expect("Failed to start Windows service dispatcher");
    }
}

// ─── Entry point ─────────────────────────────────────────────────────────────

fn main() {
    dotenv().ok();
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .init();

    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--service") {
        #[cfg(windows)]
        service::start();
    } else {
        let rt = tokio::runtime::Runtime::new().unwrap();
        if let Err(e) = rt.block_on(run_daemon()) {
            error!("Daemon error: {:#}", e);
        }
    }
}

async fn run_daemon() -> Result<()> {
    let jwt_secret = env::var("JWT_SECRET").context("JWT_SECRET missing in .env")?;
    let broker_url = env::var("BROKER_WS_URL").unwrap_or_else(|_| "ws://127.0.0.1:5695".into());
    let room       = env::var("AGENT_ROOM").unwrap_or_else(|_| "room-001".into());
    let subject    = env::var("AGENT_SUBJECT").unwrap_or_else(|_| "daemon-01".into());

    let fps: u32 = env::var("CAPTURE_FPS").unwrap_or_else(|_| "30".into()).parse().unwrap_or(30).clamp(1, 120);
    let init_kbps: u32 = env::var("INITIAL_BITRATE_KBPS").unwrap_or_else(|_| "2000".into()).parse().unwrap_or(2000);

    if !broker_url.starts_with("wss://") && !broker_url.contains("127.0.0.1") && !broker_url.contains("localhost") {
        warn!("BROKER_WS_URL uses plaintext ws:// on a non-local address — Use wss:// in production!");
    }

    let mut backoff = 2u64;
    let mut prev_thread: Option<(std::thread::JoinHandle<()>, Arc<AtomicBool>)> = None;

    loop {
        if let Some((h, f)) = prev_thread.take() {
            f.store(true, Ordering::SeqCst);
            let _ = h.join();
        }

        let stop = Arc::new(AtomicBool::new(false));
        let token = mint_agent_jwt(&jwt_secret, &subject, &room)?;
        let full_url = format!("{}/ws/{}?token={}", broker_url, room, token);

        info!("Connecting to broker at {} (backoff={}s)", broker_url, backoff);
        match connect_async(&full_url).await {
            Ok((ws, _)) => {
                backoff = 2;
                match run_session(ws, fps, init_kbps, Arc::clone(&stop), &mut prev_thread).await {
                    Ok(_)  => info!("Session ended cleanly, reconnecting"),
                    Err(e) => warn!("Session error: {:#}", e),
                }
            }
            Err(e) => warn!("Connect failed: {}", e),
        }

        tokio::time::sleep(Duration::from_secs(backoff)).await;
        backoff = (backoff * 2).min(60);
    }
}

async fn run_session(
    ws_stream: tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    fps: u32,
    init_kbps: u32,
    stop_flag: Arc<AtomicBool>,
    prev_thread: &mut Option<(std::thread::JoinHandle<()>, Arc<AtomicBool>)>,
) -> Result<()> {
    type SharedSink = Arc<Mutex<futures_util::stream::SplitSink<tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>, Message>>>;

    let (ws_sink_raw, mut ws_receiver) = ws_stream.split();
    let ws_sink: SharedSink = Arc::new(Mutex::new(ws_sink_raw));

    let broker_cfg: BrokerConfig = loop {
        match ws_receiver.next().await {
            Some(Ok(Message::Text(txt))) => {
                let v: serde_json::Value = serde_json::from_str(&txt)?;
                if v["type"] == "config" { break serde_json::from_value(v)?; }
            }
            Some(Err(e)) => return Err(anyhow::anyhow!("WS config error: {}", e)),
            None => return Err(anyhow::anyhow!("Broker closed before config")),
            _ => continue,
        }
    };

    let (src_w, _) = tokio::task::spawn_blocking(|| hw_capture::get_primary_display_resolution()).await??;
    let level_idc: u8 = if src_w >= 3840 || fps > 60 { 0x33 } else if src_w > 1920 || fps >= 60 { 0x2A } else { 0x28 };
    let profile_level_id = format!("level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e0{:02X}", level_idc);
    info!("H.264 SDP fmtp: {}", profile_level_id);

    let mut media_engine = MediaEngine::default();
    media_engine.register_codec(
        webrtc::rtp_transceiver::rtp_codec::RTCRtpCodecParameters {
            capability: RTCRtpCodecCapability {
                mime_type: MIME_TYPE_H264.to_owned(),
                clock_rate: 90_000,
                sdp_fmtp_line: profile_level_id.clone(),
                ..Default::default()
            },
            payload_type: 102,
            ..Default::default()
        },
        webrtc::rtp_transceiver::rtp_codec::RTPCodecType::Video,
    )?;

    let mut registry = Registry::new();
    registry = register_default_interceptors(registry, &mut media_engine)?;
    let api = APIBuilder::new().with_media_engine(media_engine).with_interceptor_registry(registry).build();

    let pc = Arc::new(api.new_peer_connection(RTCConfiguration {
        ice_servers: broker_cfg.ice_servers.iter().map(|s| RTCIceServer {
            urls: s.urls.clone(), username: s.username.clone(), credential: s.credential.clone(), ..Default::default()
        }).collect(),
        ..Default::default()
    }).await?);

    let video_track = Arc::new(TrackLocalStaticSample::new(
        RTCRtpCodecCapability {
            mime_type: MIME_TYPE_H264.to_owned(),
            clock_rate: 90_000,
            sdp_fmtp_line: profile_level_id,
            ..Default::default()
        },
        "video".into(), "sremote-screen".into()
    ));
    pc.add_track(Arc::clone(&video_track) as Arc<dyn TrackLocal + Send + Sync>).await?;

    let (resize_tx, resize_rx) = std::sync::mpsc::channel();
    let enigo_shared = Arc::new(std::sync::Mutex::new(Enigo::new(&Settings::default())?));
    
    let dc = pc.create_data_channel("control", None).await?;
    dc.on_message(Box::new(move |msg: DataChannelMessage| {
        let data = msg.data.to_vec();
        let rtx = resize_tx.clone();
        let eng = Arc::clone(&enigo_shared);
        Box::pin(async move {
            if let Ok(cmd) = serde_json::from_slice::<ControlCmd>(&data) {
                execute_control(cmd, &rtx, &eng);
            }
        })
    }));

    let ws_c = Arc::clone(&ws_sink);
    let pc_weak = Arc::downgrade(&pc);
    pc.on_ice_candidate(Box::new(move |candidate| {
        let ws = Arc::clone(&ws_c);
        let pc_w = pc_weak.clone();
        Box::pin(async move {
            let Some(_) = pc_w.upgrade() else { return };
            if let Some(c) = candidate {
                if let Ok(init) = c.to_json() {
                    let msg = serde_json::json!({"type":"ice","candidate":init.candidate,"sdpMLineIndex":init.sdp_mline_index,"sdpMid":init.sdp_mid});
                    let mut lock = ws.lock().await;
                    let _ = lock.send(Message::Text(msg.to_string())).await;
                }
            }
        })
    }));

    let pc_w2 = Arc::downgrade(&pc);
    pc.on_peer_connection_state_change(Box::new(move |state| {
        let pc_w = pc_w2.clone();
        Box::pin(async move {
            let Some(_) = pc_w.upgrade() else { return };
            info!("PeerConnection state: {:?}", state);
        })
    }));

    let pc_w3 = Arc::downgrade(&pc);
    pc.on_ice_connection_state_change(Box::new(move |state| {
        let pc_w = pc_w3.clone();
        Box::pin(async move {
            let Some(_) = pc_w.upgrade() else { return };
            info!("ICE Connection state: {:?}", state);
        })
    }));

    let offer = pc.create_offer(None).await?;
    pc.set_local_description(offer.clone()).await?;
    {
        let mut lock = ws_sink.lock().await;
        lock.send(Message::Text(serde_json::json!({"type":"offer","sdp":offer.sdp}).to_string())).await?;
    }

    let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(60);
    let frame_dur = Duration::from_micros(1_000_000 / fps as u64);
    let hw_stop = Arc::clone(&stop_flag);
    let capture_handle = std::thread::spawn(move || {
        let _ = hw_capture::run_hardware_capture(tx, resize_rx, fps, init_kbps, hw_stop);
    });
    *prev_thread = Some((capture_handle, stop_flag));

    tokio::spawn(async move {
        let mut count = 0u64;
        while let Some(nals) = rx.recv().await {
            count += 1;
            if count % 60 == 0 { info!("WebRTC: {} samples sent", count); }
            let _ = video_track.write_sample(&Sample { data: Bytes::from(nals), duration: frame_dur, ..Default::default() }).await;
        }
    });

    while let Some(Ok(Message::Text(txt))) = ws_receiver.next().await {
        let v: serde_json::Value = serde_json::from_str(&txt).unwrap_or_default();
        match v["type"].as_str() {
            Some("answer") => {
                let sdp = v["sdp"].as_str().unwrap_or_default().to_string();
                pc.set_remote_description(RTCSessionDescription::answer(sdp)?).await?;
                info!("Remote description set");
            }
            Some("ice") => {
                use webrtc::ice_transport::ice_candidate::RTCIceCandidateInit;
                let candidate = RTCIceCandidateInit {
                    candidate: v["candidate"].as_str().unwrap_or_default().into(),
                    sdp_mid: v["sdpMid"].as_str().map(|s| s.into()),
                    sdp_mline_index: v["sdpMLineIndex"].as_u64().map(|n| n as u16),
                    ..Default::default()
                };
                pc.add_ice_candidate(candidate).await?;
            }
            _ => {}
        }
    }

    pc.close().await?;
    Ok(())
}

fn execute_control(cmd: ControlCmd, resize_tx: &std::sync::mpsc::Sender<(Option<u32>, Option<u32>)>, enigo: &std::sync::Mutex<Enigo>) {
    if cmd.action == "resize" { let _ = resize_tx.send((cmd.width, cmd.height)); return; }
    let Ok(mut enigo) = enigo.lock() else { return };
    match cmd.action.as_str() {
        "mousemove" => { let _ = enigo.move_mouse(cmd.x, cmd.y, Coordinate::Abs); }
        "mousedown" => { let _ = enigo.button(parse_button(&cmd.button), Direction::Press); }
        "mouseup"   => { let _ = enigo.button(parse_button(&cmd.button), Direction::Release); }
        "click"     => { let _ = enigo.button(parse_button(&cmd.button), Direction::Click); }
        "scroll"    => { let _ = enigo.scroll(cmd.delta_y, enigo::Axis::Vertical); }
        "keydown"   => { if let Some(k) = parse_key(&cmd.key) { let _ = enigo.key(k, Direction::Press); } }
        "keyup"     => { if let Some(k) = parse_key(&cmd.key) { let _ = enigo.key(k, Direction::Release); } }
        _ => {}
    }
}

fn parse_button(s: &str) -> Button { match s { "right" => Button::Right, "middle" => Button::Middle, _ => Button::Left } }
fn parse_key(s: &str) -> Option<Key> {
    Some(match s.to_lowercase().as_str() {
        "enter" | "return" => Key::Return, "escape" | "esc" => Key::Escape, "backspace" => Key::Backspace,
        "tab" => Key::Tab, "delete" | "del" => Key::Delete, "arrowleft" | "left" => Key::LeftArrow,
        "arrowright" | "right" => Key::RightArrow, "arrowup" | "up" => Key::UpArrow, "arrowdown" | "down" => Key::DownArrow,
        "home" => Key::Home, "end" => Key::End, "pageup" => Key::PageUp, "pagedown" => Key::PageDown,
        "f1"=>Key::F1,"f2"=>Key::F2,"f3"=>Key::F3,"f4"=>Key::F4,"f5"=>Key::F5,"f6"=>Key::F6,"f7"=>Key::F7,"f8"=>Key::F8,"f9"=>Key::F9,"f10"=>Key::F10,"f11"=>Key::F11,"f12"=>Key::F12,
        "control" | "ctrl" => Key::Control, "alt" => Key::Alt, "shift" => Key::Shift, "meta" | "super" => Key::Meta,
        _ => {
            let mut chars = s.chars();
            if let Some(ch) = chars.next() { if chars.next().is_none() { return Some(Key::Unicode(ch)); } }
            return None;
        }
    })
}
