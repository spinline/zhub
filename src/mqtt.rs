//! MQTT bridge, laid out like Zigbee2MQTT so tools made for it work:
//! - `<base>/<name>`: device or group state (retained), lights also as `state`/`brightness`
//! - `<base>/<name>/set`: commands, with Zigbee2MQTT's `state: ON/OFF/TOGGLE` and
//!   `brightness` besides the feature keys
//! - `<base>/<name>/action`: button presses (not retained)
//! - `<base>/<name>/availability`: `{"state":"online"}` or `offline` (retained)
//! - `<base>/bridge/state`, `<base>/bridge/info`, `<base>/bridge/devices` (with
//!   Zigbee2MQTT's `exposes`), `<base>/bridge/groups`
//! - `<base>/bridge/request/permit_join` and `restart`
//! - Home Assistant discovery under `<prefix>/…/config`
//!
//! The connection restarts when the settings change.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use rumqttc::{AsyncClient, Event as MqttEvent, Incoming, LastWill, MqttOptions, QoS};
use anyhow::Context as _;
use serde_json::{Map, Value as Json, json};
use tokio::sync::{broadcast, mpsc};
use tracing::{debug, info, warn};

use crate::devices::{Access, Card, Feature, Kind, Place, Source};
use crate::hub::{DeviceView, Event, Hub};
use crate::store::{Group, MqttSettings};
use crate::z2m_light;

/// Keys a light or climate entity already covers
const LIGHT_KEYS: &[&str] = &["on", "level", "color_temp"];
const CLIMATE_KEYS: &[&str] = &["temperature", "setpoint", "system_mode", "heating"];
/// Binary features Zigbee2MQTT publishes as words (on, off) rather than true/false
const BINARY_WORDS: &[(&str, &str, &str)] = &[("child_lock", "LOCK", "UNLOCK"), ("open_window", "ON", "OFF")];

fn binary_words(key: &str) -> Option<(&'static str, &'static str)> {
    BINARY_WORDS.iter().find(|(k, _, _)| *k == key).map(|&(_, on, off)| (on, off))
}

const HA_MODES: &[&str] = &["off", "heat", "auto", "cool", "heat_cool", "dry", "fan_only"];

pub async fn run(hub: Arc<Hub>) {
    let mut settings = hub.mqtt_settings();
    loop {
        let current = settings.borrow_and_update().clone();
        if current.enabled {
            hub.set_mqtt_status("connecting");
            tokio::select! {
                () = session(hub.clone(), current) => {}
                _ = settings.changed() => info!("MQTT settings changed, reconnecting"),
            }
        } else {
            hub.set_mqtt_status("disabled");
            if settings.changed().await.is_err() {
                return;
            }
        }
    }
}

/// MQTT topic levels cannot hold wildcards
fn topic_name(name: &str) -> String {
    name.replace(['+', '#'], "_")
}

struct Bridge {
    hub: Arc<Hub>,
    client: AsyncClient,
    cfg: MqttSettings,
    /// Retained topics published per device or group ("group:<id>"), to clear them later
    state_topics: HashMap<String, String>,
    discovery: HashMap<String, BTreeMap<String, String>>,
    /// A device or group got a new topic: bridge/info lists the names too
    renamed: bool,
}

async fn session(hub: Arc<Hub>, cfg: MqttSettings) {
    let id = hub.hub_ieee().replace(':', "");
    let mut options = MqttOptions::new(format!("zhub-{id}"), cfg.host.trim(), cfg.port);
    options.set_keep_alive(Duration::from_secs(30));
    options.set_max_packet_size(256 * 1024, 256 * 1024);
    if !cfg.username.is_empty() {
        options.set_credentials(cfg.username.clone(), cfg.password.clone().unwrap_or_default());
    }
    let state_topic = format!("{}/bridge/state", cfg.base_topic);
    options.set_last_will(LastWill::new(&state_topic, r#"{"state":"offline"}"#, QoS::AtLeastOnce, true));
    let (client, mut eventloop) = AsyncClient::new(options, 512);

    // The event loop runs on its own so publishing never waits on it
    let (incoming_tx, mut incoming) = mpsc::unbounded_channel();
    let poller_hub = hub.clone();
    let _poller = AbortOnDrop(tokio::spawn(async move {
        loop {
            match eventloop.poll().await {
                Ok(MqttEvent::Incoming(Incoming::ConnAck(_))) => {
                    poller_hub.set_mqtt_status("connected");
                    let _ = incoming_tx.send(None);
                }
                Ok(MqttEvent::Incoming(Incoming::Publish(p))) => {
                    let _ = incoming_tx.send(Some((p.topic, p.payload.to_vec())));
                }
                Ok(_) => {}
                Err(e) => {
                    poller_hub.set_mqtt_status(format!("error: {e}"));
                    warn!("MQTT: {e}; retrying in 10 s");
                    tokio::time::sleep(Duration::from_secs(10)).await;
                }
            }
        }
    }));

    let mut bridge = Bridge {
        hub: hub.clone(),
        client,
        cfg,
        state_topics: HashMap::new(),
        discovery: HashMap::new(),
        renamed: false,
    };
    let mut events = hub.subscribe();
    loop {
        tokio::select! {
            message = incoming.recv() => match message {
                // (Re)connected: announce, subscribe and publish everything
                Some(None) => {
                    info!("MQTT connected to {}:{}", bridge.cfg.host, bridge.cfg.port);
                    bridge.state_topics.clear();
                    bridge.discovery.clear();
                    bridge.connected().await;
                }
                Some(Some((topic, payload))) => bridge.incoming(&topic, &payload).await,
                None => break,
            },
            event = events.recv() => match event {
                Ok(event) => {
                    bridge.hub_event(event).await;
                    if std::mem::take(&mut bridge.renamed) {
                        bridge.publish_info().await;
                    }
                }
                // Missed events: publish the whole picture again
                Err(broadcast::error::RecvError::Lagged(_)) => bridge.publish_all().await,
                Err(broadcast::error::RecvError::Closed) => break,
            },
        }
    }
}

/// The connection's event loop ends with the session (e.g. when the settings change)
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

impl Bridge {
    fn base(&self) -> &str {
        &self.cfg.base_topic
    }

    async fn publish(&self, topic: &str, payload: String, retain: bool) {
        if let Err(e) = self.client.publish(topic, QoS::AtLeastOnce, retain, payload).await {
            debug!("MQTT publish to {topic} failed: {e}");
        }
    }

    async fn connected(&mut self) {
        let base = self.base().to_owned();
        for topic in [
            format!("{base}/+/set"),
            format!("{base}/+/set/+"),
            format!("{base}/+/get"),
            format!("{base}/+/get/+"),
            format!("{base}/bridge/request/+"),
            format!("{}/status", self.cfg.discovery_prefix),
        ] {
            if let Err(e) = self.client.subscribe(topic, QoS::AtLeastOnce).await {
                warn!("MQTT subscribe failed: {e}");
            }
        }
        self.publish(&format!("{base}/bridge/state"), r#"{"state":"online"}"#.into(), true).await;
        self.publish_all().await;
    }

    async fn publish_all(&mut self) {
        self.publish_info().await;
        let devices = self.hub.devices();
        self.publish_bridge_lists(&devices).await;
        for view in &devices {
            self.device_state(view).await;
        }
        let groups = self.hub.groups();
        for group in &groups {
            self.group_state(group, &devices).await;
        }
    }

    async fn publish_bridge_lists(&self, devices: &[DeviceView]) {
        let list: Vec<Json> = devices
            .iter()
            .map(|v| {
                json!({
                    "ieee_address": v.device.ieee,
                    "network_address": v.device.nwk,
                    "friendly_name": topic_name(&v.device.name),
                    "type": if v.device.router { "Router" } else { "EndDevice" },
                    "power_source": if v.device.rx_on_when_idle { "Mains (single phase)" } else { "Battery" },
                    "model_id": v.device.model,
                    "manufacturer": v.device.manufacturer,
                    "supported": v.definition.is_some(),
                    "disabled": false,
                    "interviewing": v.interviewing,
                    "interview_completed": v.device.interviewed,
                    "definition": v.definition.map(|d| json!({
                        "model": v.device.model,
                        "vendor": d.vendor,
                        "description": d.description,
                        "exposes": exposes(v),
                        "options": [],
                        "supports_ota": false,
                        "source": "native",
                    })),
                })
            })
            .collect();
        self.publish(&format!("{}/bridge/devices", self.base()), Json::Array(list).to_string(), true)
            .await;
        let groups: Vec<Json> = self
            .hub
            .groups()
            .iter()
            .map(|g| {
                json!({
                    "id": g.id,
                    "friendly_name": topic_name(&g.name),
                    "members": g.members.iter().map(|m| json!({"ieee_address": m.ieee, "endpoint": m.ep})).collect::<Vec<_>>(),
                })
            })
            .collect();
        self.publish(&format!("{}/bridge/groups", self.base()), Json::Array(groups).to_string(), true)
            .await;
    }

    /// Zigbee2MQTT's bridge info: tools find the bridge by it
    async fn publish_info(&self) {
        let info = self.hub.info();
        let remaining = self.hub.permit_remaining();
        let now_ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64);
        let devices: Map<String, Json> = self.hub.devices().iter().map(|v| (v.device.ieee.clone(), json!({"friendly_name": v.device.name}))).collect();
        let groups: Map<String, Json> = self.hub.groups().iter().map(|g| (g.id.to_string(), json!({"friendly_name": g.name}))).collect();
        let payload = json!({
            "version": format!("zhub {}", info.version),
            "commit": null,
            "coordinator": {"ieee_address": format!("0x{}", info.ieee.replace(':', "")), "type": "Spinel", "meta": {"firmware": info.radio_firmware}},
            "network": {"channel": info.channel, "pan_id": info.pan_id, "extended_pan_id": info.extended_pan_id},
            "log_level": info.log_level,
            "permit_join": remaining > 0,
            "permit_join_end": if remaining > 0 { json!(now_ms + remaining * 1000) } else { Json::Null },
            "restart_required": false,
            "config": {"devices": devices, "groups": groups},
        });
        self.publish(&format!("{}/bridge/info", self.base()), payload.to_string(), true).await;
    }

    /// `bridge/request/<name>` as Zigbee2MQTT takes it
    fn request(&self, name: &str, payload: &[u8]) {
        let body: Json = serde_json::from_slice(payload).unwrap_or(Json::Null);
        match name {
            "permit_join" => {
                // {"value": bool, "time": s}, {"time": s} or a bare bool
                let on = body.get("value").and_then(Json::as_bool).or(body.as_bool());
                let time = body.get("time").and_then(Json::as_u64);
                let seconds = match (on, time) {
                    (Some(false), _) | (None, Some(0)) => 0,
                    (_, Some(t)) => t.min(254) as u8,
                    _ => self.hub.public_settings()["permit_join_seconds"].as_u64().unwrap_or(180).min(254) as u8,
                };
                if let Err(e) = self.hub.permit_join(seconds) {
                    warn!("MQTT permit_join: {e:#}");
                }
            }
            "restart" => self.hub.request_restart(),
            _ => debug!("MQTT bridge request {name} not supported"),
        }
    }

    async fn hub_event(&mut self, event: Event) {
        match event {
            Event::Device { device } => {
                self.device_state(&device).await;
                // A member's change moves its groups' state
                let devices = self.hub.devices();
                for group in self.hub.groups().iter().filter(|g| g.members.iter().any(|m| m.ieee == device.device.ieee)) {
                    self.group_state(group, &devices).await;
                }
                self.publish_bridge_lists(&devices).await;
            }
            Event::Removed { ieee } => {
                self.clear(&ieee).await;
                self.publish_bridge_lists(&self.hub.devices()).await;
            }
            Event::Groups { groups } => {
                let devices = self.hub.devices();
                let keys: Vec<String> = self.state_topics.keys().filter(|k| k.starts_with("group:")).cloned().collect();
                for key in keys {
                    if !groups.iter().any(|g| key == format!("group:{}", g.id)) {
                        self.clear(&key).await;
                    }
                }
                for group in &groups {
                    self.group_state(group, &devices).await;
                }
                self.publish_bridge_lists(&devices).await;
            }
            Event::Action { ieee, name, action, .. } => {
                self.publish(&format!("{}/{}/action", self.base(), topic_name(&name)), action.clone(), false).await;
                // Zigbee2MQTT also puts the action into a state message; not retained, so the
                // press is not replayed
                if let Some(view) = self.hub.device_view(&ieee)
                    && let Some(topic) = self.state_topics.get(&ieee)
                {
                    let mut payload = device_payload(&view);
                    payload["action"] = json!(action);
                    self.publish(topic, payload.to_string(), false).await;
                }
            }
            Event::PermitJoin { .. } | Event::Settings => self.publish_info().await,
        }
    }

    /// Forgets a device's or group's retained topics
    async fn clear(&mut self, key: &str) {
        if let Some(topic) = self.state_topics.remove(key) {
            self.publish(&topic, String::new(), true).await;
            self.publish(&format!("{topic}/availability"), String::new(), true).await;
        }
        for topic in self.discovery.remove(key).unwrap_or_default().into_keys() {
            self.publish(&topic, String::new(), true).await;
        }
    }

    /// Retained state topic, moved when the name changes
    async fn state_topic(&mut self, key: &str, name: &str) -> String {
        let topic = format!("{}/{}", self.base(), topic_name(name));
        if let Some(old) = self.state_topics.insert(key.to_owned(), topic.clone())
            && old != topic
        {
            // As Zigbee2MQTT's rename: the old retained state and availability go
            self.publish(&old, String::new(), true).await;
            self.publish(&format!("{old}/availability"), String::new(), true).await;
            self.renamed = true;
        }
        topic
    }

    async fn device_state(&mut self, view: &DeviceView) {
        let topic = self.state_topic(&view.device.ieee, &view.device.name).await;
        self.publish(&topic, device_payload(view).to_string(), true).await;
        let availability = json!({"state": if view.offline { "offline" } else { "online" }});
        self.publish(&format!("{topic}/availability"), availability.to_string(), true).await;
        if self.cfg.discovery {
            let configs = device_discovery(view, &self.cfg, &topic);
            self.update_discovery(view.device.ieee.clone(), configs).await;
        }
    }

    async fn group_state(&mut self, group: &Group, devices: &[DeviceView]) {
        let key = format!("group:{}", group.id);
        let topic = self.state_topic(&key, &group.name).await;
        let (payload, light) = group_payload(group, devices);
        self.publish(&topic, payload.to_string(), true).await;
        if self.cfg.discovery {
            let mut configs = BTreeMap::new();
            if let Some(light) = light {
                let mut config = light;
                config.insert("name".into(), Json::Null);
                config.insert("unique_id".into(), json!(format!("zhub_group_{}_light", group.id)));
                config.insert(
                    "device".into(),
                    json!({"identifiers": [format!("zhub_group_{}", group.id)], "name": group.name, "model": "Group", "manufacturer": "zhub"}),
                );
                self.common(&mut config, &topic);
                configs.insert(format!("{}/light/zhub_group_{}/light/config", self.cfg.discovery_prefix, group.id), Json::Object(config));
            }
            self.update_discovery(key, configs).await;
        }
    }

    fn common(&self, config: &mut Map<String, Json>, state_topic: &str) {
        common_fields(config, &self.cfg, state_topic);
    }

    /// Publishes changed configs and removes the ones that went away
    async fn update_discovery(&mut self, key: String, configs: BTreeMap<String, Json>) {
        let new: BTreeMap<String, String> = configs.into_iter().map(|(t, c)| (t, c.to_string())).collect();
        let old = self.discovery.remove(&key).unwrap_or_default();
        for (topic, payload) in &new {
            if old.get(topic) != Some(payload) {
                self.publish(topic, payload.clone(), true).await;
            }
        }
        for topic in old.keys().filter(|t| !new.contains_key(*t)) {
            self.publish(topic, String::new(), true).await;
        }
        self.discovery.insert(key, new);
    }

    async fn incoming(&mut self, topic: &str, payload: &[u8]) {
        if topic == format!("{}/status", self.cfg.discovery_prefix) {
            // Home Assistant restarted: announce everything again
            if payload == b"online" {
                self.discovery.clear();
                self.publish_all().await;
            }
            return;
        }
        let Some(rest) = topic.strip_prefix(&format!("{}/", self.base())) else {
            return;
        };
        if let Some(request) = rest.strip_prefix("bridge/request/") {
            if self.cfg.log_commands {
                info!("MQTT ← {topic} {}", String::from_utf8_lossy(payload));
            }
            self.request(request, payload);
            return;
        }
        let mut parts = rest.splitn(3, '/');
        let (Some(name), Some(verb @ ("set" | "get"))) = (parts.next(), parts.next()) else {
            return;
        };
        let text = String::from_utf8_lossy(payload);
        if self.cfg.log_commands {
            info!("MQTT ← {topic} {text}");
        }
        // `<name>/set/<key>` carries a bare value; `<name>/set` a JSON object
        let command: Map<String, Json> = match parts.next() {
            Some(key) => {
                let value = serde_json::from_str(&text).unwrap_or_else(|_| Json::String(text.trim().to_owned()));
                Map::from_iter([(key.to_owned(), value)])
            }
            None => match serde_json::from_str::<Json>(&text) {
                Ok(Json::Object(map)) => map,
                _ => {
                    warn!("MQTT {topic}: expected a JSON object, got {text}");
                    return;
                }
            },
        };
        let hub = self.hub.clone();
        let name = name.to_owned();
        let log = self.cfg.log_commands;
        let get_verb = verb == "get";
        let verb = verb.to_owned();
        // Commands may wait for a sleepy device: never hold up the bridge
        tokio::spawn(async move {
            let started = std::time::Instant::now();
            let result = if get_verb {
                get(&hub, &name, command.keys().cloned().collect()).await
            } else {
                apply(&hub, &name, command).await
            };
            match result {
                Ok(()) if log => info!("MQTT {name}/{verb} done in {} ms", started.elapsed().as_millis()),
                Ok(()) => {}
                Err(e) => warn!("MQTT {name}/{verb}: {e:#}"),
            }
        });
    }
}

fn common_fields(config: &mut Map<String, Json>, cfg: &MqttSettings, state_topic: &str) {
    config.insert("state_topic".into(), json!(state_topic));
    config.insert("availability_topic".into(), json!(format!("{}/bridge/state", cfg.base_topic)));
    config.insert("availability_template".into(), json!("{{ value_json.state }}"));
    config.insert("origin".into(), json!({"name": "zhub", "sw_version": env!("CARGO_PKG_VERSION")}));
}

/// The device's state, with Zigbee2MQTT's names for light state and brightness. The last
/// action goes to its own topic, not into the retained state.
pub fn device_payload(view: &DeviceView) -> Json {
    let mut out: Map<String, Json> = view.device.state.iter().filter(|(k, _)| *k != "action").map(|(k, v)| (k.clone(), v.clone())).collect();
    if let Some(on) = view.device.state.get("on").and_then(Json::as_bool) {
        out.insert("state".into(), json!(if on { "ON" } else { "OFF" }));
    }
    if let Some(level) = view.device.state.get("level") {
        out.insert("brightness".into(), level.clone());
    }
    if view.device.state.contains_key("color_temp") {
        out.insert("color_mode".into(), json!("color_temp"));
    }
    for (key, on, off) in BINARY_WORDS {
        if let Some(value) = out.get_mut(*key)
            && let Some(b) = value.as_bool()
        {
            *value = json!(if b { on } else { off });
        }
    }
    // Zigbee2MQTT's thermostat names
    if let Some(setpoint) = view.device.state.get("setpoint") {
        out.insert("current_heating_setpoint".into(), setpoint.clone());
    }
    if let Some(heating) = view.device.state.get("heating").and_then(Json::as_bool) {
        out.insert("running_state".into(), json!(if heating { "heat" } else { "idle" }));
    }
    if let Some(lqi) = view.device.lqi {
        out.insert("linkquality".into(), json!(lqi));
    }
    if let Some(seen) = view.device.last_seen {
        out.insert("last_seen".into(), json!(seen * 1000));
    }
    Json::Object(out)
}

/// A group's state follows its members (any on, the brightest level, the first color
/// temperature); the light config when a member is a light
fn group_payload(group: &Group, devices: &[DeviceView]) -> (Json, Option<Map<String, Json>>) {
    let members: Vec<&DeviceView> = group.members.iter().filter_map(|m| devices.iter().find(|d| d.device.ieee == m.ieee)).collect();
    let has = |key: &str| members.iter().filter(|m| m.features.iter().any(|f| f.key == key)).copied().collect::<Vec<_>>();
    let mut out = Map::new();
    let lights = has("on");
    if lights.is_empty() {
        return (Json::Object(out), None);
    }
    let on = lights.iter().any(|m| m.device.state.get("on").and_then(Json::as_bool) == Some(true));
    out.insert("state".into(), json!(if on { "ON" } else { "OFF" }));
    let levels = has("level");
    if let Some(level) = levels.iter().filter_map(|m| m.device.state.get("level").and_then(Json::as_f64)).reduce(f64::max) {
        out.insert("brightness".into(), json!(level));
    }
    let temps = has("color_temp");
    if let Some(ct) = temps.iter().find_map(|m| m.device.state.get("color_temp").cloned()) {
        out.insert("color_temp".into(), ct);
        out.insert("color_mode".into(), json!("color_temp"));
    }
    let mut light = Map::new();
    light.insert("schema".into(), json!("json"));
    if !levels.is_empty() {
        light.insert("brightness".into(), json!(true));
        light.insert("brightness_scale".into(), json!(254));
    }
    if temps.is_empty() {
        light.insert("supported_color_modes".into(), json!([if levels.is_empty() { "onoff" } else { "brightness" }]));
    } else {
        let ranges: Vec<(Option<f64>, Option<f64>)> = temps
            .iter()
            .filter_map(|m| m.features.iter().find(|f| f.key == "color_temp").map(|f| f.range(&m.device)))
            .collect();
        let min = ranges.iter().filter_map(|r| r.0).fold(f64::MIN, f64::max);
        let max = ranges.iter().filter_map(|r| r.1).fold(f64::MAX, f64::min);
        light.insert("supported_color_modes".into(), json!(["color_temp"]));
        if min > f64::MIN && max < f64::MAX {
            light.insert("min_mireds".into(), json!(min));
            light.insert("max_mireds".into(), json!(max));
        }
    }
    (Json::Object(out), Some(light))
}

/// Home Assistant entities for a device: a light or climate entity for its card, the
/// other features one entity each, and a trigger per action
fn device_discovery(view: &DeviceView, cfg: &MqttSettings, state_topic: &str) -> BTreeMap<String, Json> {
    let mut configs = BTreeMap::new();
    let ieee = view.device.ieee.replace(':', "");
    let command_topic = format!("{state_topic}/set");
    let device = json!({
        "identifiers": [format!("zhub_{ieee}")],
        "name": view.device.name,
        "model": view.device.model,
        "manufacturer": view.definition.map_or(view.device.manufacturer.as_deref().unwrap_or_default(), |d| d.vendor),
        "sw_version": view.device.firmware,
    });
    let feature = |key: &str| view.features.iter().find(|f| f.key == key);
    let mut covered: &[&str] = &[];
    let mut triggers = Vec::new();
    let mut add = |component: &str, object: &str, mut config: Map<String, Json>| {
        config.insert("unique_id".into(), json!(format!("zhub_{ieee}_{object}")));
        config.insert("device".into(), device.clone());
        common_fields(&mut config, cfg, state_topic);
        // Available while both the bridge and the device are
        config.remove("availability_topic");
        config.remove("availability_template");
        config.insert("availability_mode".into(), json!("all"));
        config.insert(
            "availability".into(),
            json!([
                {"topic": format!("{}/bridge/state", cfg.base_topic), "value_template": "{{ value_json.state }}"},
                {"topic": format!("{state_topic}/availability"), "value_template": "{{ value_json.state }}"},
            ]),
        );
        configs.insert(format!("{}/{component}/zhub_{ieee}/{object}/config", cfg.discovery_prefix), Json::Object(config));
    };

    if view.card == Card::Light && feature("on").is_some_and(writable) {
        covered = LIGHT_KEYS;
        let mut light = Map::new();
        light.insert("name".into(), Json::Null);
        light.insert("schema".into(), json!("json"));
        light.insert("command_topic".into(), json!(command_topic));
        if feature("level").is_some() {
            light.insert("brightness".into(), json!(true));
            light.insert("brightness_scale".into(), json!(254));
        }
        match feature("color_temp") {
            Some(ct) => {
                let (min, max) = ct.range(&view.device);
                light.insert("supported_color_modes".into(), json!(["color_temp"]));
                if let (Some(min), Some(max)) = (min, max) {
                    light.insert("min_mireds".into(), json!(min));
                    light.insert("max_mireds".into(), json!(max));
                }
            }
            None => {
                let mode = if feature("level").is_some() { "brightness" } else { "onoff" };
                light.insert("supported_color_modes".into(), json!([mode]));
            }
        }
        add("light", "light", light);
    }

    if view.card == Card::Thermostat
        && let Some(setpoint) = feature("setpoint")
    {
        covered = CLIMATE_KEYS;
        let mut climate = Map::new();
        climate.insert("name".into(), Json::Null);
        climate.insert("temperature_unit".into(), json!("C"));
        climate.insert("current_temperature_topic".into(), json!(state_topic));
        climate.insert("current_temperature_template".into(), json!("{{ value_json.temperature }}"));
        climate.insert("temperature_state_topic".into(), json!(state_topic));
        climate.insert("temperature_state_template".into(), json!("{{ value_json.setpoint }}"));
        climate.insert("temperature_command_topic".into(), json!(command_topic));
        climate.insert("temperature_command_template".into(), json!(r#"{"setpoint": {{ value }}}"#));
        if let Kind::Numeric { min, max, step, .. } = setpoint.kind {
            let (lo, hi) = setpoint.range(&view.device);
            if let Some(v) = lo.or(min) {
                climate.insert("min_temp".into(), json!(v));
            }
            if let Some(v) = hi.or(max) {
                climate.insert("max_temp".into(), json!(v));
            }
            if let Some(v) = step {
                climate.insert("temp_step".into(), json!(v));
                climate.insert("precision".into(), json!(if v < 1.0 { 0.1 } else { 1.0 }));
            }
        }
        if let Some(Feature {
            kind: Kind::Enum { values },
            ..
        }) = feature("system_mode")
        {
            let modes: Vec<&str> = values.iter().copied().filter(|v| HA_MODES.contains(v)).collect();
            if !modes.is_empty() {
                climate.insert("modes".into(), json!(modes));
                climate.insert("mode_state_topic".into(), json!(state_topic));
                climate.insert("mode_state_template".into(), json!("{{ value_json.system_mode }}"));
                climate.insert("mode_command_topic".into(), json!(command_topic));
                climate.insert("mode_command_template".into(), json!(r#"{"system_mode": "{{ value }}"}"#));
            }
        }
        if feature("heating").is_some() {
            climate.insert("action_topic".into(), json!(state_topic));
            climate.insert("action_template".into(), json!("{{ 'heating' if value_json.heating else 'idle' }}"));
        }
        add("climate", "climate", climate);
    }

    for f in &view.features {
        if covered.contains(&f.key) || f.place == Place::Hidden {
            continue;
        }
        let value = format!("value_json.{}", f.key);
        // A binary feature's truth, for those published as words
        let truthy = binary_words(f.key).map_or(value.clone(), |(on, _)| format!("{value} == '{on}'"));
        let mut config = Map::new();
        config.insert("name".into(), json!(f.label));
        config.insert("object_id".into(), json!(format!("{}_{}", topic_name(&view.device.name), f.key)));
        match f.place {
            Place::Config => {
                config.insert("entity_category".into(), json!("config"));
            }
            Place::Diagnostic => {
                config.insert("entity_category".into(), json!("diagnostic"));
            }
            Place::Card | Place::Hidden => {}
        }
        match (f.kind, f.access, f.source) {
            (_, _, Source::Actions(_)) => {
                let Kind::Enum { values } = f.kind else { continue };
                for action in values {
                    let mut trigger = Map::new();
                    trigger.insert("automation_type".into(), json!("trigger"));
                    trigger.insert("type".into(), json!("action"));
                    trigger.insert("subtype".into(), json!(action));
                    trigger.insert("payload".into(), json!(action));
                    trigger.insert("topic".into(), json!(format!("{state_topic}/action")));
                    trigger.insert("device".into(), device.clone());
                    triggers.push((
                        format!("{}/device_automation/zhub_{ieee}/action_{action}/config", cfg.discovery_prefix),
                        Json::Object(trigger),
                    ));
                }
                continue;
            }
            (Kind::Numeric { unit, min, max, step, .. }, Access::ReadWrite, _) => {
                let (lo, hi) = f.range(&view.device);
                config.insert("value_template".into(), json!(format!("{{{{ {value} }}}}")));
                config.insert("command_topic".into(), json!(command_topic));
                config.insert("command_template".into(), json!(format!(r#"{{"{}": {{{{ value }}}}}}"#, f.key)));
                config.insert("mode".into(), json!("box"));
                config.insert("min".into(), json!(lo.or(min).unwrap_or(-1e6)));
                config.insert("max".into(), json!(hi.or(max).unwrap_or(1e6)));
                config.insert("step".into(), json!(step.unwrap_or(1.0)));
                if !unit.is_empty() {
                    config.insert("unit_of_measurement".into(), json!(unit));
                }
                add("number", f.key, config);
            }
            (Kind::Numeric { unit, .. }, _, _) => {
                config.insert("value_template".into(), json!(format!("{{{{ {value} }}}}")));
                if !unit.is_empty() {
                    config.insert("unit_of_measurement".into(), json!(unit));
                }
                let class = match f.key {
                    "temperature" => Some("temperature"),
                    "humidity" => Some("humidity"),
                    "battery" => Some("battery"),
                    "voltage" => Some("voltage"),
                    _ => None,
                };
                if let Some(class) = class {
                    config.insert("device_class".into(), json!(class));
                    config.insert("state_class".into(), json!("measurement"));
                }
                add("sensor", f.key, config);
            }
            (Kind::Binary, Access::ReadWrite, _) => {
                config.insert("value_template".into(), json!(format!("{{{{ 'ON' if {truthy} else 'OFF' }}}}")));
                config.insert("command_topic".into(), json!(command_topic));
                config.insert("payload_on".into(), json!(format!(r#"{{"{}": true}}"#, f.key)));
                config.insert("payload_off".into(), json!(format!(r#"{{"{}": false}}"#, f.key)));
                config.insert("state_on".into(), json!("ON"));
                config.insert("state_off".into(), json!("OFF"));
                add("switch", f.key, config);
            }
            (Kind::Binary, _, _) => {
                config.insert("value_template".into(), json!(format!("{{{{ 'ON' if {truthy} else 'OFF' }}}}")));
                if f.key == "battery_low" {
                    config.insert("device_class".into(), json!("battery"));
                }
                add("binary_sensor", f.key, config);
            }
            (Kind::Enum { values }, Access::Write, _) if values.len() == 1 => {
                config.insert("command_topic".into(), json!(command_topic));
                config.insert("payload_press".into(), json!(format!(r#"{{"{}": "{}"}}"#, f.key, values[0])));
                add("button", f.key, config);
            }
            (Kind::Enum { values }, Access::ReadWrite | Access::Write, _) => {
                config.insert("options".into(), json!(values));
                config.insert("command_topic".into(), json!(command_topic));
                config.insert("command_template".into(), json!(format!(r#"{{"{}": "{{{{ value }}}}"}}"#, f.key)));
                if f.access == Access::ReadWrite {
                    config.insert("value_template".into(), json!(format!("{{{{ {value} }}}}")));
                } else {
                    config.insert("optimistic".into(), json!(true));
                }
                add("select", f.key, config);
            }
            (Kind::Enum { .. } | Kind::Text, _, _) => {
                config.insert("value_template".into(), json!(format!("{{{{ {value} }}}}")));
                add("sensor", f.key, config);
            }
            // Schedules are edited in the hub's UI
            (Kind::Schedule { .. } | Kind::WeeklySchedule { .. }, _, _) => {}
        }
    }
    configs.extend(triggers);
    configs
}

/// Zigbee2MQTT's `exposes` for a device: lights and thermostats as composites with
/// Zigbee2MQTT's property names, the other features under their own names
pub fn exposes(view: &DeviceView) -> Vec<Json> {
    let feature = |key: &str| view.features.iter().find(|f| f.key == key);
    let access = |f: &Feature| match f.access {
        Access::Read => 1,
        Access::Write => 2,
        Access::ReadWrite => 7,
    };
    let numeric = |f: &Feature, name: &str| {
        let mut e = json!({"type": "numeric", "name": name, "property": name, "label": f.label, "access": access(f)});
        if let Kind::Numeric { unit, step, .. } = f.kind {
            let (min, max) = f.range(&view.device);
            if let Some(min) = min {
                e["value_min"] = json!(min);
            }
            if let Some(max) = max {
                e["value_max"] = json!(max);
            }
            if let Some(step) = step {
                e["value_step"] = json!(step);
            }
            if !unit.is_empty() {
                e["unit"] = json!(unit);
            }
        }
        e
    };
    let mut out = Vec::new();
    let mut covered: Vec<&str> = Vec::new();
    if view.card == Card::Light
        && let Some(on) = feature("on")
    {
        let mut features = vec![json!({
            "type": "binary", "name": "state", "property": "state", "label": "State", "access": access(on),
            "value_on": "ON", "value_off": "OFF", "value_toggle": "TOGGLE",
        })];
        if let Some(f) = feature("level") {
            let mut e = numeric(f, "brightness");
            e["value_min"] = json!(0);
            e["value_max"] = json!(254);
            features.push(e);
        }
        if let Some(f) = feature("color_temp") {
            features.push(numeric(f, "color_temp"));
        }
        out.push(json!({"type": "light", "features": features}));
        covered.extend(LIGHT_KEYS);
    }
    if let Some(setpoint) = feature("setpoint") {
        // The measured temperature stays a feature of its own (`temperature`)
        let mut features = vec![numeric(setpoint, "current_heating_setpoint")];
        if let Some(f) = feature("system_mode")
            && let Kind::Enum { values } = f.kind
        {
            features.push(json!({"type": "enum", "name": "system_mode", "property": "system_mode", "label": f.label, "access": access(f), "values": values}));
            covered.push("system_mode");
        }
        if feature("heating").is_some() {
            features.push(json!({"type": "enum", "name": "running_state", "property": "running_state", "label": "Running state", "access": 1, "values": ["idle", "heat"]}));
            covered.push("heating");
        }
        out.push(json!({"type": "climate", "features": features}));
        covered.push("setpoint");
    }
    for f in view.features.iter().filter(|f| !covered.contains(&f.key)) {
        let base = |kind: &str| json!({"type": kind, "name": f.key, "property": f.key, "label": f.label, "access": access(f)});
        out.push(match f.kind {
            Kind::Numeric { .. } => numeric(f, f.key),
            Kind::Binary => {
                let mut e = base("binary");
                let (on, off) = binary_words(f.key).map_or((json!(true), json!(false)), |(on, off)| (json!(on), json!(off)));
                e["value_on"] = on;
                e["value_off"] = off;
                e
            }
            Kind::Enum { values } => {
                let mut e = base("enum");
                e["values"] = json!(values);
                e
            }
            Kind::Text => base("text"),
            // Edited in the hub's UI
            Kind::Schedule { .. } | Kind::WeeklySchedule { .. } => continue,
        });
    }
    out.push(json!({"type": "numeric", "name": "linkquality", "property": "linkquality", "label": "Linkquality", "access": 1, "unit": "lqi", "value_min": 0, "value_max": 255}));
    out
}

fn writable(f: &Feature) -> bool {
    f.access != Access::Read
}

/// Zigbee2MQTT's per-light memory between commands (the level before a fade to off)
static LIGHT_MEMORY: LazyLock<Mutex<HashMap<String, z2m_light::Memory>>> = LazyLock::new(Default::default);

/// What the light converters know of a device or group; `None` without on/off
fn light_target(views: &[&DeviceView]) -> Option<z2m_light::Target> {
    let with = |key: &str| views.iter().filter(|v| v.features.iter().any(|f| f.key == key)).copied().collect::<Vec<_>>();
    let switches = with("on");
    if switches.is_empty() {
        return None;
    }
    let levels = with("level");
    let temps = with("color_temp");
    let range = |v: &DeviceView, key: &str| v.features.iter().find(|f| f.key == key).map(|f| f.range(&v.device)).unwrap_or((None, None));
    // A group: on if any member is, the brightest level, the narrowest color range
    let on = switches.iter().filter_map(|v| v.device.state.get("on").and_then(Json::as_bool)).reduce(|a, b| a || b);
    let brightness = levels.iter().filter_map(|v| v.device.state.get("level").and_then(Json::as_f64)).reduce(f64::max);
    let color_temp_min = temps.iter().filter_map(|v| range(v, "color_temp").0).reduce(f64::max);
    let color_temp_max = temps.iter().filter_map(|v| range(v, "color_temp").1).reduce(f64::min);
    Some(z2m_light::Target {
        on,
        brightness,
        has_level: !levels.is_empty(),
        has_color_temp: !temps.is_empty(),
        color_temp_min,
        color_temp_max,
        // Zigbee2MQTT's "allEqual": every light starts at level 2 (Hue)
        turns_off_at_brightness1: !levels.is_empty() && levels.iter().all(|v| range(v, "level").0 == Some(2.0)),
    })
}

/// A command from MQTT: the device or group by name (or IEEE address). Light keys run as
/// Zigbee2MQTT's light converters do, the other keys set the hub's features.
async fn apply(hub: &Arc<Hub>, name: &str, command: Map<String, Json>) -> anyhow::Result<()> {
    let devices = hub.devices();
    let target = devices.iter().find(|d| topic_name(&d.device.name) == name || d.device.ieee == name);
    let group = hub.groups().into_iter().find(|g| topic_name(&g.name) == name);
    let (key, members): (String, Vec<&DeviceView>) = match (&target, &group) {
        (Some(d), _) => (d.device.ieee.clone(), vec![*d]),
        (None, Some(g)) => (
            format!("group:{}", g.id),
            g.members.iter().filter_map(|m| devices.iter().find(|d| d.device.ieee == m.ieee)).collect(),
        ),
        (None, None) => anyhow::bail!("no device or group named {name}"),
    };
    let light = light_target(&members).filter(|_| command.keys().any(|k| z2m_light::KEYS.contains(&k.as_str())));
    let (parts, rest, rest_first) = match &light {
        Some(t) => {
            let memory = LIGHT_MEMORY.lock().unwrap().get(&key).cloned().unwrap_or_default();
            let plan = z2m_light::plan(&command, t, &memory);
            LIGHT_MEMORY.lock().unwrap().insert(key.clone(), plan.memory);
            (plan.parts, plan.rest, plan.rest_first)
        }
        None => (Vec::new(), command, false),
    };

    let mut last_error = None;
    let mut done_any = false;
    let run_rest = async |done_any: &mut bool, last_error: &mut Option<anyhow::Error>| {
        for (key, value) in translate_rest(rest.clone()) {
            let result = match (&target, &group) {
                (Some(d), _) => hub.set(&d.device.ieee, &key, value).await.map(drop),
                (None, Some(g)) => hub.set_group(g.id, &key, &value),
                (None, None) => Ok(()),
            };
            match result {
                Ok(()) => *done_any = true,
                Err(e) => {
                    warn!("MQTT {name}/set {key}: {e:#}");
                    *last_error = Some(e);
                }
            }
        }
    };
    if rest_first {
        run_rest(&mut done_any, &mut last_error).await;
    }
    // Zigbee2MQTT runs every converter, logging the ones that fail
    for part in parts {
        let result = match part {
            Ok(part) => run_light(hub, &target, &group, &members, part).await,
            Err(e) => Err(e),
        };
        match result {
            Ok(()) => done_any = true,
            Err(e) => {
                warn!("MQTT {name}/set: {e:#}");
                last_error = Some(e);
            }
        }
    }
    if !rest_first {
        run_rest(&mut done_any, &mut last_error).await;
    }
    // Zigbee2MQTT publishes the state after a command, changed or not: a client that lost
    // track (it thought a light off that was on) is corrected by its next command
    if done_any {
        members.iter().for_each(|m| hub.republish(&m.device.ieee));
    }
    match last_error {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// Sends one light converter's commands and takes its state
async fn run_light(hub: &Arc<Hub>, target: &Option<&DeviceView>, group: &Option<Group>, members: &[&DeviceView], part: z2m_light::Part) -> anyhow::Result<()> {
    for step in &part.steps {
        match step {
            z2m_light::Step::Command { cluster, command, payload } => match (target, group) {
                (Some(d), _) => hub.send_zcl_command(&d.device.ieee, *cluster, *command, payload.clone()).await?,
                (None, Some(g)) => hub.send_group_zcl(g.id, *cluster, *command, payload, Vec::new())?,
                (None, None) => {}
            },
            z2m_light::Step::Read { cluster, attributes, after_ms } => {
                tokio::time::sleep(Duration::from_millis(*after_ms)).await;
                // A group is read through its first member that has the cluster
                let reader = members.iter().find(|m| m.device.endpoint_with(*cluster).is_some());
                if let Some(m) = reader {
                    hub.read_attrs(&m.device.ieee, *cluster, attributes).await?;
                }
            }
        }
    }
    for m in members {
        let values: Vec<(&'static str, Json)> = part.state.iter().filter(|(k, _)| m.features.iter().any(|f| f.key == *k)).cloned().collect();
        if !values.is_empty() {
            hub.set_state_quiet(&m.device.ieee, values);
        }
    }
    Ok(())
}

/// Zigbee2MQTT's names and words for the hub's other features
fn translate_rest(command: Map<String, Json>) -> Vec<(String, Json)> {
    command
        .into_iter()
        .map(|(key, mut value)| {
            let key = match key.as_str() {
                "current_heating_setpoint" | "occupied_heating_setpoint" => "setpoint".to_owned(),
                _ => key,
            };
            if let Some((on, off)) = binary_words(&key)
                && let Some(word) = value.as_str()
            {
                value = match word {
                    w if w.eq_ignore_ascii_case(on) => json!(true),
                    w if w.eq_ignore_ascii_case(off) => json!(false),
                    _ => value,
                };
            }
            (key, value)
        })
        .collect()
}

/// `<name>/get`: reads what the keys stand for, then publishes the state
async fn get(hub: &Arc<Hub>, name: &str, keys: Vec<String>) -> anyhow::Result<()> {
    let devices = hub.devices();
    let target = devices.iter().find(|d| topic_name(&d.device.name) == name || d.device.ieee == name);
    let group = hub.groups().into_iter().find(|g| topic_name(&g.name) == name);
    let reader = match (&target, &group) {
        (Some(d), _) => d.device.ieee.clone(),
        (None, Some(g)) => g.members.first().context("the group has no members")?.ieee.clone(),
        (None, None) => anyhow::bail!("no device or group named {name}"),
    };
    let mut last_error = None;
    for key in keys {
        let result = match z2m_light::get_reads(&key) {
            Some((cluster, attrs)) => hub.read_attrs(&reader, cluster, attrs).await,
            None => hub.read_feature(&reader, &translate_rest(Map::from_iter([(key, Json::Null)]))[0].0).await,
        };
        if let Err(e) = result {
            last_error = Some(e);
        }
    }
    hub.republish(&reader);
    match last_error {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devices;
    use crate::store::Device;

    fn view(model: &str, state: Json) -> DeviceView {
        let mut device = Device::new("00:17:88:01:0b:1d:bc:29".into(), 0x1234);
        device.name = "Living room".into();
        device.model = Some(model.into());
        device.state = serde_json::from_value(state).unwrap();
        DeviceView {
            card: devices::card(&device),
            definition: devices::definition_info(&device),
            features: devices::features(&device).into_iter().copied().collect(),
            interviewing: false,
            waiting_for_wake: false,
            pending: Vec::new(),
            offline: false,
            device,
        }
    }

    fn cfg() -> MqttSettings {
        MqttSettings {
            enabled: true,
            host: "pi".into(),
            ..MqttSettings::default()
        }
    }

    #[test]
    fn light_state_uses_zigbee2mqtt_names() {
        let v = view("LTA009", json!({"on": true, "level": 138.0, "color_temp": 454.0, "action": "x"}));
        let p = device_payload(&v);
        assert_eq!(p["state"], "ON");
        assert_eq!(p["brightness"], 138.0);
        assert_eq!(p["color_mode"], "color_temp");
        assert!(p.get("action").is_none());
    }

    #[test]
    fn light_discovery_is_one_light_plus_settings() {
        let v = view("LTA009", json!({"color_temp_min": 153, "color_temp_max": 454}));
        let configs = device_discovery(&v, &cfg(), "zigbee2mqtt/Living room");
        let light = &configs["homeassistant/light/zhub_001788010b1dbc29/light/config"];
        assert_eq!(light["schema"], "json");
        assert_eq!(light["command_topic"], "zigbee2mqtt/Living room/set");
        assert_eq!(light["supported_color_modes"], json!(["color_temp"]));
        assert_eq!(light["max_mireds"], 454.0);
        // Covered by the light: no separate brightness entity
        assert!(!configs.keys().any(|k| k.contains("/level/")));
        let power_on = configs.iter().find(|(k, _)| k.contains("/power_on_behavior/")).unwrap().1;
        assert_eq!(power_on["options"], json!(["off", "on", "toggle", "previous"]));
        assert_eq!(power_on["command_template"], r#"{"power_on_behavior": "{{ value }}"}"#);
        assert!(configs.keys().any(|k| k.starts_with("homeassistant/button/") && k.contains("/identify/")));
    }

    #[test]
    fn thermostat_is_a_climate_entity() {
        let mut v = view("TRVZB", json!({"setpoint": 21.0, "system_mode": "heat"}));
        v.device.ieee = "0c:ae:5f:ff:fe:b4:5b:eb".into();
        let configs = device_discovery(&v, &cfg(), "zigbee2mqtt/Valve");
        let climate = configs.iter().find(|(k, _)| k.starts_with("homeassistant/climate/")).unwrap().1;
        assert_eq!(climate["modes"], json!(["off", "auto", "heat"]));
        assert_eq!(climate["min_temp"], 4.0);
        assert_eq!(climate["temp_step"], 0.5);
        assert_eq!(climate["temperature_command_template"], r#"{"setpoint": {{ value }}}"#);
        let lock = configs.iter().find(|(k, _)| k.contains("/child_lock/")).unwrap();
        assert!(lock.0.starts_with("homeassistant/switch/"));
        assert_eq!(lock.1["payload_on"], r#"{"child_lock": true}"#);
        // No entity for the schedule
        assert!(!configs.keys().any(|k| k.contains("weekly_schedule")));
    }

    #[test]
    fn switch_actions_become_device_triggers() {
        let v = view("RDM001", json!({}));
        let configs = device_discovery(&v, &cfg(), "zigbee2mqtt/Wall switch");
        let trigger = configs.iter().find(|(k, _)| k.ends_with("/action_left_press/config")).unwrap().1;
        assert_eq!(trigger["topic"], "zigbee2mqtt/Wall switch/action");
        assert_eq!(trigger["payload"], "left_press");
        assert_eq!(configs.keys().filter(|k| k.contains("device_automation")).count(), 12);
    }

    #[test]
    fn wildcards_leave_topic_names() {
        assert_eq!(topic_name("Lamp #1 + 2"), "Lamp _1 _ 2");
    }

    #[test]
    fn exposes_use_zigbee2mqtt_names() {
        let props = |e: &[Json]| -> Vec<String> {
            e.iter()
                .flat_map(|x| x.get("features").and_then(Json::as_array).cloned().unwrap_or_else(|| vec![x.clone()]))
                .filter_map(|x| x["property"].as_str().map(str::to_owned))
                .collect()
        };
        let light = exposes(&view("LTA009", json!({"on": true, "level": 100, "color_temp": 300})));
        assert_eq!(light[0]["type"], "light");
        let p = props(&light);
        for key in ["state", "brightness", "color_temp", "power_on_behavior", "linkquality"] {
            assert!(p.iter().any(|k| k == key), "{key} in {p:?}");
        }
        assert!(!p.iter().any(|k| k == "on" || k == "level"));

        let trv = view("TRVZB", json!({"setpoint": 21.5, "heating": true, "temperature": 20.1}));
        let e = exposes(&trv);
        assert_eq!(e[0]["type"], "climate");
        let p = props(&e);
        for key in ["current_heating_setpoint", "system_mode", "running_state", "temperature", "battery"] {
            assert!(p.iter().any(|k| k == key), "{key} in {p:?}");
        }
        assert!(!p.iter().any(|k| k == "setpoint" || k == "weekly_schedule"));
        let payload = device_payload(&trv);
        assert_eq!(payload["current_heating_setpoint"], 21.5);
        assert_eq!(payload["running_state"], "heat");
    }

    #[test]
    fn child_lock_and_open_window_as_zigbee2mqtt_words() {
        let trv = view("TRVZB", json!({"child_lock": true, "open_window": false}));
        let payload = device_payload(&trv);
        assert_eq!(payload["child_lock"], "LOCK");
        assert_eq!(payload["open_window"], "OFF");
        let e = exposes(&trv);
        let lock = e.iter().find(|x| x["property"] == "child_lock").unwrap();
        assert_eq!((lock["value_on"].clone(), lock["value_off"].clone()), (json!("LOCK"), json!("UNLOCK")));
    }

}
