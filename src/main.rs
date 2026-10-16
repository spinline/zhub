mod hub;
mod hue_reset;
mod devices;
mod radio;
mod sniff;
mod store;
mod tuya;
mod web;
mod zcl;
mod zdo;

use std::io::IsTerminal;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;
use tokio::signal::unix::{SignalKind, signal};
use tracing::info;
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(version, about = "A lightweight Zigbee hub on top of Ziggurat")]
struct Args {
    /// Serial port of the RCP
    #[arg(long, default_value = "/dev/ttyUSB0")]
    device: String,

    /// Serial baud rate; SONOFF's OpenThread RCP 3.x builds use 921600
    #[arg(long, default_value_t = 921_600)]
    baudrate: u32,

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

    /// Log level (RUST_LOG with per-module filters takes precedence)
    #[arg(long, default_value = "info")]
    log_level: String,
}

fn main() -> Result<()> {
    let args = Args::parse();

    tracing_subscriber::fmt()
        // The service logs to a file, where color codes are just noise
        .with_ansi(std::io::stderr().is_terminal())
        .with_writer(std::io::stderr)
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(&args.log_level)),
        )
        .init();

    // The router has four cores but the work is mostly I/O; two threads save memory
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    runtime.block_on(run(args))
}

async fn run(args: Args) -> Result<()> {
    let hub = hub::Hub::start(hub::Options {
        device: args.device,
        baudrate: args.baudrate,
        state_path: args.state,
        channel: args.channel,
        tx_power: args.tx_power,
        sniff: args.sniff,
    })
    .await?;

    let listener = tokio::net::TcpListener::bind(&args.listen)
        .await
        .with_context(|| format!("{} dinlenemedi", args.listen))?;
    info!("Web UI: http://{}", args.listen);

    let mut terminate = signal(SignalKind::terminate())?;
    let mut interrupt = signal(SignalKind::interrupt())?;
    let server = axum::serve(listener, web::router(hub.clone()));

    tokio::select! {
        result = server => result?,
        _ = terminate.recv() => info!("SIGTERM received, shutting down"),
        _ = interrupt.recv() => info!("SIGINT received, shutting down"),
    }

    hub.refresh_tables();
    hub.save_now().await?;
    info!("Durum kaydedildi");
    Ok(())
}
