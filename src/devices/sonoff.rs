//! SONOFF cihazları (zigbee-herdsman-converters `src/devices/sonoff.ts`).

use super::common::{self, numeric, thermostat};
use super::{Card, Conv, Definition, Feature, Fingerprint, Kind, Place, Source, TimeBase, Write};
use crate::zcl::{cluster, types};

/// SONOFF'un kendi kümesi
const SONOFF: u16 = 0xFC11;

const fn fc11(attr: u16, ty: u8, conv: Conv, write: Write) -> Source {
    Source::Attr {
        cluster: SONOFF,
        attr,
        ty,
        conv,
        write,
        report: None,
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

// ---- SNZB-02P: sıcaklık ve nem sensörü ----

pub static SNZB_02P: Definition = Definition {
    fingerprint: &[Fingerprint::Model("SNZB-02P")],
    name: None,
    card: Card::Sensor,
    features: &[
        common::TEMPERATURE,
        common::HUMIDITY,
        common::BATTERY,
        Feature::new(
            "temperature_calibration",
            "Sıcaklık düzeltmesi",
            numeric("°C", Some(-50.0), Some(50.0), Some(0.1)),
            Place::Config,
            fc11(0x2003, types::INT16, hundredths(), Write::Attribute),
        ),
        Feature::new(
            "humidity_calibration",
            "Nem düzeltmesi",
            numeric("%", Some(-50.0), Some(50.0), Some(0.1)),
            Place::Config,
            fc11(0x2004, types::INT16, hundredths(), Write::Attribute),
        ),
    ],
    binds: &[cluster::POLL_CONTROL],
    time_base: TimeBase::Zigbee2000,
    tuya_time_since_2000: false,
};

// ---- TRVZB: termostatik radyatör vanası ----

const SYSTEM_MODES: &[(i64, &str)] = &[(0, "off"), (1, "auto"), (4, "heat")];
const SYSTEM_MODE_NAMES: &[&str] = &["off", "auto", "heat"];

/// 3: harici sensör yeniden bağlandı, yine harici sayılır
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
    name: Some("Radyatör vanası"),
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
            "Çocuk kilidi",
            Kind::Binary,
            Place::Config,
            fc11(0x0000, types::BOOL, ON_OFF_BOOL, Write::Attribute),
        ),
        Feature::new(
            "open_window",
            "Açık pencere algılama",
            Kind::Binary,
            Place::Config,
            fc11(0x6000, types::BOOL, ON_OFF_BOOL, Write::Attribute),
        ),
        Feature::new(
            "frost_protection_temperature",
            "Donma koruması sıcaklığı",
            numeric("°C", Some(4.0), Some(35.0), Some(0.5)),
            Place::Config,
            fc11(0x6002, types::INT16, hundredths(), Write::Attribute),
        ),
        Feature::new(
            "valve_opening_degree",
            "Vana açılma oranı",
            numeric("%", Some(0.0), Some(100.0), Some(1.0)),
            Place::Config,
            fc11(0x600B, types::UINT8, raw_number(), Write::Attribute),
        ),
        Feature::new(
            "valve_closing_degree",
            "Vana kapanma oranı",
            numeric("%", Some(0.0), Some(100.0), Some(1.0)),
            Place::Config,
            fc11(0x600C, types::UINT8, raw_number(), Write::Attribute),
        ),
        Feature::new(
            "temperature_accuracy",
            "Sıcaklık hassasiyeti",
            numeric("°C", Some(-1.0), Some(-0.2), Some(0.2)),
            Place::Config,
            fc11(0x6011, types::INT16, hundredths(), Write::Attribute),
        ),
        Feature::new(
            "smart_temperature_control",
            "Akıllı sıcaklık kontrolü (PID)",
            Kind::Binary,
            Place::Config,
            fc11(0x6017, types::BITMAP8, Conv::Bool { on: 0x02, off: 0x00 }, Write::Attribute),
        ),
        Feature::new(
            "temperature_sensor_select",
            "Sıcaklık kaynağı",
            Kind::Enum {
                values: SENSOR_SELECT_NAMES,
            },
            Place::Config,
            fc11(0x600E, types::UINT8, Conv::Enum(SENSOR_SELECT), Write::Attribute),
        ),
        Feature::new(
            "external_temperature_input",
            "Harici sıcaklık",
            numeric("°C", Some(0.0), Some(99.9), Some(0.1)),
            Place::Config,
            fc11(0x600D, types::INT16, hundredths(), Write::Attribute),
        ),
        Feature::new(
            "temporary_mode_select",
            "Geçici mod",
            Kind::Enum {
                values: TEMPORARY_MODE_NAMES,
            },
            Place::Config,
            fc11(0x6014, types::UINT8, Conv::Enum(TEMPORARY_MODES), Write::Attribute),
        ),
        Feature::new(
            "temporary_mode_duration",
            "Geçici mod süresi",
            numeric("dk", Some(0.0), Some(1440.0), Some(1.0)),
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
        ),
        Feature::new(
            "timer_mode_target_temp",
            "Zamanlayıcı hedef sıcaklığı",
            numeric("°C", Some(4.0), Some(35.0), Some(0.5)),
            Place::Config,
            // 0 "ayarlanmamış" demek
            fc11(
                0x6016,
                types::INT16,
                Conv::Number {
                    divisor: 100.0,
                    invalid: Some(0),
                },
                Write::Attribute,
            ),
        ),
        Feature::new(
            "idle_steps",
            "Boşta adım sayısı",
            numeric("", None, None, None),
            Place::Diagnostic,
            fc11(0x6003, types::UINT16, raw_number(), Write::No),
        ),
        Feature::new(
            "closing_steps",
            "Kapanma adım sayısı",
            numeric("", None, None, None),
            Place::Diagnostic,
            fc11(0x6004, types::UINT16, raw_number(), Write::No),
        ),
        Feature::new(
            "valve_opening_limit_voltage",
            "Açılma sınır voltajı",
            numeric("mV", None, None, None),
            Place::Diagnostic,
            fc11(0x6005, types::UINT16, raw_number(), Write::No),
        ),
        Feature::new(
            "valve_closing_limit_voltage",
            "Kapanma sınır voltajı",
            numeric("mV", None, None, None),
            Place::Diagnostic,
            fc11(0x6006, types::UINT16, raw_number(), Write::No),
        ),
        Feature::new(
            "valve_motor_running_voltage",
            "Motor çalışma voltajı",
            numeric("mV", None, None, None),
            Place::Diagnostic,
            fc11(0x6007, types::UINT16, raw_number(), Write::No),
        ),
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
