pub mod state;

use std::process::ExitCode;
use std::sync::Arc;

use state::{AppState, UiEvent, spawn_forwarders};

use crate::config::{self, Config};
use crate::diag::SystemInfo;
use crate::discovery::Advertisement;
use crate::net::{Identity, ListenerEvent, NetContext, PairingManager, start_listener};

/// Full application entry point (moved out of main.rs so the binary stays
/// thin and integration tests can reuse setup helpers).
pub fn run() -> ExitCode {
    match real_main() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("wireless-display: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn real_main() -> anyhow::Result<()> {
    let started = std::time::Instant::now();
    if std::env::args().any(|a| a == "--version") {
        println!("wireless-display {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    let (config, config_path) = Config::load_or_create()?;
    if let Err(problem) = config::validate(&config) {
        anyhow::bail!(
            "invalid configuration at {}: {problem}",
            config_path.display()
        );
    }
    crate::diag::init_tracing(&config.general.log_level);

    gtk::init().map_err(|e| anyhow::anyhow!("failed to initialise GTK4: {e}"))?;
    adw::init().map_err(|e| anyhow::anyhow!("failed to initialise libadwaita: {e}"))?;
    gstreamer::init().map_err(|e| anyhow::anyhow!("failed to initialise GStreamer: {e}"))?;

    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        config = %config_path.display(),
        listen_port = config.network.listen_port,
        "starting wireless-display receiver"
    );

    let system = SystemInfo::probe();
    for probe in &system.decoders {
        tracing::debug!(decoder = %probe.element, available = probe.available, "decoder probe");
    }

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;

    let state = {
        let pairing = Arc::new(PairingManager::new(&config::paired_devices_file()));
        Arc::new(AppState::new(config.clone(), system, pairing))
    };
    spawn_forwarders(&runtime, state.clone());

    // Network stack: identity, pairing store, QUIC listener, mDNS ad.
    // Failure here is logged; the UI still comes up in diagnostics mode.
    let network = match bring_up_network(&runtime, &state, &config) {
        Ok(parts) => Some(parts),
        Err(error) => {
            tracing::error!(%error, "network stack failed to start");
            state.metrics.set_text("app.state", "network-error");
            None
        }
    };

    state.metrics.increment("app.starts");
    state
        .metrics
        .set_gauge("startup.ms", started.elapsed().as_millis() as f64);
    let exit_code = crate::ui::window::run(state.clone());

    if let Some(network) = network {
        network.advertisement.stop();
        network.listener.shutdown();
    }
    runtime.shutdown_timeout(std::time::Duration::from_millis(500));

    if exit_code != 0 {
        tracing::warn!(exit_code, "application exited with non-zero status");
    } else {
        tracing::info!("shutdown complete");
    }
    Ok(())
}

struct NetworkParts {
    listener: crate::net::ListenerHandle,
    advertisement: Advertisement,
}

fn bring_up_network(
    runtime: &tokio::runtime::Runtime,
    state: &Arc<AppState>,
    config: &Config,
) -> anyhow::Result<NetworkParts> {
    let identity = Arc::new(Identity::load_or_create(&config::identity_dir())?);
    let (events_tx, events_rx) = tokio::sync::mpsc::channel(64);

    let bind_addr = std::net::SocketAddr::from(([0, 0, 0, 0], config.network.listen_port));
    let listener = {
        let ctx = NetContext::new(
            identity.clone(),
            state.pairing.clone(),
            state.session.clone(),
            events_tx,
        );
        // start_listener spawns tasks, so it must run inside the runtime.
        runtime
            .block_on(async { start_listener(ctx, bind_addr) })
            .map_err(|e| anyhow::anyhow!("{e}"))?
    };
    tracing::info!(
        addr = %listener.local_addr(),
        fingerprint = %identity.fingerprint_short(),
        "receiver network ready"
    );

    state
        .metrics
        .set_text("net.fingerprint", identity.fingerprint_short());
    state
        .metrics
        .set_gauge("net.port", f64::from(listener.local_addr().port()));

    spawn_listener_forwarder(runtime, state.clone(), events_rx);

    let advertisement = Advertisement::start(
        &format!("Wireless Display on {}", identity.host_name()),
        identity.host_name(),
        primary_local_ip(),
        listener.local_addr().port(),
        identity.fingerprint_short().as_str(),
    )?;

    // Publish initial paired-device list to the UI.
    let devices = state.pairing.list_devices();
    let ui_tx = state.ui_sender();
    runtime.spawn(async move {
        let _ = ui_tx.send(UiEvent::KnownDevices(devices)).await;
    });

    Ok(NetworkParts {
        listener,
        advertisement,
    })
}

fn spawn_listener_forwarder(
    runtime: &tokio::runtime::Runtime,
    state: Arc<AppState>,
    mut events_rx: tokio::sync::mpsc::Receiver<ListenerEvent>,
) {
    runtime.spawn(async move {
        while let Some(event) = events_rx.recv().await {
            let ui = |ev: UiEvent| state.ui_sender().try_send(ev);
            match event {
                ListenerEvent::Connected { name } => {
                    state.metrics.set_text("session.peer", &name);
                    let _ = ui(UiEvent::Peer(Some(name)));
                }
                ListenerEvent::Disconnected { clean, reason } => {
                    state.metrics.set_text("session.peer", "");
                    if !clean {
                        tracing::warn!(%reason, "connection lost");
                    }
                    let _ = ui(UiEvent::Peer(None));
                }
                ListenerEvent::PairingSucceeded { name, .. } => {
                    let _ = ui(UiEvent::PairingOutcome {
                        ok: true,
                        message: format!("Paired with {name}"),
                    });
                }
            }
        }
    });
}

fn primary_local_ip() -> std::net::IpAddr {
    if let Ok(socket) = std::net::UdpSocket::bind("0.0.0.0:0")
        && socket.connect("223.5.5.5:53").is_ok()
        && let Ok(addr) = socket.local_addr()
    {
        return addr.ip();
    }
    std::net::IpAddr::from([127, 0, 0, 1])
}
