use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::io::AsyncReadExt;
use tokio_serial::{FlowControl, SerialPortBuilderExt};

use ziggurat_phy_spinel::SpinelPhy;
use ziggurat_spinel::client::SpinelClient;

/// Serial flow control, as the RCP firmware was built
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Flow {
    None,
    /// XON/XOFF: the RCP pauses the host when its receive buffer fills. Spinel's HDLC
    /// framing escapes both bytes, so the data never contains them.
    Software,
}

/// On some dongles opening the port resets the RCP via
/// the DTR/RTS lines. Wait for that boot to finish and drain the leftover bytes, or the
/// Spinel client's first command collides with the reset.
const BOOT_SETTLE: Duration = Duration::from_secs(2);

pub async fn open(device: &str, baudrate: u32, flow: Flow, sniff: bool) -> Result<(Arc<SpinelPhy>, Arc<SpinelClient>)> {
    let mut port = tokio_serial::new(device, baudrate)
        .flow_control(match flow {
            Flow::None => FlowControl::None,
            Flow::Software => FlowControl::Software,
        })
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
