//! ZCL (Zigbee Cluster Library) frames: header, global commands and attribute data
//! types. Only the part needed for lights, sensors and thermostats.

pub const FC_CLUSTER_SPECIFIC: u8 = 0x01;
pub const FC_MANUFACTURER_SPECIFIC: u8 = 0x04;
pub const FC_SERVER_TO_CLIENT: u8 = 0x08;
pub const FC_DISABLE_DEFAULT_RESPONSE: u8 = 0x10;

pub mod cmd {
    pub const READ_ATTRIBUTES: u8 = 0x00;
    pub const READ_ATTRIBUTES_RSP: u8 = 0x01;
    pub const WRITE_ATTRIBUTES: u8 = 0x02;
    pub const WRITE_ATTRIBUTES_RSP: u8 = 0x04;
    pub const CONFIGURE_REPORTING: u8 = 0x06;
    pub const CONFIGURE_REPORTING_RSP: u8 = 0x07;
    pub const REPORT_ATTRIBUTES: u8 = 0x0A;
    pub const DEFAULT_RSP: u8 = 0x0B;
}

pub mod cluster {
    pub const BASIC: u16 = 0x0000;
    pub const POWER_CONFIG: u16 = 0x0001;
    pub const IDENTIFY: u16 = 0x0003;
    pub const POLL_CONTROL: u16 = 0x0020;
    pub const TIME: u16 = 0x000A;
    pub const THERMOSTAT: u16 = 0x0201;
    pub const ON_OFF: u16 = 0x0006;
    pub const LEVEL: u16 = 0x0008;
    pub const COLOR: u16 = 0x0300;
    pub const TEMPERATURE: u16 = 0x0402;
    pub const HUMIDITY: u16 = 0x0405;
    pub const OCCUPANCY: u16 = 0x0406;
    pub const IAS_ZONE: u16 = 0x0500;
}

pub mod types {
    pub const BOOL: u8 = 0x10;
    pub const BITMAP8: u8 = 0x18;
    pub const BITMAP16: u8 = 0x19;
    pub const UINT8: u8 = 0x20;
    pub const UINT16: u8 = 0x21;
    pub const UINT32: u8 = 0x23;
    pub const INT8: u8 = 0x28;
    pub const INT16: u8 = 0x29;
    pub const INT32: u8 = 0x2B;
    pub const ENUM8: u8 = 0x30;
    pub const ENUM16: u8 = 0x31;
    pub const UTC_TIME: u8 = 0xE2;
    pub const EUI64: u8 = 0xF0;
}

/// Encodes an integer as the attribute type; `None` if the type is not numeric or the value does not fit
pub fn encode_int(ty: u8, raw: i64) -> Option<Vec<u8>> {
    let signed = matches!(ty, 0x28..=0x2F);
    let numeric = matches!(ty, 0x08..=0x10 | 0x18..=0x2F | 0x30 | 0x31 | 0xE0..=0xE2);
    if !numeric {
        return None;
    }
    let size = fixed_size(ty)?;
    let bits = 8 * size as u32;
    let fits = if signed {
        let limit = 1i64 << (bits - 1);
        (-limit..limit).contains(&raw)
    } else {
        raw >= 0 && (bits >= 64 || raw < (1i64 << bits))
    };
    fits.then(|| raw.to_le_bytes()[..size].to_vec())
}

pub const STATUS_SUCCESS: u8 = 0x00;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub frame_control: u8,
    pub manufacturer: Option<u16>,
    pub seq: u8,
    pub command: u8,
}

impl Header {
    pub fn cluster_specific(&self) -> bool {
        self.frame_control & 0x03 == FC_CLUSTER_SPECIFIC
    }

    pub fn server_to_client(&self) -> bool {
        self.frame_control & FC_SERVER_TO_CLIENT != 0
    }

    pub fn disable_default_response(&self) -> bool {
        self.frame_control & FC_DISABLE_DEFAULT_RESPONSE != 0
    }

    pub fn parse(data: &[u8]) -> Option<(Self, &[u8])> {
        let frame_control = *data.first()?;
        let (manufacturer, rest) = if frame_control & FC_MANUFACTURER_SPECIFIC != 0 {
            let code = u16::from_le_bytes([*data.get(1)?, *data.get(2)?]);
            (Some(code), data.get(3..)?)
        } else {
            (None, data.get(1..)?)
        };
        let seq = *rest.first()?;
        let command = *rest.get(1)?;
        Some((
            Self {
                frame_control,
                manufacturer,
                seq,
                command,
            },
            &rest[2..],
        ))
    }
}

pub fn frame(frame_control: u8, seq: u8, command: u8, payload: &[u8]) -> Vec<u8> {
    manufacturer_frame(frame_control, None, seq, command, payload)
}

/// A frame with the manufacturer code when given (manufacturer-specific commands)
pub fn manufacturer_frame(frame_control: u8, manufacturer: Option<u16>, seq: u8, command: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(5 + payload.len());
    match manufacturer {
        Some(code) => {
            out.push(frame_control | FC_MANUFACTURER_SPECIFIC);
            out.extend_from_slice(&code.to_le_bytes());
        }
        None => out.push(frame_control),
    }
    out.push(seq);
    out.push(command);
    out.extend_from_slice(payload);
    out
}

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Bool(bool),
    Uint(u64),
    Int(i64),
    Float(f64),
    Str(String),
    Bytes(Vec<u8>),
}

impl Value {
    pub fn as_i64(&self) -> Option<i64> {
        match *self {
            Self::Bool(b) => Some(i64::from(b)),
            Self::Uint(v) => i64::try_from(v).ok(),
            Self::Int(v) => Some(v),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::Str(s) => Some(s),
            _ => None,
        }
    }
}

/// Byte size of the fixed-length types
fn fixed_size(ty: u8) -> Option<usize> {
    Some(match ty {
        0x00 => 0,
        0x08..=0x0F => usize::from(ty - 0x07),
        0x10 => 1,
        0x18..=0x1F => usize::from(ty - 0x17),
        0x20..=0x27 => usize::from(ty - 0x1F),
        0x28..=0x2F => usize::from(ty - 0x27),
        0x30 => 1,
        0x31 => 2,
        0x38 => 2,
        0x39 => 4,
        0x3A => 8,
        0xE0..=0xE2 => 4,
        0xE8 | 0xE9 => 2,
        0xEA => 4,
        0xF0 => 8,
        0xF1 => 16,
        _ => return None,
    })
}

fn le_uint(bytes: &[u8]) -> u64 {
    bytes
        .iter()
        .rev()
        .fold(0u64, |acc, &b| (acc << 8) | u64::from(b))
}

/// Decodes an attribute value; returns the value and the bytes it consumed
pub fn parse_value(ty: u8, data: &[u8]) -> Option<(Value, usize)> {
    match ty {
        0x41..=0x44 => {
            let (len, header) = if ty <= 0x42 {
                let len = *data.first()?;
                // 0xFF means an invalid (undefined) string
                (if len == 0xFF { 0 } else { usize::from(len) }, 1)
            } else {
                let len = u16::from_le_bytes([*data.first()?, *data.get(1)?]);
                (if len == 0xFFFF { 0 } else { usize::from(len) }, 2)
            };
            let body = data.get(header..header + len)?;
            let value = if ty == 0x42 || ty == 0x44 {
                // Some devices pad strings with NUL
                Value::Str(
                    String::from_utf8_lossy(body)
                        .trim_end_matches('\0')
                        .trim()
                        .to_owned(),
                )
            } else {
                Value::Bytes(body.to_vec())
            };
            Some((value, header + len))
        }
        _ => {
            let size = fixed_size(ty)?;
            let bytes = data.get(..size)?;
            let value = match ty {
                0x10 => Value::Bool(bytes[0] != 0),
                0x28..=0x2F => {
                    let raw = le_uint(bytes);
                    let shift = 64 - 8 * size as u32;
                    Value::Int(((raw << shift) as i64) >> shift)
                }
                0x39 => Value::Float(f64::from(f32::from_le_bytes(bytes.try_into().ok()?))),
                0x3A => Value::Float(f64::from_le_bytes(bytes.try_into().ok()?)),
                0xF0 | 0xF1 => Value::Bytes(bytes.to_vec()),
                _ => Value::Uint(le_uint(bytes)),
            };
            Some((value, size))
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct AttributeRecord {
    pub id: u16,
    pub status: u8,
    pub value: Option<Value>,
}

/// Read Attributes Response: (id, status, [type, value])*
pub fn parse_read_response(mut payload: &[u8]) -> Vec<AttributeRecord> {
    let mut records = Vec::new();
    while payload.len() >= 3 {
        let id = u16::from_le_bytes([payload[0], payload[1]]);
        let status = payload[2];
        payload = &payload[3..];
        if status != STATUS_SUCCESS {
            records.push(AttributeRecord {
                id,
                status,
                value: None,
            });
            continue;
        }
        let Some((&ty, rest)) = payload.split_first() else {
            break;
        };
        let Some((value, used)) = parse_value(ty, rest) else {
            // An unknown type: the bounds of the remaining records are unknown
            break;
        };
        records.push(AttributeRecord {
            id,
            status,
            value: Some(value),
        });
        payload = &rest[used..];
    }
    records
}

/// Report Attributes: (id, type, value)*
pub fn parse_report(mut payload: &[u8]) -> Vec<AttributeRecord> {
    let mut records = Vec::new();
    while payload.len() >= 3 {
        let id = u16::from_le_bytes([payload[0], payload[1]]);
        let ty = payload[2];
        let Some((value, used)) = parse_value(ty, &payload[3..]) else {
            break;
        };
        records.push(AttributeRecord {
            id,
            status: STATUS_SUCCESS,
            value: Some(value),
        });
        payload = &payload[3 + used..];
    }
    records
}

pub fn read_attributes_payload(ids: &[u16]) -> Vec<u8> {
    ids.iter().flat_map(|id| id.to_le_bytes()).collect()
}

pub fn write_attribute_payload(id: u16, ty: u8, value: &[u8]) -> Vec<u8> {
    let mut out = id.to_le_bytes().to_vec();
    out.push(ty);
    out.extend_from_slice(value);
    out
}

#[derive(Debug, Clone, Copy)]
pub struct ReportConfig {
    pub attribute: u16,
    pub ty: u8,
    pub min_interval: u16,
    pub max_interval: u16,
    /// Only sent for analog types; its length is the type's size
    pub reportable_change: u64,
}

fn is_analog(ty: u8) -> bool {
    matches!(ty, 0x20..=0x2F | 0x38..=0x3A | 0xE0..=0xE2)
}

pub fn configure_reporting_payload(configs: &[ReportConfig]) -> Vec<u8> {
    let mut out = Vec::new();
    for config in configs {
        out.push(0x00); // direction: the device sends the reports
        out.extend_from_slice(&config.attribute.to_le_bytes());
        out.push(config.ty);
        out.extend_from_slice(&config.min_interval.to_le_bytes());
        out.extend_from_slice(&config.max_interval.to_le_bytes());
        if is_analog(config.ty) {
            let size = fixed_size(config.ty).unwrap_or(0);
            out.extend_from_slice(&config.reportable_change.to_le_bytes()[..size]);
        }
    }
    out
}

/// The status byte of a Default Response payload
pub fn default_response_status(payload: &[u8]) -> Option<u8> {
    payload.get(1).copied()
}

/// Write/Configure Reporting response: a single success byte or (status, ...) records
pub fn all_success(payload: &[u8]) -> bool {
    payload.first() == Some(&STATUS_SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_integers_by_attribute_type() {
        assert_eq!(encode_int(types::INT16, -130), Some(vec![0x7E, 0xFF]));
        assert_eq!(encode_int(types::INT8, -127), Some(vec![0x81]));
        assert_eq!(encode_int(types::INT8, 128), None);
        assert_eq!(encode_int(types::UINT8, 255), Some(vec![0xFF]));
        assert_eq!(encode_int(types::UINT8, -1), None);
        assert_eq!(encode_int(types::UINT32, 5400), Some(vec![0x18, 0x15, 0x00, 0x00]));
        assert_eq!(encode_int(types::BOOL, 1), Some(vec![0x01]));
        assert_eq!(encode_int(types::EUI64, 1), None);
    }

    #[test]
    fn manufacturer_frame_carries_the_code() {
        // Hue candle effect: cluster specific + manufacturer specific, Signify
        let hue = manufacturer_frame(FC_CLUSTER_SPECIFIC, Some(0x100B), 7, 0x00, &[0x21, 0x00, 0x01, 0x01]);
        assert_eq!(hue, [0x05, 0x0B, 0x10, 7, 0x00, 0x21, 0x00, 0x01, 0x01]);
        let (header, body) = Header::parse(&hue).unwrap();
        assert_eq!(header.manufacturer, Some(0x100B));
        assert_eq!(body, [0x21, 0x00, 0x01, 0x01]);
        assert_eq!(frame(0x00, 1, 0x02, &[]), [0x00, 1, 0x02]);
    }

    #[test]
    fn parses_header_with_manufacturer_code() {
        let data = [0x1C, 0x5F, 0x11, 0x42, 0x0A, 0xAA];
        let (header, rest) = Header::parse(&data).unwrap();
        assert_eq!(header.manufacturer, Some(0x115F));
        assert_eq!(header.seq, 0x42);
        assert_eq!(header.command, cmd::REPORT_ATTRIBUTES);
        assert!(header.server_to_client());
        assert!(header.disable_default_response());
        assert_eq!(rest, &[0xAA]);
    }

    #[test]
    fn parses_temperature_report() {
        // MeasuredValue (0x0000), int16, 21.53 °C
        let records = parse_report(&[0x00, 0x00, 0x29, 0x69, 0x08]);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].value, Some(Value::Int(2153)));
    }

    #[test]
    fn sign_extends_negative_int16() {
        let (value, used) = parse_value(types::INT16, &[0x9C, 0xFF]).unwrap();
        assert_eq!(value, Value::Int(-100));
        assert_eq!(used, 2);
    }

    #[test]
    fn parses_read_response_with_failures_and_strings() {
        let payload = [
            0x04, 0x00, 0x00, 0x42, 0x05, b'I', b'K', b'E', b'A', 0x00, // ManufacturerName
            0xFE, 0xFF, 0x86, // unsupported attribute
            0x07, 0x00, 0x00, 0x30, 0x01, // PowerSource enum8
        ];
        let records = parse_read_response(&payload);
        assert_eq!(records.len(), 3);
        assert_eq!(records[0].value, Some(Value::Str("IKEA".into())));
        assert_eq!(records[1].status, 0x86);
        assert_eq!(records[1].value, None);
        assert_eq!(records[2].value, Some(Value::Uint(1)));
    }

    #[test]
    fn encodes_reporting_change_only_for_analog_types() {
        let payload = configure_reporting_payload(&[
            ReportConfig {
                attribute: 0x0000,
                ty: types::BOOL,
                min_interval: 0,
                max_interval: 300,
                reportable_change: 0,
            },
            ReportConfig {
                attribute: 0x0000,
                ty: types::INT16,
                min_interval: 30,
                max_interval: 3600,
                reportable_change: 10,
            },
        ]);
        assert_eq!(
            payload,
            [
                0x00, 0x00, 0x00, 0x10, 0x00, 0x00, 0x2C, 0x01, // bool, no reportable change
                0x00, 0x00, 0x00, 0x29, 0x1E, 0x00, 0x10, 0x0E, 0x0A, 0x00,
            ]
        );
    }
}
