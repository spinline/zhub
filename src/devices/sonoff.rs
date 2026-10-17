//! SONOFF devices (zigbee-herdsman-converters `src/devices/sonoff.ts`).

use super::common::{self, numeric, thermostat};
use super::{Card, Conv, Definition, Feature, Fingerprint, Kind, Place, Source, TimeBase, Write};
use crate::zcl::{cluster, types};

/// SONOFF's manufacturer-specific cluster
const SONOFF: u16 = 0xFC11;

const fn fc11(attr: u16, ty: u8, conv: Conv, write: Write) -> Source {
    Source::Attr {
        cluster: SONOFF,
        attr,
        ty,
        conv,
        write,
        report: None,
        manufacturer: None,
    }
}

const fn hundredths() -> Conv {
    Conv::Number {
        divisor: 100.0,
        invalid: None,
    }
}

const fn raw_number() -> Conv {
    Conv::Number {
        divisor: 1.0,
        invalid: None,
    }
}

const ON_OFF_BOOL: Conv = Conv::Bool { on: 1, off: 0 };

// ---- SNZB-02P: temperature and humidity sensor ----

pub static SNZB_02P: Definition = Definition {
    fingerprint: &[Fingerprint::Model("SNZB-02P")],
    vendor: "SONOFF",
    description: "Temperature and humidity sensor",
    name: None,
    card: Card::Sensor,
    features: &[
        common::TEMPERATURE,
        common::HUMIDITY,
        common::BATTERY,
        Feature::new(
            "temperature_calibration",
            "Temperature calibration",
            numeric("°C", Some(-50.0), Some(50.0), Some(0.1)),
            Place::Config,
            fc11(0x2003, types::INT16, hundredths(), Write::Attribute),
        )
        .describe("Offset to add/subtract to the reported temperature"),
        Feature::new(
            "humidity_calibration",
            "Humidity calibration",
            numeric("%", Some(-50.0), Some(50.0), Some(0.1)),
            Place::Config,
            fc11(0x2004, types::INT16, hundredths(), Write::Attribute),
        )
        .describe("Offset to add/subtract to the reported relative humidity"),
    ],
    binds: &[cluster::POLL_CONTROL],
    time_base: TimeBase::Zigbee2000,
    tuya_time_since_2000: false,
};

// ---- SNZB-02D: temperature and humidity sensor with screen ----

pub static SNZB_02D: Definition = Definition {
    fingerprint: &[Fingerprint::Model("SNZB-02D")],
    vendor: "SONOFF",
    description: "Temperature and humidity sensor with screen",
    name: None,
    card: Card::Sensor,
    features: &[
        common::TEMPERATURE,
        common::HUMIDITY,
        common::BATTERY,
        Feature::new(
            "comfort_temperature_min",
            "Comfort temperature min",
            numeric("°C", Some(-10.0), Some(60.0), Some(0.1)),
            Place::Config,
            fc11(0x0004, types::INT16, hundredths(), Write::Attribute),
        )
        .describe(concat!(
            "Minimum temperature that is considered comfortable. The device will display ❄️ when the temperature \
             is lower than this value. ",
            "Note: wake up the device by pressing the button on the back before changing this value."
        )),
        Feature::new(
            "comfort_temperature_max",
            "Comfort temperature max",
            numeric("°C", Some(-10.0), Some(60.0), Some(0.1)),
            Place::Config,
            fc11(0x0003, types::INT16, hundredths(), Write::Attribute),
        )
        .describe(concat!(
            "Maximum temperature that is considered comfortable. The device will display 🔥 when the temperature \
             is higher than this value. ",
            "Note: wake up the device by pressing the button on the back before changing this value."
        )),
        Feature::new(
            "comfort_humidity_min",
            "Comfort humidity min",
            numeric("%", Some(5.0), Some(95.0), Some(0.1)),
            Place::Config,
            fc11(0x0005, types::UINT16, hundredths(), Write::Attribute),
        )
        .describe(concat!(
            "Minimum relative humidity that is considered comfortable. The device will display ☀️ when the \
             humidity is lower than this value. ",
            "Note: wake up the device by pressing the button on the back before changing this value."
        )),
        Feature::new(
            "comfort_humidity_max",
            "Comfort humidity max",
            numeric("%", Some(5.0), Some(95.0), Some(0.1)),
            Place::Config,
            fc11(0x0006, types::UINT16, hundredths(), Write::Attribute),
        )
        .describe(concat!(
            "Maximum relative humidity that is considered comfortable. The device will display 💧 when the \
             humidity is higher than this value. ",
            "Note: wake up the device by pressing the button on the back before changing this value."
        )),
        Feature::new(
            "temperature_units",
            "Temperature units",
            Kind::Enum {
                values: &["celsius", "fahrenheit"],
            },
            Place::Config,
            fc11(0x0007, types::UINT16, Conv::Enum(&[(0, "celsius"), (1, "fahrenheit")]), Write::Attribute),
        )
        .describe(concat!(
            "The unit of the temperature displayed on the device screen. ",
            "Note: wake up the device by pressing the button on the back before changing this value."
        )),
        Feature::new(
            "temperature_calibration",
            "Temperature calibration",
            numeric("°C", Some(-50.0), Some(50.0), Some(0.1)),
            Place::Config,
            fc11(0x2003, types::INT16, hundredths(), Write::Attribute),
        )
        .describe("Offset to add/subtract to the reported temperature"),
        Feature::new(
            "humidity_calibration",
            "Humidity calibration",
            numeric("%", Some(-50.0), Some(50.0), Some(0.1)),
            Place::Config,
            fc11(0x2004, types::INT16, hundredths(), Write::Attribute),
        )
        .describe("Offset to add/subtract to the reported relative humidity"),
    ],
    binds: &[cluster::POLL_CONTROL],
    time_base: TimeBase::Zigbee2000,
    tuya_time_since_2000: false,
};

// ---- TRVZB: thermostatic radiator valve ----

const SYSTEM_MODES: &[(i64, &str)] = &[(0, "off"), (1, "auto"), (4, "heat")];
const SYSTEM_MODE_NAMES: &[&str] = &["off", "auto", "heat"];

/// 3: the external sensor reconnected, still counts as external
const SENSOR_SELECT: &[(i64, &str)] = &[
    (0, "local_temperature"),
    (1, "remote_temperature"),
    (2, "remote_source_offline"),
    (3, "remote_temperature"),
];
const SENSOR_SELECT_NAMES: &[&str] = &["local_temperature", "remote_temperature", "remote_source_offline"];

const TEMPORARY_MODES: &[(i64, &str)] = &[(0, "boost"), (1, "timer")];
const TEMPORARY_MODE_NAMES: &[&str] = &["boost", "timer"];

pub static TRVZB: Definition = Definition {
    fingerprint: &[Fingerprint::Model("TRVZB")],
    vendor: "SONOFF",
    description: "Zigbee thermostatic radiator valve",
    name: Some("Radiator valve"),
    card: Card::Thermostat,
    features: &[
        thermostat::LOCAL_TEMPERATURE,
        thermostat::setpoint(4.0, 35.0, 0.5),
        thermostat::system_mode(SYSTEM_MODES, SYSTEM_MODE_NAMES),
        thermostat::HEATING,
        common::BATTERY,
        thermostat::calibration(-12.7, 12.7, 0.2),
        Feature::new(
            "child_lock",
            "Child lock",
            Kind::Binary,
            Place::Config,
            fc11(0x0000, types::BOOL, ON_OFF_BOOL, Write::Attribute),
        )
        .describe("Enables/disables physical input on the device"),
        Feature::new(
            "open_window",
            "Open window",
            Kind::Binary,
            Place::Config,
            fc11(0x6000, types::BOOL, ON_OFF_BOOL, Write::Attribute),
        )
        .describe("Automatically turns off the radiator when local temperature drops by more than 1.5°C in 4.5 minutes."),
        Feature::new(
            "frost_protection_temperature",
            "Frost protection temperature",
            numeric("°C", Some(4.0), Some(35.0), Some(0.5)),
            Place::Config,
            fc11(0x6002, types::INT16, hundredths(), Write::Attribute),
        )
        .describe(
            "Minimum temperature at which to automatically turn on the radiator, if system mode is off, to prevent pipes freezing.",
        ),
        Feature::new(
            "valve_opening_degree",
            "Valve opening degree",
            numeric("%", Some(0.0), Some(100.0), Some(1.0)),
            Place::Config,
            fc11(0x600B, types::UINT8, raw_number(), Write::Attribute),
        )
        .describe(
            "Valve open position (percentage) control. If the opening degree is set to 100%, the valve is fully open \
             when it is opened. If the opening degree is set to 0%, the valve is fully closed when it is opened, and the \
             default value is 100%. Note: only version v1.1.4 or higher is supported.",
        ),
        Feature::new(
            "valve_closing_degree",
            "Valve closing degree",
            numeric("%", Some(0.0), Some(100.0), Some(1.0)),
            Place::Config,
            fc11(0x600C, types::UINT8, raw_number(), Write::Attribute),
        )
        .describe(
            "Valve closed position (percentage) control. If the closing degree is set to 100%, the valve is fully \
             closed when it is closed. If the closing degree is set to 0%, the valve is fully opened when it is closed, \
             and the default value is 100%. Note: Only version v1.1.4 or higher is supported.",
        ),
        Feature::new(
            "temperature_accuracy",
            "Temperature accuracy",
            numeric("°C", Some(-1.0), Some(-0.2), Some(0.2)),
            Place::Config,
            fc11(0x6011, types::INT16, hundredths(), Write::Attribute),
        )
        .describe(
            "Temperature control accuracy. The range is -0.2 ~ -1°C, with an interval of 0.2, and the default is -1. \
             If the temperature control accuracy is selected as -1°C (default value) and the target temperature is 26 \
             degrees, then TRVZB will close the valve when the room temperature reaches 26 degrees and open the valve \
             at 25 degrees. If -0.4°C is chosen as the temperature control accuracy, then the valve will close when the \
             room temperature reaches 26 degrees and open at 25.6 degrees. Note: Only version v1.3.0 or higher is \
             supported.",
        ),
        Feature::new(
            "smart_temperature_control",
            "Smart temperature control",
            Kind::Binary,
            Place::Config,
            fc11(0x6017, types::BITMAP8, Conv::Bool { on: 0x02, off: 0x00 }, Write::Attribute),
        )
        .describe(
            "Enable adaptive valve control using a PID algorithm. When enabled, \"Valve Opening Percentage\" and \
             \"Temperature Accuracy\" are unavailable.",
        ),
        Feature::new(
            "temperature_sensor_select",
            "Temperature sensor",
            Kind::Enum {
                values: SENSOR_SELECT_NAMES,
            },
            Place::Config,
            fc11(0x600E, types::UINT8, Conv::Enum(SENSOR_SELECT), Write::Attribute),
        )
        .describe(
            "Select the temperature source used for control. local_temperature: Uses the built-in sensor. \
             remote_temperature: Uses the external temperature sensor. remote_source_offline: The external sensor is \
             offline, so the device automatically uses the built-in sensor. When the external sensor reconnects, the \
             device reports 0x03 and switches back to the external temperature sensor.",
        ),
        Feature::new(
            "external_temperature_input",
            "External temperature",
            numeric("°C", Some(0.0), Some(99.9), Some(0.1)),
            Place::Config,
            fc11(0x600D, types::INT16, hundredths(), Write::Attribute),
        )
        .describe(
            "The value of an external temperature sensor. Note: synchronisation of this value with the external \
             temperature sensor needs to happen outside of the hub.",
        ),
        Feature::new(
            "temporary_mode_select",
            "Temporary mode",
            Kind::Enum {
                values: TEMPORARY_MODE_NAMES,
            },
            Place::Config,
            fc11(0x6014, types::UINT8, Conv::Enum(TEMPORARY_MODES), Write::Attribute),
        )
        .describe(
            "Boost mode: Activates maximum TRV temperature for a user-defined duration, enabling rapid heating. Timer \
             Mode: Allows customization of temperature and duration for precise heating control. After the set \
             duration, the system will return to its previous normal mode and temperature.",
        ),
        Feature::new(
            "temporary_mode_duration",
            "Temporary mode duration",
            numeric("min", Some(0.0), Some(1440.0), Some(1.0)),
            Place::Config,
            fc11(
                0x6015,
                types::UINT32,
                Conv::Number {
                    divisor: 60.0,
                    invalid: None,
                },
                Write::Attribute,
            ),
        )
        .describe(
            "Boost Mode: Sets maximum TRV temperature for up to 180 minutes. Timer Mode: Customizes temperature and \
             duration, up to 24 hours.",
        ),
        Feature::new(
            "timer_mode_target_temp",
            "Timer mode target temperature",
            numeric("°C", Some(4.0), Some(35.0), Some(0.5)),
            Place::Config,
            // 0 means "not set"
            fc11(
                0x6016,
                types::INT16,
                Conv::Number {
                    divisor: 100.0,
                    invalid: Some(0),
                },
                Write::Attribute,
            ),
        )
        .describe("In timer mode, the temperature can be set to 4-35 ℃."),
        Feature::new(
            "weekly_schedule",
            "Weekly schedule",
            Kind::WeeklySchedule {
                min: 4.0,
                max: 35.0,
                step: 0.5,
                max_transitions: 6,
            },
            Place::Config,
            Source::ThermostatSchedule,
        )
        .describe(
            "The preset heating schedule to use when the system mode is set to \"auto\" (indicated with ⏲ on the TRV). \
             Up to 6 transitions can be defined per day, where a transition is expressed in the format \
             'HH:mm/temperature', each separated by a space. The first transition for each day must start at 00:00 and \
             the valid temperature range is 4-35°C (in 0.5°C steps). The temperature will be set at the time of the \
             first transition until the time of the next transition, e.g. '00:00/20 10:00/25' will result in the \
             temperature being set to 20°C at 00:00 until 10:00, when it will change to 25°C. The valve does not \
             report its schedule, so the days show what was saved here.",
        ),
        Feature::new(
            "idle_steps",
            "Idle steps",
            numeric("", None, None, None),
            Place::Diagnostic,
            fc11(0x6003, types::UINT16, raw_number(), Write::No),
        )
        .describe("Number of steps used for calibration (no-load steps)"),
        Feature::new(
            "closing_steps",
            "Closing steps",
            numeric("", None, None, None),
            Place::Diagnostic,
            fc11(0x6004, types::UINT16, raw_number(), Write::No),
        )
        .describe("Number of steps it takes to close the valve"),
        Feature::new(
            "valve_opening_limit_voltage",
            "Valve opening limit voltage",
            numeric("mV", None, None, None),
            Place::Diagnostic,
            fc11(0x6005, types::UINT16, raw_number(), Write::No),
        )
        .describe("Valve opening limit voltage"),
        Feature::new(
            "valve_closing_limit_voltage",
            "Valve closing limit voltage",
            numeric("mV", None, None, None),
            Place::Diagnostic,
            fc11(0x6006, types::UINT16, raw_number(), Write::No),
        )
        .describe("Valve closing limit voltage"),
        Feature::new(
            "valve_motor_running_voltage",
            "Valve motor running voltage",
            numeric("mV", None, None, None),
            Place::Diagnostic,
            fc11(0x6007, types::UINT16, raw_number(), Write::No),
        )
        .describe("Valve motor running voltage"),
    ],
    binds: &[cluster::POLL_CONTROL],
    time_base: TimeBase::Unix1970,
    tuya_time_since_2000: false,
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devices::{Access, definition, features};
    use crate::store::Device;
    use serde_json::json;

    fn trvzb() -> Device {
        let mut device = Device::new("0c:ae:5f:ff:fe:b4:5b:eb".into(), 0xff71);
        device.model = Some("TRVZB".into());
        device
    }

    #[test]
    fn trvzb_is_found_by_model_and_hides_on_off() {
        let device = trvzb();
        assert!(std::ptr::eq(definition(&device).unwrap(), &TRVZB));
        let keys: Vec<_> = features(&device).iter().map(|f| f.key).collect();
        assert!(!keys.contains(&"on"));
        assert!(keys.contains(&"setpoint"));
    }

    #[test]
    fn trvzb_system_mode_maps_heat_to_four() {
        let mode = TRVZB.features.iter().find(|f| f.key == "system_mode").unwrap();
        let Source::Attr { conv, .. } = mode.source else { panic!() };
        assert_eq!(conv.decode_int(4), Some(json!("heat")));
        assert_eq!(conv.encode(&json!("off")).unwrap(), 0);
    }

    #[test]
    fn trvzb_diagnostics_are_read_only() {
        let steps = TRVZB.features.iter().find(|f| f.key == "closing_steps").unwrap();
        assert_eq!(steps.access, Access::Read);
        let lock = TRVZB.features.iter().find(|f| f.key == "child_lock").unwrap();
        assert_eq!(lock.access, Access::ReadWrite);
    }

    #[test]
    fn duration_is_stored_in_seconds() {
        let duration = TRVZB.features.iter().find(|f| f.key == "temporary_mode_duration").unwrap();
        let Source::Attr { conv, .. } = duration.source else { panic!() };
        assert_eq!(conv.decode_int(1800), Some(json!(30.0)));
        assert_eq!(conv.encode(&json!(90)).unwrap(), 5400);
    }
}
