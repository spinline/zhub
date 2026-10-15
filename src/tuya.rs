//! Tuya'nın `0xEF00` kümesi: cihaz değerlerini standart ZCL öznitelikleri yerine
//! numaralı "veri noktaları" (DP) olarak taşır. DP'lerin anlamı modele göre değişir;
//! eşlemeler zigbee-herdsman-converters'taki tanımlardan alınmıştır.

use crate::profiles::{Setting, Source};
use crate::store::{Device, ScheduleSlot};

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

pub mod dp_raw {
    pub const RAW: u8 = 0;
    pub const STRING: u8 = 3;
}

#[derive(Debug)]
pub struct ThermostatProfile {
    pub name: &'static str,
    pub system_mode: u8,
    pub preset: Option<u8>,
    pub preset_manual: u8,
    pub preset_program: u8,
    pub setpoint: u8,
    pub local_temperature: u8,
    pub running_state: Option<u8>,
    pub frost: Option<u8>,
    pub battery: Option<u8>,
    pub child_lock: Option<u8>,
    pub fault: Option<u8>,
    pub product_info: Option<u8>,
    /// Çalışma haftası DP'si ve enum değerlerinin sırayla adları
    pub working_week: Option<(u8, &'static [&'static str])>,
    /// Haftalık program: 4 baytlık [saat, dakika, sıcaklık×bölen (BE)] dilimleri
    pub schedule: Option<u8>,
    pub divisor: f64,
    pub setpoint_min: f64,
    pub setpoint_max: f64,
    pub setpoint_step: f64,
    pub time_since_2000: bool,
    /// Sayısal yapılandırma DP'leri (düzeltme, sınırlar)
    pub settings: &'static [Setting],
}

/// Hedef sıcaklığın alt/üst sınır ayarlarının anahtarları
pub const MIN_LIMIT_KEY: &str = "min_temperature";
pub const MAX_LIMIT_KEY: &str = "max_temperature";

/// RTI-Tek T5Z / Tuya ZWT07 duvar termostatı (`_TZE200_g9a3awaj`). DP'ler
/// zigbee-herdsman-converters ile tuya-local'in `rtitek_t5z_thermostat` tanımından.
const ZWT07: ThermostatProfile = ThermostatProfile {
    name: "Termostat",
    system_mode: 1,
    preset: Some(2),
    preset_manual: 1,
    preset_program: 0,
    setpoint: 16,
    local_temperature: 24,
    running_state: Some(36),
    frost: Some(10),
    battery: Some(9),
    child_lock: Some(40),
    fault: Some(45),
    product_info: Some(105),
    working_week: Some((31, &["5_2", "6_1", "7"])),
    schedule: Some(67),
    divisor: 10.0,
    setpoint_min: 5.0,
    setpoint_max: 95.0,
    setpoint_step: 0.5,
    time_since_2000: true,
    settings: &[
        Setting {
            key: "temperature_calibration",
            label: "Sıcaklık düzeltmesi",
            source: Source::TuyaDp(109),
            scale: 10.0,
            min: -9.9,
            max: 9.9,
            step: 0.1,
            unit: "°C",
        },
        Setting {
            key: MIN_LIMIT_KEY,
            label: "En düşük hedef",
            source: Source::TuyaDp(18),
            scale: 10.0,
            min: 5.0,
            max: 20.0,
            step: 1.0,
            unit: "°C",
        },
        Setting {
            key: MAX_LIMIT_KEY,
            label: "En yüksek hedef",
            source: Source::TuyaDp(19),
            scale: 10.0,
            min: 21.0,
            max: 95.0,
            step: 1.0,
            unit: "°C",
        },
    ],
};

pub fn thermostat_profile(manufacturer: Option<&str>) -> Option<&'static ThermostatProfile> {
    match manufacturer? {
        "_TZE200_g9a3awaj" => Some(&ZWT07),
        _ => None,
    }
}

impl ThermostatProfile {
    /// Bir DP'yi cihaza yansıtır; profilde yoksa `false`
    pub fn apply(&self, device: &mut Device, dp: &Datapoint) -> bool {
        // Sayı olmayan DP'ler
        if Some(dp.id) == self.schedule && dp.ty == dp_raw::RAW {
            device.state.schedule = Some(self.decode_schedule(&dp.data));
            return true;
        }
        if Some(dp.id) == self.product_info && dp.ty == dp_raw::STRING {
            let text = String::from_utf8_lossy(&dp.data).trim_end_matches('\0').trim().to_owned();
            if !text.is_empty() {
                device.firmware = Some(text);
            }
            return true;
        }

        let Some(raw) = dp.as_int() else {
            return false;
        };
        if let Some(setting) = self.settings.iter().find(|s| s.source == Source::TuyaDp(dp.id)) {
            device.settings.insert(setting.key.to_owned(), setting.from_raw(raw));
            return true;
        }

        let scaled = || ((raw as f64 / self.divisor) * 100.0).round() / 100.0;
        let state = &mut device.state;
        if dp.id == self.system_mode {
            state.system_mode = Some(if raw != 0 { "heat" } else { "off" }.to_owned());
        } else if Some(dp.id) == self.preset {
            let preset = if raw == i64::from(self.preset_manual) {
                "manual"
            } else if raw == i64::from(self.preset_program) {
                "program"
            } else {
                "unknown"
            };
            state.preset = Some(preset.to_owned());
        } else if dp.id == self.setpoint {
            state.setpoint = Some(scaled());
        } else if dp.id == self.local_temperature {
            state.temperature = Some(scaled());
        } else if Some(dp.id) == self.running_state {
            state.heating = Some(raw != 0);
        } else if Some(dp.id) == self.frost {
            state.frost = Some(raw != 0);
        } else if Some(dp.id) == self.battery {
            state.battery = Some(raw.clamp(0, 100) as u8);
        } else if Some(dp.id) == self.child_lock {
            state.child_lock = Some(raw != 0);
        } else if Some(dp.id) == self.fault {
            state.fault = Some(raw as u32);
        } else if let Some((id, names)) = self.working_week
            && dp.id == id
        {
            state.working_week = usize::try_from(raw)
                .ok()
                .and_then(|i| names.get(i))
                .map(|name| (*name).to_owned());
        } else {
            return false;
        }
        true
    }

    /// Hedef sıcaklığın geçerli aralığı: cihazdaki sınır ayarları, mutlak aralığın içinde
    pub fn setpoint_range(&self, device: &Device) -> (f64, f64) {
        let min = device
            .settings
            .get(MIN_LIMIT_KEY)
            .copied()
            .unwrap_or(self.setpoint_min)
            .max(self.setpoint_min);
        let max = device
            .settings
            .get(MAX_LIMIT_KEY)
            .copied()
            .unwrap_or(self.setpoint_max)
            .min(self.setpoint_max);
        (min, max)
    }

    pub fn setpoint_dp(&self, device: &Device, celsius: f64) -> Option<Datapoint> {
        let (min, max) = self.setpoint_range(device);
        if !celsius.is_finite() || !(min..=max).contains(&celsius) {
            return None;
        }
        let steps = (celsius / self.setpoint_step).round() * self.setpoint_step;
        Some(Datapoint::value(self.setpoint, (steps * self.divisor).round() as i32))
    }

    pub fn system_mode_dp(&self, heat: bool) -> Datapoint {
        Datapoint::bool(self.system_mode, heat)
    }

    pub fn preset_dp(&self, program: bool) -> Option<Datapoint> {
        let value = if program {
            self.preset_program
        } else {
            self.preset_manual
        };
        self.preset.map(|id| Datapoint::enumeration(id, value))
    }

    pub fn frost_dp(&self, on: bool) -> Option<Datapoint> {
        self.frost.map(|id| Datapoint::bool(id, on))
    }

    pub fn child_lock_dp(&self, on: bool) -> Option<Datapoint> {
        self.child_lock.map(|id| Datapoint::bool(id, on))
    }

    pub fn working_week_dp(&self, name: &str) -> Option<Datapoint> {
        let (id, names) = self.working_week?;
        let index = names.iter().position(|n| *n == name)?;
        Some(Datapoint::enumeration(id, index as u8))
    }

    pub fn decode_schedule(&self, data: &[u8]) -> Vec<ScheduleSlot> {
        data.chunks_exact(4)
            .map(|slot| ScheduleSlot {
                time: format!("{:02}:{:02}", slot[0], slot[1]),
                temperature: f64::from(u16::from_be_bytes([slot[2], slot[3]])) / self.divisor,
            })
            .collect()
    }

    /// Programı geri kodlar. Dilim sayısı cihazın bildirdiğiyle aynı kalmalı; cihazın
    /// beklediği düzen (hafta içi/sonu dilimleri) bu sayıya bağlı.
    pub fn schedule_dp(&self, device: &Device, slots: &[ScheduleSlot]) -> Result<Datapoint, String> {
        let id = self.schedule.ok_or("cihazda haftalık program yok")?;
        let expected = device.state.schedule.as_ref().map(Vec::len);
        if expected != Some(slots.len()) {
            return Err(format!(
                "program {} dilim olmalı (cihazın bildirdiği düzen)",
                expected.unwrap_or(0)
            ));
        }
        let (min, max) = self.setpoint_range(device);
        let mut data = Vec::with_capacity(slots.len() * 4);
        for slot in slots {
            let (hour, minute) = slot
                .time
                .split_once(':')
                .and_then(|(h, m)| Some((h.parse::<u8>().ok()?, m.parse::<u8>().ok()?)))
                .filter(|&(h, m)| h < 24 && m < 60)
                .ok_or_else(|| format!("geçersiz saat: {}", slot.time))?;
            if !slot.temperature.is_finite() || !(min..=max).contains(&slot.temperature) {
                return Err(format!("sıcaklık {min}-{max} °C arasında olmalı"));
            }
            let raw = (slot.temperature / self.setpoint_step).round() * self.setpoint_step * self.divisor;
            data.extend_from_slice(&[hour, minute]);
            data.extend_from_slice(&(raw.round() as u16).to_be_bytes());
        }
        Ok(Datapoint {
            id,
            ty: dp_raw::RAW,
            data,
        })
    }

    /// Yapılandırma DP'si (VALUE türü) yazımı
    pub fn setting_dp(&self, setting: &Setting, raw: i64) -> Option<Datapoint> {
        let Source::TuyaDp(id) = setting.source else {
            return None;
        };
        Some(Datapoint::value(id, i32::try_from(raw).ok()?))
    }
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

    fn thermostat() -> Device {
        let mut device = Device::new("a4:c1:38:dd:3d:d3:35:0b".into(), 0x254a);
        device.manufacturer = Some("_TZE200_g9a3awaj".into());
        device
    }

    #[test]
    fn zwt07_maps_datapoints_to_state() {
        let mut device = thermostat();
        for dp in [
            Datapoint::value(24, 213),
            Datapoint::value(16, 225),
            Datapoint::bool(1, true),
            Datapoint::enumeration(2, 0),
            Datapoint::enumeration(36, 1),
            Datapoint::bool(10, false),
        ] {
            assert!(ZWT07.apply(&mut device, &dp));
        }
        let state = &device.state;
        assert_eq!(state.temperature, Some(21.3));
        assert_eq!(state.setpoint, Some(22.5));
        assert_eq!(state.system_mode.as_deref(), Some("heat"));
        assert_eq!(state.preset.as_deref(), Some("program"));
        assert_eq!(state.heating, Some(true));
        assert_eq!(state.frost, Some(false));
        assert!(!ZWT07.apply(&mut device, &Datapoint::bool(99, true)));
    }

    /// Termostatın DATA_QUERY'ye verdiği gerçek yanıtlar (zhub günlüğünden)
    #[test]
    fn zwt07_decodes_observed_datapoints() {
        let observed = [
            Datapoint { id: 109, ty: 2, data: vec![0, 0, 0, 0] },
            Datapoint { id: 31, ty: 4, data: vec![0] },
            Datapoint { id: 40, ty: 1, data: vec![1] },
            Datapoint { id: 45, ty: 5, data: vec![0] },
            Datapoint { id: 19, ty: 2, data: vec![0, 0, 0x01, 0xf4] },
            Datapoint { id: 18, ty: 2, data: vec![0, 0, 0, 0x32] },
            Datapoint {
                id: 67,
                ty: 0,
                data: vec![
                    0x06, 0x3b, 0x00, 0xbe, 0x08, 0x00, 0x00, 0x96, 0x0b, 0x1e, 0x00, 0x96, 0x0c, 0x1e,
                    0x00, 0x96, 0x11, 0x00, 0x00, 0xdc, 0x16, 0x00, 0x00, 0x96, 0x08, 0x00, 0x00, 0x96,
                    0x16, 0x00, 0x00, 0xa0,
                ],
            },
            Datapoint { id: 9, ty: 2, data: vec![0, 0, 0, 0x63] },
            Datapoint { id: 105, ty: 3, data: b"zt3l-t5\0".to_vec() },
        ];
        let mut device = thermostat();
        for dp in &observed {
            assert!(ZWT07.apply(&mut device, dp), "DP {} tanınmadı", dp.id);
        }
        assert_eq!(device.settings["temperature_calibration"], 0.0);
        assert_eq!(device.settings[MIN_LIMIT_KEY], 5.0);
        assert_eq!(device.settings[MAX_LIMIT_KEY], 50.0);
        assert_eq!(device.state.working_week.as_deref(), Some("5_2"));
        assert_eq!(device.state.child_lock, Some(true));
        assert_eq!(device.state.fault, Some(0));
        assert_eq!(device.state.battery, Some(99));
        assert_eq!(device.firmware.as_deref(), Some("zt3l-t5"));

        let schedule = device.state.schedule.clone().unwrap();
        assert_eq!(schedule.len(), 8);
        assert_eq!(schedule[0], ScheduleSlot { time: "06:59".into(), temperature: 19.0 });
        assert_eq!(schedule[4], ScheduleSlot { time: "17:00".into(), temperature: 22.0 });
        assert_eq!(schedule[7], ScheduleSlot { time: "22:00".into(), temperature: 16.0 });

        // Değiştirilmeden geri kodlanan program cihazdan gelen baytlarla aynı olmalı
        let encoded = ZWT07.schedule_dp(&device, &schedule).unwrap();
        assert_eq!(encoded, observed[6]);
    }

    #[test]
    fn schedule_rejects_changed_layout_and_bad_values() {
        let mut device = thermostat();
        device.state.schedule = Some(vec![ScheduleSlot { time: "06:00".into(), temperature: 20.0 }; 2]);
        let one = [ScheduleSlot { time: "06:00".into(), temperature: 20.0 }];
        assert!(ZWT07.schedule_dp(&device, &one).is_err());
        let bad_time = [
            ScheduleSlot { time: "24:00".into(), temperature: 20.0 },
            ScheduleSlot { time: "07:00".into(), temperature: 20.0 },
        ];
        assert!(ZWT07.schedule_dp(&device, &bad_time).is_err());
        let too_hot = [
            ScheduleSlot { time: "06:00".into(), temperature: 99.0 },
            ScheduleSlot { time: "07:00".into(), temperature: 20.0 },
        ];
        assert!(ZWT07.schedule_dp(&device, &too_hot).is_err());
    }

    #[test]
    fn setpoint_snaps_to_step_and_respects_device_limits() {
        let mut device = thermostat();
        assert_eq!(ZWT07.setpoint_dp(&device, 21.3), Some(Datapoint::value(16, 215)));
        assert_eq!(ZWT07.setpoint_dp(&device, 4.0), None);
        assert!(ZWT07.setpoint_dp(&device, 90.0).is_some());
        device.settings.insert(MAX_LIMIT_KEY.into(), 50.0);
        assert_eq!(ZWT07.setpoint_dp(&device, 60.0), None);
    }

    #[test]
    fn working_week_round_trips() {
        assert_eq!(ZWT07.working_week_dp("7"), Some(Datapoint::enumeration(31, 2)));
        assert_eq!(ZWT07.working_week_dp("8"), None);
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
