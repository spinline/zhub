//! Tuya'nın `0xEF00` kümesi: cihaz değerlerini standart ZCL öznitelikleri yerine
//! numaralı "veri noktaları" (DP) olarak taşır. DP'lerin anlamı modele göre değişir;
//! eşlemeler zigbee-herdsman-converters'taki tanımlardan alınmıştır.

use serde_json::{Value as Json, json};

pub const CLUSTER: u16 = 0xEF00;

pub mod cmd {
    pub const DATA_REQUEST: u8 = 0x00;
    pub const DATA_RESPONSE: u8 = 0x01;
    pub const DATA_REPORT: u8 = 0x02;
    pub const DATA_QUERY: u8 = 0x03;
    pub const ACTIVE_STATUS_REPORT_ALT: u8 = 0x05;
    pub const ACTIVE_STATUS_REPORT: u8 = 0x06;
    pub const MCU_SYNC_TIME: u8 = 0x24;
    pub const GATEWAY_CONNECTION_STATUS: u8 = 0x25;
}

pub mod dp_type {
    pub const RAW: u8 = 0;
    pub const STRING: u8 = 3;
    pub const BOOL: u8 = 1;
    pub const VALUE: u8 = 2;
    pub const ENUM: u8 = 4;
    pub const BITMAP: u8 = 5;
}

/// 1970 ile 2000 arasındaki saniye; bazı Tuya cihazları saati 2000'den sayar
const SECONDS_1970_TO_2000: u64 = 946_684_800;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Datapoint {
    pub id: u8,
    pub ty: u8,
    pub data: Vec<u8>,
}

impl Datapoint {
    pub fn bool(id: u8, value: bool) -> Self {
        Self {
            id,
            ty: dp_type::BOOL,
            data: vec![u8::from(value)],
        }
    }

    pub fn value(id: u8, value: i32) -> Self {
        Self {
            id,
            ty: dp_type::VALUE,
            data: value.to_be_bytes().to_vec(),
        }
    }

    pub fn enumeration(id: u8, value: u8) -> Self {
        Self {
            id,
            ty: dp_type::ENUM,
            data: vec![value],
        }
    }

    /// Sayısal türlerin değeri (değerler büyük-uçlu ve işaretli)
    pub fn as_int(&self) -> Option<i64> {
        match self.ty {
            dp_type::BOOL | dp_type::ENUM => self.data.first().map(|&b| i64::from(b)),
            dp_type::VALUE => Some(i64::from(i32::from_be_bytes(self.data.get(..4)?.try_into().ok()?))),
            dp_type::BITMAP if self.data.len() <= 4 => {
                Some(self.data.iter().fold(0i64, |acc, &b| (acc << 8) | i64::from(b)))
            }
            _ => None,
        }
    }
}

/// Yanıt/rapor yükü: sıra (2 bayt) ardından (dp, tür, uzunluk BE, veri)*
pub fn parse_datapoints(payload: &[u8]) -> Vec<Datapoint> {
    let mut rest = payload.get(2..).unwrap_or_default();
    let mut out = Vec::new();
    while rest.len() >= 4 {
        let len = usize::from(u16::from_be_bytes([rest[2], rest[3]]));
        let Some(data) = rest.get(4..4 + len) else {
            break;
        };
        out.push(Datapoint {
            id: rest[0],
            ty: rest[1],
            data: data.to_vec(),
        });
        rest = &rest[4 + len..];
    }
    out
}

pub fn data_request(seq: u16, datapoints: &[Datapoint]) -> Vec<u8> {
    let mut out = seq.to_be_bytes().to_vec();
    for dp in datapoints {
        out.push(dp.id);
        out.push(dp.ty);
        out.extend_from_slice(&(dp.data.len() as u16).to_be_bytes());
        out.extend_from_slice(&dp.data);
    }
    out
}

/// mcuSyncTime yanıtı: yük uzunluğu (ZCL uint16), UTC ve yerel saat (BE saniye)
pub fn time_sync_payload(unix_seconds: u64, utc_offset: i64, since_2000: bool) -> Vec<u8> {
    let base = if since_2000 { SECONDS_1970_TO_2000 } else { 0 };
    let utc = unix_seconds.saturating_sub(base) as i64;
    let local = utc + utc_offset;
    let mut out = 8u16.to_le_bytes().to_vec();
    out.extend_from_slice(&(utc as u32).to_be_bytes());
    out.extend_from_slice(&(local as u32).to_be_bytes());
    out
}

/// Yerel saatin UTC'den farkı (saniye). Keenetic saat dilimini `/etc/TZ` dosyasına
/// POSIX biçiminde yazıyor (ör. `<+03>-3`); yaz saati kuralları yok sayılır.
pub fn utc_offset_seconds() -> i64 {
    std::env::var("TZ")
        .ok()
        .filter(|tz| !tz.is_empty())
        .or_else(|| std::fs::read_to_string("/etc/TZ").ok())
        .and_then(|tz| parse_posix_tz_offset(tz.trim()))
        .unwrap_or(0)
}

fn parse_posix_tz_offset(tz: &str) -> Option<i64> {
    // Saat dilimi adı: <...> ya da harfler
    let rest = if let Some(stripped) = tz.strip_prefix('<') {
        &stripped[stripped.find('>')? + 1..]
    } else {
        tz.trim_start_matches(|c: char| c.is_ascii_alphabetic())
    };
    let (sign, digits) = match rest.as_bytes().first()? {
        b'-' => (-1, &rest[1..]),
        b'+' => (1, &rest[1..]),
        _ => (1, rest),
    };
    let offset_text: String = digits
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == ':')
        .collect();
    let mut parts = offset_text.split(':').map(|p| p.parse::<i64>().ok());
    let hours = parts.next()??;
    let minutes = parts.next().flatten().unwrap_or(0);
    let seconds = parts.next().flatten().unwrap_or(0);
    // POSIX'te işaret ters: "-3" UTC'nin 3 saat ilerisi
    Some(-sign * (hours * 3600 + minutes * 60 + seconds))
}

/// Ham baytlı haftalık program: 4 baytlık [saat, dakika, sıcaklık×bölen (BE u16)]
/// dilimleri. Özellik değeri olarak [{"time": "SS:DD", "temperature": t}, …]
pub fn decode_schedule(data: &[u8], divisor: f64) -> Json {
    Json::Array(
        data.chunks_exact(4)
            .map(|slot| {
                json!({
                    "time": format!("{:02}:{:02}", slot[0], slot[1]),
                    "temperature": f64::from(u16::from_be_bytes([slot[2], slot[3]])) / divisor,
                })
            })
            .collect(),
    )
}

/// Programı geri kodlar. Dilim sayısı cihazın bildirdiğiyle aynı kalmalı: cihazın
/// beklediği düzen (hafta içi/sonu dilimleri) bu sayıya bağlı.
pub fn encode_schedule(
    value: &Json,
    expected_slots: Option<usize>,
    (min, max, step): (f64, f64, f64),
    divisor: f64,
) -> Result<Vec<u8>, String> {
    let slots = value.as_array().ok_or("program bir dilim listesi olmalı")?;
    if expected_slots != Some(slots.len()) {
        return Err(format!(
            "program {} dilim olmalı (cihazın bildirdiği düzen)",
            expected_slots.unwrap_or(0)
        ));
    }
    let mut data = Vec::with_capacity(slots.len() * 4);
    for slot in slots {
        let time = slot.get("time").and_then(Json::as_str).unwrap_or_default();
        let (hour, minute) = time
            .split_once(':')
            .and_then(|(h, m)| Some((h.parse::<u8>().ok()?, m.parse::<u8>().ok()?)))
            .filter(|&(h, m)| h < 24 && m < 60)
            .ok_or_else(|| format!("geçersiz saat: {time}"))?;
        let temperature = slot
            .get("temperature")
            .and_then(Json::as_f64)
            .filter(|t| t.is_finite() && (min..=max).contains(t))
            .ok_or_else(|| format!("sıcaklık {min}-{max} °C arasında olmalı"))?;
        let raw = (temperature / step).round() * step * divisor;
        data.extend_from_slice(&[hour, minute]);
        data.extend_from_slice(&(raw.round() as u16).to_be_bytes());
    }
    Ok(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_multiple_datapoints() {
        // sıra 0x0012; DP16 değer 215 (21.5 °C); DP1 bool true; DP36 enum 1
        let payload = [
            0x00, 0x12, 16, 2, 0, 4, 0, 0, 0, 215, 1, 1, 0, 1, 1, 36, 4, 0, 1, 1,
        ];
        let dps = parse_datapoints(&payload);
        assert_eq!(dps.len(), 3);
        assert_eq!(dps[0], Datapoint::value(16, 215));
        assert_eq!(dps[1].as_int(), Some(1));
        assert_eq!(dps[2], Datapoint::enumeration(36, 1));
    }

    #[test]
    fn stops_at_truncated_datapoint() {
        let dps = parse_datapoints(&[0x00, 0x01, 16, 2, 0, 4, 0, 0]);
        assert!(dps.is_empty());
    }

    #[test]
    fn negative_values_are_signed() {
        assert_eq!(Datapoint::value(24, -15).as_int(), Some(-15));
    }

    #[test]
    fn request_round_trips_through_parser() {
        let dp = Datapoint::value(16, 225);
        let payload = data_request(0x0102, std::slice::from_ref(&dp));
        assert_eq!(payload, [0x01, 0x02, 16, 2, 0, 4, 0, 0, 0, 225]);
        assert_eq!(parse_datapoints(&payload), [dp]);
    }

    /// Termostatın gerçek programı (zhub günlüğünden): değiştirilmeden geri kodlanınca
    /// aynı baytlar çıkmalı
    #[test]
    fn schedule_round_trips_observed_bytes() {
        let raw = [
            0x06, 0x3b, 0x00, 0xbe, 0x08, 0x00, 0x00, 0x96, 0x0b, 0x1e, 0x00, 0x96, 0x0c, 0x1e, 0x00,
            0x96, 0x11, 0x00, 0x00, 0xdc, 0x16, 0x00, 0x00, 0x96, 0x08, 0x00, 0x00, 0x96, 0x16, 0x00,
            0x00, 0xa0,
        ];
        let schedule = decode_schedule(&raw, 10.0);
        assert_eq!(schedule[0], json!({"time": "06:59", "temperature": 19.0}));
        assert_eq!(schedule[7], json!({"time": "22:00", "temperature": 16.0}));
        let encoded = encode_schedule(&schedule, Some(8), (5.0, 95.0, 0.5), 10.0).unwrap();
        assert_eq!(encoded, raw);
    }

    #[test]
    fn schedule_rejects_changed_layout_and_bad_values() {
        let range = (5.0, 35.0, 0.5);
        let one = json!([{"time": "06:00", "temperature": 20.0}]);
        assert!(encode_schedule(&one, Some(2), range, 10.0).is_err());
        let bad_time = json!([{"time": "24:00", "temperature": 20.0}, {"time": "07:00", "temperature": 20.0}]);
        assert!(encode_schedule(&bad_time, Some(2), range, 10.0).is_err());
        let too_hot = json!([{"time": "06:00", "temperature": 99.0}, {"time": "07:00", "temperature": 20.0}]);
        assert!(encode_schedule(&too_hot, Some(2), range, 10.0).is_err());
    }

    #[test]
    fn time_sync_counts_from_2000_with_local_offset() {
        // 2026-01-01 00:00:00 UTC
        let payload = time_sync_payload(1_767_225_600, 3 * 3600, true);
        assert_eq!(&payload[..2], &[8, 0]);
        let utc = u32::from_be_bytes(payload[2..6].try_into().unwrap());
        let local = u32::from_be_bytes(payload[6..10].try_into().unwrap());
        assert_eq!(utc, 1_767_225_600 - 946_684_800);
        assert_eq!(local - utc, 10_800);
    }

    #[test]
    fn parses_posix_offsets() {
        assert_eq!(parse_posix_tz_offset("<+03>-3"), Some(10_800));
        assert_eq!(parse_posix_tz_offset("UTC0"), Some(0));
        assert_eq!(parse_posix_tz_offset("EST5EDT,M3.2.0,M11.1.0"), Some(-18_000));
        assert_eq!(parse_posix_tz_offset("<+0530>-5:30"), Some(19_800));
    }
}
