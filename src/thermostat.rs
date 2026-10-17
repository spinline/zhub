//! ZCL Thermostat weekly schedule (Set/Get Weekly Schedule), as Zigbee2MQTT's
//! `weeklySchedule` for the SONOFF TRVZB: per day up to a few transitions
//! (minutes after midnight → heating setpoint), the first at 00:00.
//!
//! The UI value is an object of days, each a list of `{time: "HH:MM", temperature}`.

use std::collections::BTreeMap;

use anyhow::{Result, bail};
use serde_json::{Value as Json, json};

pub const SET_WEEKLY_SCHEDULE: u8 = 0x01;
/// Server to client. The TRVZB refuses Get Weekly Schedule (status 0x81), so the hub
/// shows what it saved; a device that sends its schedule updates it this way.
pub const GET_WEEKLY_SCHEDULE_RSP: u8 = 0x00;

/// Bit n of the day-of-week bitmap
pub const DAYS: [&str; 7] = ["sunday", "monday", "tuesday", "wednesday", "thursday", "friday", "saturday"];
const MODE_HEAT: u8 = 0x01;
const MODE_COOL: u8 = 0x02;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Limits {
    pub min: f64,
    pub max: f64,
    pub step: f64,
    pub max_transitions: usize,
}

/// One day's transitions: (minutes after midnight, setpoint in hundredths)
pub type Day = Vec<(u16, i16)>;

fn day_to_json(day: &Day) -> Json {
    Json::Array(
        day.iter()
            .map(|&(minutes, setpoint)| {
                json!({
                    "time": format!("{:02}:{:02}", minutes / 60, minutes % 60),
                    "temperature": f64::from(setpoint) / 100.0,
                })
            })
            .collect(),
    )
}

fn parse_time(text: &str) -> Option<u16> {
    let (h, m) = text.split_once(':')?;
    let (h, m): (u16, u16) = (h.parse().ok()?, m.parse().ok()?);
    (h < 24 && m < 60 && text.len() == 5).then_some(h * 60 + m)
}

/// Validates a (partial) week from the UI; returns the days with their transitions
/// sorted by time
pub fn parse_week(value: &Json, limits: Limits) -> Result<BTreeMap<usize, Day>> {
    let Some(object) = value.as_object() else {
        bail!("expected the days of the week");
    };
    if object.is_empty() {
        bail!("no day given");
    }
    let mut week = BTreeMap::new();
    for (name, slots) in object {
        let Some(index) = DAYS.iter().position(|d| d == name) else {
            bail!("unknown day: {name}");
        };
        let Some(slots) = slots.as_array() else {
            bail!("{name}: expected a list of transitions");
        };
        if slots.is_empty() || slots.len() > limits.max_transitions {
            bail!("{name}: 1-{} transitions", limits.max_transitions);
        }
        let mut day = Day::new();
        for slot in slots {
            let time = slot.get("time").and_then(Json::as_str).and_then(parse_time);
            let temperature = slot.get("temperature").and_then(Json::as_f64).filter(|t| t.is_finite());
            let (Some(time), Some(temperature)) = (time, temperature) else {
                bail!("{name}: each transition needs a time (HH:MM) and a temperature");
            };
            let snapped = (temperature / limits.step).round() * limits.step;
            if snapped < limits.min - 1e-9 || snapped > limits.max + 1e-9 {
                bail!("{name}: temperatures must be {}-{} °C", limits.min, limits.max);
            }
            if day.iter().any(|&(t, _)| t == time) {
                bail!("{name}: two transitions at the same time");
            }
            day.push((time, (snapped * 100.0).round() as i16));
        }
        day.sort_by_key(|&(t, _)| t);
        if day[0].0 != 0 {
            bail!("{name}: the first transition must start at 00:00");
        }
        week.insert(index, day);
    }
    Ok(week)
}

/// The days of a parsed week as the UI value
pub fn week_to_json(week: &BTreeMap<usize, Day>) -> Json {
    Json::Object(week.iter().map(|(&i, day)| (DAYS[i].to_owned(), day_to_json(day))).collect())
}

/// Set Weekly Schedule for all days sharing one schedule (one command per distinct day)
pub fn set_payloads(week: &BTreeMap<usize, Day>) -> Vec<Vec<u8>> {
    let mut groups: Vec<(u8, &Day)> = Vec::new();
    for (&index, day) in week {
        match groups.iter_mut().find(|(_, d)| *d == day) {
            Some((days, _)) => *days |= 1 << index,
            None => groups.push((1 << index, day)),
        }
    }
    groups
        .into_iter()
        .map(|(days, day)| {
            let mut out = vec![day.len() as u8, days, MODE_HEAT];
            for &(minutes, setpoint) in day {
                out.extend_from_slice(&minutes.to_le_bytes());
                out.extend_from_slice(&setpoint.to_le_bytes());
            }
            out
        })
        .collect()
}

/// Get Weekly Schedule Response: the days it covers and their transitions
pub fn parse_get_rsp(payload: &[u8]) -> Option<(Vec<usize>, Day)> {
    let [count, days, mode, rest @ ..] = payload else {
        return None;
    };
    let entry = 2 + if mode & MODE_HEAT != 0 { 2 } else { 0 } + if mode & MODE_COOL != 0 { 2 } else { 0 };
    if mode & MODE_HEAT == 0 || rest.len() < usize::from(*count) * entry {
        return None;
    }
    let day: Day = rest
        .chunks_exact(entry)
        .take(usize::from(*count))
        .map(|c| (u16::from_le_bytes([c[0], c[1]]), i16::from_le_bytes([c[2], c[3]])))
        .collect();
    let covered = (0..7).filter(|i| days & (1 << i) != 0).collect();
    Some((covered, day))
}

/// Puts days into the state's schedule object, keeping the other days
pub fn merge(state: Option<&Json>, days: &[usize], day: &Day) -> Json {
    let mut object = state.and_then(Json::as_object).cloned().unwrap_or_default();
    for &i in days {
        object.insert(DAYS[i].to_owned(), day_to_json(day));
    }
    Json::Object(object)
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIMITS: Limits = Limits {
        min: 4.0,
        max: 35.0,
        step: 0.5,
        max_transitions: 6,
    };

    #[test]
    fn days_with_the_same_schedule_share_a_command() {
        let value = json!({
            "monday": [{"time": "06:00", "temperature": 21}, {"time": "00:00", "temperature": 17}],
            "tuesday": [{"time": "00:00", "temperature": 17}, {"time": "06:00", "temperature": 21}],
            "sunday": [{"time": "00:00", "temperature": 19.3}],
        });
        let week = parse_week(&value, LIMITS).unwrap();
        // Sorted by time, snapped to the step
        assert_eq!(week[&1], vec![(0, 1700), (360, 2100)]);
        assert_eq!(week[&0], vec![(0, 1950)]);
        let payloads = set_payloads(&week);
        assert_eq!(payloads.len(), 2);
        assert_eq!(payloads[0], [1, 0b0000_0001, 0x01, 0x00, 0x00, 0x9E, 0x07]);
        assert_eq!(payloads[1], [2, 0b0000_0110, 0x01, 0x00, 0x00, 0xA4, 0x06, 0x68, 0x01, 0x34, 0x08]);
    }

    #[test]
    fn rejects_invalid_days() {
        let bad = |v: Json| parse_week(&v, LIMITS).is_err();
        assert!(bad(json!({"monday": [{"time": "06:00", "temperature": 21}]})));
        assert!(bad(json!({"monday": [{"time": "00:00", "temperature": 40}]})));
        assert!(bad(json!({"funday": [{"time": "00:00", "temperature": 20}]})));
        assert!(bad(json!({"monday": [{"time": "00:00", "temperature": 20}, {"time": "00:00", "temperature": 21}]})));
        assert!(bad(json!({"monday": [{"time": "24:00", "temperature": 20}]})));
        assert!(bad(json!({"monday": vec![json!({"time": "00:00", "temperature": 20}); 7]})));
        assert!(bad(json!({})));
    }

    #[test]
    fn response_round_trips_into_the_state() {
        // 3 transitions for Saturday and Sunday, heat only
        let rsp = [3, 0b0100_0001, 0x01, 0x00, 0x00, 0xA4, 0x06, 0x68, 0x01, 0x34, 0x08, 0x28, 0x05, 0xA4, 0x06];
        let (days, day) = parse_get_rsp(&rsp).unwrap();
        assert_eq!(days, [0, 6]);
        assert_eq!(day, vec![(0, 1700), (360, 2100), (1320, 1700)]);
        let state = merge(Some(&json!({"monday": []})), &days, &day);
        assert_eq!(state["sunday"][1], json!({"time": "06:00", "temperature": 21.0}));
        assert_eq!(state["saturday"][2]["time"], "22:00");
        assert!(state.get("monday").is_some());
        assert!(parse_get_rsp(&[2, 1, 1, 0, 0]).is_none());
    }
}
