//! State kept on disk: network parameters, security counters, stack tables and the
//! device list. A single JSON file, written atomically via a temporary file + rename.

use std::collections::BTreeMap;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value as Json;

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

/// Hub settings edited in the UI
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Minutes without a frame before a mains-powered device counts as offline
    pub availability_mains_minutes: u32,
    /// The same for battery devices, which report rarely
    pub availability_battery_minutes: u32,
    /// How long "Permit join" keeps joining open
    pub permit_join_seconds: u8,
    /// Log filter; when absent the command line's applies
    #[serde(skip_serializing_if = "Option::is_none")]
    pub log_level: Option<String>,
}

/// Zigbee2MQTT's availability defaults: 10 minutes and 25 hours
impl Default for Settings {
    fn default() -> Self {
        Self {
            availability_mains_minutes: 10,
            availability_battery_minutes: 1500,
            permit_join_seconds: 180,
            log_level: None,
        }
    }
}

impl Settings {
    pub fn validate(&self) -> anyhow::Result<()> {
        if !(1..=1440).contains(&self.availability_mains_minutes) {
            anyhow::bail!("mains devices: 1-1440 minutes");
        }
        if !(1..=10080).contains(&self.availability_battery_minutes) {
            anyhow::bail!("battery devices: 1-10080 minutes");
        }
        if self.permit_join_seconds == 0 || self.permit_join_seconds == 255 {
            anyhow::bail!("permit join: 1-254 seconds");
        }
        Ok(())
    }
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
    /// Feature key → value (see `devices`)
    #[serde(default)]
    pub state: BTreeMap<String, Json>,
    #[serde(default)]
    pub firmware: Option<String>,
    /// Fields older versions kept separately; moved into `state` on load
    #[serde(default, skip_serializing)]
    settings: BTreeMap<String, f64>,
    #[serde(default, skip_serializing)]
    zone_type: Option<u16>,
    #[serde(default, skip_serializing)]
    color_temp_min: Option<u16>,
    #[serde(default, skip_serializing)]
    color_temp_max: Option<u16>,
    #[serde(default)]
    pub last_seen: Option<u64>,
    #[serde(default)]
    pub lqi: Option<u8>,
    /// Received signal strength of the last packet (dBm)
    #[serde(default)]
    pub rssi: Option<i8>,
}

fn default_true() -> bool {
    true
}

impl Device {
    pub fn new(ieee: String, nwk: u16) -> Self {
        let suffix = ieee.replace(':', "");
        let name = format!("Device {}", &suffix[suffix.len().saturating_sub(4)..]);
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
            state: BTreeMap::new(),
            firmware: None,
            zone_type: None,
            color_temp_min: None,
            color_temp_max: None,
            settings: BTreeMap::new(),
            last_seen: None,
            lqi: None,
            rssi: None,
        }
    }

    /// Moves the older version's separate fields into the feature map; the keys are
    /// defined as features under the same names
    fn migrate_legacy(&mut self) {
        for (key, value) in std::mem::take(&mut self.settings) {
            self.state.entry(key).or_insert_with(|| serde_json::json!(value));
        }
        let legacy = [
            ("zone_type", self.zone_type.take()),
            ("color_temp_min", self.color_temp_min.take()),
            ("color_temp_max", self.color_temp_max.take()),
        ];
        for (key, value) in legacy {
            if let Some(value) = value {
                self.state.entry(key.into()).or_insert_with(|| serde_json::json!(value));
            }
        }
    }

    /// The first endpoint hosting a cluster on the server side
    pub fn endpoint_with(&self, cluster: u16) -> Option<u8> {
        self.endpoints
            .iter()
            .find(|ep| ep.in_clusters.contains(&cluster))
            .map(|ep| ep.id)
    }

    /// The endpoint with the cluster as input (server) or output (client)
    pub fn endpoint_with_any(&self, cluster: u16) -> Option<u8> {
        self.endpoint_with(cluster).or_else(|| {
            self.endpoints
                .iter()
                .find(|ep| ep.out_clusters.contains(&cluster))
                .map(|ep| ep.id)
        })
    }

    pub fn has_cluster(&self, cluster: u16) -> bool {
        self.endpoint_with(cluster).is_some()
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Persisted {
    pub network: Option<NetworkData>,
    #[serde(default)]
    pub settings: Settings,
    #[serde(default)]
    pub link_keys: Vec<LinkKey>,
    #[serde(default)]
    pub children: Vec<Child>,
    #[serde(default)]
    pub devices: BTreeMap<String, Device>,
    /// Values queued for sleepy devices: ieee → (feature → value)
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub pending_writes: BTreeMap<String, BTreeMap<String, Json>>,
}

impl Persisted {
    pub fn load(path: &Path) -> Result<Self> {
        match std::fs::read(path) {
            Ok(bytes) => {
                let mut data: Self = serde_json::from_slice(&bytes)
                    .with_context(|| format!("cannot parse {}", path.display()))?;
                for device in data.devices.values_mut() {
                    device.migrate_legacy();
                }
                Ok(data)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("cannot open {}", path.display())),
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("json.tmp");
        let bytes = serde_json::to_vec_pretty(self)?;
        {
            // Holds the network key: owner-readable only
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

/// EUI64s travel little-endian over the air; people read them most significant byte first
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
        bail!("expected a {N}-byte hex value: {text}");
    }
    let mut out = [0u8; N];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&text[i * 2..i * 2 + 2], 16)
            .with_context(|| format!("invalid hex value: {text}"))?;
    }
    Ok(out)
}

pub fn random_bytes<const N: usize>() -> [u8; N] {
    let mut out = [0u8; N];
    getrandom::fill(&mut out).expect("the operating system failed to produce random bytes");
    out
}

pub fn new_network(ieee: &Eui64, channel: u8, tx_power: i8) -> NetworkData {
    // Besides 0x0000 and 0xFFFF, everything from 0xFFF0 up is reserved
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
        // The dongle's address as read over the air: 1c 48 99 fe ff b1 5c 6c
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

    /// A file written by the previous version must migrate without loss
    #[test]
    fn migrates_previous_state_format() {
        let old = serde_json::json!({
            "network": null,
            "devices": {
                "38:39:8f:ff:fe:de:0d:70": {
                    "ieee": "38:39:8f:ff:fe:de:0d:70", "nwk": 27483, "name": "SNZB-02P",
                    "model": "SNZB-02P", "zone_type": null, "color_temp_min": 153,
                    "state": {"battery": 100, "temperature": 21.3},
                    "settings": {"temperature_calibration": 1.0}
                }
            }
        });
        let dir = std::env::temp_dir().join(format!("zhub-migrate-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state.json");
        std::fs::write(&path, serde_json::to_vec(&old).unwrap()).unwrap();

        let data = Persisted::load(&path).unwrap();
        let device = &data.devices["38:39:8f:ff:fe:de:0d:70"];
        assert_eq!(device.state["temperature"], serde_json::json!(21.3));
        assert_eq!(device.state["temperature_calibration"], serde_json::json!(1.0));
        assert_eq!(device.state["color_temp_min"], serde_json::json!(153));
        let saved = serde_json::to_value(device).unwrap();
        assert!(saved.get("settings").is_none() && saved.get("zone_type").is_none());
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn rejects_short_hex() {
        assert!(parse_eui64("00:11").is_err());
    }
}
