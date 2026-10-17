//! Zigbee2MQTT's light commands, so MQTT tools made for Zigbee2MQTT drive the hub's lights
//! the same way. Ported from zigbee-herdsman-converters' `on_off`,
//! `light_onoff_brightness`, `light_colortemp`, `light_brightness_step`,
//! `light_brightness_move`, `light_colortemp_step` and `light_colortemp_move`, and the key
//! order of Zigbee2MQTT's publish extension.
//!
//! `plan` is pure: it turns one message into ZCL commands (and reads) and the state the
//! converters report, so the behavior is fixed by tests. Where Zigbee2MQTT reads the
//! light's OnLevel before restoring a level, the plan takes OnLevel as "previous", the
//! ZCL default and what Hue lights have.

use anyhow::{Result, bail};
use serde_json::{Map, Value as Json, json};

use crate::zcl::cluster;

/// Message keys these converters take (the rest goes to the hub's features)
pub const KEYS: &[&str] = &[
    "state",
    "brightness",
    "brightness_percent",
    "on_time",
    "off_wait_time",
    "transition",
    "color_temp",
    "color_temp_percent",
    "brightness_step",
    "brightness_step_onoff",
    "brightness_move",
    "brightness_move_onoff",
    "color_temp_step",
    "color_temp_move",
    "colortemp_move",
    "rate",
];


/// What the converters know of the light or group (Zigbee2MQTT's `meta.state` and the
/// definition's metadata)
#[derive(Debug, Clone, Default)]
pub struct Target {
    pub on: Option<bool>,
    /// 0-254
    pub brightness: Option<f64>,
    pub has_level: bool,
    pub has_color_temp: bool,
    pub color_temp_min: Option<f64>,
    pub color_temp_max: Option<f64>,
    /// Hue lights: level 1 is off, so 1 becomes 2
    pub turns_off_at_brightness1: bool,
}

/// Kept per light or group between commands (Zigbee2MQTT's `globalStore`)
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Memory {
    pub brightness: Option<f64>,
    pub turned_off_with_transition: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Step {
    Command { cluster: u16, command: u8, payload: Vec<u8> },
    /// Read attributes after a pause (milliseconds); the answers update the state
    Read { cluster: u16, attributes: Vec<u16>, after_ms: u64 },
}

/// One converter's part of a message
#[derive(Debug, Clone, PartialEq)]
pub struct Part {
    pub steps: Vec<Step>,
    /// In the hub's state keys: on, level, color_temp
    pub state: Vec<(&'static str, Json)>,
}

#[derive(Debug, Default)]
pub struct Plan {
    /// Converter results in Zigbee2MQTT's order; each runs even if an earlier one fails
    pub parts: Vec<Result<Part>>,
    pub memory: Memory,
    /// Keys for the hub's other features, and whether they go before the light keys
    pub rest: Map<String, Json>,
    pub rest_first: bool,
}

const ON_OFF_OFF: u8 = 0x00;
const ON_OFF_ON: u8 = 0x01;
const ON_OFF_TOGGLE: u8 = 0x02;
const ON_WITH_TIMED_OFF: u8 = 0x42;
const LEVEL_MOVE_TO_LEVEL: u8 = 0x00;
const LEVEL_MOVE: u8 = 0x01;
const LEVEL_STEP: u8 = 0x02;
const LEVEL_STOP: u8 = 0x03;
const LEVEL_MOVE_TO_LEVEL_WITH_ON_OFF: u8 = 0x04;
const LEVEL_MOVE_WITH_ON_OFF: u8 = 0x05;
const LEVEL_STEP_WITH_ON_OFF: u8 = 0x06;
const COLOR_MOVE_TO_COLOR_TEMP: u8 = 0x0A;
const COLOR_MOVE_COLOR_TEMP: u8 = 0x4B;
const COLOR_STEP_COLOR_TEMP: u8 = 0x4C;
/// optionsMask, optionsOverride (Zigbee2MQTT sends them unless a device omits them)
const OPTIONS: [u8; 2] = [0, 0];

fn command(cluster: u16, command: u8, payload: Vec<u8>) -> Step {
    Step::Command { cluster, command, payload }
}

fn number(value: &Json, key: &str) -> Result<f64> {
    let n = match value {
        Json::Number(n) => n.as_f64(),
        Json::String(s) => s.trim().parse().ok(),
        _ => None,
    };
    match n {
        Some(n) if n.is_finite() => Ok(n),
        _ => bail!("{key}: expected a number, got {value}"),
    }
}

/// `utils.mapNumberRange` with precision 0
fn map_range(value: f64, from_low: f64, from_high: f64, to_low: f64, to_high: f64) -> f64 {
    (to_low + (value - from_low) * (to_high - to_low) / (from_high - from_low)).round()
}

fn u16_le(v: f64) -> [u8; 2] {
    (v.round().clamp(0.0, 65_535.0) as u16).to_le_bytes()
}

/// `utils.getTransition`: tenths of a second, and whether the message set it
fn transition(message: &Map<String, Json>) -> Result<(f64, bool)> {
    match message.get("transition") {
        Some(v) if !v.is_null() => Ok((number(v, "transition")? * 10.0, true)),
        _ => Ok((0.0, false)),
    }
}

/// Plans a message the way Zigbee2MQTT's converters would run it
pub fn plan(message: &Map<String, Json>, target: &Target, memory: &Memory) -> Plan {
    let mut plan = Plan {
        memory: memory.clone(),
        ..Plan::default()
    };
    // Zigbee2MQTT sorts with a one-argument comparator; on V8 that puts state and
    // brightness first unless the message turns the light off, where they go last
    let off = message.get("state").and_then(Json::as_str).is_some_and(|s| s.eq_ignore_ascii_case("off"));
    plan.rest_first = off;

    let has = |keys: &[&str]| keys.iter().any(|k| message.contains_key(*k));
    let on_off_part = has(&["state", "brightness", "brightness_percent", "on_time", "off_wait_time"]);
    let mut others: Vec<Result<Part>> = Vec::new();
    // A light without the cluster has no converter for these keys
    let level = |part: fn(&Map<String, Json>, &Target) -> Result<Part>| {
        if target.has_level { part(message, target) } else { Err(anyhow::anyhow!("No converter available for brightness steps or moves")) }
    };
    let color = |part: fn(&Map<String, Json>, &Target) -> Result<Part>| {
        if target.has_color_temp { part(message, target) } else { Err(anyhow::anyhow!("No converter available for color temperature")) }
    };
    if has(&["color_temp", "color_temp_percent"]) {
        others.push(color(color_temp));
    }
    if has(&["brightness_step", "brightness_step_onoff"]) {
        others.push(level(brightness_step));
    }
    if has(&["brightness_move", "brightness_move_onoff"]) {
        others.push(level(brightness_move));
    }
    if message.contains_key("color_temp_step") {
        others.push(color(color_temp_step));
    }
    if has(&["color_temp_move", "colortemp_move"]) {
        others.push(color(color_temp_move));
    }
    let main = on_off_part.then(|| {
        if target.has_level {
            onoff_brightness(message, target, &mut plan.memory)
        } else {
            on_off(message, target)
        }
    });
    if off {
        plan.parts.extend(others);
        plan.parts.extend(main);
    } else {
        plan.parts.extend(main);
        plan.parts.extend(others);
    }
    plan.rest = message
        .iter()
        .filter(|(k, _)| !KEYS.contains(&k.as_str()))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    plan
}

/// `on_off`: plain On, Off, Toggle, or On with Timed Off
fn on_off(message: &Map<String, Json>, target: &Target) -> Result<Part> {
    let state = message.get("state").and_then(Json::as_str).map(str::to_ascii_lowercase);
    let Some(state) = state.filter(|s| ["on", "off", "toggle"].contains(&s.as_str())) else {
        bail!("state: expected ON, OFF or TOGGLE, got {}", message.get("state").unwrap_or(&Json::Null));
    };
    if state == "on" && (message.get("on_time").is_some_and(|v| !v.is_null()) || message.get("off_wait_time").is_some_and(|v| !v.is_null())) {
        let time = |key: &str| -> Result<f64> {
            let Some(v) = message.get(key).filter(|v| !v.is_null()) else {
                return Ok(0.0);
            };
            let Some(n) = v.as_f64() else {
                bail!("The {key} value must be a number!");
            };
            let tenths = (n * 10.0).round();
            if !(0.0..=65_534.0).contains(&tenths) {
                bail!("The {key} value must be between 0 and 6553.4, got '{n}'");
            }
            Ok(tenths)
        };
        let (on_time, off_wait) = (time("on_time")?, time("off_wait_time")?);
        let mut payload = vec![0];
        payload.extend(u16_le(on_time));
        payload.extend(u16_le(off_wait));
        return Ok(Part {
            steps: vec![command(cluster::ON_OFF, ON_WITH_TIMED_OFF, payload)],
            state: Vec::new(),
        });
    }
    let (cmd, new_state) = match state.as_str() {
        "on" => (ON_OFF_ON, Some(true)),
        "off" => (ON_OFF_OFF, Some(false)),
        _ => (ON_OFF_TOGGLE, target.on.map(|on| !on)),
    };
    Ok(Part {
        steps: vec![command(cluster::ON_OFF, cmd, vec![])],
        state: new_state.map(|on| vec![("on", json!(on))]).unwrap_or_default(),
    })
}

/// `light_onoff_brightness`
fn onoff_brightness(message: &Map<String, Json>, target: &Target, memory: &mut Memory) -> Result<Part> {
    let (transition_time, transition_specified) = transition(message)?;
    // JSON null asks to keep the state: Move to Level without On/Off
    let state_null = message.get("state").is_some_and(Json::is_null);
    let mut state: Option<String> = match message.get("state") {
        Some(Json::String(s)) => Some(s.to_ascii_lowercase()),
        Some(Json::Null) | None => None,
        Some(other) => bail!("State value of message: '{other}' invalid, must be 'ON', 'OFF' or 'TOGGLE'"),
    };
    let mut brightness = if let Some(v) = message.get("brightness").filter(|v| !v.is_null()) {
        Some(number(v, "brightness")?)
    } else if let Some(v) = message.get("brightness_percent").filter(|v| !v.is_null()) {
        Some(map_range(number(v, "brightness_percent")?, 0.0, 100.0, 0.0, 255.0))
    } else {
        None
    };
    if brightness == Some(255.0) {
        brightness = Some(254.0);
    }
    if let Some(b) = brightness
        && !(0.0..=254.0).contains(&b)
    {
        bail!("Brightness value of message invalid, must be a number >= 0 and =< 254");
    }
    if let Some(s) = &state
        && !["on", "off", "toggle"].contains(&s.as_str())
    {
        bail!("State value of message: '{s}' invalid, must be 'ON', 'OFF' or 'TOGGLE'");
    }
    if state.is_none() && !state_null && brightness.is_none() {
        bail!("At least one of \"brightness\" or \"state\" must have a value");
    }
    // Infer the state from the brightness when unset
    if state.is_none() && !state_null {
        state = Some(if brightness == Some(0.0) { "off" } else { "on" }.to_owned());
    }
    let mut publish_brightness = brightness.is_some();
    let target_state = match state.as_deref() {
        Some("toggle") => Some(if target.on == Some(true) { "off" } else { "on" }),
        Some("on") => Some("on"),
        Some("off") => Some("off"),
        _ => None,
    };

    if target_state == Some("off") {
        // 'Off' with a transition is Move to Level with On/Off to 0
        brightness = (transition_specified || brightness == Some(0.0)).then_some(0.0);
        if let Some(current) = target.brightness
            && target.on == Some(true)
        {
            // The level gets clobbered: remember it for the next 'On'
            memory.brightness = Some(current);
            memory.turned_off_with_transition = brightness.is_some();
        }
    } else if target_state == Some("on")
        && brightness.is_none()
        && (transition_specified || memory.turned_off_with_transition)
    {
        // Restore the level from before a transition to off (OnLevel "previous")
        brightness = Some(memory.brightness.or(target.brightness).unwrap_or(254.0));
        publish_brightness = true;
    }

    let Some(mut level) = brightness else {
        // Plain On/Off/Toggle
        let mut part = on_off(message, target)?;
        if part.state.iter().any(|(k, v)| *k == "on" && v == &json!(true)) && target.brightness == Some(0.0) {
            part.state.push(("level", json!(1.0)));
        }
        return Ok(part);
    };
    if level == 0.0 && (target_state == Some("on") || state_null) {
        level = 1.0;
    }
    if level == 1.0 && target.turns_off_at_brightness1 {
        level = 2.0;
    }
    if target_state != Some("off") {
        memory.brightness = Some(level);
        memory.turned_off_with_transition = false;
    }
    let mut payload = vec![level.round().clamp(0.0, 254.0) as u8];
    payload.extend(u16_le(transition_time));
    payload.extend(OPTIONS);
    let cmd = if state_null { LEVEL_MOVE_TO_LEVEL } else { LEVEL_MOVE_TO_LEVEL_WITH_ON_OFF };
    let mut new_state = Vec::new();
    if publish_brightness {
        new_state.push(("level", json!(level)));
    }
    if !state_null {
        new_state.push(("on", json!(level != 0.0)));
    }
    Ok(Part {
        steps: vec![command(cluster::LEVEL, cmd, payload)],
        state: new_state,
    })
}

/// `light_colortemp`: mireds, percent of the range, or a preset name
fn color_temp(message: &Map<String, Json>, target: &Target) -> Result<Part> {
    let (min, max) = (target.color_temp_min, target.color_temp_max);
    let mut value = if let Some(v) = message.get("color_temp_percent") {
        json!(map_range(number(v, "color_temp_percent")?, 0.0, 100.0, min.unwrap_or(154.0), max.unwrap_or(500.0)))
    } else {
        message.get("color_temp").cloned().unwrap_or(Json::Null)
    };
    if let Some(name) = value.as_str() {
        let preset = match name {
            "warmest" => max,
            "warm" => Some(454.0),
            "neutral" => Some(370.0),
            "cool" => Some(250.0),
            "coolest" => min,
            _ => None,
        };
        if let Some(p) = preset {
            value = json!(p);
        }
    }
    let mut mired = number(&value, "color_temp")?;
    if let Some(m) = min
        && mired < m
    {
        mired = m;
    }
    if let Some(m) = max
        && mired > m
    {
        mired = m;
    }
    let (transition_time, _) = transition(message)?;
    let mut payload = u16_le(mired).to_vec();
    payload.extend(u16_le(transition_time));
    payload.extend(OPTIONS);
    Ok(Part {
        steps: vec![command(cluster::COLOR, COLOR_MOVE_TO_COLOR_TEMP, payload)],
        state: vec![("color_temp", json!(mired.round()))],
    })
}

/// `light_brightness_step`
fn brightness_step(message: &Map<String, Json>, target: &Target) -> Result<Part> {
    let on_off = message.contains_key("brightness_step_onoff");
    let key = if on_off { "brightness_step_onoff" } else { "brightness_step" };
    let value = number(&message[key], key)?;
    let size = value.abs();
    if size > 255.0 {
        bail!("{key}: the step must be within -255 to 255");
    }
    let (transition_time, _) = transition(message)?;
    let mut payload = vec![u8::from(value <= 0.0), size as u8];
    payload.extend(u16_le(transition_time));
    payload.extend(OPTIONS);
    let mut steps = vec![command(cluster::LEVEL, if on_off { LEVEL_STEP_WITH_ON_OFF } else { LEVEL_STEP }, payload)];
    let mut state = Vec::new();
    if let Some(current) = target.brightness {
        let is_on = target.on == Some(true);
        if value == 0.0 {
            // The new level is unknown: read it
            steps.push(Step::Read {
                cluster: cluster::LEVEL,
                attributes: vec![0x0000],
                after_ms: 0,
            });
        } else {
            let mut b = if on_off || is_on { current + value } else { current };
            b = b.min(254.0);
            b = b.max(if on_off || target.on == Some(false) { 0.0 } else { 1.0 });
            if target.turns_off_at_brightness1 {
                if on_off && value < 0.0 && b == 1.0 {
                    b = 0.0;
                } else if on_off && value > 0.0 && current == 0.0 {
                    b += 1.0;
                }
            }
            state = vec![("level", json!(b)), ("on", json!(b != 0.0))];
        }
    }
    Ok(Part { steps, state })
}

/// `light_brightness_move`: a rate, or "stop"/0 (then the state is read back)
fn brightness_move(message: &Map<String, Json>, _target: &Target) -> Result<Part> {
    let on_off = message.contains_key("brightness_move_onoff");
    let key = if on_off { "brightness_move_onoff" } else { "brightness_move" };
    let raw = &message[key];
    if raw == &json!("stop") || raw.as_f64() == Some(0.0) {
        return Ok(Part {
            steps: vec![
                command(cluster::LEVEL, LEVEL_STOP, OPTIONS.to_vec()),
                Step::Read {
                    cluster: cluster::ON_OFF,
                    attributes: vec![0x0000],
                    after_ms: 500,
                },
                Step::Read {
                    cluster: cluster::LEVEL,
                    attributes: vec![0x0000],
                    after_ms: 0,
                },
            ],
            state: Vec::new(),
        });
    }
    let value = number(raw, key)?;
    let mut payload = vec![u8::from(value <= 0.0), value.abs().min(255.0) as u8];
    payload.extend(OPTIONS);
    Ok(Part {
        steps: vec![command(cluster::LEVEL, if on_off { LEVEL_MOVE_WITH_ON_OFF } else { LEVEL_MOVE }, payload)],
        state: Vec::new(),
    })
}

/// `light_colortemp_step`: then reads the color temperature
fn color_temp_step(message: &Map<String, Json>, _target: &Target) -> Result<Part> {
    let value = number(&message["color_temp_step"], "color_temp_step")?;
    let (transition_time, _) = transition(message)?;
    let mut payload = vec![if value > 0.0 { 1 } else { 3 }];
    payload.extend(u16_le(value.abs()));
    payload.extend(u16_le(transition_time));
    payload.extend(u16_le(0.0));
    payload.extend(u16_le(600.0));
    payload.extend(OPTIONS);
    Ok(Part {
        steps: vec![
            command(cluster::COLOR, COLOR_STEP_COLOR_TEMP, payload),
            Step::Read {
                cluster: cluster::COLOR,
                attributes: vec![0x0007],
                after_ms: 100 + (transition_time * 100.0) as u64,
            },
        ],
        state: Vec::new(),
    })
}

/// `light_colortemp_move`: a signed rate, "up"/"down"/"stop", or {rate, minimum, maximum}
fn color_temp_move(message: &Map<String, Json>, _target: &Target) -> Result<Part> {
    let key = if message.contains_key("color_temp_move") { "color_temp_move" } else { "colortemp_move" };
    let value = &message[key];
    let (mut minimum, mut maximum) = (0.0, 600.0);
    let signed = |rate: f64| if rate == 0.0 { (1.0, 0) } else { (rate.abs(), if rate > 0.0 { 1 } else { 3 }) };
    let (rate, mode) = match value {
        Json::String(s) => {
            let given = message.get("rate").map(|r| number(r, "rate")).transpose()?.unwrap_or(55.0);
            let rm = match s.to_ascii_lowercase().as_str() {
                "stop" | "release" | "0" => (1.0, 0),
                "up" | "1" => (given, 1),
                "down" => (given, 3),
                _ => bail!("{key}: invalid string value \"{s}\". Expected \"stop\", \"release\", \"0\", \"up\", \"1\", or \"down\""),
            };
            (minimum, maximum) = (153.0, 370.0);
            rm
        }
        Json::Number(_) => signed(number(value, key)?),
        Json::Object(o) => {
            let Some(rate) = o.get("rate").filter(|r| !r.is_null()) else {
                bail!("{key}: object must contain 'rate' property");
            };
            let rm = signed(number(rate, &format!("{key}.rate"))?);
            if let Some(m) = o.get("minimum").filter(|m| !m.is_null()) {
                minimum = number(m, &format!("{key}.minimum"))?;
            }
            if let Some(m) = o.get("maximum").filter(|m| !m.is_null()) {
                maximum = number(m, &format!("{key}.maximum"))?;
            }
            if minimum >= maximum {
                bail!("{key}: minimum ({minimum}) must be less than maximum ({maximum})");
            }
            rm
        }
        _ => bail!("{key}: invalid value type. Expected number, string, or object with rate property"),
    };
    let mut payload = vec![mode];
    payload.extend(u16_le(rate));
    payload.extend(u16_le(minimum));
    payload.extend(u16_le(maximum));
    payload.extend(OPTIONS);
    Ok(Part {
        steps: vec![command(cluster::COLOR, COLOR_MOVE_COLOR_TEMP, payload)],
        state: Vec::new(),
    })
}

/// `convertGet` of the light converters: which attributes `/get` reads for a key
pub fn get_reads(key: &str) -> Option<(u16, &'static [u16])> {
    match key {
        "state" => Some((cluster::ON_OFF, &[0x0000])),
        "brightness" | "brightness_percent" => Some((cluster::LEVEL, &[0x0000])),
        "color_temp" | "color_temp_percent" => Some((cluster::COLOR, &[0x0008, 0x0007])),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hue(on: bool, brightness: f64) -> Target {
        Target {
            on: Some(on),
            brightness: Some(brightness),
            has_level: true,
            has_color_temp: true,
            color_temp_min: Some(153.0),
            color_temp_max: Some(454.0),
            turns_off_at_brightness1: true,
        }
    }

    fn msg(v: Json) -> Map<String, Json> {
        v.as_object().unwrap().clone()
    }

    fn parts(m: Json, t: &Target, memory: &Memory) -> (Vec<Part>, Memory) {
        let p = plan(&msg(m), t, memory);
        (p.parts.into_iter().map(Result::unwrap).collect(), p.memory)
    }

    fn cmd(p: &Part) -> (u16, u8, Vec<u8>) {
        match &p.steps[0] {
            Step::Command { cluster, command, payload } => (*cluster, *command, payload.clone()),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn plain_states_use_on_off_commands() {
        let (p, _) = parts(json!({"state": "ON"}), &hue(false, 200.0), &Memory::default());
        assert_eq!(cmd(&p[0]), (cluster::ON_OFF, ON_OFF_ON, vec![]));
        assert_eq!(p[0].state, [("on", json!(true))]);
        let (p, _) = parts(json!({"state": "off"}), &hue(true, 200.0), &Memory::default());
        assert_eq!(cmd(&p[0]), (cluster::ON_OFF, ON_OFF_OFF, vec![]));
        let (p, _) = parts(json!({"state": "TOGGLE"}), &hue(true, 200.0), &Memory::default());
        assert_eq!(cmd(&p[0]), (cluster::ON_OFF, ON_OFF_TOGGLE, vec![]));
        assert_eq!(p[0].state, [("on", json!(false))]);
    }

    #[test]
    fn brightness_moves_to_level_with_on_off() {
        let (p, memory) = parts(json!({"state": "ON", "brightness": 204}), &hue(false, 100.0), &Memory::default());
        assert_eq!(cmd(&p[0]), (cluster::LEVEL, LEVEL_MOVE_TO_LEVEL_WITH_ON_OFF, vec![204, 0, 0, 0, 0]));
        assert_eq!(p[0].state, [("level", json!(204.0)), ("on", json!(true))]);
        assert_eq!(memory.brightness, Some(204.0));
        // 0 alone turns off; 0 or 1 with ON is 2 on a Hue light; 255 is 254
        let (p, _) = parts(json!({"brightness": 0}), &hue(true, 100.0), &Memory::default());
        assert_eq!(cmd(&p[0]).2[0], 0);
        assert_eq!(p[0].state, [("level", json!(0.0)), ("on", json!(false))]);
        let (p, _) = parts(json!({"state": "ON", "brightness": 0}), &hue(true, 100.0), &Memory::default());
        assert_eq!(cmd(&p[0]).2[0], 2);
        let (p, _) = parts(json!({"brightness": 255}), &hue(true, 100.0), &Memory::default());
        assert_eq!(cmd(&p[0]).2[0], 254);
        assert!(plan(&msg(json!({"brightness": 300})), &hue(true, 1.0), &Memory::default()).parts[0].is_err());
        // brightness_percent maps 0-100 to 0-255
        let (p, _) = parts(json!({"brightness_percent": 50}), &hue(true, 100.0), &Memory::default());
        assert_eq!(cmd(&p[0]).2[0], 128);
        // null state keeps the state: Move to Level
        let (p, _) = parts(json!({"state": null, "brightness": 50}), &hue(false, 100.0), &Memory::default());
        assert_eq!(cmd(&p[0]).1, LEVEL_MOVE_TO_LEVEL);
        assert_eq!(p[0].state, [("level", json!(50.0))]);
    }

    #[test]
    fn off_with_transition_fades_and_on_restores_the_level() {
        let (p, memory) = parts(json!({"state": "OFF", "transition": 2}), &hue(true, 180.0), &Memory::default());
        assert_eq!(cmd(&p[0]), (cluster::LEVEL, LEVEL_MOVE_TO_LEVEL_WITH_ON_OFF, vec![0, 20, 0, 0, 0]));
        assert_eq!(memory, Memory { brightness: Some(180.0), turned_off_with_transition: true });
        // A plain ON afterwards restores 180 (the level was clobbered to the minimum)
        let (p, memory) = parts(json!({"state": "ON"}), &hue(false, 2.0), &memory);
        assert_eq!(cmd(&p[0]), (cluster::LEVEL, LEVEL_MOVE_TO_LEVEL_WITH_ON_OFF, vec![180, 0, 0, 0, 0]));
        assert_eq!(p[0].state, [("level", json!(180.0)), ("on", json!(true))]);
        assert!(!memory.turned_off_with_transition);
        // Without a transition: plain Off, the level is remembered but not clobbered
        let (p, memory) = parts(json!({"state": "OFF"}), &hue(true, 120.0), &Memory::default());
        assert_eq!(cmd(&p[0]), (cluster::ON_OFF, ON_OFF_OFF, vec![]));
        assert_eq!(memory, Memory { brightness: Some(120.0), turned_off_with_transition: false });
        let (p, _) = parts(json!({"state": "ON"}), &hue(false, 120.0), &memory);
        assert_eq!(cmd(&p[0]), (cluster::ON_OFF, ON_OFF_ON, vec![]));
        // ON with a transition fades in to the level
        let (p, _) = parts(json!({"state": "ON", "transition": 1.5}), &hue(false, 90.0), &Memory::default());
        assert_eq!(cmd(&p[0]), (cluster::LEVEL, LEVEL_MOVE_TO_LEVEL_WITH_ON_OFF, vec![90, 15, 0, 0, 0]));
    }

    #[test]
    fn keys_run_in_zigbee2mqtts_order() {
        // ON: state and brightness first, then the color temperature
        let p = plan(&msg(json!({"color_temp": 250, "state": "ON", "brightness": 204})), &hue(false, 100.0), &Memory::default());
        let clusters: Vec<u16> = p.parts.iter().map(|r| cmd(r.as_ref().unwrap()).0).collect();
        assert_eq!(clusters, [cluster::LEVEL, cluster::COLOR]);
        assert!(!p.rest_first);
        // OFF: the color temperature first, then off
        let p = plan(&msg(json!({"state": "OFF", "color_temp": 300})), &hue(true, 100.0), &Memory::default());
        let clusters: Vec<u16> = p.parts.iter().map(|r| cmd(r.as_ref().unwrap()).0).collect();
        assert_eq!(clusters, [cluster::COLOR, cluster::ON_OFF]);
        assert!(p.rest_first);
        // Other keys are left to the hub
        let p = plan(&msg(json!({"state": "ON", "power_on_behavior": "on"})), &hue(false, 100.0), &Memory::default());
        assert_eq!(p.rest, msg(json!({"power_on_behavior": "on"})));
    }

    #[test]
    fn color_temperature_clamps_and_takes_presets() {
        let t = Target { color_temp_min: Some(222.0), ..hue(true, 100.0) };
        let (p, _) = parts(json!({"color_temp": 160, "transition": 1}), &t, &Memory::default());
        assert_eq!(cmd(&p[0]), (cluster::COLOR, COLOR_MOVE_TO_COLOR_TEMP, vec![222, 0, 10, 0, 0, 0]));
        let (p, _) = parts(json!({"color_temp": "warm"}), &t, &Memory::default());
        assert_eq!(p[0].state, [("color_temp", json!(454.0))]);
        let (p, _) = parts(json!({"color_temp_percent": 0}), &t, &Memory::default());
        assert_eq!(p[0].state, [("color_temp", json!(222.0))]);
    }

    #[test]
    fn steps_and_moves() {
        let (p, _) = parts(json!({"brightness_step": -50}), &hue(true, 100.0), &Memory::default());
        assert_eq!(cmd(&p[0]), (cluster::LEVEL, LEVEL_STEP, vec![1, 50, 0, 0, 0, 0]));
        assert_eq!(p[0].state, [("level", json!(50.0)), ("on", json!(true))]);
        // With on/off, a Hue light stepping down to 1 is off
        let (p, _) = parts(json!({"brightness_step_onoff": -99}), &hue(true, 100.0), &Memory::default());
        assert_eq!(p[0].state, [("level", json!(0.0)), ("on", json!(false))]);
        let (p, _) = parts(json!({"brightness_move": "stop"}), &hue(true, 100.0), &Memory::default());
        assert_eq!(cmd(&p[0]), (cluster::LEVEL, LEVEL_STOP, vec![0, 0]));
        assert_eq!(p[0].steps.len(), 3);
        let (p, _) = parts(json!({"brightness_move_onoff": -40}), &hue(true, 100.0), &Memory::default());
        assert_eq!(cmd(&p[0]), (cluster::LEVEL, LEVEL_MOVE_WITH_ON_OFF, vec![1, 40, 0, 0]));
        let (p, _) = parts(json!({"color_temp_step": 20}), &hue(true, 100.0), &Memory::default());
        assert_eq!(cmd(&p[0]), (cluster::COLOR, COLOR_STEP_COLOR_TEMP, vec![1, 20, 0, 0, 0, 0, 0, 0x58, 0x02, 0, 0]));
        let (p, _) = parts(json!({"color_temp_move": "up"}), &hue(true, 100.0), &Memory::default());
        assert_eq!(cmd(&p[0]), (cluster::COLOR, COLOR_MOVE_COLOR_TEMP, vec![1, 55, 0, 153, 0, 0x72, 0x01, 0, 0]));
    }

    #[test]
    fn on_with_timed_off() {
        let t = Target { has_level: false, ..hue(false, 0.0) };
        let (p, _) = parts(json!({"state": "ON", "on_time": 30}), &t, &Memory::default());
        assert_eq!(cmd(&p[0]), (cluster::ON_OFF, ON_WITH_TIMED_OFF, vec![0, 0x2C, 0x01, 0, 0]));
        // A plug has no level or color converters
        let t = Target { has_level: false, has_color_temp: false, ..hue(true, 0.0) };
        let p = plan(&msg(json!({"brightness_step": 10, "color_temp": 300})), &t, &Memory::default());
        assert!(p.parts.iter().all(Result::is_err));
    }
}
