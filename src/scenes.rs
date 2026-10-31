//! Zigbee Scenes (0x0005). Values live on device endpoints; this is confirmed
//! metadata and the optimistic state used while reports catch up after a recall.
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value as Json, json};

pub const KEYS: &[&str] = &[
    "scene_store",
    "scene_add",
    "scene_recall",
    "scene_remove",
    "scene_remove_all",
    "scene_rename",
];
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredScene {
    pub ieee: String,
    pub ep: u8,
    pub group_id: u16,
    pub id: u8,
    pub name: String,
    pub transition: f64,
    pub values: Map<String, Json>,
}
#[derive(Debug, Clone)]
pub struct Operation {
    pub key: String,
    pub id: u8,
    pub name: Option<String>,
    pub group_id: Option<u16>,
    pub ep: Option<u8>,
    pub fields: Map<String, Json>,
}
fn integer(v: &Json, name: &str, max: u64) -> Result<u64> {
    v.as_u64()
        .filter(|n| *n <= max)
        .with_context(|| format!("{name} must be an integer from 0 to {max}"))
}
impl Operation {
    pub fn parse(key: &str, value: &Json) -> Result<Self> {
        ensure!(KEYS.contains(&key), "unknown scene operation");
        let fields = value.as_object().cloned().unwrap_or_default();
        let all = key == "scene_remove_all";
        if matches!(key, "scene_add" | "scene_rename") {
            ensure!(value.is_object(), "{key} requires an object");
        }
        if all {
            ensure!(
                value == "" || value.is_object(),
                "scene_remove_all requires an empty string or options object"
            );
        }
        let id = if all {
            0
        } else {
            integer(fields.get("ID").unwrap_or(value), "ID", 255)? as u8
        };
        let name = fields
            .get("name")
            .map(|v| {
                let s = v.as_str().context("scene name must be text")?.trim();
                ensure!(
                    !s.is_empty() && s.chars().count() <= 64,
                    "scene name must be 1-64 characters"
                );
                Ok(s.to_owned())
            })
            .transpose()?;
        ensure!(
            key != "scene_rename" || name.is_some(),
            "scene_rename requires a name"
        );
        let group_id = fields
            .get("group_id")
            .map(|v| integer(v, "group_id", 0xfff7).map(|n| n as u16))
            .transpose()?;
        let ep = fields
            .get("ep")
            .map(|v| {
                integer(v, "ep", 240).and_then(|n| {
                    ensure!(n > 0, "ep must be positive");
                    Ok(n as u8)
                })
            })
            .transpose()?;
        for k in fields.keys() {
            ensure!(
                matches!(k.as_str(), "ID" | "name" | "group_id" | "ep")
                    || key == "scene_add"
                        && matches!(
                            k.as_str(),
                            "transition" | "state" | "brightness" | "color_temp" | "color"
                        ),
                "unsupported {key} field: {k}"
            );
        }
        if key == "scene_add" {
            let mut validate = fields.clone();
            if let Some(v) = fields.get("color_temp") {
                validate.insert(
                    "color_temp".into(),
                    json!(number(v, "color_temp", 0.0, 65534.0)?.clamp(153.0, 500.0)),
                );
            }
            add_payload(0, id, &validate)?;
        }
        Ok(Self {
            key: key.to_owned(),
            id,
            name,
            group_id,
            ep,
            fields,
        })
    }
    pub fn validate_scope(&self, group_id: u16) -> Result<()> {
        ensure!(
            self.key == "scene_remove_all" || group_id != 0 || self.id != 0,
            "scene 0 in group 0 is reserved for the On/Off global scene"
        );
        Ok(())
    }
}
pub fn identity(group_id: u16, id: u8) -> Vec<u8> {
    let mut b = group_id.to_le_bytes().to_vec();
    b.push(id);
    b
}
fn field(out: &mut Vec<u8>, cluster: u16, bytes: &[u8]) {
    out.extend(cluster.to_le_bytes());
    out.push(bytes.len() as u8);
    out.extend(bytes);
}
fn number(v: &Json, name: &str, min: f64, max: f64) -> Result<f64> {
    v.as_f64()
        .filter(|n| n.is_finite() && (min..=max).contains(n))
        .with_context(|| format!("{name} must be between {min} and {max}"))
}
/// Standard Planckian locus approximation for the color temperatures used by lamps.
fn mired_xy(mired: f64) -> Result<(f64, f64)> {
    let t = 1_000_000.0 / mired;
    ensure!(
        (1667.0..=25000.0).contains(&t),
        "color_temp is outside the supported 1667-25000 K range"
    );
    let x = if t <= 4000.0 {
        -0.2661239e9 / t.powi(3) - 0.2343580e6 / t.powi(2) + 0.8776956e3 / t + 0.179910
    } else {
        -3.0258469e9 / t.powi(3) + 2.1070379e6 / t.powi(2) + 0.2226347e3 / t + 0.240390
    };
    let y = if t <= 2222.0 {
        -1.1063814 * x.powi(3) - 1.34811020 * x.powi(2) + 2.18555832 * x - 0.20219683
    } else if t <= 4000.0 {
        -0.9549476 * x.powi(3) - 1.37418593 * x.powi(2) + 2.09137015 * x - 0.16748867
    } else {
        3.0817580 * x.powi(3) - 5.87338670 * x.powi(2) + 3.75112997 * x - 0.37001483
    };
    Ok((x, y))
}
fn rgb_xy(r: f64, g: f64, b: f64) -> (f64, f64) {
    let linear = |v: f64| {
        if v > 0.04045 {
            ((v + 0.055) / 1.055).powf(2.4)
        } else {
            v / 12.92
        }
    };
    let (r, g, b) = (linear(r), linear(g), linear(b));
    let x = r * 0.664511 + g * 0.154324 + b * 0.162028;
    let y = r * 0.283881 + g * 0.668433 + b * 0.047685;
    let z = r * 0.000088 + g * 0.072310 + b * 0.986039;
    if x + y + z == 0.0 {
        (0.0, 0.0)
    } else {
        (x / (x + y + z), y / (x + y + z))
    }
}
fn color_xy(v: &Json) -> Result<(f64, f64)> {
    if let Some(s) = v.as_str() {
        ensure!(
            s.len() == 7 && s.starts_with('#') && s[1..].bytes().all(|b| b.is_ascii_hexdigit()),
            "color must be #RRGGBB"
        );
        let n = u32::from_str_radix(&s[1..], 16)?;
        return Ok(rgb_xy(
            ((n >> 16) & 255) as f64 / 255.0,
            ((n >> 8) & 255) as f64 / 255.0,
            (n & 255) as f64 / 255.0,
        ));
    }
    let o = v
        .as_object()
        .context("color must be x/y, hue/saturation or #RRGGBB")?;
    ensure!(
        o.keys()
            .all(|k| matches!(k.as_str(), "x" | "y" | "hue" | "saturation")),
        "unknown color field"
    );
    if o.contains_key("x") || o.contains_key("y") {
        let x = number(o.get("x").context("color.x missing")?, "color.x", 0.0, 1.0)?;
        let y = number(o.get("y").context("color.y missing")?, "color.y", 0.0, 1.0)?;
        ensure!(x + y <= 1.00001, "color x+y must not exceed 1");
        return Ok((x, y));
    }
    let h = number(
        o.get("hue").context("color.hue missing")?,
        "hue",
        0.0,
        360.0,
    )? / 60.0;
    let s = number(
        o.get("saturation").context("color.saturation missing")?,
        "saturation",
        0.0,
        100.0,
    )? / 100.0;
    let x = s * (1.0 - (h % 2.0 - 1.0).abs());
    let m = 1.0 - s;
    let (r, g, b) = match h as u8 {
        0 | 6 => (s, x, 0.0),
        1 => (x, s, 0.0),
        2 => (0.0, s, x),
        3 => (0.0, x, s),
        4 => (x, 0.0, s),
        _ => (s, 0.0, x),
    };
    Ok(rgb_xy(r + m, g + m, b + m))
}
/// Add Scene for integer seconds; Enhanced Add Scene for tenths of a second.
/// Color temperature uses XY as Zigbee2MQTT does for older Scenes servers.
pub fn add_payload(
    group_id: u16,
    id: u8,
    fields: &Map<String, Json>,
) -> Result<(u8, Vec<u8>, Map<String, Json>, f64)> {
    ensure!(
        !(fields.contains_key("color_temp") && fields.contains_key("color")),
        "use color_temp or color, not both"
    );
    let time = fields
        .get("transition")
        .map(|v| number(v, "transition", 0.0, 65534.0))
        .transpose()?
        .unwrap_or(0.0);
    let enhanced = time.fract() != 0.0;
    ensure!(
        !enhanced || time <= 6553.4,
        "fractional transition must not exceed 6553.4 seconds"
    );
    let ticks = if enhanced {
        (time * 10.0).floor() as u16
    } else {
        time as u16
    };
    let mut b = identity(group_id, id);
    b.extend(ticks.to_le_bytes());
    b.push(0); // device-side name empty; UI name is metadata
    let mut values = Map::new();
    if let Some(v) = fields.get("state") {
        let s = v
            .as_str()
            .context("state must be ON or OFF")?
            .to_uppercase();
        ensure!(s == "ON" || s == "OFF", "state must be ON or OFF");
        field(&mut b, 6, &[u8::from(s == "ON")]);
        values.insert("on".into(), json!(s == "ON"));
    }
    if let Some(v) = fields.get("brightness") {
        let level = integer(v, "brightness", 254)? as u8;
        field(&mut b, 8, &[level]);
        values.insert("level".into(), json!(level));
    }
    let xy = if let Some(v) = fields.get("color_temp") {
        let m = number(v, "color_temp", 40.0, 599.0)?;
        values.insert("color_temp".into(), json!(m));
        Some(mired_xy(m)?)
    } else if let Some(v) = fields.get("color") {
        let (x, y) = color_xy(v)?;
        values.insert("color".into(), json!({"x":x,"y":y}));
        Some((x, y))
    } else {
        None
    };
    if let Some((x, y)) = xy {
        let mut p = ((x * 65535.0).round() as u16).to_le_bytes().to_vec();
        p.extend(((y * 65535.0).round() as u16).to_le_bytes());
        field(&mut b, 0x0300, &p);
    }
    Ok((
        if enhanced { 0x40 } else { 0 },
        b,
        values,
        if enhanced {
            f64::from(ticks) / 10.0
        } else {
            time
        },
    ))
}
/// Require a Scenes response with matching identity; Default Response success does
/// not prove that a device saved an entry. Nonzero status never changes metadata.
pub fn response(body: &[u8], group: u16, id: Option<u8>, allow_missing: bool) -> Result<()> {
    ensure!(
        body.len() >= 3 + usize::from(id.is_some()),
        "truncated Scenes response"
    );
    ensure!(
        u16::from_le_bytes([body[1], body[2]]) == group && id.is_none_or(|id| body[3] == id),
        "Scenes response identity mismatch"
    );
    ensure!(
        body[0] == 0 || allow_missing && matches!(body[0], 0x85 | 0x8b),
        "device rejected scene (status 0x{:02x})",
        body[0]
    );
    Ok(())
}
pub fn membership(body: &[u8], group: u16) -> Result<Vec<u8>> {
    ensure!(
        body.len() >= 5 && body[0] == 0,
        "invalid or rejected scene membership response"
    );
    ensure!(
        u16::from_le_bytes([body[2], body[3]]) == group,
        "scene membership group mismatch"
    );
    ensure!(
        body.len() == 5 + body[4] as usize,
        "truncated scene membership list"
    );
    Ok(body[5..].to_vec())
}
pub fn view(
    body: &[u8],
    group: u16,
    id: u8,
    enhanced: bool,
) -> Result<(f64, String, Map<String, Json>)> {
    response(body, group, Some(id), false)?;
    ensure!(body.len() >= 7, "truncated View Scene response");
    let time =
        f64::from(u16::from_le_bytes([body[4], body[5]])) / if enhanced { 10.0 } else { 1.0 };
    let len = body[6] as usize;
    ensure!(len != 255 && body.len() >= 7 + len, "invalid scene name");
    let name = String::from_utf8(body[7..7 + len].to_vec())?;
    let mut rest = &body[7 + len..];
    let mut values = Map::new();
    while !rest.is_empty() {
        ensure!(rest.len() >= 3, "truncated scene extension header");
        let cluster = u16::from_le_bytes([rest[0], rest[1]]);
        let n = rest[2] as usize;
        rest = &rest[3..];
        ensure!(rest.len() >= n, "truncated scene extension");
        let p = &rest[..n];
        rest = &rest[n..];
        match cluster {
            6 if n >= 1 => {
                ensure!(p[0] <= 1, "invalid scene on/off value");
                values.insert("on".into(), json!(p[0] != 0));
            }
            8 if n >= 1 => {
                values.insert("level".into(), json!(p[0]));
            }
            0x0300 if n >= 4 => {
                let x = f64::from(u16::from_le_bytes([p[0], p[1]])) / 65535.0;
                let y = f64::from(u16::from_le_bytes([p[2], p[3]])) / 65535.0;
                values.insert("color".into(), json!({"x":x,"y":y}));
                if n >= 13 {
                    let ct = u16::from_le_bytes([p[11], p[12]]);
                    if ct != 0 && ct != 0xffff {
                        values.insert("color_temp".into(), json!(ct));
                    }
                }
            }
            6 | 8 | 0x0300 => bail!("truncated known scene extension"),
            _ => {}
        }
    }
    Ok((time, name, values))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_bad_commands_before_any_writes() {
        for (key, v) in [
            ("scene_store", json!(-1)),
            ("scene_store", json!(256)),
            ("scene_add", json!({"ID":1,"state":"TOGGLE"})),
            ("scene_add", json!({"ID":1,"brightness":1.5})),
            (
                "scene_add",
                json!({"ID":1,"color_temp":250,"color":{"x":0.3,"y":0.3}}),
            ),
            ("scene_add", json!({"ID":1,"transition":7000.5})),
            ("scene_add", json!({"ID":1,"foo":1})),
            ("scene_rename", json!({"ID":1})),
            ("scene_store", json!({"ID":1,"ep":0})),
        ] {
            assert!(Operation::parse(key, &v).is_err(), "{key} {v}");
        }
        assert!(
            Operation::parse("scene_store", &json!(0))
                .unwrap()
                .validate_scope(0)
                .is_err()
        );
        assert!(
            Operation::parse("scene_store", &json!(0))
                .unwrap()
                .validate_scope(3)
                .is_ok()
        );
    }
    #[test]
    fn wire_encoding_and_partial_attributes() {
        let (_, b, values, time) = add_payload(
            3,
            2,
            json!({"brightness":38,"transition":1}).as_object().unwrap(),
        )
        .unwrap();
        assert_eq!(b, vec![3, 0, 2, 1, 0, 0, 8, 0, 1, 38]);
        assert_eq!(values.len(), 1);
        assert_eq!(time, 1.0);
        let (cmd, b, _, time) = add_payload(
            3,
            2,
            json!({"state":"ON","transition":0.2}).as_object().unwrap(),
        )
        .unwrap();
        assert_eq!(cmd, 0x40);
        assert_eq!(&b[3..5], &[2, 0]);
        assert_eq!(time, 0.2);
    }
    #[test]
    fn ct_and_rgb_use_xy_extensions() {
        let (_, b, values, _) =
            add_payload(1, 1, json!({"color_temp":250}).as_object().unwrap()).unwrap();
        assert_eq!(&b[6..9], &[0, 3, 4]);
        assert_eq!(values["color_temp"], 250.0);
        let (x, y) = mired_xy(250.0).unwrap();
        assert!((x - 0.3805).abs() < 0.001 && (y - 0.3767).abs() < 0.001);
        for c in [json!("#FF0000"), json!({"hue":0,"saturation":100})] {
            let (x, y) = color_xy(&c).unwrap();
            assert!(x > 0.6 && y > 0.2);
        }
        assert!(color_xy(&json!("#zzzzzz")).is_err());
        assert!(color_xy(&json!({"x":0.8,"y":0.8})).is_err());
    }
    #[test]
    fn response_identity_status_and_bounds_are_checked() {
        assert!(response(&[0, 3, 0, 2], 3, Some(2), false).is_ok());
        assert!(response(&[0, 4, 0, 2], 3, Some(2), false).is_err());
        assert!(response(&[0x89, 3, 0, 2], 3, Some(2), false).is_err());
        assert!(response(&[0x8b, 3, 0, 2], 3, Some(2), true).is_ok());
        assert_eq!(membership(&[0, 12, 3, 0, 2, 1, 2], 3).unwrap(), vec![1, 2]);
        assert!(membership(&[0, 12, 3, 0, 2, 1], 3).is_err());
        let b = [0, 3, 0, 2, 2, 0, 0, 6, 0, 1, 1, 8, 0, 1, 38];
        let (time, _, v) = view(&b, 3, 2, true).unwrap();
        assert_eq!(time, 0.2);
        assert_eq!(v["on"], true);
        assert_eq!(v["level"], 38);
        assert!(view(&b[..b.len() - 1], 3, 2, true).is_err());
    }
    #[test]
    fn old_state_loads_and_scene_metadata_round_trips() {
        let mut state: crate::store::Persisted = serde_json::from_value(json!({})).unwrap();
        assert!(state.scenes.is_empty());
        state.scenes.push(StoredScene {
            ieee: "lamp".into(),
            ep: 11,
            group_id: 3,
            id: 2,
            name: "Night".into(),
            transition: 0.2,
            values: Map::new(),
        });
        let restored: crate::store::Persisted =
            serde_json::from_slice(&serde_json::to_vec(&state).unwrap()).unwrap();
        assert_eq!(restored.scenes[0].group_id, 3);
        assert_eq!(restored.scenes[0].name, "Night");
    }
}

/// State an automation may assume after a recall. An attribute is predictable for
/// a group only when every scene member sets it; omitted fields stay unchanged.
pub fn automation_descriptor(target: &str, entries: &[&StoredScene]) -> Json {
    let mut state = Map::new();
    if entries
        .iter()
        .all(|s| s.values.get("on").and_then(Json::as_bool).is_some())
    {
        state.insert(
            "state".into(),
            json!(if entries.iter().any(|s| s.values["on"] == true) {
                "ON"
            } else {
                "OFF"
            }),
        );
    }
    if entries
        .iter()
        .all(|s| s.values.get("level").and_then(Json::as_f64).is_some())
    {
        state.insert(
            "brightness".into(),
            json!(
                entries
                    .iter()
                    .filter_map(|s| s.values["level"].as_f64())
                    .fold(0.0, f64::max)
            ),
        );
    }
    if entries
        .iter()
        .all(|s| s.values.get("color_temp").and_then(Json::as_f64).is_some())
    {
        state.insert("color_temp".into(), entries[0].values["color_temp"].clone());
    }
    json!({"target":target,"id":entries[0].id,"state":state,"transition":entries.iter().map(|s|s.transition).fold(0.0,f64::max)})
}

#[cfg(test)]
mod automation_tests {
    use super::*;
    fn entry(ieee: &str, values: Json, transition: f64) -> StoredScene {
        StoredScene {
            ieee: ieee.into(),
            ep: 11,
            group_id: 3,
            id: 1,
            name: "Normal".into(),
            transition,
            values: values.as_object().unwrap().clone(),
        }
    }
    #[test]
    fn group_predictions_use_any_on_max_brightness_and_longest_transition() {
        let a = entry("a", json!({"on":false,"level":38,"color_temp":400}), 0.2);
        let b = entry("b", json!({"on":true,"level":80,"color_temp":400}), 1.0);
        let d = automation_descriptor("Right", &[&a, &b]);
        assert_eq!(
            d,
            json!({"target":"Right","id":1,"state":{"state":"ON","brightness":80.0,"color_temp":400},"transition":1.0})
        );
    }
    #[test]
    fn partial_scene_never_invents_unset_group_attributes() {
        let a = entry("a", json!({"color_temp":400}), 1.0);
        let b = entry("b", json!({"color_temp":400,"level":80}), 0.2);
        let d = automation_descriptor("Right", &[&a, &b]);
        assert_eq!(d["state"], json!({"color_temp":400}));
    }
}
