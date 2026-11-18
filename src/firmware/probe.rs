//! Identify the running serial protocol without entering the bootloader.

use std::io;
use std::time::Duration;

use serde::Serialize;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::time::{sleep, timeout};
use tokio_serial::{ClearBuffer, FlowControl, SerialPort, SerialPortBuilderExt};
use ziggurat_spinel::{HdlcLiteFrame, SpinelCommandId, parse_spinel_parts};

use super::{crc16, read_ember_ack, wait_for};

const RESPONSE_TIMEOUT: Duration = Duration::from_millis(650);

#[derive(Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Ember,
    ZStack,
    OpenThread,
    Spinel,
    Cpc,
    GeckoBootloader,
}

#[derive(Clone, Serialize)]
pub struct Firmware {
    kind: Kind,
    name: &'static str,
    version: Option<String>,
    baudrate: u32,
}

impl Firmware {
    pub fn name(&self) -> &'static str {
        self.name
    }

    fn new(kind: Kind, version: Option<String>, baudrate: u32) -> Self {
        let name = match kind {
            Kind::Ember => "EmberZNet",
            Kind::ZStack => "Z-Stack (ZNP)",
            Kind::OpenThread => "OpenThread (Spinel)",
            Kind::Spinel => "Spinel",
            Kind::Cpc => "Silicon Labs CPC",
            Kind::GeckoBootloader => "Gecko bootloader",
        };
        Self {
            kind,
            name,
            version,
            baudrate,
        }
    }
}

pub enum Outcome {
    Unavailable,
    Unknown,
    Found(Firmware),
}

impl Outcome {
    pub fn firmware(&self) -> Option<&Firmware> {
        match self {
            Self::Found(firmware) => Some(firmware),
            Self::Unavailable | Self::Unknown => None,
        }
    }

    pub fn kind(&self) -> Option<Kind> {
        self.firmware().map(|firmware| firmware.kind)
    }
}

pub async fn identify(device: &str, configured_baud: u32) -> Outcome {
    // Ember can use software flow control. Give ASH its own connection and
    // reset window before trying the binary protocols on a raw serial port.
    if probe_ember(device, 115_200).await.is_ok() {
        return Outcome::Found(Firmware::new(Kind::Ember, None, 115_200));
    }
    if configured_baud != 115_200 && probe_ember(device, configured_baud).await.is_ok() {
        return Outcome::Found(Firmware::new(Kind::Ember, None, configured_baud));
    }

    let baudrates = [115_200, configured_baud, 460_800, 230_400];
    let open = |baudrate| {
        tokio_serial::new(device, baudrate)
            .flow_control(FlowControl::None)
            .open_native_async()
    };
    let Ok(mut port) = open(115_200).or_else(|_| open(configured_baud)) else {
        return Outcome::Unavailable;
    };
    sleep(Duration::from_millis(750)).await;

    for (index, &baudrate) in baudrates.iter().enumerate() {
        if baudrates[..index].contains(&baudrate) {
            continue;
        }
        if port.baud_rate().ok() != Some(baudrate) && port.set_baud_rate(baudrate).is_err() {
            continue;
        }

        // Check for a bootloader prompt before sending binary requests at its baud
        // rate. A newline elicits the menu without selecting an option.
        if baudrate == 115_200
            && port.clear(ClearBuffer::Input).is_ok()
            && matches!(
                timeout(RESPONSE_TIMEOUT, async {
                    port.write_all(b"\n").await?;
                    wait_for(&mut port, b"Gecko Bootloader").await?;
                    wait_for(&mut port, b"BL >").await
                })
                .await,
                Ok(Ok(()))
            )
        {
            return Outcome::Found(Firmware::new(Kind::GeckoBootloader, None, baudrate));
        }

        let _ = port.clear(ClearBuffer::Input);
        if let Ok(Ok(version)) = timeout(RESPONSE_TIMEOUT, async {
            // Spinel PROP_VALUE_GET, TID 1, NCP_VERSION.
            let frame = HdlcLiteFrame {
                data: vec![0x81, 0x02, 0x02],
            };
            port.write_all(&frame.to_bytes_with_flags()).await?;
            read_spinel_version(&mut port).await
        })
        .await
        {
            let kind = if version.to_ascii_lowercase().contains("openthread") {
                Kind::OpenThread
            } else {
                Kind::Spinel
            };
            return Outcome::Found(Firmware::new(kind, Some(version), baudrate));
        }

        let _ = port.clear(ClearBuffer::Input);
        if let Ok(Ok(version)) = timeout(RESPONSE_TIMEOUT, async {
            // Texas Instruments MT SYS_VERSION request.
            port.write_all(&[0xfe, 0, 0x21, 0x02, 0x23]).await?;
            read_znp_version(&mut port).await
        })
        .await
        {
            return Outcome::Found(Firmware::new(Kind::ZStack, Some(version), baudrate));
        }

        let _ = port.clear(ClearBuffer::Input);
        if let Ok(Ok(version)) = timeout(RESPONSE_TIMEOUT, async {
            port.write_all(&cpc_version_request()).await?;
            read_cpc_version(&mut port).await
        })
        .await
        {
            return Outcome::Found(Firmware::new(Kind::Cpc, Some(version), baudrate));
        }
    }
    Outcome::Unknown
}

async fn probe_ember(device: &str, baudrate: u32) -> anyhow::Result<()> {
    let mut port = tokio_serial::new(device, baudrate)
        .flow_control(FlowControl::Software)
        .open_native_async()?;
    sleep(Duration::from_millis(750)).await;
    port.clear(ClearBuffer::Input)?;
    // ASH CANCEL, followed by RST, its CRC and the frame delimiter.
    port.write_all(&[0x1a, 0xc0, 0x38, 0xbc, 0x7e]).await?;
    timeout(Duration::from_millis(1500), read_ember_ack(&mut port)).await??;
    Ok(())
}

async fn read_spinel_version(port: &mut (impl AsyncRead + Unpin)) -> io::Result<String> {
    let mut packet = Vec::with_capacity(128);
    let mut incoming = Vec::with_capacity(128);
    loop {
        let byte = port.read_u8().await?;
        if byte != 0x7e {
            if packet.len() < 256 {
                packet.push(byte);
            }
            continue;
        }
        if ziggurat_spinel::hdlc_unescape_into(&packet, &mut incoming).is_ok()
            && let Ok((header, SpinelCommandId::PropValueIs, [0x02, version @ ..])) =
                parse_spinel_parts(&incoming)
            && header.transaction_id == 1
            && header.network_link_id == 0
            && let Some(end) = version.iter().position(|&byte| byte == 0)
            && let Ok(version) = std::str::from_utf8(&version[..end])
            && !version.is_empty()
            && !version.chars().any(char::is_control)
        {
            return Ok(version.to_owned());
        }
        packet.clear();
    }
}

async fn read_znp_version(port: &mut (impl AsyncRead + Unpin)) -> io::Result<String> {
    loop {
        if port.read_u8().await? != 0xfe {
            continue;
        }
        let len = usize::from(port.read_u8().await?);
        let mut frame = [0u8; 253];
        if len > 250 {
            continue;
        }
        let frame = &mut frame[..len + 3];
        port.read_exact(frame).await?;
        if !(5..=32).contains(&len)
            || frame[..2] != [0x61, 0x02]
            || frame
                .iter()
                .fold(len as u8, |checksum, byte| checksum ^ byte)
                != 0
        {
            continue;
        }
        let version = format!("{}.{}.{}", frame[4], frame[5], frame[6]);
        if len >= 9 {
            let revision = u32::from_le_bytes(frame[7..11].try_into().unwrap());
            return Ok(format!("{version} ({revision})"));
        }
        return Ok(version);
    }
}

fn cpc_version_request() -> [u8; 17] {
    // CPC unnumbered system endpoint frame, PROP_VALUE_GET for SECONDARY_CPC_VERSION.
    let mut packet = [0x14, 0, 10, 0, 0xc4, 0, 0, 2, 0, 4, 0, 3, 0, 0, 0, 0, 0];
    let header_crc = crc16(&packet[..5], 0).to_le_bytes();
    let body_crc = crc16(&packet[7..15], 0).to_le_bytes();
    packet[5..7].copy_from_slice(&header_crc);
    packet[15..].copy_from_slice(&body_crc);
    packet
}

async fn read_cpc_version(port: &mut (impl AsyncRead + Unpin)) -> io::Result<String> {
    loop {
        if port.read_u8().await? != 0x14 {
            continue;
        }
        let mut header = [0x14, 0, 0, 0, 0, 0, 0];
        port.read_exact(&mut header[1..]).await?;
        let length = u16::from_le_bytes([header[2], header[3]]);
        if crc16(&header[..5], 0).to_le_bytes() != header[5..7] || length != 22 {
            continue;
        }
        let mut body = [0; 22];
        port.read_exact(&mut body).await?;
        if header[1] != 0
            || header[4] & 0xc0 != 0xc0
            || !body.starts_with(&[6, 0, 16, 0, 3, 0, 0, 0])
            || crc16(&body[..20], 0).to_le_bytes() != body[20..]
        {
            continue;
        }
        let version = |offset| u32::from_le_bytes(body[offset..offset + 4].try_into().unwrap());
        return Ok(format!("{}.{}.{}", version(8), version(12), version(16)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn reads_the_znp_version_after_a_corrupted_response() {
        let valid = [0xfe, 9, 0x61, 2, 2, 1, 3, 0, 2, 0x44, 0x26, 0x35, 1, 0x3e];
        let mut invalid = valid;
        invalid[13] = 0;
        let combined = [invalid, valid].concat();
        assert_eq!(
            read_znp_version(&mut combined.as_slice()).await.unwrap(),
            "3.0.2 (20260420)"
        );
        assert!(read_znp_version(&mut invalid.as_slice()).await.is_err());
    }

    #[tokio::test]
    async fn checks_cpc_header_and_payload() {
        assert_eq!(
            cpc_version_request(),
            [
                0x14, 0, 0x0a, 0, 0xc4, 0x55, 0xd3, 2, 0, 4, 0, 3, 0, 0, 0, 0xdb, 0x12
            ],
        );
        let mut reply = [
            0x14, 0, 0x16, 0, 0xc4, 0x57, 0xe5, 6, 0, 0x10, 0, 3, 0, 0, 0, 4, 0, 0, 0, 5, 0, 0, 0,
            6, 0, 0, 0, 0xeb, 0xc5,
        ];
        assert_eq!(
            read_cpc_version(&mut reply.as_slice()).await.unwrap(),
            "4.5.6"
        );
        reply[15] ^= 1;
        assert!(read_cpc_version(&mut reply.as_slice()).await.is_err());
    }

    #[tokio::test]
    async fn spinel_version_must_answer_the_expected_transaction() {
        let make_version = |header| {
            HdlcLiteFrame {
                data: [&[header, 6, 2], &b"OPENTHREAD/test\0"[..]].concat(),
            }
            .to_bytes_with_flags()
        };
        let invalid = make_version(0x82);
        assert!(read_spinel_version(&mut invalid.as_slice()).await.is_err());
        let valid = make_version(0x81);
        assert_eq!(
            read_spinel_version(&mut valid.as_slice()).await.unwrap(),
            "OPENTHREAD/test"
        );
    }
}
