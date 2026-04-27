// main.rs — entry point, service scaffolding, env config only

mod capture;
mod encoder;
mod session;
mod control;

use std::sync::{Arc, OnceLock};
use std::sync::atomic::AtomicBool;
use dotenvy::dotenv;
use tracing::{info, Level};
use tracing_subscriber::FmtSubscriber;
use crate::session::DaemonConfig;

/// Shared shutdown flag.  The service stop handler sets this; run_daemon
/// checks it at the top of its reconnect loop and exits cleanly.
pub(crate) static SHUTDOWN: OnceLock<Arc<AtomicBool>> = OnceLock::new();

fn main() {
    dotenv().ok();
    let subscriber = FmtSubscriber::builder()
        .with_max_level(Level::INFO)
        .finish();
    tracing::subscriber::set_global_default(subscriber).ok();

    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--service") {
        #[cfg(windows)]
        service::start();
    } else {
        let config = load_config().expect("Missing required env vars");
        info!("Starting sRemote daemon (standalone)");

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            if let Err(e) = session::run_daemon(config).await {
                eprintln!("Fatal: {:#}", e);
            }
        });
    }
}

fn load_config() -> Option<DaemonConfig> {
    let args: Vec<String> = std::env::args().collect();

    let jwt_secret   = std::env::var("JWT_SECRET").ok()?;
    let broker_url   = std::env::var("BROKER_WS_URL").unwrap_or_else(|_| "ws://127.0.0.1:5695".into());
    let room         = std::env::var("AGENT_ROOM").unwrap_or_else(|_| "room-001".into());
    let subject      = std::env::var("AGENT_SUBJECT").unwrap_or_else(|_| "daemon-01".into());
    // FPS: CLI > Env > 30
    let mut fps = std::env::var("CAPTURE_FPS").ok().and_then(|s| s.parse().ok()).unwrap_or(30u32);
    if let Some(pos) = args.iter().position(|a| a == "--fps") {
        if let Some(val) = args.get(pos + 1).and_then(|s| s.parse().ok()) {
            fps = val;
        }
    }

    // Bitrate
    let bitrate_kbps = std::env::var("INITIAL_BITRATE_KBPS").ok().and_then(|s| s.parse().ok()).unwrap_or(3000u32);

    // Width/Height: CLI > Env > None
    let mut width = std::env::var("CAPTURE_WIDTH").ok().and_then(|s| s.parse().ok());
    if let Some(pos) = args.iter().position(|a| a == "--width") {
        if let Some(val) = args.get(pos + 1).and_then(|s| s.parse().ok()) {
            width = Some(val);
        }
    }

    let mut height = std::env::var("CAPTURE_HEIGHT").ok().and_then(|s| s.parse().ok());
    if let Some(pos) = args.iter().position(|a| a == "--height") {
        if let Some(val) = args.get(pos + 1).and_then(|s| s.parse().ok()) {
            height = Some(val);
        }
    }


    Some(DaemonConfig { jwt_secret, broker_url, room, subject, fps, bitrate_kbps, width, height })
}

#[cfg(windows)]
mod service {
    use std::ffi::OsString;
    use std::sync::atomic::Ordering;
    use std::sync::Arc;
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
            tracing::error!("Service error: {:#}", e);
        }
    }

    fn run_service() -> windows_service::Result<()> {
        // Register the stop handler before updating status so SCM can call Stop.
        let status_handle = service_control_handler::register(
            "sRemoteDaemon",
            move |ctrl| match ctrl {
                ServiceControl::Stop => {
                    // Signal the daemon's reconnect loop to exit.
                    if let Some(flag) = super::SHUTDOWN.get() {
                        flag.store(true, Ordering::SeqCst);
                    }
                    ServiceControlHandlerResult::NoError
                }
                ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
                _ => ServiceControlHandlerResult::NotImplemented,
            },
        )?;

        status_handle.set_service_status(ServiceStatus {
            service_type:     ServiceType::OWN_PROCESS,
            current_state:    ServiceState::Running,
            controls_accepted: ServiceControlAccept::STOP,
            exit_code:        ServiceExitCode::Win32(0),
            checkpoint:       0,
            wait_hint:        Duration::default(),
            process_id:       None,
        })?;

        let config = super::load_config().expect("Config missing for service");

        // Store the shutdown flag so the stop handler can access it.
        let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
        super::SHUTDOWN.set(Arc::clone(&shutdown)).ok();

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            // run_daemon loops until SHUTDOWN is set.
            while !shutdown.load(Ordering::SeqCst) {
                if let Err(e) = super::session::run_daemon_once(&config).await {
                    tracing::warn!("Session error: {:#}", e);
                    tokio::time::sleep(Duration::from_secs(4)).await;
                }
            }
        });

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
