//! Tuya'nın `0xEF00` kümesi: cihaz değerlerini standart ZCL öznitelikleri yerine
//! numaralı "veri noktaları" (DP) olarak taşır. DP'lerin anlamı modele göre değişir;
//! eşlemeler zigbee-herdsman-converters'taki tanımlardan alınmıştır.

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

#[derive(Debug, Clone, Copy)]
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
    pub divisor: f64,
    pub setpoint_min: f64,
    pub setpoint_max: f64,
    pub setpoint_step: f64,
    pub time_since_2000: bool,
}

/// Tuya ZWT07 duvar termostatı (`_TZE200_g9a3awaj`)
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
    divisor: 10.0,
    setpoint_min: 5.0,
    setpoint_max: 60.0,
    setpoint_step: 0.5,
    time_since_2000: true,
};

pub fn thermostat_profile(manufacturer: Option<&str>) -> Option<&'static ThermostatProfile> {
    match manufacturer? {
        "_TZE200_g9a3awaj" => Some(&ZWT07),
        _ => None,
    }
}

impl ThermostatProfile {
    /// Bir DP'yi cihaz durumuna yansıtır; profilde yoksa `false`
    pub fn apply(&self, state: &mut crate::store::DeviceState, dp: &Datapoint) -> bool {
        let Some(raw) = dp.as_int() else {
            return false;
        };
        let scaled = || ((raw as f64 / self.divisor) * 100.0).round() / 100.0;
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
        } else {
            return false;
        }
        true
    }

    pub fn setpoint_dp(&self, celsius: f64) -> Option<Datapoint> {
        if !(self.setpoint_min..=self.setpoint_max).contains(&celsius) {
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::DeviceState;

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

    #[test]
    fn zwt07_maps_datapoints_to_state() {
        let mut state = DeviceState::default();
        for dp in [
            Datapoint::value(24, 213),
            Datapoint::value(16, 225),
            Datapoint::bool(1, true),
            Datapoint::enumeration(2, 0),
            Datapoint::enumeration(36, 1),
            Datapoint::bool(10, false),
        ] {
            assert!(ZWT07.apply(&mut state, &dp));
        }
        assert_eq!(state.temperature, Some(21.3));
        assert_eq!(state.setpoint, Some(22.5));
        assert_eq!(state.system_mode.as_deref(), Some("heat"));
        assert_eq!(state.preset.as_deref(), Some("program"));
        assert_eq!(state.heating, Some(true));
        assert_eq!(state.frost, Some(false));
        assert!(!ZWT07.apply(&mut state, &Datapoint::bool(99, true)));
    }

    #[test]
    fn setpoint_snaps_to_step_and_respects_range() {
        assert_eq!(ZWT07.setpoint_dp(21.3), Some(Datapoint::value(16, 215)));
        assert_eq!(ZWT07.setpoint_dp(4.0), None);
        assert_eq!(ZWT07.setpoint_dp(61.0), None);
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
