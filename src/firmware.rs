//! Startup recovery for supported Silicon Labs OpenThread RCP adapters.

mod probe;

use std::io::ErrorKind;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail, ensure};
use axum::body::Bytes;
use serde::Serialize;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio::time::{sleep, timeout};
use tokio_serial::{FlowControl, SerialPort, SerialPortBuilderExt, UsbPortInfo};

use crate::radio::{self, Radio, SerialSettings};
use crate::store::UiAuth;

pub const MAX_IMAGE_SIZE: usize = 512 * 1024;
const BLOCK_SIZE: usize = 128;
const SERIAL_TIMEOUT: Duration = Duration::from_secs(3);
const SONOFF_RCP_SERIAL: SerialSettings = SerialSettings {
    baudrate: 460_800,
    flow_control: radio::Flow::Software,
};
const SMLIGHT_RCP_SERIAL: SerialSettings = SerialSettings {
    baudrate: 921_600,
    flow_control: radio::Flow::Hardware,
};
const SMLIGHT_BOOTLOADER_HINT: &str = "Unplug the dongle and open its enclosure. Hold FLSH to GND while plugging the dongle back in, then select Flash firmware.";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Model {
    ZbdongleE,
    DonglePmg24,
    DongleLmg21,
    Slzb07,
    Slzb07Mg24,
}

impl Model {
    fn name(self) -> &'static str {
        match self {
            Self::ZbdongleE => "SONOFF ZBDongle-E (USB Dongle Plus V2)",
            Self::DonglePmg24 => "SONOFF Dongle-PMG24",
            Self::DongleLmg21 => "SONOFF Dongle Lite MG21",
            Self::Slzb07 => "SMLIGHT SLZB-07",
            Self::Slzb07Mg24 => "SMLIGHT SLZB-07MG24",
        }
    }

    fn manual_bootloader_hint(self) -> Option<&'static str> {
        match self {
            Self::ZbdongleE | Self::DonglePmg24 | Self::DongleLmg21 => None,
            Self::Slzb07 | Self::Slzb07Mg24 => Some(SMLIGHT_BOOTLOADER_HINT),
        }
    }

    // Only images verified for the board and UART settings are accepted.
    fn image_profile(self) -> (&'static [&'static str], SerialSettings) {
        match self {
            Self::ZbdongleE => (
                &[
                    "3551f38a9707278cc4afa6b844607f5898c8d0a3a1f16e81862bebdaf4471191", // OTBR 2.4.2.0
                    "962100cc25c52ffad0f7320cff2b14678fb69419324923e8f627f382a258d816", // Nerivec 3.1.2
                ],
                SONOFF_RCP_SERIAL,
            ),
            Self::DonglePmg24 => (
                &[
                    "fe8fd88e304b68a678ffd473b95cf5ccbc97e32867d6924ebb8679f6dd5bff87", // Nerivec 3.1.2
                ],
                SONOFF_RCP_SERIAL,
            ),
            Self::DongleLmg21 => (
                &[
                    "5853ee6bfef48572574f48f267347c7ca727f33d0690da8fa0c346794da8ab8c", // Nerivec 3.1.2
                ],
                SONOFF_RCP_SERIAL,
            ),
            Self::Slzb07 => (
                &[
                    "b03b3db01afba5882a55f312050a9ae9fc84119926e38a464b5a3ae886954947", // Nerivec 3.1.2
                ],
                SMLIGHT_RCP_SERIAL,
            ),
            Self::Slzb07Mg24 => (
                &[
                    "13831f1534de2cd33695e204b65ff5e7dd7cefc64db8c790a162f73d2d6dc580", // Nerivec 3.1.2
                ],
                SMLIGHT_RCP_SERIAL,
            ),
        }
    }
}

#[derive(Clone)]
struct Dongle {
    model: Model,
    usb: UsbPortInfo,
    needs_confirmation: bool,
}

#[derive(Serialize)]
pub struct DongleView {
    model: &'static str,
    needs_confirmation: bool,
    current_firmware: Option<&'static str>,
    manual_bootloader_hint: Option<&'static str>,
}

struct Discovery {
    present: bool,
    usb: Option<UsbPortInfo>,
    outcome: probe::Outcome,
    dongle: Option<Dongle>,
}

#[derive(Default, Serialize)]
pub struct DiscoveryView {
    pub dongle: Option<DongleView>,
    pub firmware: Option<probe::Firmware>,
    pub usb_device: Option<String>,
    pub serial_available: bool,
    pub detecting: bool,
}

pub struct Job {
    image: Bytes,
    dongle: Dongle,
    serial: SerialSettings,
}

#[derive(Clone, Serialize)]
#[serde(tag = "phase", rename_all = "snake_case")]
pub enum Status {
    Idle,
    Queued,
    EnteringBootloader,
    Writing { sent: usize, total: usize },
    Restarting,
    Succeeded,
    Failed { error: String },
}

pub struct Setup {
    pub device: String,
    pub startup_error: String,
    pub auth: Option<UiAuth>,
    discovery: Mutex<Option<Discovery>>,
    status: Mutex<Status>,
    jobs: mpsc::Sender<Job>,
}

impl Setup {
    pub fn new(
        device: String,
        startup_error: String,
        auth: Option<UiAuth>,
    ) -> (Arc<Self>, mpsc::Receiver<Job>) {
        let (jobs, receive) = mpsc::channel(1);
        let setup = Self {
            device,
            startup_error,
            auth,
            discovery: Mutex::new(None),
            status: Mutex::new(Status::Idle),
            jobs,
        };
        (Arc::new(setup), receive)
    }

    pub fn status(&self) -> Status {
        self.status.lock().unwrap().clone()
    }

    pub fn discovery(&self) -> DiscoveryView {
        let current = self.discovery.lock().unwrap();
        let Some(current) = current.as_ref() else {
            return DiscoveryView {
                detecting: true,
                ..Default::default()
            };
        };
        let firmware = current.outcome.firmware();
        DiscoveryView {
            dongle: current.dongle.as_ref().map(|dongle| DongleView {
                model: dongle.model.name(),
                needs_confirmation: dongle.needs_confirmation,
                current_firmware: firmware.map(probe::Firmware::name),
                manual_bootloader_hint: dongle.model.manual_bootloader_hint(),
            }),
            firmware: firmware.cloned(),
            usb_device: current.usb.as_ref().and_then(|usb| usb.product.clone()),
            serial_available: !matches!(current.outcome, probe::Outcome::Unavailable),
            detecting: false,
        }
    }

    pub fn submit(&self, bytes: Bytes, confirmed: bool) -> Result<()> {
        let dongle = self
            .discovery
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|discovery| discovery.dongle.clone())
            .context("No supported dongle detected")?;
        ensure!(
            !dongle.needs_confirmation || confirmed,
            "Confirm the dongle model before flashing"
        );
        let serial = validate_image(dongle.model, &bytes)?;
        let mut status = self.status.lock().unwrap();
        ensure!(
            matches!(*status, Status::Idle | Status::Failed { .. }),
            "A firmware operation is already in progress"
        );
        self.jobs
            .try_send(Job {
                image: bytes,
                dongle,
                serial,
            })
            .context("Cannot queue the firmware upload")?;
        *status = Status::Queued;
        Ok(())
    }

    async fn detect_dongle(&self, configured_baud: u32, refresh: bool) {
        let present = Path::new(&self.device).exists();
        let usb = usb_port(&self.device);
        if !refresh
            && self
                .discovery
                .lock()
                .unwrap()
                .as_ref()
                .is_some_and(|discovery| {
                    discovery.present == present
                        && discovery.usb == usb
                        && !(present && matches!(discovery.outcome, probe::Outcome::Unavailable))
                })
        {
            return;
        }
        *self.discovery.lock().unwrap() = None;
        let outcome = if present {
            probe::identify(&self.device, configured_baud).await
        } else {
            probe::Outcome::Unavailable
        };
        let dongle = if matches!(outcome, probe::Outcome::Unavailable) {
            None
        } else {
            usb.as_ref()
                .and_then(|usb| flashable_dongle(usb, outcome.kind()))
        };
        *self.discovery.lock().unwrap() = Some(Discovery {
            present,
            usb,
            outcome,
            dongle,
        });
    }

    fn set_status(&self, status: Status) {
        *self.status.lock().unwrap() = status;
    }

    pub async fn wait_for_radio(
        &self,
        mut receive: mpsc::Receiver<Job>,
        mut serial: SerialSettings,
        sniff: bool,
    ) -> (Radio, SerialSettings, bool) {
        let mut uploaded = false;
        self.detect_dongle(serial.baudrate, false).await;
        loop {
            let mut verifying_upload = false;
            tokio::select! {
                biased;
                Some(job) = receive.recv() => {
                    let image_serial = job.serial;
                    let result = timeout(Duration::from_secs(180), self.flash(job)).await;
                    match result {
                        Ok(Ok(())) => {
                            serial = image_serial;
                            uploaded = true;
                            self.set_status(Status::Restarting);
                            verifying_upload = true;
                        }
                        Ok(Err(error)) => {
                            self.set_status(Status::Failed { error: format!("{error:#}") });
                            self.detect_dongle(serial.baudrate, true).await;
                            continue;
                        }
                        Err(_) => {
                            self.set_status(Status::Failed { error: "Firmware upload timed out".into() });
                            self.detect_dongle(serial.baudrate, true).await;
                            continue;
                        }
                    }
                }
                _ = sleep(Duration::from_secs(10)) => {}
            }
            match radio::open(&self.device, serial, sniff).await {
                Ok(radio) => {
                    if verifying_upload {
                        self.set_status(Status::Succeeded);
                        sleep(Duration::from_secs(3)).await;
                    }
                    return (radio, serial, uploaded);
                }
                Err(error) if verifying_upload => {
                    self.set_status(Status::Failed {
                        error: format!("Firmware written, but the RCP did not respond: {error:#}"),
                    });
                    self.detect_dongle(serial.baudrate, true).await;
                }
                Err(_) => self.detect_dongle(serial.baudrate, false).await,
            }
        }
    }

    async fn flash(&self, job: Job) -> Result<()> {
        ensure!(
            usb_port(&self.device).as_ref() == Some(&job.dongle.usb),
            "The dongle changed before flashing"
        );
        let image = &job.image;
        self.set_status(Status::EnteringBootloader);
        let mut port = tokio_serial::new(&self.device, 115_200)
            .flow_control(FlowControl::None)
            .open_native_async()
            .with_context(|| format!("Cannot open {}", self.device))?;
        if let Some(discovery) = self.discovery.lock().unwrap().as_mut() {
            discovery.outcome = probe::Outcome::Unknown;
        }

        let manual_hint = job.dongle.model.manual_bootloader_hint();
        if manual_hint.is_none() {
            // SONOFF's USB-UART modem lines drive BOOT and RESET.
            for (dtr, rts, delay_ms) in [(false, true, 100), (true, false, 500), (false, false, 0)]
            {
                port.write_data_terminal_ready(dtr)
                    .context("Cannot set DTR")?;
                port.write_request_to_send(rts).context("Cannot set RTS")?;
                sleep(Duration::from_millis(delay_ms)).await;
            }
            sleep(Duration::from_millis(250)).await;
        }
        open_gecko_menu(&mut port).await.with_context(|| {
            manual_hint.map_or_else(
                || "Gecko bootloader did not answer".to_owned(),
                |hint| format!("Gecko bootloader did not answer. {hint}"),
            )
        })?;

        write_image(&mut port, image, |sent| {
            self.set_status(Status::Writing {
                sent,
                total: image.len(),
            })
        })
        .await
    }
}

fn validate_image(model: Model, bytes: &[u8]) -> Result<SerialSettings> {
    ensure!(
        !bytes.is_empty() && bytes.len() <= MAX_IMAGE_SIZE,
        "Invalid firmware file size"
    );
    let digest = crate::store::hex(&Sha256::digest(bytes));
    let (digests, serial) = model.image_profile();
    ensure!(
        digests.contains(&digest.as_str()),
        "This file is not a supported OpenThread RCP image for {}",
        model.name()
    );
    Ok(serial)
}

#[cfg(target_os = "linux")]
fn usb_port(device: &str) -> Option<UsbPortInfo> {
    let tty = Path::new(device).canonicalize().ok()?;
    let sysfs = Path::new("/sys/class/tty")
        .join(tty.file_name()?)
        .join("device")
        .canonicalize()
        .ok()?;
    let usb = sysfs
        .ancestors()
        .find(|path| path.join("idVendor").is_file())?;
    let read = |name| {
        std::fs::read_to_string(usb.join(name))
            .ok()
            .map(|value| value.trim().to_owned())
    };
    Some(UsbPortInfo {
        vid: u16::from_str_radix(&read("idVendor")?, 16).ok()?,
        pid: u16::from_str_radix(&read("idProduct")?, 16).ok()?,
        serial_number: read("serial"),
        manufacturer: read("manufacturer"),
        product: read("product"),
    })
}

#[cfg(not(target_os = "linux"))]
fn usb_port(device: &str) -> Option<UsbPortInfo> {
    let device = Path::new(device).canonicalize().ok()?;
    tokio_serial::available_ports()
        .ok()?
        .into_iter()
        .find_map(|port| match port.port_type {
            tokio_serial::SerialPortType::UsbPort(usb)
                if Path::new(&port.port_name).canonicalize().ok().as_ref() == Some(&device) =>
            {
                Some(usb)
            }
            _ => None,
        })
}

fn match_usb(usb: &UsbPortInfo) -> Option<(Model, bool)> {
    let manufacturer = usb.manufacturer.as_deref()?;
    let product = usb.product.as_deref()?.to_ascii_lowercase();
    match (usb.vid, usb.pid, product.as_str()) {
        (0x10c4, 0xea60, "sonoff dongle plus mg24" | "sonoff dongle-pmg24" | "dongle-pmg24")
            if manufacturer.eq_ignore_ascii_case("sonoff") =>
        {
            Some((Model::DonglePmg24, false))
        }
        (0x10c4, 0xea60, "sonoff dongle lite mg21" | "sonoff dongle-lmg21" | "dongle-lmg21")
            if manufacturer.eq_ignore_ascii_case("sonoff") =>
        {
            Some((Model::DongleLmg21, false))
        }
        (0x10c4, 0xea60, "smlight slzb-07" | "slzb-07")
            if manufacturer.eq_ignore_ascii_case("smlight") =>
        {
            Some((Model::Slzb07, false))
        }
        (0x10c4, 0xea60, "smlight slzb-07mg24" | "slzb-07mg24")
            if manufacturer.eq_ignore_ascii_case("smlight") =>
        {
            Some((Model::Slzb07Mg24, false))
        }
        (0x10c4, 0xea60, "sonoff zigbee 3.0 usb dongle plus v2")
        | (0x1a86, 0x55d4, "sonoff zigbee 3.0 usb dongle plus v2")
            if manufacturer.eq_ignore_ascii_case("itead")
                || manufacturer.eq_ignore_ascii_case("sonoff") =>
        {
            Some((Model::ZbdongleE, false))
        }
        (0x10c4, 0xea60, "sonoff zigbee 3.0 usb dongle plus")
            if manufacturer.eq_ignore_ascii_case("itead") =>
        {
            Some((Model::ZbdongleE, true))
        }
        _ => None,
    }
}

fn flashable_dongle(usb: &UsbPortInfo, firmware: Option<probe::Kind>) -> Option<Dongle> {
    let (model, ambiguous) = match_usb(usb)?;
    if firmware == Some(probe::Kind::ZStack) {
        return None;
    }
    // E and P units can share a USB descriptor. A validated Silicon Labs
    // response distinguishes the E model from the TI-based P.
    let silicon_labs = matches!(
        firmware,
        Some(probe::Kind::Ember | probe::Kind::Cpc | probe::Kind::GeckoBootloader)
    );
    Some(Dongle {
        model,
        usb: usb.clone(),
        needs_confirmation: ambiguous && !silicon_labs,
    })
}

async fn read_ember_ack(port: &mut (impl AsyncRead + Unpin)) -> std::io::Result<()> {
    let mut bytes = [0u8; 5];
    let mut size = 0;
    let mut escaped = false;
    loop {
        let byte = port.read_u8().await?;
        match byte {
            0x7e | 0x1a | 0x18 => {
                if byte == 0x7e
                    && size == bytes.len()
                    && !escaped
                    && bytes[..2] == [0xc1, 2]
                    && crc16(&bytes[..3], 0xffff).to_be_bytes() == bytes[3..]
                {
                    return Ok(());
                }
                size = 0;
                escaped = false;
            }
            0x11 | 0x13 => {}
            0x7d => escaped = true,
            byte => {
                if let Some(place) = bytes.get_mut(size) {
                    *place = if escaped { byte ^ 0x20 } else { byte };
                }
                size = (size + 1).min(bytes.len() + 1);
                escaped = false;
            }
        }
    }
}

async fn write_image(
    port: &mut (impl AsyncRead + AsyncWrite + Unpin),
    image: &[u8],
    progress: impl Fn(usize),
) -> Result<()> {
    port.write_all(b"1").await?;
    // Gecko sends a status line followed by the XMODEM-CRC handshake byte.
    wait_for(port, b"begin upload")
        .await
        .context("Bootloader did not start the upload")?;
    wait_for(port, b"C")
        .await
        .context("Bootloader did not request XMODEM-CRC")?;
    progress(0);
    for (index, block) in image.chunks(BLOCK_SIZE).enumerate() {
        let packet = xmodem_packet(index.wrapping_add(1) as u8, block);
        send_xmodem(port, &packet)
            .await
            .with_context(|| format!("Firmware block {}", index + 1))?;
        progress(index * BLOCK_SIZE + block.len());
    }
    send_xmodem(port, &[0x04])
        .await
        .context("Ending XMODEM transfer")?;
    wait_for(port, b"Serial upload complete")
        .await
        .context("Bootloader did not confirm the image")?;
    if wait_for(port, b"BL >").await.is_err() {
        port.write_all(b"\n").await?;
        wait_for(port, b"BL >")
            .await
            .context("Bootloader menu is unavailable after flashing")?;
    }
    port.write_all(b"2")
        .await
        .context("Cannot start the new firmware")?;
    port.flush().await?;
    Ok(())
}

fn xmodem_packet(number: u8, block: &[u8]) -> [u8; BLOCK_SIZE + 5] {
    let mut packet = [0xff; BLOCK_SIZE + 5];
    packet[..3].copy_from_slice(&[0x01, number, !number]);
    packet[3..3 + block.len()].copy_from_slice(block);
    let crc = crc16(&packet[3..3 + BLOCK_SIZE], 0);
    packet[3 + BLOCK_SIZE..].copy_from_slice(&crc.to_be_bytes());
    packet
}

fn crc16(bytes: &[u8], mut crc: u16) -> u16 {
    for &byte in bytes {
        crc ^= u16::from(byte) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 == 0 {
                crc << 1
            } else {
                (crc << 1) ^ 0x1021
            };
        }
    }
    crc
}

async fn send_xmodem(
    port: &mut (impl AsyncRead + AsyncWrite + Unpin),
    packet: &[u8],
) -> Result<()> {
    for _ in 0..3 {
        timeout(SERIAL_TIMEOUT, port.write_all(packet)).await??;
        let response = timeout(SERIAL_TIMEOUT, async {
            loop {
                let byte = port.read_u8().await?;
                if matches!(byte, 0x06 | 0x15 | 0x18) {
                    return Ok::<_, std::io::Error>(byte);
                }
            }
        })
        .await;
        match response {
            Ok(Ok(0x06)) => return Ok(()),
            Ok(Ok(0x18)) => bail!("Bootloader cancelled XMODEM"),
            Ok(Err(error)) if error.kind() != ErrorKind::TimedOut => return Err(error.into()),
            _ => {}
        }
    }
    Err(anyhow!("Bootloader did not acknowledge the XMODEM packet"))
}

async fn open_gecko_menu(port: &mut (impl AsyncRead + AsyncWrite + Unpin)) -> Result<()> {
    port.write_all(b"\n").await?;
    wait_for(port, b"Gecko Bootloader").await?;
    wait_for(port, b"BL >").await
}

async fn wait_for(port: &mut (impl AsyncRead + Unpin), marker: &[u8]) -> Result<()> {
    timeout(SERIAL_TIMEOUT, async {
        let mut matched = 0;
        loop {
            let byte = port.read_u8().await?;
            if byte == marker[matched] {
                matched += 1;
                if matched == marker.len() {
                    return Ok::<_, std::io::Error>(());
                }
            } else {
                matched = usize::from(byte == marker[0]);
            }
        }
    })
    .await??;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn only_matches_supported_usb_descriptors() {
        let mut usb = UsbPortInfo {
            vid: 0x10c4,
            pid: 0xea60,
            serial_number: None,
            manufacturer: Some("ITead".into()),
            product: Some("Sonoff Zigbee 3.0 USB Dongle Plus".into()),
        };
        assert_eq!(match_usb(&usb), Some((Model::ZbdongleE, true)));
        assert!(flashable_dongle(&usb, Some(probe::Kind::ZStack)).is_none());
        assert!(
            !flashable_dongle(&usb, Some(probe::Kind::Ember))
                .unwrap()
                .needs_confirmation
        );
        usb.product = Some("Sonoff Zigbee 3.0 USB Dongle Plus V2".into());
        assert_eq!(match_usb(&usb), Some((Model::ZbdongleE, false)));
        usb.product = Some("CP2102N USB to UART Bridge Controller".into());
        assert_eq!(match_usb(&usb), None);
        assert!(flashable_dongle(&usb, Some(probe::Kind::Ember)).is_none());
        usb.manufacturer = Some("SONOFF".into());
        usb.product = Some("SONOFF Dongle Plus MG24".into());
        assert_eq!(match_usb(&usb), Some((Model::DonglePmg24, false)));
        usb.product = Some("SONOFF Dongle Lite MG21".into());
        assert_eq!(match_usb(&usb), Some((Model::DongleLmg21, false)));
        usb.manufacturer = Some("SMLIGHT".into());
        usb.product = Some("SMLIGHT SLZB-07".into());
        assert_eq!(match_usb(&usb), Some((Model::Slzb07, false)));
        usb.product = Some("SMLIGHT SLZB-07MG24".into());
        assert_eq!(match_usb(&usb), Some((Model::Slzb07Mg24, false)));
        usb.product = Some("SMLIGHT SLZB-07P7".into());
        assert_eq!(match_usb(&usb), None);
    }

    #[tokio::test]
    async fn takes_the_gecko_menu_as_bootloader_confirmation() {
        for (response, expected) in [
            (&b"\r\nGecko Bootloader v2.4.2\r\nBL > "[..], true),
            (&b"BL > "[..], false),
        ] {
            let (mut host, mut bootloader) = tokio::io::duplex(64);
            let emulate = async {
                assert_eq!(bootloader.read_u8().await.unwrap(), b'\n');
                bootloader.write_all(response).await.unwrap();
                bootloader.shutdown().await.unwrap();
            };
            let (result, ()) = tokio::join!(open_gecko_menu(&mut host), emulate);
            assert_eq!(result.is_ok(), expected);
        }
    }

    #[test]
    fn rejects_unknown_firmware_for_supported_dongles() {
        assert!(
            validate_image(Model::ZbdongleE, b"test")
                .unwrap_err()
                .to_string()
                .contains("not a supported OpenThread RCP image")
        );
    }

    #[tokio::test]
    async fn ember_confirmation_requires_a_valid_rstack() {
        let (mut input, mut output) = tokio::io::duplex(64);
        output
            .write_all(&[0xc1, 2, 0x0b, 0, 0, 0x7e])
            .await
            .unwrap();
        output.shutdown().await.unwrap();
        assert!(read_ember_ack(&mut input).await.is_err());

        let (mut input, mut output) = tokio::io::duplex(64);
        output
            .write_all(&[0x1a, 0xc1, 2, 0x0b, 0x0a, 0x52, 0x7e])
            .await
            .unwrap();
        assert!(
            timeout(Duration::from_millis(100), read_ember_ack(&mut input))
                .await
                .unwrap()
                .is_ok()
        );
    }

    #[tokio::test]
    async fn writes_xmodem_crc_and_retries_a_rejected_block() {
        let (mut host, mut bootloader) = tokio::io::duplex(256);
        let received = Cell::new(0);
        // CRC-16/XMODEM for `123456789` followed by 119 bytes of 0xff.
        let expected_crc = [0x4d, 0x46];
        let transfer = write_image(&mut host, b"123456789", |size| received.set(size));
        let emulate = async {
            assert_eq!(bootloader.read_u8().await.unwrap(), b'1');
            bootloader
                .write_all(b"\r\nbegin upload\r\nC")
                .await
                .unwrap();
            let mut packet = [0; BLOCK_SIZE + 5];
            bootloader.read_exact(&mut packet).await.unwrap();
            assert_eq!(&packet[..12], b"\x01\x01\xfe123456789");
            assert_eq!(&packet[12..131], &[0xff; 119]);
            assert_eq!(&packet[131..], &expected_crc);
            bootloader.write_u8(0x15).await.unwrap();
            let mut retried = [0; BLOCK_SIZE + 5];
            bootloader.read_exact(&mut retried).await.unwrap();
            assert_eq!(packet, retried);
            bootloader.write_u8(0x06).await.unwrap();
            assert_eq!(bootloader.read_u8().await.unwrap(), 0x04);
            bootloader
                .write_all(b"\x06\r\nSerial upload complete\r\nBL > ")
                .await
                .unwrap();
            assert_eq!(bootloader.read_u8().await.unwrap(), b'2');
        };
        let (result, ()) = tokio::join!(transfer, emulate);
        result.unwrap();
        assert_eq!(received.get(), 9);
    }
}
