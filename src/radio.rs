use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::io::AsyncReadExt;
use tokio_serial::{FlowControl, SerialPortBuilderExt};

use ziggurat_phy_spinel::SpinelPhy;
use ziggurat_spinel::client::SpinelClient;

/// On some dongles (e.g. SONOFF Dongle Plus MG24) opening the port resets the RCP via
/// the DTR/RTS lines. Wait for that boot to finish and drain the leftover bytes, or the
/// Spinel client's first command collides with the reset.
const BOOT_SETTLE: Duration = Duration::from_secs(2);

pub async fn open(device: &str, baudrate: u32, sniff: bool) -> Result<(Arc<SpinelPhy>, Arc<SpinelClient>)> {
    let mut port = tokio_serial::new(device, baudrate)
        .flow_control(FlowControl::None)
        .open_native_async()
        .with_context(|| format!("cannot open {device}"))?;

    tokio::time::sleep(BOOT_SETTLE).await;
    let mut scratch = [0u8; 256];
    while let Ok(Ok(read)) =
        tokio::time::timeout(Duration::from_millis(100), port.read(&mut scratch)).await
    {
        if read == 0 {
            break;
        }
    }

    let client = if sniff {
        Arc::new(SpinelClient::new(Box::new(crate::sniff::Tap::new(port))))
    } else {
        Arc::new(SpinelClient::new(Box::new(port)))
    };
    Ok((Arc::new(SpinelPhy::new(client.clone())), client))
}
