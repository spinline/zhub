mod backup;
mod devices;
mod firmware;
mod hub;
mod hue_reset;
mod logging;
#[cfg(feature = "matter")]
mod matter;
mod mqtt;
mod networkmap;
mod radio;
mod scenes;
mod sniff;
mod store;
mod thermostat;
mod tuya;
mod web;
mod z2m_light;
mod zcl;
mod zdo;

use std::io::IsTerminal;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;
use tokio::signal::unix::{SignalKind, signal};
use tracing::info;

#[derive(Debug, Parser)]
#[command(version, about = "A lightweight Zigbee hub on top of Ziggurat")]
struct Args {
    /// Serial port of the RCP
    #[arg(long, default_value = "/dev/ttyUSB0")]
    device: String,

    /// Serial baud rate; it must match the RCP firmware build
    #[arg(long, default_value_t = 921_600)]
    baudrate: u32,

    /// Serial flow control must match the RCP build (software for sw_flow, hardware for hw_flow)
    #[arg(long, value_enum, default_value_t = radio::Flow::None)]
    flow_control: radio::Flow,

    /// File holding the network and device records
    #[arg(long, default_value = "/opt/var/lib/zhub/state.json")]
    state: PathBuf,

    /// Address the web UI listens on
    #[arg(long, default_value = "0.0.0.0:8099")]
    listen: String,

    /// Only used when forming a new network
    #[arg(long, default_value_t = 11, value_parser = clap::value_parser!(u8).range(11..=26))]
    channel: u8,

    /// Transmit power (dBm); only used when forming a new network
    #[arg(long, default_value_t = 8)]
    tx_power: i8,

    /// Diagnostics: put the radio in promiscuous mode and log MAC command frames
    #[arg(long)]
    sniff: bool,

    /// Remove the web UI password (for a forgotten one), then run as usual
    #[arg(long)]
    reset_ui_password: bool,

    /// Log level (RUST_LOG takes precedence; a level saved in the UI overrides both)
    #[arg(long, default_value = "info")]
    log_level: String,
}

fn main() -> Result<()> {
    let args = Args::parse();

    let level = std::env::var("RUST_LOG").unwrap_or_else(|_| args.log_level.clone());
    // The service logs to a file, where color codes are just noise
    let logging = logging::Logging::init(&level, std::io::stderr().is_terminal())?;

    // The router has four cores but the work is mostly I/O; two threads save memory
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    runtime.block_on(run(args, logging))
}

async fn run(args: Args, logging: std::sync::Arc<logging::Logging>) -> Result<()> {
    backup::apply_pending(&args.state).context("cannot complete backup restore")?;
    let mut saved = store::Persisted::load(&args.state)?;
    if args.reset_ui_password && saved.settings.ui_auth.take().is_some() {
        saved.save(&args.state)?;
    }
    let cli_serial = radio::SerialSettings {
        baudrate: args.baudrate,
        flow_control: args.flow_control,
    };
    let mut serial = saved
        .radio_serial
        .as_ref()
        .filter(|saved| saved.device == args.device && saved.original_cli == cli_serial)
        .map_or(cli_serial, |saved| saved.installed);
    let mut terminate = signal(SignalKind::terminate())?;
    let mut interrupt = signal(SignalKind::interrupt())?;
    let bind = || async {
        tokio::net::TcpListener::bind(&args.listen)
            .await
            .with_context(|| format!("cannot listen on {}", args.listen))
    };

    let radio = match radio::open(&args.device, serial, args.sniff).await {
        Ok(radio) => radio,
        Err(error) => {
            tracing::warn!("Radio setup required: {error:#}");
            let (setup, receive) = firmware::Setup::new(
                args.device.clone(),
                format!("{error:#}"),
                saved.settings.ui_auth.clone(),
            );
            let listener = bind().await?;
            info!("Dongle firmware setup: http://{}", args.listen);
            let server = axum::serve(listener, web::firmware_router(setup.clone()));
            let (radio, connected_serial, uploaded) = tokio::select! {
                result = server => return result.map_err(Into::into),
                connected = setup.wait_for_radio(receive, serial, args.sniff) => connected,
                _ = terminate.recv() => return Ok(()),
                _ = interrupt.recv() => return Ok(()),
            };
            serial = connected_serial;
            if uploaded {
                saved.radio_serial = Some(radio::InstalledFirmwareSerial {
                    device: args.device.clone(),
                    original_cli: cli_serial,
                    installed: serial,
                });
            }
            radio
        }
    };

    let hub = hub::Hub::start(
        hub::Options {
            device: args.device,
            baudrate: serial.baudrate,
            flow_control: serial.flow_control,
            state_path: args.state,
            channel: args.channel,
            tx_power: args.tx_power,
            sniff: args.sniff,
            logging,
        },
        saved,
        radio,
    )
    .await?;

    tokio::spawn(mqtt::run(hub.clone()));
    #[cfg(feature = "matter")]
    if let Some(commands) = hub.take_matter_commands() {
        matter::start(hub.clone(), hub.data_dir().join("matter"), commands);
    }

    let listener = bind().await?;
    info!("Web UI: http://{}", args.listen);

    let server = axum::serve(listener, web::router(hub.clone()));

    tokio::select! {
        result = server => result?,
        _ = terminate.recv() => info!("SIGTERM received, shutting down"),
        _ = interrupt.recv() => info!("SIGINT received, shutting down"),
        // The service script starts zhub again after it exits
        _ = hub.restart_requested() => info!("Restarting"),
    }

    hub.refresh_tables();
    hub.save_now().await?;
    info!("State saved");
    Ok(())
}
