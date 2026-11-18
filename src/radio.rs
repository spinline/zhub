use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncReadExt;
use tokio_serial::{FlowControl, SerialPortBuilderExt};

use ziggurat_driver::ziggurat_ieee_802154::types::Eui64;
use ziggurat_phy_spinel::SpinelPhy;
use ziggurat_spinel::client::SpinelClient;

/// Serial flow control, as the RCP firmware was built
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Flow {
    None,
    /// XON/XOFF: the RCP pauses the host when its receive buffer fills. Spinel's HDLC
    /// framing escapes both bytes, so the data never contains them.
    Software,
    /// RTS/CTS uses hardware lines to pause either end of the serial link.
    Hardware,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SerialSettings {
    pub baudrate: u32,
    pub flow_control: Flow,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstalledFirmwareSerial {
    pub device: String,
    pub original_cli: SerialSettings,
    pub installed: SerialSettings,
}

pub struct Radio {
    pub phy: Arc<SpinelPhy>,
    pub spinel: Arc<SpinelClient>,
    pub ieee: Eui64,
}

/// On some dongles opening the port resets the RCP via
/// the DTR/RTS lines. Wait for that boot to finish and drain the leftover bytes, or the
/// Spinel client's first command collides with the reset.
const BOOT_SETTLE: Duration = Duration::from_secs(2);

pub async fn open(device: &str, settings: SerialSettings, sniff: bool) -> Result<Radio> {
    let mut port = tokio_serial::new(device, settings.baudrate)
        .flow_control(match settings.flow_control {
            Flow::None => FlowControl::None,
            Flow::Software => FlowControl::Software,
            Flow::Hardware => FlowControl::Hardware,
        })
        .open_native_async()
        .with_context(|| format!("cannot open {device}"))?;

    tokio::time::sleep(BOOT_SETTLE).await;
    let mut scratch = [0u8; 256];
    let _ = tokio::time::timeout(Duration::from_millis(200), async {
        loop {
            if port.read(&mut scratch).await? == 0 {
                return Ok::<_, std::io::Error>(());
            }
        }
    })
    .await;

    let spinel = if sniff {
        Arc::new(SpinelClient::new(Box::new(crate::sniff::Tap::new(port))))
    } else {
        Arc::new(SpinelClient::new(Box::new(port)))
    };
    spinel.set_exit_on_serial_failure(false);
    let phy = Arc::new(SpinelPhy::new(spinel.clone()));
    match phy.hw_address().await {
        Ok(ieee) => {
            spinel.set_exit_on_serial_failure(true);
            Ok(Radio { phy, spinel, ieee })
        }
        Err(error) => {
            spinel.stop_reader().await;
            Err(anyhow!("OpenThread RCP probe failed: {error}"))
        }
    }
}
