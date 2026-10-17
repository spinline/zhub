//! Philips Hue devices (zigbee-herdsman-converters `src/devices/philips.ts`,
//! `src/lib/philips.ts`).

use anyhow::{Result, bail};

use super::common::{self, level_config};
use super::{
    Action, Card, Command, CommandFrame, Conv, Definition, Feature, Fingerprint, Kind, Place, Preset, Source, TimeBase,
    Write,
};
use crate::zcl::{cluster, types};

/// Signify's manufacturer-specific cluster carrying Hue effects
const HUE_EFFECT_CLUSTER: u16 = 0xFC03;
/// Signify's cluster the Hue switches send their button notifications on
const HUE_NOTIFICATION_CLUSTER: u16 = 0xFC00;
const SIGNIFY: u16 = 0x100B;

/// ZCL Trigger Effect identifiers
const ZCL_EFFECTS: &[(&str, u8)] = &[
    ("blink", 0x00),
    ("breathe", 0x01),
    ("okay", 0x02),
    ("channel_change", 0x0B),
    ("finish_effect", 0xFE),
    ("stop_effect", 0xFF),
];

/// Hue effect types (`hueEffects` in Zigbee2MQTT); those of white ambiance lights
const HUE_EFFECTS: &[(&str, u8)] = &[
    ("candle", 0x01),
    ("sunrise", 0x09),
    ("sparkle", 0x0A),
    ("opal", 0x0B),
    ("glisten", 0x0C),
    ("sunset", 0x0D),
    ("underwater", 0x0E),
    ("cosmos", 0x0F),
    ("sunbeam", 0x10),
    ("enchant", 0x11),
];

/// The effect list Zigbee2MQTT exposes for a Hue light without color
const EFFECTS: &[&str] = &[
    "blink",
    "breathe",
    "okay",
    "channel_change",
    "candle",
    "sunset",
    "sunrise",
    "sparkle",
    "opal",
    "glisten",
    "underwater",
    "cosmos",
    "sunbeam",
    "enchant",
    "none",
    "finish_effect",
    "stop_effect",
    "stop_hue_effect",
];

/// Signify's multi-color command: flags (LE), then the fields the flags announce.
/// 0x01 on/off, 0x20 effect type.
fn hue_frame(payload: Vec<u8>) -> CommandFrame {
    CommandFrame {
        cluster: HUE_EFFECT_CLUSTER,
        manufacturer: Some(SIGNIFY),
        command: 0x00,
        payload,
    }
}

/// As Zigbee2MQTT's `philipsTz.effect`: the stop variants all end Hue effects, and the
/// ZCL ones also stop the ZCL effect
pub fn effect_frames(name: &str) -> Result<Vec<CommandFrame>> {
    let stop_hue = || hue_frame(vec![0x20, 0x00, 0x00]);
    let zcl = |name: &str| ZCL_EFFECTS.iter().find(|(n, _)| *n == name).map(|(_, id)| common::trigger_effect_frame(*id));
    Ok(match name {
        "none" | "stop_hue_effect" => vec![stop_hue()],
        "finish_effect" | "stop_effect" => vec![stop_hue(), zcl(name).expect("stop effects are ZCL effects")],
        _ => {
            if let Some(&(_, effect)) = HUE_EFFECTS.iter().find(|(n, _)| *n == name) {
                vec![hue_frame(vec![0x21, 0x00, 0x01, effect])]
            } else if let Some(frame) = zcl(name) {
                vec![frame]
            } else {
                bail!("unknown effect: {name}");
            }
        }
    })
}

/// The effect state Zigbee2MQTT keeps: Hue effects by name, stops as "none"; the short
/// ZCL effects leave no state
pub fn effect_state(name: &str) -> Option<&'static str> {
    if matches!(name, "none" | "stop_hue_effect" | "finish_effect" | "stop_effect") {
        return Some("none");
    }
    HUE_EFFECTS.iter().find(|(n, _)| *n == name).map(|(n, _)| *n)
}

const EFFECT: Feature = Feature::new(
    "effect",
    "Effect",
    Kind::Enum { values: EFFECTS },
    Place::Config,
    Source::Command(Command::HueEffect),
)
.describe("Triggers an effect on the light (e.g. make light blink for a few seconds)");

// ---- White ambiance lights ----

/// Zigbee2MQTT's color temperature presets for a range; for these lights all five fall
/// inside it
const fn color_temp_presets(coolest: i64, warmest: i64) -> [Preset; 5] {
    [
        Preset {
            name: "coolest",
            raw: coolest,
            description: "Coolest temperature supported",
        },
        Preset {
            name: "cool",
            raw: 250,
            description: "Cool temperature (250 mireds / 4000 Kelvin)",
        },
        Preset {
            name: "neutral",
            raw: 370,
            description: "Neutral temperature (370 mireds / 2700 Kelvin)",
        },
        Preset {
            name: "warm",
            raw: 454,
            description: "Warm temperature (454 mireds / 2200 Kelvin)",
        },
        Preset {
            name: "warmest",
            raw: warmest,
            description: "Warmest temperature supported",
        },
    ]
}

const fn startup_presets(presets: [Preset; 5]) -> [Preset; 6] {
    [
        presets[0],
        presets[1],
        presets[2],
        presets[3],
        presets[4],
        Preset {
            name: "previous",
            raw: 0xFFFF,
            description: "Restore previous color_temp on cold power on",
        },
    ]
}

/// `philips.m.light({colorTemp: {range}})` + `m.identify()`
const fn white_ambiance(min: f64, max: f64, presets: &'static [Preset], startup: &'static [Preset]) -> [Feature; 11] {
    [
        common::ON_OFF,
        common::brightness(2.0),
        common::color_temp(min, max, presets),
        common::COLOR_TEMP_MIN,
        common::COLOR_TEMP_MAX,
        common::color_temp_startup(min, max, startup),
        common::POWER_ON_BEHAVIOR,
        // Not exposed by Zigbee2MQTT for these models, but the lights report them (the
        // transition times and on_level they do not support)
        level_config::CURRENT_LEVEL_STARTUP,
        level_config::EXECUTE_IF_OFF,
        EFFECT,
        common::IDENTIFY,
    ]
}

static LTA009_PRESETS: [Preset; 5] = color_temp_presets(153, 454);
static LTA009_STARTUP: [Preset; 6] = startup_presets(color_temp_presets(153, 454));
static LTA009_FEATURES: [Feature; 11] = white_ambiance(153.0, 454.0, &LTA009_PRESETS, &LTA009_STARTUP);

pub static LTA009: Definition = Definition {
    fingerprint: &[Fingerprint::Model("LTA009")],
    vendor: "Philips",
    description: "Hue white ambiance E27 1100lm with Bluetooth",
    name: None,
    card: Card::Light,
    features: &LTA009_FEATURES,
    binds: &[],
    time_base: TimeBase::Zigbee2000,
    tuya_time_since_2000: false,
};

static LTA005_PRESETS: [Preset; 5] = color_temp_presets(222, 454);
static LTA005_STARTUP: [Preset; 6] = startup_presets(color_temp_presets(222, 454));
static LTA005_FEATURES: [Feature; 11] = white_ambiance(222.0, 454.0, &LTA005_PRESETS, &LTA005_STARTUP);

pub static LTA005: Definition = Definition {
    fingerprint: &[Fingerprint::Model("LTA005")],
    vendor: "Philips",
    description: "Hue White Ambiance E27 filament screw globe",
    name: None,
    card: Card::Light,
    features: &LTA005_FEATURES,
    binds: &[],
    time_base: TimeBase::Zigbee2000,
    tuya_time_since_2000: false,
};

/// `philips.m.light()` + `m.identify()`: dimmable white, no color temperature
static WHITE_FEATURES: [Feature; 7] = [
    common::ON_OFF,
    common::brightness(2.0),
    common::POWER_ON_BEHAVIOR,
    // As for the white ambiance lights: reported though Zigbee2MQTT does not expose them
    level_config::CURRENT_LEVEL_STARTUP,
    level_config::EXECUTE_IF_OFF,
    EFFECT,
    common::IDENTIFY,
];

pub static LWA017: Definition = Definition {
    fingerprint: &[Fingerprint::Model("LWA017")],
    vendor: "Philips",
    description: "Hue white A60 bulb E27 1050lm with Bluetooth",
    name: None,
    card: Card::Light,
    features: &WHITE_FEATURES,
    binds: &[],
    time_base: TimeBase::Zigbee2000,
    tuya_time_since_2000: false,
};

// ---- RDM001: Hue wall switch module ----

/// Signify's basic cluster attribute: how the connected switches are wired
const DEVICE_MODE: Feature = Feature::new(
    "device_mode",
    "Device mode",
    Kind::Enum {
        values: &["single_rocker", "single_push_button", "dual_rocker", "dual_push_button"],
    },
    Place::Config,
    Source::Attr {
        cluster: cluster::BASIC,
        attr: 0x0034,
        ty: types::ENUM8,
        conv: Conv::Enum(&[
            (0, "single_rocker"),
            (1, "single_push_button"),
            (2, "dual_rocker"),
            (3, "dual_push_button"),
        ]),
        write: Write::Attribute,
        report: None,
        manufacturer: Some(SIGNIFY),
    },
)
// Zigbee2MQTT has no description for this one
.describe("The switches connected to the module: one or two, rockers or push buttons");

pub static RDM001: Definition = Definition {
    fingerprint: &[
        Fingerprint::Model("RDM001"),
        Fingerprint::Model("9290030171"),
        Fingerprint::Model("RDM004"),
    ],
    vendor: "Philips",
    description: "Hue wall switch module",
    name: None,
    card: Card::Sensor,
    features: &[
        common::BATTERY,
        common::action(
            &[
                "left_press",
                "left_press_release",
                "right_press",
                "right_press_release",
                "left_hold",
                "left_hold_release",
                "right_hold",
                "right_hold_release",
                "toggle",
                "brightness_move_up",
                "brightness_move_down",
                "brightness_stop",
            ],
            &[
                Action::HueButtons(&[(1, "left"), (2, "right")]),
                Action::Toggle,
                Action::BrightnessMove,
            ],
        ),
        DEVICE_MODE,
    ],
    // As Zigbee2MQTT: the switch's On/Off commands and Hue notifications come to the hub
    binds: &[cluster::ON_OFF, HUE_NOTIFICATION_CLUSTER],
    time_base: TimeBase::Zigbee2000,
    tuya_time_since_2000: false,
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devices::{Access, definition, features};
    use crate::store::Device;
    use serde_json::json;

    fn lamp() -> Device {
        let mut device = Device::new("00:17:88:01:0b:1d:bc:29".into(), 0x1234);
        device.model = Some("LTA009".into());
        device
    }

    fn feature(key: &str) -> &'static Feature {
        LTA009.features.iter().find(|f| f.key == key).unwrap()
    }

    #[test]
    fn found_by_model() {
        assert!(std::ptr::eq(definition(&lamp()).unwrap(), &LTA009));
        assert_eq!(features(&lamp()).len(), LTA009.features.len());
    }

    /// The bytes of Zigbee2MQTT's `hueEffects` table
    #[test]
    fn hue_effects_match_zigbee2mqtt() {
        let candle = effect_frames("candle").unwrap();
        assert_eq!(candle, [hue_frame(vec![0x21, 0x00, 0x01, 0x01])]);
        assert_eq!(effect_frames("enchant").unwrap()[0].payload, [0x21, 0x00, 0x01, 0x11]);
        assert_eq!(effect_frames("none").unwrap(), [hue_frame(vec![0x20, 0x00, 0x00])]);
        let stop = effect_frames("stop_effect").unwrap();
        assert_eq!(stop[1], common::trigger_effect_frame(0xFF));
        assert_eq!(effect_frames("channel_change").unwrap(), [common::trigger_effect_frame(0x0B)]);
        assert!(effect_frames("fireplace").is_err());
        for name in EFFECTS {
            assert!(effect_frames(name).is_ok(), "{name}");
        }
        assert_eq!(effect_state("sunset"), Some("sunset"));
        assert_eq!(effect_state("finish_effect"), Some("none"));
        assert_eq!(effect_state("blink"), None);
    }

    #[test]
    fn commands_are_write_only() {
        assert_eq!(feature("effect").access, Access::Write);
        assert_eq!(feature("identify").access, Access::Write);
        assert!(feature("identify").validate(&lamp(), json!("identify")).is_ok());
        assert!(feature("effect").validate(&lamp(), json!("rainbow")).is_err());
    }

    #[test]
    fn special_presets_show_by_name() {
        let startup = feature("color_temp_startup");
        assert_eq!(startup.decode_int(0xFFFF), Some(json!("previous")));
        // Inside the range a preset is only a shortcut: the number stays
        assert_eq!(startup.decode_int(153), Some(json!(153.0)));
        assert_eq!(feature("current_level_startup").decode_int(0), Some(json!("minimum")));
        assert_eq!(feature("current_level_startup").decode_int(255), Some(json!("previous")));
        assert_eq!(level_config::ON_LEVEL.decode_int(255), Some(json!("previous")));
        assert_eq!(level_config::ON_TRANSITION_TIME.decode_int(0xFFFF), Some(json!("disabled")));
        assert_eq!(level_config::ON_TRANSITION_TIME.decode_int(15), Some(json!(1.5)));
    }

    #[test]
    fn presets_validate_and_encode() {
        let device = lamp();
        let startup = feature("color_temp_startup");
        assert_eq!(startup.validate(&device, json!("previous")).unwrap(), json!("previous"));
        assert_eq!(startup.encode(&json!("previous")).unwrap(), 0xFFFF);
        // A shortcut is written as its value
        assert_eq!(startup.validate(&device, json!("neutral")).unwrap(), json!(370.0));
        assert!(startup.validate(&device, json!("bogus")).is_err());
        assert_eq!(feature("color_temp").validate(&device, json!("coolest")).unwrap(), json!(153.0));
        let off = &level_config::OFF_TRANSITION_TIME;
        assert_eq!(off.encode(&off.validate(&device, json!(2.5)).unwrap()).unwrap(), 25);
        assert_eq!(off.encode(&json!("disabled")).unwrap(), 0xFFFF);
        assert!(off.validate(&device, json!(6553.5)).is_err());
    }

    #[test]
    fn filament_globe_has_its_own_range() {
        let mut device = lamp();
        device.model = Some("LTA005".into());
        assert!(std::ptr::eq(definition(&device).unwrap(), &LTA005));
        let ct = features(&device).into_iter().find(|f| f.key == "color_temp").unwrap();
        assert_eq!(ct.validate(&device, json!("coolest")).unwrap(), json!(222.0));
        assert!(ct.validate(&device, json!(153)).is_err());
    }

    #[test]
    fn wall_switch_mode_is_manufacturer_specific() {
        let mut device = lamp();
        device.model = Some("RDM001".into());
        let mode = features(&device).into_iter().find(|f| f.key == "device_mode").unwrap();
        let Source::Attr { manufacturer, .. } = mode.source else { panic!() };
        assert_eq!(manufacturer, Some(SIGNIFY));
        assert_eq!(mode.encode(&json!("dual_rocker")).unwrap(), 2);
        let action = features(&device).into_iter().find(|f| f.key == "action").unwrap();
        assert_eq!(action.access, Access::Read);
    }

    #[test]
    fn brightness_starts_at_two() {
        assert!(feature("level").validate(&lamp(), json!(1)).is_err());
        assert!(feature("level").validate(&lamp(), json!(2)).is_ok());
    }

    #[test]
    fn white_bulb_has_no_color_temperature() {
        let mut device = lamp();
        device.model = Some("LWA017".into());
        assert!(std::ptr::eq(definition(&device).unwrap(), &LWA017));
        let keys: Vec<_> = features(&device).into_iter().map(|f| f.key).collect();
        assert_eq!(keys, ["on", "level", "power_on_behavior", "current_level_startup", "execute_if_off", "effect", "identify"]);
    }
}
