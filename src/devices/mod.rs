//! Device definitions, following the Zigbee2MQTT model: every device is a fingerprint
//! plus a list of features. A feature carries how an incoming value is decoded
//! (fromZigbee), how a write is performed (toZigbee) and its UI schema (exposes).
//! The hub itself knows no device types; it only applies these definitions.
//!
//! Definitions are ported from zigbee-herdsman-converters (and from tuya-local where it
//! has more detail).

pub mod common;
pub mod philips;
pub mod sonoff;
pub mod tuya;

use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde_json::{Value as Json, json};

use crate::store::Device;
use crate::zcl::{self, cluster};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Access {
    Read,
    ReadWrite,
    /// Commands such as identify or effects: sent, never read
    Write,
}

/// Where the feature appears in the UI
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Place {
    /// On the card: primary values and frequently used controls
    Card,
    /// A device setting
    Config,
    /// Read-only diagnostic information
    Diagnostic,
    /// Only used to compute other features (e.g. the color temperature range)
    Hidden,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Kind {
    Numeric {
        unit: &'static str,
        min: Option<f64>,
        max: Option<f64>,
        step: Option<f64>,
        /// Two other features the device reports that narrow the range (min, max)
        #[serde(skip_serializing_if = "Option::is_none")]
        range_keys: Option<(&'static str, &'static str)>,
    },
    Binary,
    Enum {
        values: &'static [&'static str],
    },
    Text,
    /// Weekly schedule: (time, temperature) slots
    Schedule {
        min: f64,
        max: f64,
        step: f64,
    },
    /// ZCL thermostat schedule: per day 1..max_transitions (time, temperature), the
    /// first at 00:00
    WeeklySchedule {
        min: f64,
        max: f64,
        step: f64,
        max_transitions: u8,
    },
}

/// Conversion between a raw value and the feature value
#[derive(Debug, Clone, Copy)]
pub enum Conv {
    /// raw / divisor; `invalid` is the raw value meaning "no reading"
    Number { divisor: f64, invalid: Option<i64> },
    /// On when raw == on; writes use the on/off values
    Bool { on: i64, off: i64 },
    /// A single bit (read-only)
    Bit(u32),
    /// Raw value ↔ name; several raw values may map to one name, writes use the first
    Enum(&'static [(i64, &'static str)]),
    /// Battery in half-percent units; 0xFF means unknown
    BatteryHalfPercent,
    Text,
}

impl Conv {
    pub fn decode_int(&self, raw: i64) -> Option<Json> {
        match *self {
            Self::Number { divisor, invalid } => {
                if invalid == Some(raw) {
                    return None;
                }
                Some(round2(raw as f64 / divisor))
            }
            Self::Bool { on, .. } => Some(Json::Bool(raw == on)),
            Self::Bit(bit) => Some(Json::Bool(raw >> bit & 1 == 1)),
            Self::Enum(map) => map
                .iter()
                .find(|(value, _)| *value == raw)
                .map(|(_, name)| json!(name)),
            Self::BatteryHalfPercent => (raw != 0xFF).then(|| json!((raw / 2).clamp(0, 100))),
            Self::Text => None,
        }
    }

    pub fn decode_text(&self, text: &str) -> Option<Json> {
        matches!(self, Self::Text)
            .then(|| text.trim_end_matches('\0').trim())
            .filter(|t| !t.is_empty())
            .map(|t| json!(t))
    }

    /// Converts a feature value to the raw integer
    pub fn encode(&self, value: &Json) -> Result<i64> {
        match *self {
            Self::Number { divisor, .. } => match value.as_f64() {
                Some(v) if v.is_finite() => Ok((v * divisor).round() as i64),
                _ => bail!("expected a number"),
            },
            Self::Bool { on, off } => match value.as_bool() {
                Some(b) => Ok(if b { on } else { off }),
                None => bail!("expected true or false"),
            },
            Self::Enum(map) => {
                let name = value.as_str().unwrap_or_default();
                match map.iter().find(|(_, n)| *n == name) {
                    Some((raw, _)) => Ok(*raw),
                    None => bail!("invalid option: {value}"),
                }
            }
            Self::Bit(_) | Self::BatteryHalfPercent | Self::Text => bail!("this feature is read-only"),
        }
    }
}

fn round2(value: f64) -> Json {
    json!((value * 100.0).round() / 100.0)
}

/// How an attribute is written: a plain attribute write, or a command as with lights
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Write {
    No,
    Attribute,
    /// On/Off cluster: Off (0x00) / On (0x01)
    OnOff,
    /// Level cluster: Move to Level (with On/Off)
    Level,
    /// Color cluster: Move to Color Temperature
    ColorTemp,
}

#[derive(Debug, Clone, Copy)]
pub struct Report {
    pub min: u16,
    pub max: u16,
    pub change: u64,
}

/// A named value of a numeric feature. Inside the value range it is a shortcut (e.g.
/// "coolest"); outside it is a special value the device reports as such (e.g. 0xFFFF
/// "previous"), which then shows by its name.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct Preset {
    pub name: &'static str,
    #[serde(skip)]
    pub raw: i64,
    pub description: &'static str,
}

/// Cluster commands that carry no state of their own
#[derive(Debug, Clone, Copy)]
pub enum Command {
    /// Identify cluster: makes the device identify itself for a few seconds
    Identify,
    /// The Hue effect list: ZCL Trigger Effect or Signify's effect command
    HueEffect,
}

/// One ZCL cluster command to send
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandFrame {
    pub cluster: u16,
    pub manufacturer: Option<u16>,
    pub command: u8,
    pub payload: Vec<u8>,
}

impl Command {
    pub fn frames(&self, value: &Json) -> Result<Vec<CommandFrame>> {
        match self {
            Self::Identify => Ok(vec![common::identify_frame()]),
            Self::HueEffect => philips::effect_frames(value.as_str().unwrap_or_default()),
        }
    }

    /// The state the command leaves behind, if any
    pub fn state_after(&self, value: &Json) -> Option<Json> {
        match self {
            Self::Identify => None,
            Self::HueEffect => philips::effect_state(value.as_str().unwrap_or_default()).map(|s| json!(s)),
        }
    }
}

/// Commands a device sends, such as button presses, decoded to an action name
/// (Zigbee2MQTT's `action`)
#[derive(Debug, Clone, Copy)]
pub enum Action {
    /// Signify's Hue notification (0xFC00 command 0x00): button id → name, then
    /// press / hold / press_release / hold_release
    HueButtons(&'static [(u8, &'static str)]),
    /// On/Off Toggle → "toggle"
    Toggle,
    /// Level Move (with On/Off) → brightness_move_up/down; Stop → brightness_stop
    BrightnessMove,
}

const HUE_NOTIFICATION_CLUSTER: u16 = 0xFC00;

impl Action {
    pub fn decode(&self, cluster_id: u16, header: &zcl::Header, payload: &[u8]) -> Option<String> {
        match *self {
            Self::HueButtons(buttons) => {
                if cluster_id != HUE_NOTIFICATION_CLUSTER || header.command != 0x00 {
                    return None;
                }
                // button (u8), unknown (u24), type (u8), …
                let button = buttons.iter().find(|(id, _)| Some(id) == payload.first())?.1;
                let kind = match payload.get(4)? {
                    0 => "press",
                    1 => "hold",
                    2 => "press_release",
                    3 => "hold_release",
                    _ => return None,
                };
                Some(format!("{button}_{kind}"))
            }
            Self::Toggle => (cluster_id == cluster::ON_OFF && header.command == 0x02).then(|| "toggle".to_owned()),
            Self::BrightnessMove => {
                if cluster_id != cluster::LEVEL {
                    return None;
                }
                match header.command {
                    0x01 | 0x05 => {
                        let direction = if *payload.first()? == 1 { "down" } else { "up" };
                        Some(format!("brightness_move_{direction}"))
                    }
                    0x03 | 0x07 => Some("brightness_stop".to_owned()),
                    _ => None,
                }
            }
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub enum Source {
    Attr {
        cluster: u16,
        attr: u16,
        ty: u8,
        conv: Conv,
        write: Write,
        report: Option<Report>,
        /// Manufacturer-specific attributes are read and written with this code
        manufacturer: Option<u16>,
    },
    Tuya {
        dp: u8,
        dp_type: u8,
        conv: Conv,
    },
    /// Tuya's raw weekly schedule (4-byte slots)
    TuyaSchedule { dp: u8, divisor: f64 },
    Command(Command),
    /// Commands the device sends; the last one is kept as the feature's value
    Actions(&'static [Action]),
    /// ZCL Thermostat Set/Get Weekly Schedule
    ThermostatSchedule,
}

impl Source {
    const fn access(&self) -> Access {
        let writable = match self {
            Self::Attr { write, .. } => !matches!(write, Write::No),
            Self::Tuya { conv, .. } => matches!(conv, Conv::Number { .. } | Conv::Bool { .. } | Conv::Enum(_)),
            Self::TuyaSchedule { .. } | Self::ThermostatSchedule => true,
            Self::Command(_) => return Access::Write,
            Self::Actions(_) => false,
        };
        if writable { Access::ReadWrite } else { Access::Read }
    }

    const fn conv(&self) -> Option<Conv> {
        match *self {
            Self::Attr { conv, .. } | Self::Tuya { conv, .. } => Some(conv),
            Self::TuyaSchedule { .. } | Self::Command(_) | Self::Actions(_) | Self::ThermostatSchedule => None,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct Feature {
    pub key: &'static str,
    pub label: &'static str,
    #[serde(skip_serializing_if = "str::is_empty")]
    pub description: &'static str,
    pub kind: Kind,
    pub access: Access,
    pub place: Place,
    #[serde(skip_serializing_if = "<[_]>::is_empty")]
    pub presets: &'static [Preset],
    #[serde(skip)]
    pub source: Source,
}

impl Feature {
    pub const fn new(key: &'static str, label: &'static str, kind: Kind, place: Place, source: Source) -> Self {
        Self {
            key,
            label,
            description: "",
            kind,
            access: source.access(),
            place,
            presets: &[],
            source,
        }
    }

    pub const fn presets(mut self, presets: &'static [Preset]) -> Self {
        self.presets = presets;
        self
    }

    /// The help text shown under the feature (taken from Zigbee2MQTT where it has one)
    pub const fn describe(mut self, description: &'static str) -> Self {
        self.description = description;
        self
    }

    /// Show as read-only even though the source could be written
    pub const fn read_only(mut self) -> Self {
        self.access = Access::Read;
        self
    }

    /// A preset outside the definition's value range: a special value, shown by name
    fn special_preset(&self, raw: i64) -> Option<&'static Preset> {
        let Kind::Numeric { min, max, .. } = self.kind else {
            return None;
        };
        let preset = self.presets.iter().find(|p| p.raw == raw)?;
        let value = self.source.conv()?.decode_int(raw)?.as_f64()?;
        let outside = min.is_some_and(|m| value < m - 1e-9) || max.is_some_and(|m| value > m + 1e-9);
        outside.then_some(preset)
    }

    /// Decodes a raw value the device reported
    pub fn decode_int(&self, raw: i64) -> Option<Json> {
        if let Some(preset) = self.special_preset(raw) {
            return Some(json!(preset.name));
        }
        self.source.conv()?.decode_int(raw)
    }

    /// Converts a validated value to the raw integer to write
    pub fn encode(&self, value: &Json) -> Result<i64> {
        if let Some(preset) = value.as_str().and_then(|name| self.presets.iter().find(|p| p.name == name)) {
            return Ok(preset.raw);
        }
        match self.source.conv() {
            Some(conv) => conv.encode(value),
            None => bail!("{} has no raw value", self.label),
        }
    }

    pub fn matches_dp(&self, id: u8) -> bool {
        matches!(self.source, Source::Tuya { dp, .. } | Source::TuyaSchedule { dp, .. } if dp == id)
    }

    /// The valid range of a numeric value: the definition's limits, narrowed by the
    /// limits the device reports (range_keys)
    pub fn range(&self, device: &Device) -> (Option<f64>, Option<f64>) {
        let Kind::Numeric { min, max, range_keys, .. } = self.kind else {
            return (None, None);
        };
        let Some((min_key, max_key)) = range_keys else {
            return (min, max);
        };
        let reported = |key| device.state.get(key).and_then(Json::as_f64);
        let narrowed_min = match (min, reported(min_key)) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        };
        let narrowed_max = match (max, reported(max_key)) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        (narrowed_min, narrowed_max)
    }

    /// Validates a value to be written and snaps numbers to the step
    pub fn validate(&self, device: &Device, value: Json) -> Result<Json> {
        if self.access == Access::Read {
            bail!("{} is read-only", self.label);
        }
        match self.kind {
            Kind::Numeric { .. } if value.is_string() => {
                let name = value.as_str().unwrap_or_default();
                let Some(preset) = self.presets.iter().find(|p| p.name == name) else {
                    bail!("{}: invalid option {name}", self.label);
                };
                if self.special_preset(preset.raw).is_some() {
                    return Ok(value);
                }
                // A shortcut stands for its value
                self.source
                    .conv()
                    .and_then(|conv| conv.decode_int(preset.raw))
                    .context("the preset has no value")
            }
            Kind::Numeric { step, unit, .. } => {
                let Some(mut v) = value.as_f64().filter(|v| v.is_finite()) else {
                    bail!("{}: expected a number", self.label);
                };
                if let Some(step) = step {
                    v = (v / step).round() * step;
                }
                let (min, max) = self.range(device);
                if min.is_some_and(|m| v < m - 1e-9) || max.is_some_and(|m| v > m + 1e-9) {
                    bail!(
                        "{} must be between {} and {} {unit}",
                        self.label,
                        min.map_or("-∞".into(), |m| m.to_string()),
                        max.map_or("∞".into(), |m| m.to_string())
                    );
                }
                Ok(round2(v))
            }
            Kind::Binary if value.is_boolean() => Ok(value),
            Kind::Binary => bail!("{}: expected true or false", self.label),
            Kind::Enum { values } => match value.as_str() {
                Some(v) if values.contains(&v) => Ok(value),
                _ => bail!("{}: invalid option", self.label),
            },
            Kind::WeeklySchedule {
                min,
                max,
                step,
                max_transitions,
            } => {
                let limits = crate::thermostat::Limits {
                    min,
                    max,
                    step,
                    max_transitions: usize::from(max_transitions),
                };
                let week = crate::thermostat::parse_week(&value, limits).with_context(|| self.label.to_owned())?;
                Ok(crate::thermostat::week_to_json(&week))
            }
            Kind::Schedule { .. } | Kind::Text => Ok(value),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub enum Fingerprint {
    Model(&'static str),
    Manufacturer(&'static str),
}

/// The card layout in the UI
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Card {
    Light,
    Thermostat,
    Sensor,
    Generic,
}

/// The epoch of the hub's Time cluster responses
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeBase {
    /// ZCL standard: seconds since 2000-01-01 UTC
    Zigbee2000,
    /// Some devices (SONOFF TRVZB) expect Unix time
    Unix1970,
}

#[derive(Debug)]
pub struct Definition {
    pub fingerprint: &'static [Fingerprint],
    /// As in Zigbee2MQTT
    pub vendor: &'static str,
    pub description: &'static str,
    /// Default device name; the model code when absent
    pub name: Option<&'static str>,
    pub card: Card,
    pub features: &'static [Feature],
    /// Extra clusters bound to the hub without reporting (e.g. Poll Control)
    pub binds: &'static [u16],
    pub time_base: TimeBase,
    /// Whether the Tuya mcuSyncTime response counts from 2000
    pub tuya_time_since_2000: bool,
}

/// What the UI shows about a matched definition
#[derive(Debug, Clone, Copy, Serialize)]
pub struct DefinitionInfo {
    pub vendor: &'static str,
    pub description: &'static str,
}

const DEFINITIONS: &[&Definition] = &[
    &philips::LTA005,
    &philips::LTA009,
    &philips::RDM001,
    &sonoff::SNZB_02D,
    &sonoff::SNZB_02P,
    &sonoff::TRVZB,
    &tuya::ZWT07,
];

pub fn definition(device: &Device) -> Option<&'static Definition> {
    DEFINITIONS.iter().copied().find(|def| {
        def.fingerprint.iter().any(|fp| match fp {
            Fingerprint::Model(model) => device.model.as_deref() == Some(model),
            Fingerprint::Manufacturer(name) => device.manufacturer.as_deref() == Some(name),
        })
    })
}

pub fn definition_info(device: &Device) -> Option<DefinitionInfo> {
    definition(device).map(|def| DefinitionInfo {
        vendor: def.vendor,
        description: def.description,
    })
}

/// The device's features: from its definition, or from the standard clusters it offers
pub fn features(device: &Device) -> Vec<&'static Feature> {
    if let Some(def) = definition(device) {
        return def.features.iter().collect();
    }
    common::from_clusters(device)
}

pub fn card(device: &Device) -> Card {
    if let Some(def) = definition(device) {
        return def.card;
    }
    let features = common::from_clusters(device);
    let has = |key: &str| features.iter().any(|f| f.key == key);
    if has("on") && device.router {
        Card::Light
    } else if ["temperature", "humidity", "occupancy", "alarm"].iter().any(|k| has(k)) {
        Card::Sensor
    } else {
        Card::Generic
    }
}

pub fn time_base(device: &Device) -> TimeBase {
    definition(device).map_or(TimeBase::Zigbee2000, |def| def.time_base)
}

/// Extra clusters to bind, input or output (reported clusters are derived from the
/// features)
pub fn extra_binds(device: &Device) -> &'static [u16] {
    definition(device).map_or(&[], |def| def.binds)
}

/// The name to use when the user has not named the device
pub fn default_name(device: &Device) -> Option<String> {
    definition(device)
        .and_then(|def| def.name.map(str::to_owned))
        .or_else(|| device.model.clone())
}

/// Devices with an IAS zone are enrolled with the hub
pub fn has_ias_zone(device: &Device) -> bool {
    device.has_cluster(cluster::IAS_ZONE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn number_conversion_round_trips_and_rejects_invalid() {
        let conv = Conv::Number {
            divisor: 100.0,
            invalid: Some(-0x8000),
        };
        assert_eq!(conv.decode_int(2153), Some(json!(21.53)));
        assert_eq!(conv.decode_int(-0x8000), None);
        assert_eq!(conv.encode(&json!(21.5)).unwrap(), 2150);
    }

    #[test]
    fn enum_conversion_accepts_aliases_but_writes_first() {
        let conv = Conv::Enum(&[(0, "local"), (1, "remote"), (3, "remote")]);
        assert_eq!(conv.decode_int(3), Some(json!("remote")));
        assert_eq!(conv.encode(&json!("remote")).unwrap(), 1);
        assert!(conv.encode(&json!("other")).is_err());
    }

    #[test]
    fn battery_and_bits() {
        assert_eq!(Conv::BatteryHalfPercent.decode_int(200), Some(json!(100)));
        assert_eq!(Conv::BatteryHalfPercent.decode_int(0xFF), None);
        assert_eq!(Conv::Bit(3).decode_int(0x0009), Some(json!(true)));
        assert_eq!(Conv::Bit(1).decode_int(0x0009), Some(json!(false)));
    }

    #[test]
    fn validation_snaps_and_uses_reported_range() {
        let mut device = Device::new("a4:c1:38:dd:3d:d3:35:0b".into(), 1);
        device.manufacturer = Some("_TZE200_g9a3awaj".into());
        let setpoint = features(&device).into_iter().find(|f| f.key == "setpoint").unwrap();
        assert_eq!(setpoint.validate(&device, json!(21.3)).unwrap(), json!(21.5));
        assert!(setpoint.validate(&device, json!(90.0)).is_ok());
        device.state.insert("max_temperature".into(), json!(50.0));
        assert!(setpoint.validate(&device, json!(60.0)).is_err());
        assert!(setpoint.validate(&device, json!("x")).is_err());
    }

    #[test]
    fn read_only_features_reject_writes() {
        let device = Device::new("00:00:00:00:00:00:00:01".into(), 1);
        let temperature = &common::TEMPERATURE;
        assert!(temperature.validate(&device, json!(20)).is_err());
    }

    fn header(command: u8) -> zcl::Header {
        zcl::Header::parse(&[0x01, 0x10, command]).unwrap().0
    }

    #[test]
    fn actions_decode_like_zigbee2mqtt() {
        let hue = Action::HueButtons(&[(1, "left"), (2, "right")]);
        // Observed Hue notification layout: button, 3 unknown bytes, type, …
        let payload = [0x02, 0x00, 0x00, 0x30, 0x02, 0x21, 0x01, 0x00];
        assert_eq!(hue.decode(0xFC00, &header(0x00), &payload).as_deref(), Some("right_press_release"));
        assert_eq!(hue.decode(0xFC00, &header(0x00), &[0x03, 0, 0, 0, 0]), None);
        assert_eq!(hue.decode(cluster::ON_OFF, &header(0x00), &payload), None);
        assert_eq!(Action::Toggle.decode(cluster::ON_OFF, &header(0x02), &[]).as_deref(), Some("toggle"));
        assert_eq!(Action::Toggle.decode(cluster::ON_OFF, &header(0x01), &[]), None);
        let mv = Action::BrightnessMove;
        assert_eq!(mv.decode(cluster::LEVEL, &header(0x05), &[0x01, 0x40]).as_deref(), Some("brightness_move_down"));
        assert_eq!(mv.decode(cluster::LEVEL, &header(0x01), &[0x00, 0x40]).as_deref(), Some("brightness_move_up"));
        assert_eq!(mv.decode(cluster::LEVEL, &header(0x07), &[]).as_deref(), Some("brightness_stop"));
    }

    /// Every feature of every definition should explain itself in the UI
    #[test]
    fn every_defined_feature_has_a_description() {
        for def in DEFINITIONS {
            for feature in def.features {
                assert!(!feature.description.is_empty(), "{}: {}", def.description, feature.key);
            }
        }
    }
}
