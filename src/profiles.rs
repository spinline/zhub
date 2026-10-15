//! Modele özel ayarlar: üreticinin kendi kümelerindeki yazılabilir öznitelikler.
//! Tanımlar zigbee-herdsman-converters'tan alınmıştır.

use serde::Serialize;

use crate::zcl::{Value, cluster, types};

#[derive(Debug, Clone, Copy, Serialize)]
pub struct Setting {
    pub key: &'static str,
    pub label: &'static str,
    #[serde(skip)]
    pub cluster: u16,
    #[serde(skip)]
    pub attribute: u16,
    #[serde(skip)]
    pub ty: u8,
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
        let raw = value.as_i64()?;
        Some((raw as f64 / self.scale * 100.0).round() / 100.0)
    }

    /// Değeri adıma yuvarlar ve öznitelik türünde kodlar; aralık dışıysa `None`
    pub fn encode(&self, value: f64) -> Option<(f64, Vec<u8>)> {
        if !value.is_finite() || !(self.min..=self.max).contains(&value) {
            return None;
        }
        let snapped = (value / self.step).round() * self.step;
        let raw = (snapped * self.scale).round() as i64;
        let bytes = match self.ty {
            types::INT16 => i16::try_from(raw).ok()?.to_le_bytes().to_vec(),
            types::UINT16 => u16::try_from(raw).ok()?.to_le_bytes().to_vec(),
            types::UINT8 => vec![u8::try_from(raw).ok()?],
            _ => return None,
        };
        Some(((snapped * 100.0).round() / 100.0, bytes))
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
            cluster: SONOFF_CLUSTER,
            attribute: 0x2003,
            ty: types::INT16,
            scale: 100.0,
            min: -50.0,
            max: 50.0,
            step: 0.1,
            unit: "°C",
        },
        Setting {
            key: "humidity_calibration",
            label: "Nem düzeltmesi",
            cluster: SONOFF_CLUSTER,
            attribute: 0x2004,
            ty: types::INT16,
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
        let (snapped, bytes) = setting.encode(-1.26).unwrap();
        assert_eq!(snapped, -1.3);
        assert_eq!(bytes, (-130i16).to_le_bytes());
        assert_eq!(setting.decode(&Value::Int(-130)), Some(-1.3));
    }

    #[test]
    fn rejects_out_of_range_values() {
        let setting = SNZB_02P.setting("humidity_calibration").unwrap();
        assert!(setting.encode(50.5).is_none());
        assert!(setting.encode(f64::NAN).is_none());
    }
}
