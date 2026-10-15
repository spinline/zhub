mod hub;
mod radio;
mod store;
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
#[command(version, about = "Ziggurat üzerinde hafif Zigbee merkezi")]
struct Args {
    /// RCP'nin seri portu
    #[arg(long, default_value = "/dev/ttyUSB0")]
    device: String,

    /// Seri hız; Sonoff'un OpenThread RCP 3.x derlemeleri 921600 kullanıyor
    #[arg(long, default_value_t = 921_600)]
    baudrate: u32,

    /// Ağ ve cihaz kayıtlarının tutulduğu dosya
    #[arg(long, default_value = "/opt/var/lib/zhub/state.json")]
    state: PathBuf,

    /// Web arayüzünün dinleyeceği adres
    #[arg(long, default_value = "0.0.0.0:8099")]
    listen: String,

    /// Yalnızca yeni ağ kurulurken kullanılır
    #[arg(long, default_value_t = 11, value_parser = clap::value_parser!(u8).range(11..=26))]
    channel: u8,

    /// Verici gücü (dBm); yalnızca yeni ağ kurulurken kullanılır
    #[arg(long, default_value_t = 8)]
    tx_power: i8,

    /// Günlük düzeyi (RUST_LOG ayrıntılı süzgeçlerle üstün gelir)
    #[arg(long, default_value = "info")]
    log_level: String,
}

fn main() -> Result<()> {
    let args = Args::parse();

    tracing_subscriber::fmt()
        // Servis günlüğü dosyaya yazar; renk kodları orada yalnızca gürültü
        .with_ansi(std::io::stderr().is_terminal())
        .with_writer(std::io::stderr)
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(&args.log_level)),
        )
        .init();

    // Modemin dört çekirdeği var ama işin çoğu G/Ç; iki iş parçacığı belleği korur
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
    })
    .await?;

    let listener = tokio::net::TcpListener::bind(&args.listen)
        .await
        .with_context(|| format!("{} dinlenemedi", args.listen))?;
    info!("Web arayüzü: http://{}", args.listen);

    let mut terminate = signal(SignalKind::terminate())?;
    let mut interrupt = signal(SignalKind::interrupt())?;
    let server = axum::serve(listener, web::router(hub.clone()));

    tokio::select! {
        result = server => result?,
        _ = terminate.recv() => info!("SIGTERM alındı, kapanıyor"),
        _ = interrupt.recv() => info!("SIGINT alındı, kapanıyor"),
    }

    hub.refresh_tables();
    hub.save_now().await?;
    info!("Durum kaydedildi");
    Ok(())
}
