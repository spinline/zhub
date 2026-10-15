//! Modele özel ayarlar: üreticinin kendi kümelerindeki yazılabilir öznitelikler.
//! Tanımlar zigbee-herdsman-converters'tan alınmıştır.

use serde::Serialize;

use crate::zcl::{Value, cluster, types};

/// Ayarın cihazdaki yeri
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Attribute { cluster: u16, attribute: u16, ty: u8 },
    /// Tuya'nın `0xEF00` kümesindeki sayısal (VALUE) veri noktası
    TuyaDp(u8),
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct Setting {
    pub key: &'static str,
    pub label: &'static str,
    #[serde(skip)]
    pub source: Source,
    /// Ham değer = gösterilen değer × ölçek
    #[serde(skip)]
    pub scale: f64,
    pub min: f64,
    pub max: f64,
    pub step: f64,
    pub unit: &'static str,
}

impl Setting {
    pub fn decode(&self, value: &Value) -> Option<f64> {
        Some(self.from_raw(value.as_i64()?))
    }

    pub fn from_raw(&self, raw: i64) -> f64 {
        (raw as f64 / self.scale * 100.0).round() / 100.0
    }

    /// Değeri adıma yuvarlar ve ham tamsayıya çevirir; aralık dışıysa `None`
    pub fn to_raw(&self, value: f64) -> Option<(f64, i64)> {
        if !value.is_finite() || !(self.min..=self.max).contains(&value) {
            return None;
        }
        let snapped = (value / self.step).round() * self.step;
        Some(((snapped * 100.0).round() / 100.0, (snapped * self.scale).round() as i64))
    }

    /// Öznitelik kaynaklı ayarın yazılacak baytları
    pub fn attribute_bytes(&self, raw: i64) -> Option<Vec<u8>> {
        let Source::Attribute { ty, .. } = self.source else {
            return None;
        };
        Some(match ty {
            types::INT16 => i16::try_from(raw).ok()?.to_le_bytes().to_vec(),
            types::UINT16 => u16::try_from(raw).ok()?.to_le_bytes().to_vec(),
            types::UINT8 => vec![u8::try_from(raw).ok()?],
            _ => return None,
        })
    }

    pub fn is_attribute(&self, cluster_id: u16, attribute_id: u16) -> bool {
        matches!(self.source, Source::Attribute { cluster, attribute, .. } if cluster == cluster_id && attribute == attribute_id)
    }
}

#[derive(Debug)]
pub struct Profile {
    pub settings: &'static [Setting],
    /// Raporlama ayarı olmadan merkeze bağlanacak ek kümeler
    pub binds: &'static [u16],
}

impl Profile {
    pub fn setting(&self, key: &str) -> Option<&'static Setting> {
        self.settings.iter().find(|s| s.key == key)
    }
}

/// Sonoff'un kendi kümesi
const SONOFF_CLUSTER: u16 = 0xFC11;

const SNZB_02P: Profile = Profile {
    settings: &[
        Setting {
            key: "temperature_calibration",
            label: "Sıcaklık düzeltmesi",
            source: Source::Attribute {
                cluster: SONOFF_CLUSTER,
                attribute: 0x2003,
                ty: types::INT16,
            },
            scale: 100.0,
            min: -50.0,
            max: 50.0,
            step: 0.1,
            unit: "°C",
        },
        Setting {
            key: "humidity_calibration",
            label: "Nem düzeltmesi",
            source: Source::Attribute {
                cluster: SONOFF_CLUSTER,
                attribute: 0x2004,
                ty: types::INT16,
            },
            scale: 100.0,
            min: -50.0,
            max: 50.0,
            step: 0.1,
            unit: "%",
        },
    ],
    binds: &[cluster::POLL_CONTROL],
};

pub fn profile(model: Option<&str>) -> Option<&'static Profile> {
    match model? {
        "SNZB-02P" => Some(&SNZB_02P),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calibration_round_trips_through_int16() {
        let setting = SNZB_02P.setting("temperature_calibration").unwrap();
        let (snapped, raw) = setting.to_raw(-1.26).unwrap();
        assert_eq!(snapped, -1.3);
        assert_eq!(setting.attribute_bytes(raw).unwrap(), (-130i16).to_le_bytes());
        assert_eq!(setting.decode(&Value::Int(-130)), Some(-1.3));
    }

    #[test]
    fn rejects_out_of_range_values() {
        let setting = SNZB_02P.setting("humidity_calibration").unwrap();
        assert!(setting.to_raw(50.5).is_none());
        assert!(setting.to_raw(f64::NAN).is_none());
    }
}
