//! Diskte tutulan durum: ağ parametreleri, güvenlik sayaçları, yığın tabloları ve
//! cihaz listesi. Tek bir JSON dosyası; yazma geçici dosya + rename ile atomik.

use std::collections::BTreeMap;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use ziggurat_driver::ziggurat_ieee_802154::types::{Eui64, Key};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkData {
    pub channel: u8,
    pub pan_id: u16,
    pub extended_pan_id: String,
    pub ieee: String,
    pub network_key: String,
    #[serde(default)]
    pub network_key_seq: u8,
    #[serde(default)]
    pub nwk_frame_counter: u32,
    #[serde(default)]
    pub aps_frame_counter: u32,
    #[serde(default)]
    pub update_id: u8,
    pub tx_power: i8,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LinkKey {
    pub ieee: String,
    pub key: String,
    #[serde(default)]
    pub rx_counter: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Child {
    pub ieee: String,
    pub nwk: u16,
    pub rx_on_when_idle: bool,
    pub router: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Endpoint {
    pub id: u8,
    pub profile: u16,
    pub device_id: u16,
    pub in_clusters: Vec<u16>,
    pub out_clusters: Vec<u16>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DeviceState {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub on: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub level: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color_temp: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub humidity: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub occupancy: Option<bool>,
    /// IAS Zone alarm1 biti: kapı sensöründe açık, hareket sensöründe hareket
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alarm: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub battery_low: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub battery: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub voltage: Option<f64>,
    /// Termostat: "heat" ya da "off"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub setpoint: Option<f64>,
    /// Termostat şu an ısıtıyor mu
    #[serde(skip_serializing_if = "Option::is_none")]
    pub heating: Option<bool>,
    /// Termostat: "manual" ya da "program"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preset: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frost: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub child_lock: Option<bool>,
    /// Termostatın arıza bit alanı; 0 arıza yok
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fault: Option<u32>,
    /// Haftalık programın gün düzeni: "5_2", "6_1" ya da "7"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub working_week: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub schedule: Option<Vec<ScheduleSlot>>,
}

/// Haftalık programın bir dilimi: bu saatten itibaren hedef sıcaklık
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScheduleSlot {
    /// "SS:DD"
    pub time: String,
    pub temperature: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Device {
    pub ieee: String,
    pub nwk: u16,
    pub name: String,
    #[serde(default)]
    pub manufacturer: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub router: bool,
    #[serde(default = "default_true")]
    pub rx_on_when_idle: bool,
    #[serde(default)]
    pub endpoints: Vec<Endpoint>,
    #[serde(default)]
    pub interviewed: bool,
    #[serde(default)]
    pub zone_type: Option<u16>,
    #[serde(default)]
    pub color_temp_min: Option<u16>,
    #[serde(default)]
    pub color_temp_max: Option<u16>,
    #[serde(default)]
    pub state: DeviceState,
    #[serde(default)]
    pub firmware: Option<String>,
    /// Modele özel ayarların cihazdan okunan değerleri
    #[serde(default)]
    pub settings: BTreeMap<String, f64>,
    #[serde(default)]
    pub last_seen: Option<u64>,
    #[serde(default)]
    pub lqi: Option<u8>,
}

fn default_true() -> bool {
    true
}

impl Device {
    pub fn new(ieee: String, nwk: u16) -> Self {
        let suffix = ieee.replace(':', "");
        let name = format!("Cihaz {}", &suffix[suffix.len().saturating_sub(4)..]);
        Self {
            ieee,
            nwk,
            name,
            manufacturer: None,
            model: None,
            router: false,
            rx_on_when_idle: true,
            endpoints: Vec::new(),
            interviewed: false,
            zone_type: None,
            color_temp_min: None,
            color_temp_max: None,
            state: DeviceState::default(),
            firmware: None,
            settings: BTreeMap::new(),
            last_seen: None,
            lqi: None,
        }
    }

    /// Bir kümeyi sunucu tarafında barındıran ilk uç nokta
    pub fn endpoint_with(&self, cluster: u16) -> Option<u8> {
        self.endpoints
            .iter()
            .find(|ep| ep.in_clusters.contains(&cluster))
            .map(|ep| ep.id)
    }

    pub fn has_cluster(&self, cluster: u16) -> bool {
        self.endpoint_with(cluster).is_some()
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Persisted {
    pub network: Option<NetworkData>,
    #[serde(default)]
    pub link_keys: Vec<LinkKey>,
    #[serde(default)]
    pub children: Vec<Child>,
    #[serde(default)]
    pub devices: BTreeMap<String, Device>,
}

impl Persisted {
    pub fn load(path: &Path) -> Result<Self> {
        match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .with_context(|| format!("{} okunamadı", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("{} açılamadı", path.display())),
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("json.tmp");
        let bytes = serde_json::to_vec_pretty(self)?;
        {
            // Ağ anahtarını içerir; yalnız sahibi okuyabilsin
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&tmp)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
        }
        std::fs::rename(&tmp, path)?;
        Ok(())
    }
}

/// EUI64 havada küçük-uçlu taşınır; insanlar en anlamlı bayttan başlayarak okur
pub fn eui64_to_string(eui64: &Eui64) -> String {
    eui64
        .0
        .iter()
        .rev()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(":")
}

pub fn parse_eui64(text: &str) -> Result<Eui64> {
    let mut bytes: [u8; 8] = decode_hex(&text.replace(':', ""))?;
    bytes.reverse();
    Ok(Eui64(bytes))
}

pub fn key_to_string(key: &Key) -> String {
    key.0.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn parse_key(text: &str) -> Result<Key> {
    Ok(Key(decode_hex(text)?))
}

fn decode_hex<const N: usize>(text: &str) -> Result<[u8; N]> {
    if text.len() != N * 2 {
        bail!("{N} baytlık onaltılık değer bekleniyordu: {text}");
    }
    let mut out = [0u8; N];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&text[i * 2..i * 2 + 2], 16)
            .with_context(|| format!("geçersiz onaltılık değer: {text}"))?;
    }
    Ok(out)
}

pub fn random_bytes<const N: usize>() -> [u8; N] {
    let mut out = [0u8; N];
    getrandom::fill(&mut out).expect("işletim sistemi rastgele sayı üretemedi");
    out
}

pub fn new_network(ieee: &Eui64, channel: u8, tx_power: i8) -> NetworkData {
    // 0x0000 ve 0xFFFF dışında, 0xFFF0 üstü de ayrılmış
    let pan_id = loop {
        let candidate = u16::from_le_bytes(random_bytes());
        if (0x0001..0xFFF0).contains(&candidate) {
            break candidate;
        }
    };
    NetworkData {
        channel,
        pan_id,
        extended_pan_id: eui64_to_string(&Eui64(random_bytes())),
        ieee: eui64_to_string(ieee),
        network_key: key_to_string(&Key(random_bytes())),
        network_key_seq: 0,
        nwk_frame_counter: 0,
        aps_frame_counter: 0,
        update_id: 0,
        tx_power,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eui64_round_trips_in_display_order() {
        // Dongle'ın havadan okunan adresi: 1c 48 99 fe ff b1 5c 6c
        let eui64 = Eui64([0x1c, 0x48, 0x99, 0xfe, 0xff, 0xb1, 0x5c, 0x6c]);
        let text = eui64_to_string(&eui64);
        assert_eq!(text, "6c:5c:b1:ff:fe:99:48:1c");
        assert_eq!(parse_eui64(&text).unwrap(), eui64);
    }

    #[test]
    fn key_round_trips() {
        let key = Key(*b"ZigBeeAlliance09");
        assert_eq!(parse_key(&key_to_string(&key)).unwrap(), key);
    }

    #[test]
    fn rejects_short_hex() {
        assert!(parse_eui64("00:11").is_err());
    }
}
