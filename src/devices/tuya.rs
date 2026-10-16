//! Tuya cihazları. DP eşlemeleri zigbee-herdsman-converters `tuya.ts` ve daha ayrıntılı
//! olduğunda tuya-local tanımlarından.

use super::common::numeric;
use super::{Card, Conv, Definition, Feature, Fingerprint, Kind, Place, Source, TimeBase};
use crate::tuya::dp_type;

const fn dp(dp: u8, dp_type: u8, conv: Conv) -> Source {
    Source::Tuya { dp, dp_type, conv }
}

const fn tenths() -> Conv {
    Conv::Number {
        divisor: 10.0,
        invalid: None,
    }
}

const fn whole() -> Conv {
    Conv::Number {
        divisor: 1.0,
        invalid: None,
    }
}

const BOOL: Conv = Conv::Bool { on: 1, off: 0 };

// ---- RTI-Tek T5Z / Tuya ZWT07 duvar termostatı (`_TZE200_g9a3awaj`) ----
// tuya-local: rtitek_t5z_thermostat.yaml

const MODES: &[(i64, &str)] = &[(0, "off"), (1, "heat")];
const PRESETS: &[(i64, &str)] = &[(1, "manual"), (0, "program")];
const WORKING_WEEK: &[(i64, &str)] = &[(0, "5_2"), (1, "6_1"), (2, "7")];

pub static ZWT07: Definition = Definition {
    fingerprint: &[Fingerprint::Manufacturer("_TZE200_g9a3awaj")],
    name: Some("Termostat"),
    card: Card::Thermostat,
    features: &[
        Feature::new(
            "temperature",
            "Oda sıcaklığı",
            numeric("°C", None, None, None),
            Place::Card,
            dp(24, dp_type::VALUE, tenths()),
        ),
        Feature::new(
            "setpoint",
            "Hedef sıcaklık",
            Kind::Numeric {
                unit: "°C",
                min: Some(5.0),
                max: Some(95.0),
                step: Some(0.5),
                range_keys: Some(("min_temperature", "max_temperature")),
            },
            Place::Card,
            dp(16, dp_type::VALUE, tenths()),
        ),
        Feature::new(
            "system_mode",
            "Mod",
            Kind::Enum { values: &["off", "heat"] },
            Place::Card,
            dp(1, dp_type::BOOL, Conv::Enum(MODES)),
        ),
        Feature::new(
            "preset",
            "Çalışma",
            Kind::Enum {
                values: &["manual", "program"],
            },
            Place::Card,
            dp(2, dp_type::ENUM, Conv::Enum(PRESETS)),
        ),
        Feature::new("heating", "Isıtıyor", Kind::Binary, Place::Card, dp(36, dp_type::ENUM, BOOL)).read_only(),
        Feature::new("battery", "Pil", numeric("%", None, None, None), Place::Card, dp(9, dp_type::VALUE, whole()))
            .read_only(),
        Feature::new("frost", "Donma koruması", Kind::Binary, Place::Config, dp(10, dp_type::BOOL, BOOL)),
        Feature::new("child_lock", "Çocuk kilidi", Kind::Binary, Place::Config, dp(40, dp_type::BOOL, BOOL)),
        Feature::new(
            "working_week",
            "Çalışma haftası",
            Kind::Enum {
                values: &["5_2", "6_1", "7"],
            },
            Place::Config,
            dp(31, dp_type::ENUM, Conv::Enum(WORKING_WEEK)),
        ),
        Feature::new(
            "temperature_calibration",
            "Sıcaklık düzeltmesi",
            numeric("°C", Some(-9.9), Some(9.9), Some(0.1)),
            Place::Config,
            dp(109, dp_type::VALUE, tenths()),
        ),
        Feature::new(
            "min_temperature",
            "En düşük hedef",
            numeric("°C", Some(5.0), Some(20.0), Some(1.0)),
            Place::Config,
            dp(18, dp_type::VALUE, tenths()),
        ),
        Feature::new(
            "max_temperature",
            "En yüksek hedef",
            numeric("°C", Some(21.0), Some(95.0), Some(1.0)),
            Place::Config,
            dp(19, dp_type::VALUE, tenths()),
        ),
        Feature::new(
            "schedule",
            "Haftalık program",
            Kind::Schedule {
                min: 5.0,
                max: 95.0,
                step: 0.5,
            },
            Place::Config,
            Source::TuyaSchedule { dp: 67, divisor: 10.0 },
        ),
        Feature::new("fault", "Arıza kodu", numeric("", None, None, None), Place::Diagnostic, dp(45, dp_type::BITMAP, whole()))
            .read_only(),
        Feature::new("product_info", "Ürün bilgisi", Kind::Text, Place::Diagnostic, dp(105, dp_type::STRING, Conv::Text)),
    ],
    binds: &[],
    time_base: TimeBase::Zigbee2000,
    tuya_time_since_2000: true,
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devices::{Access, features};
    use crate::store::Device;
    use crate::tuya::Datapoint;
    use serde_json::json;

    fn thermostat() -> Device {
        let mut device = Device::new("a4:c1:38:dd:3d:d3:35:0b".into(), 0x254a);
        device.manufacturer = Some("_TZE200_g9a3awaj".into());
        device
    }

    fn decode(dp: &Datapoint) -> Option<(&'static str, serde_json::Value)> {
        let feature = ZWT07.features.iter().find(|f| f.matches_dp(dp.id))?;
        let Source::Tuya { conv, .. } = feature.source else {
            return None;
        };
        let value = match dp.as_int() {
            Some(raw) => conv.decode_int(raw),
            None => conv.decode_text(&String::from_utf8_lossy(&dp.data)),
        }?;
        Some((feature.key, value))
    }

    /// Termostatın DATA_QUERY'ye verdiği gerçek yanıtlar (zhub günlüğünden)
    #[test]
    fn decodes_observed_datapoints() {
        let observed = [
            (Datapoint { id: 24, ty: 2, data: vec![0, 0, 0, 213] }, "temperature", json!(21.3)),
            (Datapoint { id: 1, ty: 1, data: vec![1] }, "system_mode", json!("heat")),
            (Datapoint { id: 2, ty: 4, data: vec![0] }, "preset", json!("program")),
            (Datapoint { id: 36, ty: 4, data: vec![1] }, "heating", json!(true)),
            (Datapoint { id: 109, ty: 2, data: vec![0, 0, 0, 0] }, "temperature_calibration", json!(0.0)),
            (Datapoint { id: 31, ty: 4, data: vec![0] }, "working_week", json!("5_2")),
            (Datapoint { id: 40, ty: 1, data: vec![1] }, "child_lock", json!(true)),
            (Datapoint { id: 45, ty: 5, data: vec![0] }, "fault", json!(0.0)),
            (Datapoint { id: 19, ty: 2, data: vec![0, 0, 0x01, 0xf4] }, "max_temperature", json!(50.0)),
            (Datapoint { id: 18, ty: 2, data: vec![0, 0, 0, 0x32] }, "min_temperature", json!(5.0)),
            (Datapoint { id: 9, ty: 2, data: vec![0, 0, 0, 0x63] }, "battery", json!(99.0)),
            (Datapoint { id: 105, ty: 3, data: b"zt3l-t5\0".to_vec() }, "product_info", json!("zt3l-t5")),
        ];
        for (dp, key, value) in observed {
            assert_eq!(decode(&dp), Some((key, value)), "DP {}", dp.id);
        }
    }

    #[test]
    fn setpoint_respects_reported_limits() {
        let mut device = thermostat();
        let setpoint = features(&device).into_iter().find(|f| f.key == "setpoint").unwrap();
        assert!(setpoint.validate(&device, json!(60.0)).is_ok());
        device.state.insert("max_temperature".into(), json!(50.0));
        assert!(setpoint.validate(&device, json!(60.0)).is_err());
    }

    #[test]
    fn heating_and_battery_are_read_only() {
        for key in ["heating", "battery", "fault", "product_info"] {
            let feature = ZWT07.features.iter().find(|f| f.key == key).unwrap();
            assert_eq!(feature.access, Access::Read, "{key}");
        }
    }
}
