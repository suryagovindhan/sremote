// session.rs — WebRTC session lifecycle, signaling, reconnect loop

use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message};
use tracing::{error, info, warn};
use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::{MediaEngine, MIME_TYPE_H264};
use webrtc::api::APIBuilder;
use webrtc::data_channel::data_channel_message::DataChannelMessage;
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::interceptor::registry::Registry;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::sdp::sdp_type::RTCSdpType;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::rtp_transceiver::rtp_codec::RTCRtpCodecCapability;
use webrtc::track::track_local::track_local_static_sample::TrackLocalStaticSample;
use webrtc::track::track_local::TrackLocal;
use webrtc_media::Sample;

use crate::capture;
use crate::control;
use crate::encoder;
use crate::SHUTDOWN;

pub struct DaemonConfig {
    pub jwt_secret:   String,
    pub broker_url:   String,
    pub room:         String,
    pub subject:      String,
    pub fps:          u32,
    pub bitrate_kbps: u32,
    pub width:        Option<u32>,
    pub height:       Option<u32>,
}

#[derive(Serialize, Deserialize)]
struct Claims { sub: String, role: String, room: String, exp: usize }

fn mint_jwt(config: &DaemonConfig) -> Result<String> {
    let exp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as usize + 7 * 86400;
    let claims = Claims { sub: config.subject.clone(), role: "agent".into(), room: config.room.clone(), exp };
    let key = EncodingKey::from_secret(config.jwt_secret.as_bytes());
    encode(&Header::new(Algorithm::HS256), &claims, &key).map_err(|e| anyhow::anyhow!(e))
}

type WsRead = futures_util::stream::SplitStream<
    tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
>;

fn should_shutdown() -> bool {
    SHUTDOWN
        .get()
        .map(|flag| flag.load(Ordering::SeqCst))
        .unwrap_or(false)
}

fn recommended_bitrate_kbps(width: u32, height: u32, fps: u32) -> u32 {
    let pixels = width.saturating_mul(height);
    let motion_boost = if fps >= 60 { 1400 } else if fps >= 30 { 700 } else { 0 };

    let base = if pixels >= 1920 * 1080 {
        4000
    } else if pixels >= 1280 * 720 {
        2800
    } else if pixels >= 960 * 540 {
        1800
    } else {
        1200
    };

    base + motion_boost
}

fn stop_pipeline(prev: &mut Option<(std::thread::JoinHandle<()>, Arc<AtomicBool>)>) {
    if let Some((handle, stop)) = prev.take() {
        stop.store(true, Ordering::SeqCst);
        let _ = handle.join();
    }
}

async fn next_ws_message(ws_read: &mut WsRead, stop_flag: &Arc<AtomicBool>) -> Result<Option<Message>> {
    loop {
        if stop_flag.load(Ordering::SeqCst) || should_shutdown() {
            return Ok(None);
        }

        match tokio::time::timeout(Duration::from_millis(500), ws_read.next()).await {
            Ok(Some(Ok(msg))) => return Ok(Some(msg)),
            Ok(Some(Err(err))) => return Err(err.into()),
            Ok(None) => return Ok(None),
            Err(_) => continue,
        }
    }
}

pub async fn run_daemon_once(config: &DaemonConfig) -> Result<()> {
    let mut prev: Option<(std::thread::JoinHandle<()>, Arc<AtomicBool>)> = None;
    let stop  = Arc::new(AtomicBool::new(false));
    let token = mint_jwt(config)?;
    let ws_url = format!("{}/ws/{}?token={}", config.broker_url, config.room, token);
    let (ws, _) = tokio_tungstenite::connect_async(&ws_url).await.map_err(|e| anyhow::anyhow!("Connect failed: {}", e))?;
    let result = run_single_session(ws, config, stop, &mut prev).await;
    stop_pipeline(&mut prev);
    result
}

pub async fn run_daemon(config: DaemonConfig) -> Result<()> {
    let mut backoff = 2u64;
    let mut prev: Option<(std::thread::JoinHandle<()>, Arc<AtomicBool>)> = None;

    loop {
        if should_shutdown() {
            stop_pipeline(&mut prev);
            return Ok(());
        }
        stop_pipeline(&mut prev);

        let stop  = Arc::new(AtomicBool::new(false));
        let token = mint_jwt(&config)?;
        let ws_url = format!("{}/ws/{}?token={}", config.broker_url, config.room, token);

        match connect_async(&ws_url).await {
            Ok((ws, _)) => {
                match run_single_session(ws, &config, Arc::clone(&stop), &mut prev).await {
                    Ok(_) => { info!("Session ended cleanly — reconnecting immediately."); backoff = 2; continue; }
                    Err(e) => warn!("Session error: {:#}", e),
                }
            }
            Err(e) => warn!("Connect failed: {}", e),
        }
        if should_shutdown() {
            stop_pipeline(&mut prev);
            return Ok(());
        }
        tokio::time::sleep(Duration::from_secs(backoff)).await;
        backoff = (backoff * 2).min(60);
    }
}

async fn run_single_session(
    ws: tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    config: &DaemonConfig,
    stop_flag: Arc<AtomicBool>,
    prev: &mut Option<(std::thread::JoinHandle<()>, Arc<AtomicBool>)>,
) -> Result<()> {
    let result: Result<()> = async {
        let (ws_write, mut ws_read) = ws.split();
        let ws_sender = Arc::new(Mutex::new(ws_write));

        let Some(broker_msg) = next_ws_message(&mut ws_read, &stop_flag).await? else {
            return Ok(());
        };
        let v: serde_json::Value = serde_json::from_str(broker_msg.to_text()?)?;
        let ice_servers_json = v["ice_servers"].as_array().context("No ice_servers in broker config")?;
        let ice_servers: Vec<RTCIceServer> = ice_servers_json.iter().map(|s| RTCIceServer {
            urls: s["urls"].as_array().map(|a| a.iter().map(|u| u.as_str().unwrap_or("").to_string()).collect()).unwrap_or_default(),
            username: s["username"].as_str().unwrap_or("").to_string(),
            credential: s["credential"].as_str().unwrap_or("").to_string(),
            ..Default::default()
        }).collect();

        let (real_w, real_h) = tokio::task::spawn_blocking(capture::get_primary_display_resolution).await??;
        let resize_state = Arc::new(capture::ResizeState::new(config.width, config.height));
        let (target_w, target_h) = resize_state.target_size(real_w, real_h);
        let profile_w = real_w.max(target_w);
        let level_idc: u8 = if profile_w >= 3840 || config.fps > 60 { 0x33 } else if profile_w > 1920 || config.fps >= 60 { 0x2A } else { 0x28 };
        let profile_level_id = format!("level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e0{:02X}", level_idc);

        let mut media_engine = MediaEngine::default();
        media_engine.register_codec(webrtc::rtp_transceiver::rtp_codec::RTCRtpCodecParameters {
            capability: RTCRtpCodecCapability { mime_type: MIME_TYPE_H264.to_owned(), clock_rate: 90000, sdp_fmtp_line: profile_level_id.clone(), ..Default::default() },
            payload_type: 102, ..Default::default()
        }, webrtc::rtp_transceiver::rtp_codec::RTPCodecType::Video)?;

        let mut registry = Registry::new();
        registry = register_default_interceptors(registry, &mut media_engine)?;
        let pc = Arc::new(APIBuilder::new().with_media_engine(media_engine).with_interceptor_registry(registry).build().new_peer_connection(RTCConfiguration { ice_servers, ..Default::default() }).await?);

        let video_track = Arc::new(TrackLocalStaticSample::new(
            RTCRtpCodecCapability {
                mime_type: MIME_TYPE_H264.to_owned(),
                clock_rate: 90000,
                sdp_fmtp_line: profile_level_id,
                rtcp_feedback: vec![
                    webrtc::rtp_transceiver::RTCPFeedback {
                        typ: "nack".to_owned(),
                        parameter: "pli".to_owned(),
                    },
                ],
                ..Default::default()
            },
            "video".to_owned(),
            "sremote-screen".to_owned()
        ));
        pc.add_track(Arc::clone(&video_track) as Arc<dyn TrackLocal + Send + Sync>).await?;

        let enigo = Arc::new(std::sync::Mutex::new(enigo::Enigo::new(&enigo::Settings::default())?));

        // The daemon is the offerer, so it must create the data channel proactively.
        let dc = pc.create_data_channel("control", None).await?;

        let resize_state_for_dc = Arc::clone(&resize_state);
        let enigo_c = Arc::clone(&enigo);

        dc.on_message(Box::new(move |msg: DataChannelMessage| {
            let resize_state = Arc::clone(&resize_state_for_dc);
            let enigo_c = Arc::clone(&enigo_c);
            let data = msg.data.to_vec();

            Box::pin(async move {
                if let Ok(cmd) = serde_json::from_slice::<control::ControlCmd>(&data) {
                    control::execute_control(cmd, resize_state.as_ref(), &enigo_c);
                }
            })
        }));

        let pc_weak = Arc::downgrade(&pc);
        let ws_sender_for_ice = Arc::clone(&ws_sender);
        pc.on_ice_candidate(Box::new(move |candidate| {
            let ws = Arc::clone(&ws_sender_for_ice); let pcw = pc_weak.clone();
            Box::pin(async move {
                let Some(_) = pcw.upgrade() else { return };
                if let Some(c) = candidate {
                    if let Ok(init) = c.to_json() {
                        let msg = serde_json::json!({ "type": "ice", "candidate": init.candidate, "sdpMLineIndex": init.sdp_mline_index, "sdpMid": init.sdp_mid });
                        ws.lock().await.send(Message::Text(msg.to_string())).await.ok();
                    }
                }
            })
        }));

        let fps = config.fps;
        let effective_bitrate_kbps = config
            .bitrate_kbps
            .max(recommended_bitrate_kbps(target_w, target_h, fps));
        info!(
            "Encoder target: {}x{} @ {} fps, bitrate {} kbps",
            target_w,
            target_h,
            fps,
            effective_bitrate_kbps
        );
        let enc_cfg = encoder::EncoderConfig {
            width: target_w,
            height: target_h,
            fps,
            bitrate_kbps: effective_bitrate_kbps,
        };
        let (frame_tx, frame_rx) = std::sync::mpsc::sync_channel::<capture::CaptureEvent>(4);
        let (nal_tx, nal_rx) = tokio::sync::mpsc::channel::<encoder::EncodedFrame>(2);
        let mut nal_rx_opt = Some(nal_rx);

        let mut frame_tx_opt = Some(frame_tx);
        let mut frame_rx_opt = Some(frame_rx);
        let mut nal_tx_opt = Some(nal_tx);
        let mut pipeline_started = false;
        let mut offer_sent = false;

        while let Some(msg) = next_ws_message(&mut ws_read, &stop_flag).await? {
            if let Ok(text) = msg.to_text() {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(text) {
                    match v["type"].as_str() {
                        Some("peer_joined") if !offer_sent => {
                            offer_sent = true;
                            let offer = pc.create_offer(None).await?;
                            pc.set_local_description(offer.clone()).await?;
                            ws_sender.lock().await.send(Message::Text(serde_json::json!({"type":"offer","sdp":offer.sdp}).to_string())).await?;
                        }
                        Some("answer") => {
                            let sdp = v["sdp"].as_str().unwrap_or_default().to_string();
                            let mut desc = RTCSessionDescription::default();
                            desc.sdp_type = RTCSdpType::Answer; desc.sdp = sdp;
                            pc.set_remote_description(desc).await?;

                            if !pipeline_started {
                                pipeline_started = true;
                                if let (Some(ftx), Some(frx), Some(ntx)) = (frame_tx_opt.take(), frame_rx_opt.take(), nal_tx_opt.take()) {
                                    let stop_cap = Arc::clone(&stop_flag);
                                    let stop_enc = Arc::clone(&stop_flag);
                                    let enc_cfg2 = enc_cfg.clone();
                                    let resize_state_cap = Arc::clone(&resize_state);
                                    let resize_state_enc = Arc::clone(&resize_state);

                                    let h = std::thread::spawn(move || {
                                        let capture_handle = std::thread::spawn(move || {
                                            if let Err(e) = capture::run_capture_loop(ftx, stop_cap, fps, resize_state_cap) {
                                                error!("Capture loop failed: {:#}", e);
                                            }
                                        });

                                        if let Err(e) = encoder::run_encoder_loop(frx, ntx, enc_cfg2, stop_enc, resize_state_enc) {
                                            error!("Encoder loop failed: {:#}", e);
                                        }

                                        let _ = capture_handle.join();
                                    });
                                    *prev = Some((h, Arc::clone(&stop_flag)));

                                    if let Some(mut nrx) = nal_rx_opt.take() {
                                        let vt = Arc::clone(&video_track);
                                        let dur = Duration::from_millis(1000 / fps.max(1) as u64);
                                        tokio::spawn(async move {
                                            let max_frame_age = Duration::from_millis(180);
                                            while let Some(frame) = nrx.recv().await {
                                                let mut latest = frame;
                                                while let Ok(next) = nrx.try_recv() {
                                                    latest = next;
                                                }

                                                if latest.captured_at.elapsed() > max_frame_age {
                                                    continue;
                                                }

                                                let _ = vt.write_sample(&Sample {
                                                    data: bytes::Bytes::from(latest.data),
                                                    duration: latest.duration.max(dur),
                                                    ..Default::default()
                                                }).await;
                                            }
                                        });
                                    }
                                }
                            }
                        }
                        Some("ice") => {
                            if let Some(candidate) = v["candidate"].as_str() {
                                pc.add_ice_candidate(webrtc::ice_transport::ice_candidate::RTCIceCandidateInit {
                                    candidate: candidate.to_string(),
                                    sdp_mid: v["sdpMid"].as_str().map(|s| s.to_owned()),
                                    sdp_mline_index: v["sdpMLineIndex"].as_u64().map(|n| n as u16),
                                    ..Default::default()
                                }).await?;
                            }
                        }
                        _ => {}
                    }
                }
            }
        }

        pc.close().await?;
        Ok(())
    }.await;

    stop_flag.store(true, Ordering::SeqCst);
    stop_pipeline(prev);
    result
}
