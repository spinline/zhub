//! Features of the standard ZCL clusters (the counterpart of Zigbee2MQTT's modernExtend).
//! Devices without a definition get one built from these, based on their clusters.
//! Descriptions are taken from Zigbee2MQTT's exposes.

use super::{Action, Command, CommandFrame, Conv, Feature, Kind, Place, Preset, Report, Source, Write};
use crate::store::Device;
use crate::zcl::{cluster, types};

const fn attr(cluster: u16, attr: u16, ty: u8, conv: Conv, write: Write, report: Option<Report>) -> Source {
    Source::Attr {
        cluster,
        attr,
        ty,
        conv,
        write,
        report,
        manufacturer: None,
    }
}

const fn report(min: u16, max: u16, change: u64) -> Option<Report> {
    Some(Report { min, max, change })
}

const fn number(divisor: f64, invalid: Option<i64>) -> Conv {
    Conv::Number { divisor, invalid }
}

pub const fn numeric(unit: &'static str, min: Option<f64>, max: Option<f64>, step: Option<f64>) -> Kind {
    Kind::Numeric {
        unit,
        min,
        max,
        step,
        range_keys: None,
    }
}

const NO_READING_INT16: Option<i64> = Some(-0x8000);

// ---- Light ----

pub const ON_OFF: Feature = Feature::new(
    "on",
    "State",
    Kind::Binary,
    Place::Card,
    attr(cluster::ON_OFF, 0x0000, types::BOOL, Conv::Bool { on: 1, off: 0 }, Write::OnOff, report(0, 300, 0)),
)
.describe("On/off state of this light");

pub const BRIGHTNESS: Feature = brightness(1.0);

/// Lights that turn off at level 1 (Hue) start at 2, as Zigbee2MQTT does
pub const fn brightness(min: f64) -> Feature {
    Feature::new(
        "level",
        "Brightness",
        numeric("", Some(min), Some(254.0), Some(1.0)),
        Place::Card,
        attr(cluster::LEVEL, 0x0000, types::UINT8, number(1.0, None), Write::Level, report(1, 300, 1)),
    )
    .describe("Brightness of this light")
}

pub const COLOR_TEMP: Feature = color_temp(153.0, 500.0, &[]);

/// The device may narrow the range further by reporting its limits
pub const fn color_temp(min: f64, max: f64, presets: &'static [Preset]) -> Feature {
    Feature::new(
        "color_temp",
        "Color temperature",
        Kind::Numeric {
            unit: "mired",
            min: Some(min),
            max: Some(max),
            step: Some(1.0),
            range_keys: Some(("color_temp_min", "color_temp_max")),
        },
        Place::Card,
        attr(cluster::COLOR, 0x0007, types::UINT16, number(1.0, None), Write::ColorTemp, report(1, 300, 1)),
    )
    .describe("Color temperature of this light")
    .presets(presets)
}

/// Presets should include "previous" (0xFFFF)
pub const fn color_temp_startup(min: f64, max: f64, presets: &'static [Preset]) -> Feature {
    Feature::new(
        "color_temp_startup",
        "Color temp startup",
        numeric("mired", Some(min), Some(max), Some(1.0)),
        Place::Config,
        attr(cluster::COLOR, 0x4010, types::UINT16, number(1.0, None), Write::Attribute, None),
    )
    .describe("Color temperature after cold power on of this light")
    .presets(presets)
}

pub const COLOR_TEMP_MIN: Feature = Feature::new(
    "color_temp_min",
    "Coldest color temperature",
    numeric("mired", None, None, None),
    Place::Hidden,
    attr(cluster::COLOR, 0x400B, types::UINT16, number(1.0, None), Write::No, None),
)
.describe("Coldest color temperature the light supports");

pub const COLOR_TEMP_MAX: Feature = Feature::new(
    "color_temp_max",
    "Warmest color temperature",
    numeric("mired", None, None, None),
    Place::Hidden,
    attr(cluster::COLOR, 0x400C, types::UINT16, number(1.0, None), Write::No, None),
)
.describe("Warmest color temperature the light supports");

pub const POWER_ON_BEHAVIOR: Feature = Feature::new(
    "power_on_behavior",
    "Power-on behavior",
    Kind::Enum {
        values: &["off", "on", "toggle", "previous"],
    },
    Place::Config,
    attr(
        cluster::ON_OFF,
        0x4003,
        types::ENUM8,
        Conv::Enum(&[(0, "off"), (1, "on"), (2, "toggle"), (255, "previous")]),
        Write::Attribute,
        None,
    ),
)
.describe("Controls the behavior when the device is powered on after power loss");

/// Zigbee2MQTT's level_config: the Level Control cluster's settings. Definitions pick
/// the ones the device supports (Hue lights lack the transition times and on_level).
#[allow(dead_code)]
pub mod level_config {
    use super::*;

    /// Transition times are in tenths of a second; 0xFFFF means "use on_off_transition_time"
    const TENTHS: Conv = Conv::Number {
        divisor: 10.0,
        invalid: None,
    };
    const DISABLED: &[Preset] = &[Preset {
        name: "disabled",
        raw: 0xFFFF,
        description: "Use on_off_transition_time value",
    }];

    pub const ON_OFF_TRANSITION_TIME: Feature = Feature::new(
        "on_off_transition_time",
        "ON/OFF transition time",
        numeric("s", Some(0.0), Some(6553.5), Some(0.1)),
        Place::Config,
        attr(cluster::LEVEL, 0x0010, types::UINT16, TENTHS, Write::Attribute, None),
    )
    .describe("Seconds taken to move to or from the target level when On or Off commands are received by an On/Off cluster");

    pub const ON_TRANSITION_TIME: Feature = Feature::new(
        "on_transition_time",
        "ON transition time",
        numeric("s", Some(0.0), Some(6553.4), Some(0.1)),
        Place::Config,
        attr(cluster::LEVEL, 0x0012, types::UINT16, TENTHS, Write::Attribute, None),
    )
    .describe("Seconds taken to move the current level from the minimum level to the maximum level when an On command is received")
    .presets(DISABLED);

    pub const OFF_TRANSITION_TIME: Feature = Feature::new(
        "off_transition_time",
        "OFF transition time",
        numeric("s", Some(0.0), Some(6553.4), Some(0.1)),
        Place::Config,
        attr(cluster::LEVEL, 0x0013, types::UINT16, TENTHS, Write::Attribute, None),
    )
    .describe("Seconds taken to move the current level from the maximum level to the minimum level when an Off command is received")
    .presets(DISABLED);

    /// Options bit 0
    pub const EXECUTE_IF_OFF: Feature = Feature::new(
        "execute_if_off",
        "Execute if off",
        Kind::Binary,
        Place::Config,
        attr(cluster::LEVEL, 0x000F, types::BITMAP8, Conv::Bool { on: 1, off: 0 }, Write::Attribute, None),
    )
    .describe("this setting can affect the \"on_level\", \"current_level_startup\" or \"brightness\" setting");

    pub const ON_LEVEL: Feature = Feature::new(
        "on_level",
        "On level",
        numeric("", Some(1.0), Some(254.0), Some(1.0)),
        Place::Config,
        attr(cluster::LEVEL, 0x0011, types::UINT8, number(1.0, None), Write::Attribute, None),
    )
    .describe("Specifies the level that shall be applied, when an on/toggle command causes the light to turn on.")
    .presets(&[Preset {
        name: "previous",
        raw: 0xFF,
        description: "Use previous value",
    }]);

    pub const CURRENT_LEVEL_STARTUP: Feature = Feature::new(
        "current_level_startup",
        "Current level startup",
        numeric("", Some(1.0), Some(254.0), Some(1.0)),
        Place::Config,
        attr(cluster::LEVEL, 0x4000, types::UINT8, number(1.0, None), Write::Attribute, None),
    )
    .describe("Defines the desired startup level for a device when it is supplied with power")
    .presets(&[
        Preset {
            name: "minimum",
            raw: 0x00,
            description: "Use minimum permitted value",
        },
        Preset {
            name: "previous",
            raw: 0xFF,
            description: "Use previous value",
        },
    ]);
}

// ---- Identify cluster ----

pub const IDENTIFY: Feature = Feature::new(
    "identify",
    "Identify",
    Kind::Enum { values: &["identify"] },
    Place::Config,
    Source::Command(Command::Identify),
)
.describe("Initiate device identification");

/// Identify for 3 seconds (Zigbee2MQTT's default identify_timeout)
pub fn identify_frame() -> CommandFrame {
    CommandFrame {
        cluster: cluster::IDENTIFY,
        manufacturer: None,
        command: 0x00,
        payload: 3u16.to_le_bytes().to_vec(),
    }
}

/// ZCL Trigger Effect: blink 0x00, breathe 0x01, okay 0x02, channel change 0x0B,
/// finish 0xFE, stop 0xFF
pub fn trigger_effect_frame(effect: u8) -> CommandFrame {
    CommandFrame {
        cluster: cluster::IDENTIFY,
        manufacturer: None,
        command: 0x40,
        payload: vec![effect, 0x00],
    }
}

// ---- Remotes ----

pub const fn action(values: &'static [&'static str], actions: &'static [Action]) -> Feature {
    Feature::new(
        "action",
        "Action",
        Kind::Enum { values },
        Place::Card,
        Source::Actions(actions),
    )
    .describe("Triggered action (e.g. a button click)")
}

// ---- Measurements ----

pub const TEMPERATURE: Feature = Feature::new(
    "temperature",
    "Temperature",
    numeric("°C", None, None, None),
    Place::Card,
    attr(cluster::TEMPERATURE, 0x0000, types::INT16, number(100.0, NO_READING_INT16), Write::No, report(30, 1800, 20)),
)
.describe("Measured temperature value");

pub const HUMIDITY: Feature = Feature::new(
    "humidity",
    "Humidity",
    numeric("%", None, None, None),
    Place::Card,
    attr(cluster::HUMIDITY, 0x0000, types::UINT16, number(100.0, Some(0xFFFF)), Write::No, report(30, 1800, 100)),
)
.describe("Measured relative humidity");

pub const OCCUPANCY: Feature = Feature::new(
    "occupancy",
    "Occupancy",
    Kind::Binary,
    Place::Card,
    attr(cluster::OCCUPANCY, 0x0000, types::BITMAP8, Conv::Bit(0), Write::No, report(0, 1800, 0)),
)
.describe("Indicates whether the device detected occupancy");

pub const BATTERY: Feature = Feature::new(
    "battery",
    "Battery",
    numeric("%", None, None, None),
    Place::Card,
    attr(cluster::POWER_CONFIG, 0x0021, types::UINT8, Conv::BatteryHalfPercent, Write::No, report(3600, 21600, 2)),
)
.describe("Remaining battery in %, can take up to 24 hours before reported");

pub const VOLTAGE: Feature = Feature::new(
    "voltage",
    "Voltage",
    numeric("V", None, None, None),
    Place::Diagnostic,
    attr(cluster::POWER_CONFIG, 0x0020, types::UINT8, number(10.0, Some(0xFF)), Write::No, None),
)
.describe("Voltage of the battery");

// ---- IAS zone (contact, motion, water leak…) ----

pub const ALARM: Feature = Feature::new(
    "alarm",
    "Alarm",
    Kind::Binary,
    Place::Card,
    attr(cluster::IAS_ZONE, 0x0002, types::BITMAP16, Conv::Bit(0), Write::No, None),
)
.describe("Zone alarm 1: open for a contact sensor, motion for a motion sensor");

pub const BATTERY_LOW: Feature = Feature::new(
    "battery_low",
    "Battery low",
    Kind::Binary,
    Place::Diagnostic,
    attr(cluster::IAS_ZONE, 0x0002, types::BITMAP16, Conv::Bit(3), Write::No, None),
)
.describe("Indicates if the battery of this device is almost empty");

/// The UI names the alarm after this: 0x0015 contact, 0x000D motion…
pub const ZONE_TYPE: Feature = Feature::new(
    "zone_type",
    "Zone type",
    numeric("", None, None, None),
    Place::Hidden,
    attr(cluster::IAS_ZONE, 0x0001, types::ENUM16, number(1.0, None), Write::No, None),
)
.describe("IAS zone type reported by the device");

// ---- Thermostat cluster (0x0201) ----

pub mod thermostat {
    use super::*;

    pub const LOCAL_TEMPERATURE: Feature = Feature::new(
        "temperature",
        "Local temperature",
        numeric("°C", None, None, None),
        Place::Card,
        attr(cluster::THERMOSTAT, 0x0000, types::INT16, number(100.0, NO_READING_INT16), Write::No, report(10, 3600, 10)),
    )
    .describe("Current temperature measured on the device");

    pub const fn setpoint(min: f64, max: f64, step: f64) -> Feature {
        Feature::new(
            "setpoint",
            "Heating setpoint",
            numeric("°C", Some(min), Some(max), Some(step)),
            Place::Card,
            attr(cluster::THERMOSTAT, 0x0012, types::INT16, number(100.0, None), Write::Attribute, report(0, 3600, 10)),
        )
        .describe("Temperature setpoint")
    }

    pub const fn system_mode(map: &'static [(i64, &'static str)], names: &'static [&'static str]) -> Feature {
        Feature::new(
            "system_mode",
            "System mode",
            Kind::Enum { values: names },
            Place::Card,
            attr(cluster::THERMOSTAT, 0x001C, types::ENUM8, Conv::Enum(map), Write::Attribute, report(0, 3600, 0)),
        )
        .describe("Mode of this device")
    }

    /// Running State bit 0: heating
    pub const HEATING: Feature = Feature::new(
        "heating",
        "Running state",
        Kind::Binary,
        Place::Card,
        attr(cluster::THERMOSTAT, 0x0029, types::BITMAP16, Conv::Bit(0), Write::No, report(0, 3600, 0)),
    )
    .describe("The current running state: heating or idle");

    pub const fn calibration(min: f64, max: f64, step: f64) -> Feature {
        Feature::new(
            "temperature_calibration",
            "Local temperature calibration",
            numeric("°C", Some(min), Some(max), Some(step)),
            Place::Config,
            attr(cluster::THERMOSTAT, 0x0010, types::INT8, number(10.0, None), Write::Attribute, None),
        )
        .describe("Offset to add/subtract to the local temperature")
    }
}

/// A device without a definition: features based on the standard clusters it offers
pub fn from_clusters(device: &Device) -> Vec<&'static Feature> {
    let mut features: Vec<&'static Feature> = Vec::new();
    let has = |c| device.has_cluster(c);
    if has(cluster::ON_OFF) {
        features.push(&ON_OFF);
    }
    if has(cluster::LEVEL) {
        features.push(&BRIGHTNESS);
    }
    if has(cluster::COLOR) {
        features.extend([&COLOR_TEMP, &COLOR_TEMP_MIN, &COLOR_TEMP_MAX]);
    }
    if has(cluster::TEMPERATURE) {
        features.push(&TEMPERATURE);
    }
    if has(cluster::HUMIDITY) {
        features.push(&HUMIDITY);
    }
    if has(cluster::OCCUPANCY) {
        features.push(&OCCUPANCY);
    }
    if has(cluster::IAS_ZONE) {
        features.extend([&ALARM, &BATTERY_LOW, &ZONE_TYPE]);
    }
    if has(cluster::POWER_CONFIG) && !device.router {
        features.extend([&BATTERY, &VOLTAGE]);
    }
    features
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Endpoint;

    fn device_with(clusters: &[u16], router: bool) -> Device {
        let mut device = Device::new("00:17:88:01:02:03:04:05".into(), 0x1234);
        device.router = router;
        device.endpoints.push(Endpoint {
            id: 11,
            profile: 0x0104,
            device_id: 0x010C,
            in_clusters: clusters.to_vec(),
            out_clusters: vec![],
        });
        device
    }

    fn keys(features: &[&Feature]) -> Vec<&'static str> {
        features.iter().map(|f| f.key).collect()
    }

    #[test]
    fn hue_white_ambiance_is_a_light() {
        let device = device_with(&[0x0000, 0x0006, 0x0008, 0x0300], true);
        assert_eq!(keys(&from_clusters(&device)), ["on", "level", "color_temp", "color_temp_min", "color_temp_max"]);
        assert_eq!(super::super::card(&device), super::super::Card::Light);
    }

    #[test]
    fn contact_sensor_features() {
        let device = device_with(&[0x0000, 0x0001, 0x0500], false);
        assert_eq!(keys(&from_clusters(&device)), ["alarm", "battery_low", "zone_type", "battery", "voltage"]);
        assert_eq!(super::super::card(&device), super::super::Card::Sensor);
    }

    #[test]
    fn mains_powered_devices_have_no_battery() {
        let device = device_with(&[0x0001, 0x0006], true);
        assert_eq!(keys(&from_clusters(&device)), ["on"]);
    }
}
