//! Cihaz tanımları, Zigbee2MQTT'deki mantıkla: her cihaz bir parmak izi ve bir özellik
//! listesinden oluşur. Bir özellik hem gelen değerin nasıl çözüleceğini (fromZigbee),
//! hem yazmanın nasıl yapılacağını (toZigbee), hem de arayüz şemasını (exposes) taşır.
//! Ana uygulama hiçbir cihaz türünü bilmez; yalnızca bu tanımları uygular.
//!
//! Tanımlar zigbee-herdsman-converters'tan (ve gerektiğinde tuya-local'den) aktarılır.

pub mod common;
pub mod sonoff;
pub mod tuya;

use anyhow::{Result, bail};
use serde::Serialize;
use serde_json::{Value as Json, json};

use crate::store::Device;
use crate::zcl::cluster;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Access {
    Read,
    ReadWrite,
}

/// Arayüzde nerede durduğu
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Place {
    /// Kartın üzerinde: ana değerler ve sık kullanılan denetimler
    Card,
    /// Ayrıntılar'da, ayar olarak
    Config,
    /// Ayrıntılar'da, salt bilgi
    Diagnostic,
    /// Yalnızca başka özelliklerin hesabında kullanılır (ör. renk sıcaklığı aralığı)
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
        /// Aralığı cihazın bildirdiği başka iki özellik daraltıyorsa (en az, en çok)
        #[serde(skip_serializing_if = "Option::is_none")]
        range_keys: Option<(&'static str, &'static str)>,
    },
    Binary,
    Enum {
        values: &'static [&'static str],
    },
    Text,
    /// Haftalık program: (saat, sıcaklık) dilimleri
    Schedule {
        min: f64,
        max: f64,
        step: f64,
    },
}

/// Ham değer ile özellik değeri arasındaki dönüşüm
#[derive(Debug, Clone, Copy)]
pub enum Conv {
    /// ham / bölen; `invalid` "ölçüm yok" anlamına gelen ham değer
    Number { divisor: f64, invalid: Option<i64> },
    /// ham == on ise açık; yazarken on/off değerleri
    Bool { on: i64, off: i64 },
    /// Tek bir bit (salt okunur)
    Bit(u32),
    /// Ham değer ↔ ad; aynı ada birden çok ham değer düşebilir, yazmada ilki kullanılır
    Enum(&'static [(i64, &'static str)]),
    /// Yarım yüzde birimli pil; 0xFF bilinmiyor
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

    /// Özellik değerini ham tamsayıya çevirir
    pub fn encode(&self, value: &Json) -> Result<i64> {
        match *self {
            Self::Number { divisor, .. } => match value.as_f64() {
                Some(v) if v.is_finite() => Ok((v * divisor).round() as i64),
                _ => bail!("sayı bekleniyordu"),
            },
            Self::Bool { on, off } => match value.as_bool() {
                Some(b) => Ok(if b { on } else { off }),
                None => bail!("açık/kapalı bekleniyordu"),
            },
            Self::Enum(map) => {
                let name = value.as_str().unwrap_or_default();
                match map.iter().find(|(_, n)| *n == name) {
                    Some((raw, _)) => Ok(*raw),
                    None => bail!("geçersiz seçim: {value}"),
                }
            }
            Self::Bit(_) | Self::BatteryHalfPercent | Self::Text => bail!("bu özellik yazılamaz"),
        }
    }
}

fn round2(value: f64) -> Json {
    json!((value * 100.0).round() / 100.0)
}

/// Öznitelik yazmanın yolu: doğrudan öznitelik yazma ya da ışıklarda olduğu gibi komut
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Write {
    No,
    Attribute,
    /// On/Off kümesi: Kapat (0x00) / Aç (0x01)
    OnOff,
    /// Level kümesi: Move to Level (with On/Off)
    Level,
    /// Color kümesi: Move to Color Temperature
    ColorTemp,
}

#[derive(Debug, Clone, Copy)]
pub struct Report {
    pub min: u16,
    pub max: u16,
    pub change: u64,
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
    },
    Tuya {
        dp: u8,
        dp_type: u8,
        conv: Conv,
    },
    /// Tuya'nın ham baytlı haftalık programı (4 baytlık dilimler)
    TuyaSchedule { dp: u8, divisor: f64 },
}

impl Source {
    const fn writable(&self) -> bool {
        match self {
            Self::Attr { write, .. } => !matches!(write, Write::No),
            Self::Tuya { conv, .. } => matches!(conv, Conv::Number { .. } | Conv::Bool { .. } | Conv::Enum(_)),
            Self::TuyaSchedule { .. } => true,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct Feature {
    pub key: &'static str,
    pub label: &'static str,
    pub kind: Kind,
    pub access: Access,
    pub place: Place,
    #[serde(skip)]
    pub source: Source,
}

impl Feature {
    pub const fn new(key: &'static str, label: &'static str, kind: Kind, place: Place, source: Source) -> Self {
        let access = if source.writable() {
            Access::ReadWrite
        } else {
            Access::Read
        };
        Self {
            key,
            label,
            kind,
            access,
            place,
            source,
        }
    }

    /// Yalnızca okunur olsun (ör. yazılabilir öznitelik ama arayüzde değiştirilmesin)
    pub const fn read_only(mut self) -> Self {
        self.access = Access::Read;
        self
    }

    pub fn matches_dp(&self, id: u8) -> bool {
        matches!(self.source, Source::Tuya { dp, .. } | Source::TuyaSchedule { dp, .. } if dp == id)
    }

    /// Sayısal değerin geçerli aralığı: tanımdaki sınırlar, cihazın bildirdiği
    /// sınırlarla (range_keys) daraltılmış
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

    /// Yazılacak değeri doğrular ve sayıları adıma yuvarlar
    pub fn validate(&self, device: &Device, value: Json) -> Result<Json> {
        if self.access != Access::ReadWrite {
            bail!("{} değiştirilemez", self.label);
        }
        match self.kind {
            Kind::Numeric { step, unit, .. } => {
                let Some(mut v) = value.as_f64().filter(|v| v.is_finite()) else {
                    bail!("{}: sayı bekleniyordu", self.label);
                };
                if let Some(step) = step {
                    v = (v / step).round() * step;
                }
                let (min, max) = self.range(device);
                if min.is_some_and(|m| v < m - 1e-9) || max.is_some_and(|m| v > m + 1e-9) {
                    bail!(
                        "{} {}…{} {unit} arasında olmalı",
                        self.label,
                        min.map_or("-∞".into(), |m| m.to_string()),
                        max.map_or("∞".into(), |m| m.to_string())
                    );
                }
                Ok(round2(v))
            }
            Kind::Binary if value.is_boolean() => Ok(value),
            Kind::Binary => bail!("{}: açık/kapalı bekleniyordu", self.label),
            Kind::Enum { values } => match value.as_str() {
                Some(v) if values.contains(&v) => Ok(value),
                _ => bail!("{}: geçersiz seçim", self.label),
            },
            Kind::Schedule { .. } | Kind::Text => Ok(value),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub enum Fingerprint {
    Model(&'static str),
    Manufacturer(&'static str),
}

/// Arayüzün kart düzeni
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Card {
    Light,
    Thermostat,
    Sensor,
    Generic,
}

/// Merkezin Time kümesi yanıtında zamanın başlangıcı
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeBase {
    /// ZCL standardı: 2000-01-01 UTC'den beri saniye
    Zigbee2000,
    /// Bazı cihazlar (Sonoff TRVZB) Unix zamanı bekliyor
    Unix1970,
}

#[derive(Debug)]
pub struct Definition {
    pub fingerprint: &'static [Fingerprint],
    /// Varsayılan ad; yoksa model kodu
    pub name: Option<&'static str>,
    pub card: Card,
    pub features: &'static [Feature],
    /// Raporlama olmadan merkeze bağlanacak ek kümeler (ör. Poll Control)
    pub binds: &'static [u16],
    pub time_base: TimeBase,
    /// Tuya mcuSyncTime yanıtında saat 2000'den mi sayılsın
    pub tuya_time_since_2000: bool,
}

const DEFINITIONS: &[&Definition] = &[&sonoff::SNZB_02P, &sonoff::TRVZB, &tuya::ZWT07];

pub fn definition(device: &Device) -> Option<&'static Definition> {
    DEFINITIONS.iter().copied().find(|def| {
        def.fingerprint.iter().any(|fp| match fp {
            Fingerprint::Model(model) => device.model.as_deref() == Some(model),
            Fingerprint::Manufacturer(name) => device.manufacturer.as_deref() == Some(name),
        })
    })
}

/// Cihazın özellikleri: varsa tanımından, yoksa sunduğu standart kümelerden
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

/// Bağlanacak ek kümeler (raporlamalı kümeler özelliklerden çıkarılır)
pub fn extra_binds(device: &Device) -> &'static [u16] {
    definition(device).map_or(&[], |def| def.binds)
}

/// Kullanıcı ad vermediyse kullanılacak ad
pub fn default_name(device: &Device) -> Option<String> {
    definition(device)
        .and_then(|def| def.name.map(str::to_owned))
        .or_else(|| device.model.clone())
}

/// IAS bölgesi olan cihazlar merkeze kaydedilir
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
}
