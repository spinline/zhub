//! The Zigbee hub: brings up the stack, handles incoming traffic, interviews joining
//! devices and sends the user's commands.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::future::Future;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use serde::Serialize;
use serde_json::{Value as Json, json};
use tokio::sync::{Notify, broadcast, mpsc, oneshot, watch};
use tracing::{debug, info, warn};

use ziggurat_driver::runtime::TokioSpawner;
use ziggurat_driver::zigbee_stack::{
    ApsAck, DeviceLeaveReason, NetworkConfig, NwkDeviceType, RouteDirective, Tunables,
    TxPriority, ZigbeeNotification, ZigbeeStack,
};
use ziggurat_driver::ziggurat_ieee_802154::types::{Eui64, Nwk, PanId};
use ziggurat_driver::ziggurat_zigbee::constants::WELL_KNOWN_LINK_KEY;
use ziggurat_phy::{ExclusiveRadio, RadioPhy, TxFrame};
use ziggurat_phy_spinel::SpinelPhy;
use ziggurat_spinel::client::SpinelClient;
use ziggurat_protocol as proto;

use crate::devices::{self, Card, CommandFrame, Feature, Kind, Source, TimeBase, Write};
use crate::store::{
    self, Child, Device, LinkKey, NetworkData, Persisted, eui64_to_string, key_to_string,
    parse_eui64, parse_key,
};
use crate::zcl::{self, ReportConfig, Value, cluster, types};
use crate::{tuya, zdo};

/// The coordinator's own endpoint; bindings and reports arrive here
pub const HUB_ENDPOINT: u8 = 1;
const HA_PROFILE: u16 = 0x0104;
const ZDO_PROFILE: u16 = 0x0000;
const GREEN_POWER_PROFILE: u16 = 0xA1E0;
const GREEN_POWER_ENDPOINT: u8 = 242;
/// NWK radius; 0 would make the first relay drop the frame
const RADIUS: u8 = 30;
/// The stack reports the frame counter every 100 frames. This much is added to the
/// saved value on restart so the counter never goes backwards.
const FRAME_COUNTER_MARGIN: u32 = 1000;
const SAVE_DELAY: Duration = Duration::from_secs(5);
const TABLE_SNAPSHOT_INTERVAL: Duration = Duration::from_secs(300);
const AVAILABILITY_CHECK: Duration = Duration::from_secs(30);

/// Puts `patch` into `target`: objects merge key by key, anything else replaces
fn merge_json(target: &mut Json, patch: Json) {
    match (target, patch) {
        (Json::Object(target), Json::Object(patch)) => {
            for (key, value) in patch {
                merge_json(target.entry(key).or_insert(Json::Null), value);
            }
        }
        (target, patch) => *target = patch,
    }
}

/// Zigbee2MQTT's availability: no message for longer than the device type's limit
fn offline(settings: &store::Settings, device: &Device) -> bool {
    let Some(seen) = device.last_seen else {
        return false;
    };
    let minutes = if device.rx_on_when_idle {
        settings.availability_mains_minutes
    } else {
        settings.availability_battery_minutes
    };
    now_unix().saturating_sub(seen) > u64::from(minutes) * 60
}
const REPLY_TIMEOUT: Duration = Duration::from_secs(10);
/// A sleepy device only receives a request at its next poll
const SLEEPY_REPLY_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_PERMIT_SECONDS: u8 = 254;
/// Fast poll duration requested in the check-in response (quarter seconds): 20 s
const FAST_POLL_QUARTER_SECONDS: u16 = 80;
/// How long writes to a sleepy device are attempted right away, and the reply wait of each
/// attempt (the parent keeps the frame 7.68 s, then the device answers)
const EAGER_WRITE_WINDOW: Duration = Duration::from_secs(60);
const EAGER_WRITE_WAIT: Duration = Duration::from_secs(10);
/// How long a write waits for another write to the same device to finish
const SLOT_WAIT: Duration = Duration::from_secs(3);
/// How long a request to a sleepy device (leave, bind) is repeated before giving up
const SLEEPY_REQUEST_WINDOW: Duration = Duration::from_secs(30);
/// Attributes per read request, so the response fits in one frame
const READ_CHUNK: usize = 4;
const SECONDS_1970_TO_2000: u64 = 946_684_800;

pub struct Options {
    pub device: String,
    pub baudrate: u32,
    pub state_path: PathBuf,
    pub channel: u8,
    pub tx_power: i8,
    /// Diagnostics: put the radio in promiscuous mode and log MAC command frames
    pub sniff: bool,
    pub logging: Arc<crate::logging::Logging>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    Device { device: DeviceView },
    Removed { ieee: String },
    PermitJoin { remaining: u64 },
    /// The hub settings changed (the UI reloads them)
    Settings,
    /// Groups were created, renamed, deleted or changed members
    Groups { groups: Vec<store::Group> },
    /// A button press or other command the device sent (Zigbee2MQTT's `action`)
    Action {
        ieee: String,
        name: String,
        key: String,
        action: String,
    },
}

#[derive(Debug, Clone, Serialize)]
pub struct DeviceView {
    #[serde(flatten)]
    pub device: Device,
    pub card: Card,
    /// Vendor and description of the matched definition (as in Zigbee2MQTT)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub definition: Option<devices::DefinitionInfo>,
    /// The UI schema: the device's features
    pub features: Vec<Feature>,
    pub interviewing: bool,
    /// A sleepy device to be interviewed when it wakes up
    pub waiting_for_wake: bool,
    /// Features to write when the device wakes up
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub pending: Vec<String>,
    /// No message for longer than the availability limit
    pub offline: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum PendingKey {
    Zdo { cluster: u16, tsn: u8 },
    Zcl { nwk: u16, seq: u8 },
}

pub struct Hub {
    stack: Arc<ZigbeeStack<SpinelPhy>>,
    /// For raw frames and channel changes (Hue reset); normal traffic goes through the stack
    phy: Arc<SpinelPhy>,
    ieee: Eui64,
    state_path: PathBuf,
    data: Mutex<Persisted>,
    dirty: Notify,
    pending: Mutex<HashMap<PendingKey, oneshot::Sender<Vec<u8>>>>,
    seq: AtomicU8,
    events: broadcast::Sender<Event>,
    permit_until: Mutex<Option<Instant>>,
    interviews: Mutex<HashSet<String>>,
    /// Values to write to sleepy devices when they wake: ieee → (feature → value)
    pending_writes: Mutex<HashMap<String, BTreeMap<String, Json>>>,
    /// Sleepy devices to re-interview when they wake
    wake_interviews: Mutex<HashSet<String>>,
    /// Devices being written to right now: one write at a time per device
    writing: Mutex<HashSet<String>>,
    /// Sleepy devices whose writes are being attempted right now
    eager_writes: Mutex<HashSet<String>>,
    /// The last action frame per device (cluster, ZCL sequence), to drop repeats
    last_action_seq: Mutex<HashMap<String, (u16, u8)>>,
    /// For radio properties the stack does not manage at runtime (TX power)
    spinel: Arc<SpinelClient>,
    /// The RCP firmware's version string
    ncp_version: Option<String>,
    started: Instant,
    serial_port: String,
    baudrate: u32,
    logging: Arc<crate::logging::Logging>,
    /// Asks main to shut down; the service script starts zhub again
    restart: Notify,
    /// The MQTT bridge restarts when these change
    mqtt_settings: watch::Sender<store::MqttSettings>,
    /// The Matter bridge, likewise
    matter_settings: watch::Sender<store::MatterSettings>,
    matter_status: Mutex<Json>,
    matter_commands: mpsc::UnboundedSender<MatterCommand>,
    /// Taken by the Matter bridge when it starts
    matter_command_rx: Mutex<Option<mpsc::UnboundedReceiver<MatterCommand>>>,
    /// Devices last seen offline by the availability check
    offline: Mutex<HashSet<String>>,
    mqtt_status: Mutex<String>,
}

/// Requests from the UI to the Matter bridge
#[derive(Debug, Clone, Copy)]
pub enum MatterCommand {
    /// Lets a controller add the bridge (another controller, or after losing the pairing)
    OpenCommissioning,
    /// Forgets every controller: removes the fabrics and opens commissioning
    Reset,
}

/// What the settings page shows about the hub
#[derive(Debug, Serialize)]
pub struct HubInfo {
    pub version: &'static str,
    pub uptime_seconds: u64,
    pub radio_firmware: Option<String>,
    pub ieee: String,
    pub devices: usize,
    pub channel: u8,
    pub pan_id: u16,
    pub extended_pan_id: String,
    pub tx_power: i8,
    pub serial_port: String,
    pub baudrate: u32,
    pub log_level: String,
    pub settings: Json,
    pub mqtt_status: String,
    pub matter: Json,
}

/// Transmit power the MG24 RCP accepts (dBm)
const TX_POWER_RANGE: std::ops::RangeInclusive<i8> = -10..=20;

/// A binding with its destination's name (the hub or a known device)
#[derive(Debug, Serialize)]
pub struct BindingView {
    #[serde(flatten)]
    pub binding: zdo::Binding,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dst_name: Option<String>,
}

fn no_reply_hint(e: anyhow::Error, what: &str) -> anyhow::Error {
    if e.is::<NoReply>() {
        anyhow!("the device did not answer, so it could not {what}. Battery devices: press its button and try again")
    } else {
        e
    }
}

/// The device did not answer a request in time
#[derive(Debug)]
struct NoReply;

impl std::fmt::Display for NoReply {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the device did not respond")
    }
}

impl std::error::Error for NoReply {}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn network_config(network: &NetworkData) -> Result<NetworkConfig> {
    Ok(NetworkConfig {
        role: NwkDeviceType::Coordinator,
        channel: network.channel,
        update_id: network.update_id,
        pan_id: PanId(network.pan_id),
        extended_pan_id: parse_eui64(&network.extended_pan_id)?,
        network_address: Nwk(0x0000),
        ieee_address: parse_eui64(&network.ieee)?,
        network_key: parse_key(&network.network_key)?,
        network_key_seq_number: network.network_key_seq,
        network_key_tx_counter: network.nwk_frame_counter,
        tc_link_key: WELL_KNOWN_LINK_KEY,
        tclk_seed: None,
        tx_power: network.tx_power,
        source_routing: false,
    })
}

/// Drops the written value from the queue if it is still the queued one; a newer value
/// entered while the write was in flight stays and is written at the next wake-up
fn settle_pending(pending: &mut HashMap<String, BTreeMap<String, Json>>, ieee: &str, key: &str, written: &Json) {
    let Some(entries) = pending.get_mut(ieee) else {
        return;
    };
    if entries.get(key) == Some(written) {
        entries.remove(key);
    }
    if entries.is_empty() {
        pending.remove(ieee);
    }
}

/// No two devices share a name: the last four IEEE digits are appended if needed
fn unique_name(base: String, ieee: &str, taken: &[String]) -> String {
    if !taken.contains(&base) {
        return base;
    }
    let suffix = ieee.replace(':', "");
    format!("{base} {}", &suffix[suffix.len().saturating_sub(4)..])
}

/// Whether the user kept the automatic name: "Device xxxx" or a default an earlier
/// version assigned (the Turkish ones, or the model code)
fn untouched_name(device: &Device) -> bool {
    /// Automatic names of earlier, Turkish versions
    const LEGACY: &[&str] = &["Radyatör vanası", "Termostat"];
    let generated = Device::new(device.ieee.clone(), device.nwk).name;
    device.name == generated
        || device.name == generated.replacen("Device", "Cihaz", 1)
        || LEGACY.contains(&device.name.as_str())
        || device.model.as_deref() == Some(device.name.as_str())
}

/// Drops stale state keys the definition does not know (e.g. after the definition changed)
fn prune_state(device: &mut Device) {
    let keys: Vec<&str> = devices::features(device).iter().map(|f| f.key).collect();
    device.state.retain(|key, _| keys.contains(&key.as_str()));
}

/// Gives interviewed devices that still have an automatic name a name from their
/// definition or model; clears stale state keys
fn apply_default_names(data: &mut Persisted) {
    let ieees: Vec<String> = data.devices.keys().cloned().collect();
    for ieee in ieees {
        let taken: Vec<String> = data
            .devices
            .values()
            .filter(|d| d.ieee != ieee)
            .map(|d| d.name.clone())
            .collect();
        let Some(device) = data.devices.get_mut(&ieee) else {
            continue;
        };
        if !device.interviewed {
            continue;
        }
        prune_state(device);
        if untouched_name(device)
            && let Some(base) = devices::default_name(device)
        {
            device.name = unique_name(base, &device.ieee, &taken);
        }
    }
}

/// Applies ZCL attribute records to the device state through its definition's features
fn apply_zcl_records(device: &mut Device, cluster_id: u16, manufacturer: Option<u16>, records: &[zcl::AttributeRecord]) {
    if cluster_id == cluster::BASIC {
        for record in records {
            let text = record.value.as_ref().and_then(Value::as_str).filter(|s| !s.is_empty());
            match record.id {
                0x0004 => device.manufacturer = text.map(str::to_owned),
                0x0005 => device.model = text.map(str::to_owned),
                0x4000 => device.firmware = text.map(str::to_owned),
                _ => {}
            }
        }
    }
    // If the model was just learned the definition may have changed: fetch features now
    for feature in devices::features(device) {
        let Source::Attr {
            cluster,
            attr,
            conv,
            manufacturer: code,
            ..
        } = feature.source
        else {
            continue;
        };
        // A manufacturer-specific attribute id only means that attribute with the code
        if cluster != cluster_id || code.is_some_and(|code| manufacturer != Some(code)) {
            continue;
        }
        for record in records.iter().filter(|r| r.id == attr) {
            let decoded = match &record.value {
                Some(Value::Str(text)) => conv.decode_text(text),
                Some(value) => value.as_i64().and_then(|raw| feature.decode_int(raw)),
                None => None,
            };
            if let Some(decoded) = decoded {
                device.state.insert(feature.key.to_owned(), decoded);
            }
        }
    }
}

/// Applies Tuya datapoints to the device state; returns the ones the definition does not know
fn apply_tuya_datapoints<'a>(device: &mut Device, datapoints: &'a [tuya::Datapoint]) -> Vec<&'a tuya::Datapoint> {
    let features = devices::features(device);
    let mut unknown = Vec::new();
    for dp in datapoints {
        let matching: Vec<_> = features.iter().filter(|f| f.matches_dp(dp.id)).collect();
        if matching.is_empty() {
            unknown.push(dp);
        }
        for feature in matching {
            let value = match feature.source {
                Source::Tuya { conv, .. } => match dp.as_int() {
                    Some(raw) => feature.decode_int(raw),
                    None => conv.decode_text(&String::from_utf8_lossy(&dp.data)),
                },
                Source::TuyaSchedule { divisor, .. } => Some(tuya::decode_schedule(&dp.data, divisor)),
                Source::Attr { .. } | Source::Command(_) | Source::Actions(_) | Source::ThermostatSchedule => None,
            };
            if let Some(value) = value {
                device.state.insert(feature.key.to_owned(), value);
            }
        }
    }
    unknown
}

/// Converts a feature value to a Tuya datapoint
fn tuya_datapoint(id: u8, dp_type: u8, raw: i64) -> Result<tuya::Datapoint> {
    Ok(match dp_type {
        tuya::dp_type::BOOL => tuya::Datapoint::bool(id, raw != 0),
        tuya::dp_type::ENUM => tuya::Datapoint::enumeration(id, u8::try_from(raw).context("option value does not fit")?),
        tuya::dp_type::VALUE => tuya::Datapoint::value(id, i32::try_from(raw).context("value does not fit")?),
        other => bail!("Tuya datapoint type {other} cannot be written"),
    })
}

/// The hub's Time cluster: a Read Attributes Response payload for the requested attributes
fn time_read_response(ids: &[u16], unix_now: u64, utc_offset: i64, base: TimeBase) -> Vec<u8> {
    let time = match base {
        TimeBase::Zigbee2000 => unix_now.saturating_sub(SECONDS_1970_TO_2000),
        TimeBase::Unix1970 => unix_now,
    } as i64;
    let mut out = Vec::new();
    for &id in ids {
        out.extend_from_slice(&id.to_le_bytes());
        let (ty, value): (u8, Vec<u8>) = match id {
            0x0000 => (types::UTC_TIME, (time as u32).to_le_bytes().to_vec()),
            // Bit 0: this device is the time master
            0x0001 => (types::BITMAP8, vec![0x01]),
            0x0002 => (types::INT32, (utc_offset as i32).to_le_bytes().to_vec()),
            0x0007 => (types::UINT32, ((time + utc_offset) as u32).to_le_bytes().to_vec()),
            _ => {
                out.push(0x86); // UNSUPPORTED_ATTRIBUTE
                continue;
            }
        };
        out.push(zcl::STATUS_SUCCESS);
        out.push(ty);
        out.extend(value);
    }
    out
}

async fn retry<T, F, Fut>(what: &str, mut attempt: F) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T>>,
{
    let mut last_error = None;
    for n in 1..=3 {
        match attempt().await {
            Ok(value) => return Ok(value),
            Err(e) => {
                debug!("{what} attempt {n} failed: {e:#}");
                last_error = Some(e);
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
    }
    Err(last_error.unwrap_or_else(|| anyhow!("{what} failed")))
}

impl Hub {
    pub async fn start(opts: Options) -> Result<Arc<Self>> {
        let mut data = Persisted::load(&opts.state_path)?;

        let (phy, spinel) = crate::radio::open(&opts.device, opts.baudrate, opts.sniff).await?;
        let radio_ieee = phy
            .hw_address()
            .await
            .map_err(|e| anyhow!("cannot read the radio address: {e}"))?;
        info!("Radio ready: {}", eui64_to_string(&radio_ieee));
        let ncp_version = match spinel.prop_value_get(ziggurat_spinel::SpinelPropertyId::NcpVersion).await {
            Ok(bytes) => {
                let text = String::from_utf8_lossy(&bytes).trim_end_matches('\0').trim().to_owned();
                info!("Radio firmware: {text}");
                Some(text)
            }
            Err(e) => {
                warn!("cannot read the radio firmware version: {e:?}");
                None
            }
        };
        if let Some(level) = &data.settings.log_level
            && let Err(e) = opts.logging.set_level(level)
        {
            warn!("saved log level not applied: {e:#}");
        }

        let mut network = match data.network.take() {
            Some(network) => network,
            None => {
                let network = store::new_network(&radio_ieee, opts.channel, opts.tx_power);
                info!(
                    "No saved network; forming a new one (channel {}, PAN 0x{:04x})",
                    network.channel, network.pan_id
                );
                network
            }
        };
        network.nwk_frame_counter = network.nwk_frame_counter.saturating_add(FRAME_COUNTER_MARGIN);
        network.aps_frame_counter = network.aps_frame_counter.saturating_add(FRAME_COUNTER_MARGIN);
        data.network = Some(network.clone());
        // Counters hit the disk before use, so even a crash loop cannot rewind them
        data.save(&opts.state_path)?;

        let ieee = parse_eui64(&network.ieee)?;
        let stack = ZigbeeStack::new(
            phy.clone(),
            network_config(&network)?,
            Tunables::default(),
            TokioSpawner::default(),
        );
        stack
            .state
            .core
            .lock()
            .aib
            .aps_security
            .restore_outgoing_frame_counter(network.aps_frame_counter);
        restore_tables(&stack, &data);

        stack
            .start_network()
            .await
            .map_err(|e| anyhow!("cannot start the network: {e}"))?;
        stack.spawn_tracked(|stack| async move {
            stack.run().await;
        });
        if opts.sniff {
            // Disable the radio's filter so frames without a destination or for another
            // PAN show too. Ziggurat restores the setting if the radio resets.
            spinel
                .prop_value_set(ziggurat_spinel::SpinelPropertyId::MacPromiscuousMode, vec![2])
                .await
                .map_err(|e| anyhow!("cannot enable promiscuous mode: {e:?}"))?;
            warn!("DIAGNOSTICS: radio in promiscuous mode, logging MAC command frames");
        }
        info!(
            "Network up: channel {}, PAN 0x{:04x}, {} devices",
            network.channel,
            network.pan_id,
            data.devices.len()
        );

        apply_default_names(&mut data);
        let mqtt_settings = data.settings.mqtt.clone();
        let matter_settings = data.settings.matter.clone();
        let (matter_commands, matter_command_rx) = mpsc::unbounded_channel();
        // Writes the previous run could not deliver yet
        let pending_writes: HashMap<_, _> = std::mem::take(&mut data.pending_writes).into_iter().collect();
        if !pending_writes.is_empty() {
            info!("{} device(s) have writes waiting from before the restart", pending_writes.len());
        }

        let (events, _) = broadcast::channel(256);
        let hub = Arc::new(Self {
            stack,
            phy,
            ieee,
            state_path: opts.state_path,
            data: Mutex::new(data),
            dirty: Notify::new(),
            pending: Mutex::new(HashMap::new()),
            seq: AtomicU8::new(store::random_bytes::<1>()[0]),
            events,
            permit_until: Mutex::new(None),
            interviews: Mutex::new(HashSet::new()),
            pending_writes: Mutex::new(pending_writes),
            wake_interviews: Mutex::new(HashSet::new()),
            writing: Mutex::new(HashSet::new()),
            eager_writes: Mutex::new(HashSet::new()),
            last_action_seq: Mutex::new(HashMap::new()),
            spinel,
            ncp_version,
            started: Instant::now(),
            serial_port: opts.device,
            baudrate: opts.baudrate,
            logging: opts.logging,
            restart: Notify::new(),
            mqtt_settings: watch::Sender::new(mqtt_settings),
            matter_settings: watch::Sender::new(matter_settings),
            matter_status: Mutex::new(json!({"supported": cfg!(feature = "matter"), "state": "disabled"})),
            matter_commands,
            matter_command_rx: Mutex::new(Some(matter_command_rx)),
            mqtt_status: Mutex::new("disabled".into()),
            offline: Mutex::new(HashSet::new()),
        });

        tokio::spawn(hub.clone().notification_loop());
        tokio::spawn(hub.clone().saver());
        tokio::spawn(hub.clone().table_snapshots());
        tokio::spawn(hub.clone().availability());

        // Resume interviews the previous run left unfinished
        let unfinished: Vec<String> = hub
            .data
            .lock()
            .unwrap()
            .devices
            .values()
            .filter(|d| !d.interviewed)
            .map(|d| d.ieee.clone())
            .collect();
        for ieee in unfinished {
            hub.spawn_interview(ieee);
        }

        Ok(hub)
    }

    // ---- Hub settings ----

    pub fn info(&self) -> HubInfo {
        let data = self.data.lock().unwrap();
        let network = data.network.as_ref();
        HubInfo {
            version: env!("CARGO_PKG_VERSION"),
            uptime_seconds: self.started.elapsed().as_secs(),
            radio_firmware: self.ncp_version.clone(),
            ieee: eui64_to_string(&self.ieee),
            devices: data.devices.len(),
            channel: network.map_or(0, |n| n.channel),
            pan_id: network.map_or(0, |n| n.pan_id),
            extended_pan_id: network.map(|n| n.extended_pan_id.clone()).unwrap_or_default(),
            tx_power: network.map_or(0, |n| n.tx_power),
            serial_port: self.serial_port.clone(),
            baudrate: self.baudrate,
            log_level: self.logging.level(),
            settings: data.settings.public(),
            mqtt_status: self.mqtt_status.lock().unwrap().clone(),
            matter: self.matter_status.lock().unwrap().clone(),
        }
    }

    /// For the API (no MQTT password)
    pub fn public_settings(&self) -> Json {
        self.data.lock().unwrap().settings.public()
    }

    pub fn matter_settings(&self) -> watch::Receiver<store::MatterSettings> {
        self.matter_settings.subscribe()
    }

    pub fn set_matter_status(&self, status: impl Serialize) {
        if let Ok(value) = serde_json::to_value(status) {
            *self.matter_status.lock().unwrap() = value;
        }
    }

    pub fn take_matter_commands(&self) -> Option<mpsc::UnboundedReceiver<MatterCommand>> {
        self.matter_command_rx.lock().unwrap().take()
    }

    pub fn matter_command(&self, command: MatterCommand) -> Result<()> {
        if !cfg!(feature = "matter") {
            bail!("this build has no Matter support");
        }
        if !self.data.lock().unwrap().settings.matter.enabled {
            bail!("Matter is disabled");
        }
        self.matter_commands.send(command).map_err(|_| anyhow!("the Matter bridge is not running"))
    }

    // ---- For the Matter bridge ----

    /// Sends a cluster-specific ZCL command to the device. Matter's On/Off, Level Control,
    /// Color Control and Identify commands are the ZCL ones, ids included.
    pub async fn send_zcl_command(&self, ieee: &str, cluster_id: u16, command: u8, payload: Vec<u8>) -> Result<()> {
        let device = self.device(ieee).context("device not found")?;
        self.cluster_command(
            &device,
            CommandFrame {
                cluster: cluster_id,
                manufacturer: None,
                command,
                payload,
            },
        )
        .await
    }

    /// Sends a cluster-specific ZCL command to a group (one multicast frame); the members
    /// that have the given features get `state` at once
    pub fn send_group_zcl(&self, id: u16, cluster_id: u16, command: u8, payload: &[u8], state: Vec<(&'static str, Json)>) -> Result<()> {
        let members = {
            let data = self.data.lock().unwrap();
            data.groups.iter().find(|g| g.id == id).context("group not found")?.members.clone()
        };
        let frame = zcl::frame(
            zcl::FC_CLUSTER_SPECIFIC | zcl::FC_DISABLE_DEFAULT_RESPONSE,
            self.next_seq(),
            command,
            payload,
        );
        self.stack
            .send_aps_groupcast(id, HA_PROFILE, cluster_id, HUB_ENDPOINT, RADIUS, frame, TxPriority::UserNormal)
            .map_err(|e| anyhow!("cannot send to the group: {e:?}"))?;
        for member in members {
            let Some(device) = self.device(&member.ieee) else {
                continue;
            };
            let features = devices::features(&device);
            let values: Vec<_> = state.iter().filter(|(k, _)| features.iter().any(|f| f.key == *k)).cloned().collect();
            if !values.is_empty() {
                self.set_state_values(&member.ieee, values);
            }
        }
        Ok(())
    }

    /// Sets state values ahead of the device's own report
    /// Reads attributes of a device; the answers update its state as reports do
    pub async fn read_attrs(&self, ieee: &str, cluster_id: u16, attrs: &[u16]) -> Result<()> {
        let device = self.device(ieee).context("device not found")?;
        let ep = device.endpoint_with(cluster_id).context("the device does not have this cluster")?;
        self.read_attributes(device.nwk, ep, cluster_id, None, attrs).await.map(drop)
    }

    /// Reads the attribute behind a feature (Zigbee2MQTT's `/get`)
    pub async fn read_feature(&self, ieee: &str, key: &str) -> Result<()> {
        let device = self.device(ieee).context("device not found")?;
        let feature = devices::features(&device).into_iter().find(|f| f.key == key).with_context(|| format!("unknown feature: {key}"))?;
        let Source::Attr { cluster, attr, manufacturer, .. } = feature.source else {
            bail!("{} cannot be read", feature.label);
        };
        let ep = device.endpoint_with(cluster).context("the device does not have this cluster")?;
        self.read_attributes(device.nwk, ep, cluster, manufacturer, &[attr]).await.map(drop)
    }

    /// Zigbee2MQTT's device options the hub takes; null removes one
    pub fn set_device_options(&self, ieee: &str, options: serde_json::Map<String, Json>) -> Result<()> {
        self.device(ieee).context("device not found")?;
        for (key, value) in &options {
            match key.as_str() {
                "transition" if value.is_null() => {}
                "transition" => {
                    value
                        .as_f64()
                        .filter(|t| t.is_finite() && (0.0..=6553.4).contains(t))
                        .context("transition: seconds, 0 to 6553.4")?;
                }
                _ => bail!("unknown option: {key}"),
            }
        }
        self.update_device(ieee, |d| {
            for (key, value) in options {
                if value.is_null() {
                    d.options.remove(&key);
                } else {
                    d.options.insert(key, value);
                }
            }
        });
        Ok(())
    }

    /// Sets state values without telling anyone yet (a `republish` follows)
    pub fn set_state_quiet(&self, ieee: &str, values: Vec<(&'static str, Json)>) {
        let mut data = self.data.lock().unwrap();
        if let Some(device) = data.devices.get_mut(ieee) {
            for (key, value) in values {
                device.state.insert(key.to_owned(), value);
            }
            drop(data);
            self.dirty.notify_one();
        }
    }

    pub fn set_state_values(&self, ieee: &str, values: Vec<(&'static str, Json)>) {
        self.update_device(ieee, |d| {
            for (key, value) in values {
                d.state.insert(key.to_owned(), value);
            }
        });
    }

    pub fn device_view(&self, ieee: &str) -> Option<DeviceView> {
        let data = self.data.lock().unwrap();
        data.devices.get(ieee).map(|d| self.view(d, &data.settings))
    }

    /// The device's Matter endpoint: allocated once, from 2 up, never reused
    pub fn matter_endpoint(&self, ieee: &str) -> u16 {
        let mut data = self.data.lock().unwrap();
        if let Some(&id) = data.matter_endpoints.get(ieee) {
            return id;
        }
        let id = data.matter_endpoints.values().max().map_or(2, |max| max + 1);
        data.matter_endpoints.insert(ieee.to_owned(), id);
        drop(data);
        self.dirty.notify_one();
        id
    }

    /// Tells everyone when a device goes offline or comes back: being offline is no
    /// message for a while, which no message announces
    async fn availability(self: Arc<Self>) {
        loop {
            tokio::time::sleep(AVAILABILITY_CHECK).await;
            let now_offline: HashSet<String> = {
                let data = self.data.lock().unwrap();
                data.devices.values().filter(|d| offline(&data.settings, d)).map(|d| d.ieee.clone()).collect()
            };
            let changed: Vec<String> = {
                let mut offline = self.offline.lock().unwrap();
                let changed = offline.symmetric_difference(&now_offline).cloned().collect();
                *offline = now_offline;
                changed
            };
            for ieee in changed {
                let Some(view) = self.device_view(&ieee) else {
                    continue;
                };
                info!("{} is {}", view.device.name, if view.offline { "offline" } else { "online again" });
                let _ = self.events.send(Event::Device { device: view });
            }
        }
    }

    /// Where the hub keeps its files (the state file's directory)
    pub fn data_dir(&self) -> PathBuf {
        self.state_path.parent().map(PathBuf::from).unwrap_or_default()
    }

    pub fn mqtt_settings(&self) -> watch::Receiver<store::MqttSettings> {
        self.mqtt_settings.subscribe()
    }

    pub fn set_mqtt_status(&self, status: impl Into<String>) {
        *self.mqtt_status.lock().unwrap() = status.into();
    }

    pub fn hub_ieee(&self) -> String {
        eui64_to_string(&self.ieee)
    }

    /// Changes the settings the patch names (objects merge key by key)
    pub fn patch_settings(&self, patch: Json) -> Result<()> {
        let mut merged = serde_json::to_value(&self.data.lock().unwrap().settings)?;
        merge_json(&mut merged, patch);
        self.update_settings(serde_json::from_value(merged).context("invalid settings")?)
    }

    pub fn update_settings(&self, mut settings: store::Settings) -> Result<()> {
        settings.validate()?;
        // The setup code never comes from the page; it is drawn once
        {
            let current = &self.data.lock().unwrap().settings.matter;
            settings.matter.passcode = current.passcode;
            settings.matter.discriminator = current.discriminator;
        }
        if settings.matter.enabled {
            settings.matter.ensure_setup_code();
        }
        // The page never sees the password: an empty one keeps the saved one
        if settings.mqtt.password.as_deref().is_none_or(str::is_empty) {
            settings.mqtt.password = self.data.lock().unwrap().settings.mqtt.password.clone();
        }
        if let Some(level) = &settings.log_level {
            self.logging.set_level(level)?;
        }
        info!(
            "Settings changed: availability {}/{} min, permit join {} s, log level {:?}, MQTT {} {}:{}",
            settings.availability_mains_minutes,
            settings.availability_battery_minutes,
            settings.permit_join_seconds,
            settings.log_level,
            if settings.mqtt.enabled { "on" } else { "off" },
            settings.mqtt.host,
            settings.mqtt.port
        );
        self.matter_settings.send_if_modified(|current| {
            let changed = *current != settings.matter;
            *current = settings.matter.clone();
            changed
        });
        self.mqtt_settings.send_if_modified(|current| {
            let changed = *current != settings.mqtt;
            *current = settings.mqtt.clone();
            changed
        });
        self.data.lock().unwrap().settings = settings;
        self.dirty.notify_one();
        let _ = self.events.send(Event::Settings);
        Ok(())
    }

    /// Changes the radio's transmit power now and keeps it for the next start. The
    /// stack's own copy (used if the radio resets) changes only with a restart.
    pub async fn set_tx_power(&self, dbm: i8) -> Result<i8> {
        if !TX_POWER_RANGE.contains(&dbm) {
            bail!("transmit power must be {}..{} dBm", TX_POWER_RANGE.start(), TX_POWER_RANGE.end());
        }
        self.spinel
            .prop_value_set(ziggurat_spinel::SpinelPropertyId::PhyTxPower, vec![dbm as u8])
            .await
            .map_err(|e| anyhow!("the radio refused the transmit power: {e:?}"))?;
        // The radio may round to a level it supports
        let actual = match self.spinel.prop_value_get(ziggurat_spinel::SpinelPropertyId::PhyTxPower).await {
            Ok(bytes) => bytes.first().map_or(dbm, |&b| b as i8),
            Err(_) => dbm,
        };
        info!("Transmit power: {actual} dBm");
        if let Some(network) = self.data.lock().unwrap().network.as_mut() {
            network.tx_power = actual;
        }
        self.dirty.notify_one();
        Ok(actual)
    }

    pub fn request_restart(&self) {
        info!("Restart requested from the web UI");
        self.restart.notify_one();
    }

    pub async fn restart_requested(&self) {
        self.restart.notified().await;
    }

    pub fn log_lines(&self, after: u64) -> Vec<crate::logging::Line> {
        self.logging.buffer.since(after, 500)
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.events.subscribe()
    }

    pub fn devices(&self) -> Vec<DeviceView> {
        let data = self.data.lock().unwrap();
        data.devices.values().map(|d| self.view(d, &data.settings)).collect()
    }

    fn view(&self, device: &Device, settings: &store::Settings) -> DeviceView {
        DeviceView {
            offline: offline(settings, device),
            card: devices::card(device),
            definition: devices::definition_info(device),
            features: devices::features(device).into_iter().copied().collect(),
            interviewing: self.interviews.lock().unwrap().contains(&device.ieee),
            waiting_for_wake: self.wake_interviews.lock().unwrap().contains(&device.ieee),
            pending: self
                .pending_writes
                .lock()
                .unwrap()
                .get(&device.ieee)
                .map(|m| m.keys().cloned().collect())
                .unwrap_or_default(),
            device: device.clone(),
        }
    }

    fn device(&self, ieee: &str) -> Option<Device> {
        self.data.lock().unwrap().devices.get(ieee).cloned()
    }

    fn ieee_for_nwk(&self, nwk: u16) -> Option<String> {
        let data = self.data.lock().unwrap();
        data.devices
            .values()
            .find(|d| d.nwk == nwk)
            .map(|d| d.ieee.clone())
    }

    fn is_sleepy(&self, nwk: u16) -> bool {
        let data = self.data.lock().unwrap();
        data.devices
            .values()
            .find(|d| d.nwk == nwk)
            .is_some_and(|d| !d.rx_on_when_idle)
    }

    /// Changes a device; if anything changed, publishes it and schedules a save
    fn update_device(&self, ieee: &str, change: impl FnOnce(&mut Device)) {
        let view = {
            let mut guard = self.data.lock().unwrap();
            let data = &mut *guard;
            let Some(device) = data.devices.get_mut(ieee) else {
                return;
            };
            let before = serde_json::to_value(&*device).ok();
            change(device);
            if serde_json::to_value(&*device).ok() == before {
                return;
            }
            self.view(device, &data.settings)
        };
        let _ = self.events.send(Event::Device { device: view });
        self.dirty.notify_one();
    }

    /// Publishes a device's state again although it did not change: Zigbee2MQTT answers
    /// every command this way, which corrects a client that lost track
    pub fn republish(&self, ieee: &str) {
        self.emit_device(ieee);
    }

    fn emit_device(&self, ieee: &str) {
        if let Some(device) = self.device_view(ieee) {
            let _ = self.events.send(Event::Device { device });
        }
    }

    fn next_seq(&self) -> u8 {
        self.seq.fetch_add(1, Ordering::Relaxed)
    }

    // ---- Incoming traffic ----

    async fn notification_loop(self: Arc<Self>) {
        loop {
            for notification in self.stack.next_notifications().await {
                self.handle_notification(notification);
            }
        }
    }

    fn handle_notification(self: &Arc<Self>, notification: ZigbeeNotification) {
        match notification {
            ZigbeeNotification::ReceivedApsCommand {
                source,
                destination,
                profile_id,
                cluster_id,
                src_ep,
                dst_ep,
                lqi,
                rssi,
                data,
                ..
            } => {
                self.touch(source.0, lqi, rssi);
                self.flush_pending_writes(source.0);
                self.start_wake_interview(source.0);
                if profile_id == ZDO_PROFILE {
                    self.handle_zdo(source.0, destination.0, cluster_id, &data);
                } else {
                    self.handle_zcl(source.0, src_ep, dst_ep, cluster_id, &data);
                }
            }
            ZigbeeNotification::FrameCounterUpdate { frame_counter } => {
                if let Some(network) = self.data.lock().unwrap().network.as_mut() {
                    network.nwk_frame_counter = frame_counter;
                }
                self.dirty.notify_one();
            }
            ZigbeeNotification::ApsFrameCounterUpdate { frame_counter } => {
                if let Some(network) = self.data.lock().unwrap().network.as_mut() {
                    network.aps_frame_counter = frame_counter;
                }
                self.dirty.notify_one();
            }
            ZigbeeNotification::LinkKeyUpdate { ieee, key } => {
                let ieee = eui64_to_string(&ieee);
                let mut data = self.data.lock().unwrap();
                data.link_keys.retain(|k| k.ieee != ieee);
                data.link_keys.push(LinkKey {
                    ieee,
                    key: key_to_string(&key),
                    rx_counter: 0,
                });
                drop(data);
                self.dirty.notify_one();
            }
            ZigbeeNotification::DeviceJoined {
                nwk,
                ieee,
                device_type,
                rx_on_when_idle,
                ..
            } => {
                let router = matches!(device_type, Some(NwkDeviceType::Router));
                self.device_joined(eui64_to_string(&ieee), nwk.0, rx_on_when_idle, router);
            }
            ZigbeeNotification::DeviceLeft { nwk, ieee, reason } => {
                let ieee = ieee.map_or_else(|| self.ieee_for_nwk(nwk.0), |e| Some(eui64_to_string(&e)));
                info!("Device left: {ieee:?} ({reason:?})");
                // Forget a device that left without announcing a rejoin; sleepy children
                // that timed out rejoin by themselves
                if let (Some(ieee), DeviceLeaveReason::Announced { rejoin: false }) = (ieee, reason) {
                    self.forget(&ieee);
                }
            }
            ZigbeeNotification::ApsDecryptionFailure { source_ieee, .. } => {
                warn!(
                    "cannot decrypt an APS frame from {}; the device may need to be paired again",
                    eui64_to_string(&source_ieee)
                );
            }
            ZigbeeNotification::RouteRecord { .. } => {}
        }
    }

    fn touch(&self, nwk: u16, lqi: u8, rssi: i8) {
        let mut data = self.data.lock().unwrap();
        if let Some(device) = data.devices.values_mut().find(|d| d.nwk == nwk) {
            device.last_seen = Some(now_unix());
            device.lqi = Some(lqi);
            device.rssi = Some(rssi);
        }
    }

    fn device_joined(self: &Arc<Self>, ieee: String, nwk: u16, rx_on_when_idle: bool, router: bool) {
        let (needs_interview, changed) = {
            let mut data = self.data.lock().unwrap();
            let is_new = !data.devices.contains_key(&ieee);
            let device = data
                .devices
                .entry(ieee.clone())
                .or_insert_with(|| Device::new(ieee.clone(), nwk));
            let changed = is_new || device.nwk != nwk;
            device.nwk = nwk;
            device.rx_on_when_idle = rx_on_when_idle;
            device.router |= router;
            device.last_seen = Some(now_unix());
            (!device.interviewed, changed)
        };
        // The join, Update-Device and Device_annce may report the same event three times
        if changed {
            info!("Device joined: {ieee} (nwk 0x{nwk:04x})");
        }
        self.emit_device(&ieee);
        self.dirty.notify_one();
        if needs_interview {
            self.spawn_interview(ieee);
        }
    }

    fn complete(&self, key: PendingKey, payload: Vec<u8>) {
        if let Some(waiter) = self.pending.lock().unwrap().remove(&key) {
            let _ = waiter.send(payload);
        }
    }

    fn handle_zdo(self: &Arc<Self>, source: u16, destination: u16, cluster_id: u16, data: &[u8]) {
        let Some((&tsn, payload)) = data.split_first() else {
            return;
        };
        if cluster_id & zdo::cluster::RESPONSE != 0 {
            self.complete(
                PendingKey::Zdo {
                    cluster: cluster_id,
                    tsn,
                },
                payload.to_vec(),
            );
            return;
        }

        let to_us = destination == 0x0000;
        let reply = match cluster_id {
            zdo::cluster::DEVICE_ANNCE => {
                if let Ok((nwk, ieee, capability)) = zdo::parse_device_annce(payload) {
                    self.device_joined(
                        eui64_to_string(&Eui64(ieee)),
                        nwk,
                        zdo::annce_rx_on_when_idle(capability),
                        capability & 0x02 != 0,
                    );
                }
                None
            }
            zdo::cluster::NODE_DESC_REQ if to_us => {
                (zdo::parse_nwk_of_interest(payload) == Some(0x0000)).then(|| zdo::our_node_desc_rsp(0x0000))
            }
            zdo::cluster::ACTIVE_EP_REQ if to_us => Some(zdo::our_active_ep_rsp(0x0000, HUB_ENDPOINT)),
            zdo::cluster::SIMPLE_DESC_REQ if to_us => {
                (payload.get(2) == Some(&HUB_ENDPOINT)).then(|| zdo::our_simple_desc_rsp(0x0000, HUB_ENDPOINT))
            }
            zdo::cluster::MATCH_DESC_REQ => {
                // IAS sensors look for the hub with this; without an answer they never enroll
                matches!(
                    zdo::parse_nwk_of_interest(payload),
                    Some(0x0000 | 0xFFFC | 0xFFFD | 0xFFFF)
                )
                .then(|| zdo::our_match_desc_rsp(0x0000, HUB_ENDPOINT))
            }
            zdo::cluster::IEEE_ADDR_REQ => (zdo::parse_nwk_of_interest(payload) == Some(0x0000))
                .then(|| zdo::our_addr_rsp(self.ieee.0, 0x0000)),
            zdo::cluster::NWK_ADDR_REQ => (zdo::parse_nwk_addr_req_ieee(payload) == Some(self.ieee.0))
                .then(|| zdo::our_addr_rsp(self.ieee.0, 0x0000)),
            _ => None,
        };

        if let Some(body) = reply {
            let mut frame = vec![tsn];
            frame.extend(body);
            if let Err(e) = self.send(
                source,
                ZDO_PROFILE,
                cluster_id | zdo::cluster::RESPONSE,
                0,
                0,
                frame,
                false,
            ) {
                debug!("cannot send ZDO response: {e:#}");
            }
        }
    }

    fn handle_zcl(self: &Arc<Self>, source: u16, src_ep: u8, dst_ep: u8, cluster_id: u16, data: &[u8]) {
        let Some((header, payload)) = zcl::Header::parse(data) else {
            return;
        };
        let ieee = self.ieee_for_nwk(source);

        if !header.cluster_specific() {
            match header.command {
                // The device reads from the hub: only the Time cluster is served
                zcl::cmd::READ_ATTRIBUTES if !header.server_to_client() => {
                    if cluster_id == cluster::TIME {
                        self.answer_time_read(source, src_ep, dst_ep, &header, payload, ieee.as_deref());
                    }
                    return;
                }
                zcl::cmd::REPORT_ATTRIBUTES => {
                    if let Some(ieee) = &ieee {
                        self.apply_records(ieee, cluster_id, header.manufacturer, &zcl::parse_report(payload));
                    }
                    self.default_response(source, src_ep, cluster_id, &header);
                }
                zcl::cmd::READ_ATTRIBUTES_RSP => {
                    if let Some(ieee) = &ieee {
                        self.apply_records(ieee, cluster_id, header.manufacturer, &zcl::parse_read_response(payload));
                    }
                }
                _ => {}
            }
            self.complete(
                PendingKey::Zcl {
                    nwk: source,
                    seq: header.seq,
                },
                data.to_vec(),
            );
            return;
        }

        // A cluster-specific response (e.g. Add Group Response) answers a request of ours;
        // a frame nobody waits for passes through unchanged
        if header.server_to_client() {
            self.complete(
                PendingKey::Zcl {
                    nwk: source,
                    seq: header.seq,
                },
                data.to_vec(),
            );
        }

        if cluster_id == cluster::THERMOSTAT
            && header.server_to_client()
            && header.command == crate::thermostat::GET_WEEKLY_SCHEDULE_RSP
        {
            // TRVZB refuses Get Weekly Schedule (0x81), but a device may still send these
            if let (Some(ieee), Some((days, day))) = (&ieee, crate::thermostat::parse_get_rsp(payload)) {
                self.update_device(ieee, |d| {
                    let merged = crate::thermostat::merge(d.state.get("weekly_schedule"), &days, &day);
                    d.state.insert("weekly_schedule".into(), merged);
                });
            }
            return;
        }

        if cluster_id == cluster::IAS_ZONE && header.server_to_client() {
            match header.command {
                // Zone Status Change Notification: handled like a report of the status attribute
                0x00 => {
                    if let (Some(ieee), Some(bytes)) = (&ieee, payload.get(..2)) {
                        let status = u16::from_le_bytes([bytes[0], bytes[1]]);
                        let record = zcl::AttributeRecord {
                            id: 0x0002,
                            status: zcl::STATUS_SUCCESS,
                            value: Some(Value::Uint(u64::from(status))),
                        };
                        self.apply_records(ieee, cluster::IAS_ZONE, None, &[record]);
                    }
                    self.default_response(source, src_ep, cluster_id, &header);
                }
                // Zone Enroll Request
                0x01 => {
                    if let (Some(ieee), Some(bytes)) = (&ieee, payload.get(..2)) {
                        let zone_type = u16::from_le_bytes([bytes[0], bytes[1]]);
                        self.update_device(ieee, |d| {
                            d.state.insert("zone_type".into(), json!(zone_type));
                        });
                    }
                    self.send_enroll_response(source, src_ep);
                }
                _ => {}
            }
            return;
        }

        if cluster_id == cluster::POLL_CONTROL && header.server_to_client() && header.command == 0x00 {
            // Check-in: with a write pending, ask for fast polling so the write gets through
            let has_pending = ieee
                .as_ref()
                .is_some_and(|ieee| self.pending_writes.lock().unwrap().contains_key(ieee));
            let mut body = vec![u8::from(has_pending)];
            body.extend_from_slice(&FAST_POLL_QUARTER_SECONDS.to_le_bytes());
            let frame = zcl::frame(
                zcl::FC_CLUSTER_SPECIFIC | zcl::FC_DISABLE_DEFAULT_RESPONSE,
                header.seq,
                0x00,
                &body,
            );
            if let Err(e) = self.send(source, HA_PROFILE, cluster::POLL_CONTROL, HUB_ENDPOINT, src_ep, frame, false) {
                debug!("cannot send check-in response: {e:#}");
            }
            return;
        }

        if cluster_id == tuya::CLUSTER && header.server_to_client() {
            self.handle_tuya(source, src_ep, ieee.as_deref(), &header, payload);
            return;
        }

        if let Some(ieee) = &ieee
            && self.handle_action(ieee, cluster_id, &header, payload)
        {
            self.default_response(source, src_ep, cluster_id, &header);
            return;
        }

        debug!(
            "Unhandled ZCL command: nwk 0x{source:04x} cluster 0x{cluster_id:04x} command 0x{:02x}",
            header.command
        );
    }

    /// A command the device sent (a button press): becomes the value of its action
    /// feature. Returns whether a definition knew the command.
    fn handle_action(&self, ieee: &str, cluster_id: u16, header: &zcl::Header, payload: &[u8]) -> bool {
        let Some(device) = self.device(ieee) else {
            return false;
        };
        let decoded = devices::features(&device).into_iter().find_map(|feature| match feature.source {
            Source::Actions(actions) => actions
                .iter()
                .find_map(|action| action.decode(cluster_id, header, payload))
                .map(|name| (feature.key, name)),
            _ => None,
        });
        let Some((key, name)) = decoded else {
            return false;
        };
        // The same frame can arrive twice (e.g. a retry); as Zigbee2MQTT, act once
        let repeat = self.last_action_seq.lock().unwrap().insert(ieee.to_owned(), (cluster_id, header.seq))
            == Some((cluster_id, header.seq));
        if !repeat {
            info!("{ieee} {key} = {name}");
            self.update_device(ieee, |d| {
                d.state.insert(key.to_owned(), json!(name));
            });
            // Every press, also one that repeats the last value (the state then does not
            // change and no device event goes out)
            let _ = self.events.send(Event::Action {
                ieee: ieee.to_owned(),
                name: device.name.clone(),
                key: key.to_owned(),
                action: name,
            });
        }
        true
    }

    fn answer_time_read(&self, nwk: u16, src_ep: u8, dst_ep: u8, header: &zcl::Header, payload: &[u8], ieee: Option<&str>) {
        let ids: Vec<u16> = payload.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        let base = ieee
            .and_then(|ieee| self.device(ieee))
            .map_or(TimeBase::Zigbee2000, |d| devices::time_base(&d));
        let body = time_read_response(&ids, now_unix(), tuya::utc_offset_seconds(), base);
        let frame = zcl::frame(
            zcl::FC_SERVER_TO_CLIENT | zcl::FC_DISABLE_DEFAULT_RESPONSE,
            header.seq,
            zcl::cmd::READ_ATTRIBUTES_RSP,
            &body,
        );
        debug!("Answered time read: nwk 0x{nwk:04x} {ids:04x?} ({base:?})");
        let src = if dst_ep == 0 { HUB_ENDPOINT } else { dst_ep };
        if let Err(e) = self.send(nwk, HA_PROFILE, cluster::TIME, src, src_ep, frame, false) {
            debug!("cannot send time response: {e:#}");
        }
    }

    fn handle_tuya(&self, nwk: u16, ep: u8, ieee: Option<&str>, header: &zcl::Header, payload: &[u8]) {
        match header.command {
            tuya::cmd::DATA_RESPONSE
            | tuya::cmd::DATA_REPORT
            | tuya::cmd::ACTIVE_STATUS_REPORT
            | tuya::cmd::ACTIVE_STATUS_REPORT_ALT => {
                let Some(ieee) = ieee else {
                    return;
                };
                let datapoints = tuya::parse_datapoints(payload);
                self.update_device(ieee, |device| {
                    for dp in apply_tuya_datapoints(device, &datapoints) {
                        // Logged to map the datapoints when adding a new model
                        info!("{ieee} Tuya DP {} (type {}): {:02x?}", dp.id, dp.ty, dp.data);
                    }
                });
            }
            tuya::cmd::MCU_SYNC_TIME => {
                let since_2000 = ieee
                    .and_then(|ieee| self.device(ieee))
                    .and_then(|d| devices::definition(&d))
                    .is_none_or(|def| def.tuya_time_since_2000);
                let body = tuya::time_sync_payload(now_unix(), tuya::utc_offset_seconds(), since_2000);
                self.send_tuya(nwk, ep, tuya::cmd::MCU_SYNC_TIME, &body);
            }
            tuya::cmd::GATEWAY_CONNECTION_STATUS => {
                // The gateway is online: some devices only report after hearing this
                self.send_tuya(nwk, ep, tuya::cmd::GATEWAY_CONNECTION_STATUS, &[0x01, 0x00, 0x01]);
            }
            _ => {}
        }
        self.default_response(nwk, ep, tuya::CLUSTER, header);
    }

    fn send_tuya(&self, nwk: u16, ep: u8, command: u8, body: &[u8]) {
        let frame = zcl::frame(
            zcl::FC_CLUSTER_SPECIFIC | zcl::FC_DISABLE_DEFAULT_RESPONSE,
            self.next_seq(),
            command,
            body,
        );
        if let Err(e) = self.send(nwk, HA_PROFILE, tuya::CLUSTER, HUB_ENDPOINT, ep, frame, true) {
            warn!("cannot send Tuya command 0x{command:02x}: {e:#}");
        }
    }

    fn apply_records(&self, ieee: &str, cluster_id: u16, manufacturer: Option<u16>, records: &[zcl::AttributeRecord]) {
        self.update_device(ieee, |device| apply_zcl_records(device, cluster_id, manufacturer, records));
    }

    fn default_response(&self, nwk: u16, ep: u8, cluster_id: u16, header: &zcl::Header) {
        if header.disable_default_response() || header.manufacturer.is_some() {
            return;
        }
        let direction = if header.server_to_client() {
            0
        } else {
            zcl::FC_SERVER_TO_CLIENT
        };
        let frame = zcl::frame(
            zcl::FC_DISABLE_DEFAULT_RESPONSE | direction,
            header.seq,
            zcl::cmd::DEFAULT_RSP,
            &[header.command, zcl::STATUS_SUCCESS],
        );
        if let Err(e) = self.send(nwk, HA_PROFILE, cluster_id, HUB_ENDPOINT, ep, frame, false) {
            debug!("cannot send Default Response: {e:#}");
        }
    }

    fn send_enroll_response(&self, nwk: u16, ep: u8) {
        // The zone ID only has meaning within this hub
        let zone_id = (nwk & 0x7F) as u8;
        let frame = zcl::frame(
            zcl::FC_CLUSTER_SPECIFIC | zcl::FC_DISABLE_DEFAULT_RESPONSE,
            self.next_seq(),
            0x00,
            &[zcl::STATUS_SUCCESS, zone_id],
        );
        if let Err(e) = self.send(nwk, HA_PROFILE, cluster::IAS_ZONE, HUB_ENDPOINT, ep, frame, true) {
            warn!("cannot send IAS enroll response: {e:#}");
        }
    }

    // ---- Outgoing requests ----

    #[allow(clippy::too_many_arguments)]
    fn send(
        &self,
        nwk: u16,
        profile: u16,
        cluster_id: u16,
        src_ep: u8,
        dst_ep: u8,
        data: Vec<u8>,
        aps_ack: bool,
    ) -> Result<()> {
        let sleepy = self.is_sleepy(nwk);
        self.stack
            .send_aps_unicast(
                Nwk(nwk),
                profile,
                cluster_id,
                src_ep,
                dst_ep,
                if aps_ack { ApsAck::Request } else { ApsAck::None },
                RADIUS,
                data,
                None,
                sleepy,
                TxPriority::UserNormal,
                RouteDirective::StackDecides,
            )
            .map(drop)
            .map_err(|e| anyhow!("cannot queue the send: {e:?}"))
    }

    #[allow(clippy::too_many_arguments)]
    async fn request(
        &self,
        key: PendingKey,
        nwk: u16,
        profile: u16,
        cluster_id: u16,
        src_ep: u8,
        dst_ep: u8,
        data: Vec<u8>,
        wait: Option<Duration>,
    ) -> Result<Vec<u8>> {
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(key, tx);
        if let Err(e) = self.send(nwk, profile, cluster_id, src_ep, dst_ep, data, true) {
            self.pending.lock().unwrap().remove(&key);
            return Err(e);
        }
        let wait = wait.unwrap_or(if self.is_sleepy(nwk) {
            SLEEPY_REPLY_TIMEOUT
        } else {
            REPLY_TIMEOUT
        });
        match tokio::time::timeout(wait, rx).await {
            Ok(Ok(response)) => Ok(response),
            _ => {
                self.pending.lock().unwrap().remove(&key);
                Err(NoReply.into())
            }
        }
    }

    async fn zdo_request(&self, nwk: u16, cluster_id: u16, payload: Vec<u8>) -> Result<Vec<u8>> {
        self.zdo_request_within(nwk, cluster_id, payload, None).await
    }

    /// `wait` overrides the reply timeout
    async fn zdo_request_within(&self, nwk: u16, cluster_id: u16, payload: Vec<u8>, wait: Option<Duration>) -> Result<Vec<u8>> {
        let tsn = self.next_seq();
        let mut data = vec![tsn];
        data.extend(payload);
        let key = PendingKey::Zdo {
            cluster: cluster_id | zdo::cluster::RESPONSE,
            tsn,
        };
        self.request(key, nwk, ZDO_PROFILE, cluster_id, 0, 0, data, wait)
            .await
    }

    async fn zcl_request(
        &self,
        nwk: u16,
        ep: u8,
        cluster_id: u16,
        frame_control: u8,
        command: u8,
        payload: &[u8],
    ) -> Result<(zcl::Header, Vec<u8>)> {
        self.zcl_request_with(nwk, ep, cluster_id, frame_control, None, command, payload)
            .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn zcl_request_with(
        &self,
        nwk: u16,
        ep: u8,
        cluster_id: u16,
        frame_control: u8,
        manufacturer: Option<u16>,
        command: u8,
        payload: &[u8],
    ) -> Result<(zcl::Header, Vec<u8>)> {
        self.zcl_request_within(nwk, ep, cluster_id, frame_control, manufacturer, command, payload, None)
            .await
    }

    /// `wait` overrides the reply timeout
    #[allow(clippy::too_many_arguments)]
    async fn zcl_request_within(
        &self,
        nwk: u16,
        ep: u8,
        cluster_id: u16,
        frame_control: u8,
        manufacturer: Option<u16>,
        command: u8,
        payload: &[u8],
        wait: Option<Duration>,
    ) -> Result<(zcl::Header, Vec<u8>)> {
        let seq = self.next_seq();
        let frame = zcl::manufacturer_frame(frame_control, manufacturer, seq, command, payload);
        let key = PendingKey::Zcl { nwk, seq };
        let response = self
            .request(key, nwk, HA_PROFILE, cluster_id, HUB_ENDPOINT, ep, frame, wait)
            .await?;
        let (header, body) =
            zcl::Header::parse(&response).ok_or_else(|| anyhow!("malformed ZCL response"))?;
        Ok((header, body.to_vec()))
    }

    async fn read_attributes(
        &self,
        nwk: u16,
        ep: u8,
        cluster_id: u16,
        manufacturer: Option<u16>,
        ids: &[u16],
    ) -> Result<Vec<zcl::AttributeRecord>> {
        let (header, body) = self
            .zcl_request_with(
                nwk,
                ep,
                cluster_id,
                0x00,
                manufacturer,
                zcl::cmd::READ_ATTRIBUTES,
                &zcl::read_attributes_payload(ids),
            )
            .await?;
        if header.command != zcl::cmd::READ_ATTRIBUTES_RSP {
            bail!("attribute read rejected (command 0x{:02x})", header.command);
        }
        Ok(zcl::parse_read_response(&body))
    }

    // ---- Interview ----

    /// A short delay after joining so the device finishes the key exchange
    pub fn spawn_interview(self: &Arc<Self>, ieee: String) {
        self.spawn_interview_after(ieee, Duration::from_secs(2));
    }

    /// A sleepy device just sent something: interview it right away while it is awake
    fn start_wake_interview(self: &Arc<Self>, nwk: u16) {
        let Some(ieee) = self.ieee_for_nwk(nwk) else {
            return;
        };
        if self.wake_interviews.lock().unwrap().remove(&ieee) {
            info!("{ieee} woke up, interviewing");
            self.spawn_interview_after(ieee, Duration::ZERO);
        }
    }

    fn spawn_interview_after(self: &Arc<Self>, ieee: String, delay: Duration) {
        if !self.interviews.lock().unwrap().insert(ieee.clone()) {
            return;
        }
        self.emit_device(&ieee);
        let hub = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            let result = hub.interview(&ieee).await;
            hub.interviews.lock().unwrap().remove(&ieee);
            match result {
                Ok(()) => info!("{ieee} interviewed"),
                Err(e) => warn!("{ieee} interview failed: {e:#}"),
            }
            hub.emit_device(&ieee);
        });
    }

    async fn interview(self: &Arc<Self>, ieee: &str) -> Result<()> {
        let nwk = self.device(ieee).context("device not in the list")?.nwk;
        info!("{ieee} interviewing");

        let desc = retry("Node_Desc", || async move {
            let rsp = self
                .zdo_request(nwk, zdo::cluster::NODE_DESC_REQ, zdo::node_desc_req(nwk))
                .await?;
            zdo::parse_node_desc_rsp(&rsp)
        })
        .await?;
        self.update_device(ieee, |d| {
            d.router = desc.logical_type == 1;
            d.rx_on_when_idle = desc.rx_on_when_idle();
        });

        let endpoint_ids = retry("Active_EP", || async move {
            let rsp = self
                .zdo_request(nwk, zdo::cluster::ACTIVE_EP_REQ, zdo::active_ep_req(nwk))
                .await?;
            zdo::parse_active_ep_rsp(&rsp)
        })
        .await?;

        let mut endpoints = Vec::new();
        for ep in endpoint_ids {
            if ep == GREEN_POWER_ENDPOINT {
                continue;
            }
            let endpoint = retry("Simple_Desc", || async move {
                let rsp = self
                    .zdo_request(nwk, zdo::cluster::SIMPLE_DESC_REQ, zdo::simple_desc_req(nwk, ep))
                    .await?;
                zdo::parse_simple_desc_rsp(&rsp)
            })
            .await?;
            if endpoint.profile != GREEN_POWER_PROFILE {
                endpoints.push(endpoint);
            }
        }
        self.update_device(ieee, |d| d.endpoints = endpoints);

        let device = self.device(ieee).context("device not in the list")?;
        if let Some(ep) = device.endpoint_with(cluster::BASIC) {
            // Tuya devices do not send data until these are read in a single request;
            // harmless for other devices
            if let Err(e) = self
                .read_attributes(nwk, ep, cluster::BASIC, None, &[0x0004, 0x0000, 0x0001, 0x0005, 0x0007, 0xFFFE, 0x4000])
                .await
            {
                warn!("{ieee} cannot read basic info: {e:#}");
            }
        }

        self.configure(ieee).await;

        if let Some(ep) = self.device(ieee).and_then(|d| d.endpoint_with(tuya::CLUSTER)) {
            // Ask for all datapoints once; later values arrive in the device's own reports
            self.send_tuya(nwk, ep, tuya::cmd::DATA_QUERY, &[]);
        }

        let taken: Vec<String> = {
            let data = self.data.lock().unwrap();
            data.devices
                .values()
                .filter(|d| d.ieee != ieee)
                .map(|d| d.name.clone())
                .collect()
        };
        self.update_device(ieee, |d| {
            d.interviewed = true;
            prune_state(d);
            if untouched_name(d) && let Some(base) = devices::default_name(d) {
                d.name = unique_name(base, &d.ieee, &taken);
            }
        });
        Ok(())
    }

    /// Derived from the device's features: binds the reported clusters to the hub,
    /// configures reporting, enrolls the IAS zone and reads every readable attribute.
    /// Individual failures do not stop the interview: some devices skip some steps.
    async fn configure(self: &Arc<Self>, ieee: &str) {
        let Some(device) = self.device(ieee) else {
            return;
        };
        let Ok(device_ieee) = parse_eui64(ieee) else {
            return;
        };
        let nwk = device.nwk;
        let features = devices::features(&device);

        // Manufacturer-specific attributes go in requests of their own
        let mut reports: BTreeMap<(u16, Option<u16>), Vec<ReportConfig>> = BTreeMap::new();
        let mut reads: BTreeMap<(u16, Option<u16>), Vec<u16>> = BTreeMap::new();
        for feature in &features {
            let Source::Attr {
                cluster,
                attr,
                ty,
                report,
                manufacturer,
                ..
            } = feature.source
            else {
                continue;
            };
            let attrs = reads.entry((cluster, manufacturer)).or_default();
            if !attrs.contains(&attr) {
                attrs.push(attr);
            }
            if let Some(report) = report {
                reports.entry((cluster, manufacturer)).or_default().push(ReportConfig {
                    attribute: attr,
                    ty,
                    min_interval: report.min,
                    max_interval: report.max,
                    reportable_change: report.change,
                });
            }
        }

        let mut bound = HashSet::new();
        for (&(cluster_id, manufacturer), configs) in &reports {
            let Some(ep) = device.endpoint_with(cluster_id) else {
                continue;
            };
            if bound.insert(cluster_id) {
                self.bind(ieee, nwk, device_ieee, ep, cluster_id).await;
            }
            match self
                .zcl_request_with(
                    nwk,
                    ep,
                    cluster_id,
                    0x00,
                    manufacturer,
                    zcl::cmd::CONFIGURE_REPORTING,
                    &zcl::configure_reporting_payload(configs),
                )
                .await
            {
                Ok((header, body))
                    if header.command == zcl::cmd::CONFIGURE_REPORTING_RSP && zcl::all_success(&body) => {}
                Ok((header, body)) => warn!(
                    "{ieee} cluster 0x{cluster_id:04x} rejected the reporting configuration (command 0x{:02x}, {body:02x?})",
                    header.command
                ),
                Err(e) => warn!("{ieee} cluster 0x{cluster_id:04x} reporting not configured: {e:#}"),
            }
        }

        for &cluster_id in devices::extra_binds(&device) {
            if let Some(ep) = device.endpoint_with_any(cluster_id) {
                self.bind(ieee, nwk, device_ieee, ep, cluster_id).await;
            }
        }
        if devices::has_ias_zone(&device)
            && let Some(ep) = device.endpoint_with(cluster::IAS_ZONE)
        {
            self.enroll(ieee, nwk, ep).await;
        }

        for ((cluster_id, manufacturer), attrs) in reads {
            let Some(ep) = device.endpoint_with(cluster_id) else {
                continue;
            };
            for chunk in attrs.chunks(READ_CHUNK) {
                if let Err(e) = self.read_attributes(nwk, ep, cluster_id, manufacturer, chunk).await {
                    debug!("{ieee} cluster 0x{cluster_id:04x} {chunk:04x?} not read: {e:#}");
                }
            }
        }
    }

    async fn bind(&self, ieee: &str, nwk: u16, device_ieee: Eui64, ep: u8, cluster_id: u16) {
        let bind = retry("Bind", || async move {
            let rsp = self
                .zdo_request(
                    nwk,
                    zdo::cluster::BIND_REQ,
                    zdo::bind_req(device_ieee.0, ep, cluster_id, self.ieee.0, HUB_ENDPOINT),
                )
                .await?;
            zdo::parse_status_rsp(&rsp)
        })
        .await;
        if let Err(e) = bind {
            warn!("{ieee} cluster 0x{cluster_id:04x} not bound: {e:#}");
        }
    }

    /// Enrolls the IAS zone with the hub: writes the CIE address and sends the enroll
    /// response up front (some sensors wait without sending an enroll request)
    async fn enroll(&self, ieee: &str, nwk: u16, ep: u8) {
        let payload = zcl::write_attribute_payload(0x0010, types::EUI64, &self.ieee.0);
        match self
            .zcl_request(nwk, ep, cluster::IAS_ZONE, 0x00, zcl::cmd::WRITE_ATTRIBUTES, &payload)
            .await
        {
            Ok((header, body))
                if header.command == zcl::cmd::WRITE_ATTRIBUTES_RSP && zcl::all_success(&body) => {}
            Ok((_, body)) => warn!("{ieee} rejected the IAS CIE address: {body:02x?}"),
            Err(e) => warn!("{ieee} IAS CIE address not written: {e:#}"),
        }
        self.send_enroll_response(nwk, ep);
    }

    // ---- User actions ----

    /// Writes a feature value: an attribute write, a light command or a Tuya datapoint,
    /// depending on its source. Attribute writes to sleepy devices are queued; returns
    /// `true` when the write waits for the device to wake up.
    pub async fn set(self: &Arc<Self>, ieee: &str, key: &str, value: Json) -> Result<bool> {
        let device = self.device(ieee).context("device not found")?;
        let feature = devices::features(&device)
            .into_iter()
            .find(|f| f.key == key)
            .with_context(|| format!("unknown feature: {key}"))?;
        let value = feature.validate(&device, value)?;

        match feature.source {
            Source::Attr {
                write: Write::Attribute,
                ..
            } => {
                if device.rx_on_when_idle {
                    self.write_attribute(&device, feature, &value).await?;
                    return Ok(false);
                }
                // Most sleepy devices (TRVZB) take it at their next poll, within a second
                // or so: try once and answer like a mains device; queue only if it
                // does not arrive
                if self.take_writing_slot(ieee, SLOT_WAIT).await {
                    let first = self.write_attribute_within(&device, feature, &value, Some(EAGER_WRITE_WAIT)).await;
                    self.writing.lock().unwrap().remove(ieee);
                    match first {
                        Ok(()) => {
                            // The newer value wins over one still queued
                            let mut pending = self.pending_writes.lock().unwrap();
                            if let Some(values) = pending.get_mut(ieee) {
                                values.remove(key);
                                if values.is_empty() {
                                    pending.remove(ieee);
                                }
                            }
                            drop(pending);
                            self.dirty.notify_one();
                            self.emit_device(ieee);
                            return Ok(false);
                        }
                        Err(e) if !e.is::<NoReply>() => return Err(e),
                        Err(_) => {}
                    }
                }
                self.pending_writes
                    .lock()
                    .unwrap()
                    .entry(ieee.to_owned())
                    .or_default()
                    .insert(key.to_owned(), value);
                self.dirty.notify_one();
                self.emit_device(ieee);
                self.spawn_eager_writes(ieee.to_owned());
                Ok(true)
            }
            Source::Attr { cluster, write, .. } => {
                self.light_command(&device, feature, cluster, write, &value).await?;
                Ok(false)
            }
            // Validation already refuses these
            Source::Actions(_) => bail!("{} is read-only", feature.label),
            Source::ThermostatSchedule => {
                let Kind::WeeklySchedule {
                    min,
                    max,
                    step,
                    max_transitions,
                } = feature.kind
                else {
                    bail!("expected a weekly schedule kind");
                };
                let limits = crate::thermostat::Limits {
                    min,
                    max,
                    step,
                    max_transitions: usize::from(max_transitions),
                };
                let week = crate::thermostat::parse_week(&value, limits)?;
                let ep = device.endpoint_with(cluster::THERMOSTAT).context("the device has no thermostat cluster")?;
                for payload in crate::thermostat::set_payloads(&week) {
                    let (header, body) = self
                        .zcl_patient(&device, ep, cluster::THERMOSTAT, crate::thermostat::SET_WEEKLY_SCHEDULE, &payload)
                        .await
                        .map_err(|e| no_reply_hint(e, "take the schedule"))?;
                    if header.command == zcl::cmd::DEFAULT_RSP
                        && let Some(status) = zcl::default_response_status(&body)
                        && status != zcl::STATUS_SUCCESS
                    {
                        bail!("the device rejected the schedule (status 0x{status:02x})");
                    }
                }
                info!("{ieee} weekly schedule set: {value}");
                self.update_device(ieee, |d| {
                    let mut merged = d.state.get(key).and_then(Json::as_object).cloned().unwrap_or_default();
                    if let Some(days) = value.as_object() {
                        merged.extend(days.clone());
                    }
                    d.state.insert(key.to_owned(), Json::Object(merged));
                });
                Ok(false)
            }
            Source::Command(command) => {
                for frame in command.frames(&value)? {
                    self.cluster_command(&device, frame).await?;
                }
                if let Some(state) = command.state_after(&value) {
                    self.update_device(ieee, |d| {
                        d.state.insert(key.to_owned(), state);
                    });
                }
                Ok(false)
            }
            Source::Tuya { dp, dp_type, .. } => {
                let datapoint = tuya_datapoint(dp, dp_type, feature.encode(&value)?)?;
                self.send_tuya_datapoint(&device, datapoint)?;
                // The device's own datapoint report confirms it; the UI need not wait
                self.update_device(ieee, |d| {
                    d.state.insert(key.to_owned(), value);
                });
                Ok(false)
            }
            Source::TuyaSchedule { dp, divisor } => {
                let Kind::Schedule { min, max, step } = feature.kind else {
                    bail!("expected a schedule kind");
                };
                let expected = device.state.get(key).and_then(Json::as_array).map(Vec::len);
                let data = tuya::encode_schedule(&value, expected, (min, max, step), divisor).map_err(|e| anyhow!(e))?;
                let normalized = tuya::decode_schedule(&data, divisor);
                self.send_tuya_datapoint(
                    &device,
                    tuya::Datapoint {
                        id: dp,
                        ty: tuya::dp_type::RAW,
                        data,
                    },
                )?;
                self.update_device(ieee, |d| {
                    d.state.insert(key.to_owned(), normalized);
                });
                Ok(false)
            }
        }
    }

    fn send_tuya_datapoint(&self, device: &Device, datapoint: tuya::Datapoint) -> Result<()> {
        let ep = device.endpoint_with(tuya::CLUSTER).context("the device has no Tuya cluster")?;
        let seq = u16::from(self.next_seq());
        self.send_tuya(device.nwk, ep, tuya::cmd::DATA_REQUEST, &tuya::data_request(seq, &[datapoint]));
        Ok(())
    }

    /// Light cluster commands: On/Off, Move to Level (with On/Off), Move to Color Temperature
    async fn light_command(&self, device: &Device, feature: &Feature, cluster_id: u16, write: Write, value: &Json) -> Result<()> {
        // Half-second transition
        let transition: u16 = 5;
        let number = || value.as_f64().unwrap_or_default().round();
        let (command, payload): (u8, Vec<u8>) = match write {
            Write::OnOff => (u8::from(value.as_bool() == Some(true)), vec![]),
            Write::Level => {
                let level = number().clamp(0.0, 254.0) as u8;
                let mut payload = vec![level];
                payload.extend_from_slice(&transition.to_le_bytes());
                (0x04, payload)
            }
            Write::ColorTemp => {
                let mut payload = (number().clamp(0.0, 65_279.0) as u16).to_le_bytes().to_vec();
                payload.extend_from_slice(&transition.to_le_bytes());
                (0x0A, payload)
            }
            Write::No | Write::Attribute => bail!("{} is not written with a command", feature.label),
        };
        self.cluster_command(
            device,
            CommandFrame {
                cluster: cluster_id,
                manufacturer: None,
                command,
                payload,
            },
        )
        .await?;
        // Reports follow shortly; the UI need not wait
        self.update_device(&device.ieee, |d| {
            d.state.insert(feature.key.to_owned(), value.clone());
            if write == Write::Level {
                d.state.insert("on".into(), json!(number() > 0.0));
            }
        });
        Ok(())
    }

    /// Sends a cluster-specific command and checks the device's Default Response
    async fn cluster_command(&self, device: &Device, frame: CommandFrame) -> Result<()> {
        let ep = device.endpoint_with(frame.cluster).context("the device does not support this command")?;
        let (header, body) = self
            .zcl_request_with(
                device.nwk,
                ep,
                frame.cluster,
                zcl::FC_CLUSTER_SPECIFIC,
                frame.manufacturer,
                frame.command,
                &frame.payload,
            )
            .await?;
        if header.command == zcl::cmd::DEFAULT_RSP
            && let Some(status) = zcl::default_response_status(&body)
            && status != zcl::STATUS_SUCCESS
        {
            bail!("the device rejected the command (status 0x{status:02x})");
        }
        Ok(())
    }

    async fn write_attribute(&self, device: &Device, feature: &Feature, value: &Json) -> Result<()> {
        self.write_attribute_within(device, feature, value, None).await
    }

    async fn write_attribute_within(&self, device: &Device, feature: &Feature, value: &Json, wait: Option<Duration>) -> Result<()> {
        let Source::Attr {
            cluster,
            attr,
            ty,
            manufacturer,
            ..
        } = feature.source
        else {
            bail!("not an attribute");
        };
        let raw = feature.encode(value)?;
        let bytes = zcl::encode_int(ty, raw).context("the value does not fit the attribute type")?;
        let ep = device.endpoint_with(cluster).context("the device lacks the attribute's cluster")?;
        let payload = zcl::write_attribute_payload(attr, ty, &bytes);
        let (header, body) = self
            .zcl_request_within(device.nwk, ep, cluster, 0x00, manufacturer, zcl::cmd::WRITE_ATTRIBUTES, &payload, wait)
            .await?;
        if header.command != zcl::cmd::WRITE_ATTRIBUTES_RSP || !zcl::all_success(&body) {
            bail!("the device rejected the value ({body:02x?})");
        }
        info!("{} {} = {value}", device.ieee, feature.key);
        self.update_device(&device.ieee, |d| {
            d.state.insert(feature.key.to_owned(), value.clone());
        });
        Ok(())
    }

    /// Factory resets Hue lights by serial number: opens joining first (so the reset light
    /// can join right away), broadcasts the inter-PAN request on every channel and
    /// returns to the network channel. Network traffic waits meanwhile (~20 s).
    pub async fn hue_factory_reset(self: &Arc<Self>, serials: &[String]) -> Result<()> {
        if serials.is_empty() || serials.len() > 8 {
            bail!("enter 1-8 serial numbers");
        }
        let numbers = serials
            .iter()
            .map(|s| crate::hue_reset::parse_serial(s))
            .collect::<Result<Vec<_>>>()?;
        let network = self.data.lock().unwrap().network.clone().context("no network")?;
        let extended_pan_id = parse_eui64(&network.extended_pan_id)?;

        self.permit_join(MAX_PERMIT_SECONDS)?;
        info!("Hue factory reset request: {serials:?}");

        let radio = self.phy.lock().await;
        let mut result = Ok(());
        for (i, &channel) in crate::hue_reset::CHANNELS.iter().enumerate() {
            if let Err(e) = radio.set_channel(channel).await {
                result = Err(anyhow!("cannot switch to channel {channel}: {e}"));
                break;
            }
            let psdu = crate::hue_reset::reset_frame(
                i as u8,
                network.pan_id,
                &self.ieee,
                &extended_pan_id,
                &numbers,
            );
            // Broadcasts are not acknowledged; send twice per channel to be safe
            for _ in 0..2 {
                let frame = TxFrame {
                    psdu: psdu.clone(),
                    channel: None,
                    csma_ca: true,
                    max_frame_retries: 0,
                    max_csma_backoffs: 4,
                    security_processed: true,
                };
                if let Err(e) = radio.transmit(frame).await {
                    warn!("Hue reset on channel {channel}: send failed: {e}");
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
        // Return to the network channel no matter what
        let restore = radio.set_channel(network.channel).await;
        drop(radio);
        restore.map_err(|e| anyhow!("cannot return to the network channel ({}): {e}", network.channel))?;
        info!("Hue reset request sent on every channel");
        result
    }

    /// A mains-powered device is interviewed now, a sleepy one at its next wake-up;
    /// returns `true` when waiting for the wake-up
    pub fn reinterview(self: &Arc<Self>, ieee: &str) -> Result<bool> {
        let device = self.device(ieee).context("device not found")?;
        if device.rx_on_when_idle {
            self.spawn_interview_after(ieee.to_owned(), Duration::ZERO);
            return Ok(false);
        }
        self.wake_interviews.lock().unwrap().insert(ieee.to_owned());
        self.emit_device(ieee);
        Ok(true)
    }

    pub fn permit_join(self: &Arc<Self>, seconds: u8) -> Result<()> {
        let seconds = seconds.min(MAX_PERMIT_SECONDS);
        self.stack.permit_joins(u64::from(seconds), true);

        // Tell the routers too, so devices can join through the nearest light
        let mut data = vec![self.next_seq()];
        data.extend(zdo::mgmt_permit_joining_req(seconds));
        self.stack
            .send_aps_broadcast(
                Nwk(0xFFFC),
                ZDO_PROFILE,
                zdo::cluster::MGMT_PERMIT_JOINING_REQ,
                0,
                0,
                RADIUS,
                data,
                TxPriority::UserNormal,
            )
            .map_err(|e| anyhow!("cannot broadcast the permit join: {e:?}"))?;

        let until = (seconds > 0).then(|| Instant::now() + Duration::from_secs(u64::from(seconds)));
        *self.permit_until.lock().unwrap() = until;
        info!("Permit join: {seconds} s");
        let _ = self.events.send(Event::PermitJoin {
            remaining: u64::from(seconds),
        });

        if seconds > 0 {
            let hub = self.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(u64::from(seconds))).await;
                if hub.permit_remaining() == 0 {
                    let _ = hub.events.send(Event::PermitJoin { remaining: 0 });
                }
            });
        }
        Ok(())
    }

    pub fn permit_remaining(&self) -> u64 {
        self.permit_until
            .lock()
            .unwrap()
            .map_or(0, |until| until.saturating_duration_since(Instant::now()).as_secs())
    }

    /// A device that just sent something is awake: perform its pending writes now. A value
    /// stays queued until the device confirms it; a failed write is retried at the next
    /// wake-up.
    fn flush_pending_writes(self: &Arc<Self>, nwk: u16) {
        let Some(device) = self.data.lock().unwrap().devices.values().find(|d| d.nwk == nwk).cloned() else {
            return;
        };
        let Some(pending) = self.pending_writes.lock().unwrap().get(&device.ieee).cloned() else {
            return;
        };
        // The write's own response lands here too; never start a second concurrent write
        if !self.writing.lock().unwrap().insert(device.ieee.clone()) {
            return;
        }
        let hub = self.clone();
        tokio::spawn(async move {
            for (key, error) in hub.write_pending(&device, pending, None).await {
                warn!("{} {key} not written, retrying at the next wake-up: {error:#}", device.ieee);
            }
            hub.writing.lock().unwrap().remove(&device.ieee);
            hub.emit_device(&device.ieee);
        });
    }

    /// Writes queued values; a confirmed one leaves the queue. Returns the failures. The
    /// caller holds the device's `writing` slot.
    async fn write_pending(
        &self,
        device: &Device,
        pending: BTreeMap<String, Json>,
        wait: Option<Duration>,
    ) -> Vec<(String, anyhow::Error)> {
        let features = devices::features(device);
        let mut failures = Vec::new();
        for (key, value) in pending {
            let Some(feature) = features.iter().find(|f| f.key == key) else {
                continue;
            };
            match self.write_attribute_within(device, feature, &value, wait).await {
                Ok(()) => {
                    settle_pending(&mut self.pending_writes.lock().unwrap(), &device.ieee, &key, &value);
                    self.dirty.notify_one();
                }
                Err(e) => failures.push((key, e)),
            }
        }
        failures
    }

    /// Waits up to `wait` for the device's write slot (one write at a time per device)
    async fn take_writing_slot(&self, ieee: &str, wait: Duration) -> bool {
        let started = Instant::now();
        loop {
            if self.writing.lock().unwrap().insert(ieee.to_owned()) {
                return true;
            }
            if started.elapsed() >= wait {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// A sleepy device takes a frame when it next polls its parent, and the frame waits
    /// for that only 7.68 s. Devices that poll every few seconds (TRVZB) get their writes
    /// right away this way, as with Zigbee2MQTT; the attempts repeat for a while, after
    /// which the queue waits for the device's next wake-up.
    fn spawn_eager_writes(self: &Arc<Self>, ieee: String) {
        if !self.eager_writes.lock().unwrap().insert(ieee.clone()) {
            return; // the running attempts pick up the new value
        }
        let hub = self.clone();
        tokio::spawn(async move {
            let started = Instant::now();
            let mut attempt = 0;
            while started.elapsed() < EAGER_WRITE_WINDOW {
                let Some(pending) = hub.pending_writes.lock().unwrap().get(&ieee).cloned() else {
                    break;
                };
                let Some(device) = hub.device(&ieee) else {
                    break;
                };
                if !hub.writing.lock().unwrap().insert(ieee.clone()) {
                    // A wake-up write is under way
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    continue;
                }
                attempt += 1;
                let failures = hub.write_pending(&device, pending, Some(EAGER_WRITE_WAIT)).await;
                hub.writing.lock().unwrap().remove(&ieee);
                hub.emit_device(&ieee);
                if failures.is_empty() {
                    info!(
                        "{ieee} written {:.1} s after the request (attempt {attempt})",
                        started.elapsed().as_secs_f32()
                    );
                    break;
                }
                // A rejection is final; only a missing reply is worth repeating
                if failures.iter().any(|(_, e)| !e.is::<NoReply>()) {
                    for (key, e) in &failures {
                        warn!("{ieee} {key} not written: {e:#}");
                    }
                    break;
                }
            }
            if hub.pending_writes.lock().unwrap().contains_key(&ieee) {
                info!("{ieee} did not take the write within {} s; waiting for its next wake-up", EAGER_WRITE_WINDOW.as_secs());
            }
            hub.eager_writes.lock().unwrap().remove(&ieee);
        });
    }

    pub fn rename(&self, ieee: &str, name: &str) -> Result<()> {
        let name = name.trim();
        if name.is_empty() || name.chars().count() > 64 {
            bail!("the name must be 1-64 characters");
        }
        if self.device(ieee).is_none() {
            bail!("device not found");
        }
        self.update_device(ieee, |d| d.name = name.to_owned());
        Ok(())
    }

    /// Asks the device to leave the network and deletes its records
    /// As in Zigbee2MQTT: a normal removal asks the device to leave and forgets it once
    /// it confirms; `force` only forgets it (for a device that is gone or silent). A
    /// sleepy device gets the request repeated for a while, so a button press that wakes
    /// it lets it answer.
    pub async fn remove(self: &Arc<Self>, ieee: &str, force: bool) -> Result<()> {
        let device = self.device(ieee).context("device not found")?;
        if !force {
            let device_ieee = parse_eui64(ieee)?;
            let rsp = self
                .zdo_patient(&device, zdo::cluster::MGMT_LEAVE_REQ, zdo::mgmt_leave_req(device_ieee.0))
                .await
                .map_err(|e| {
                    if e.is::<NoReply>() {
                        anyhow!(
                            "the device did not confirm leaving. Wake it up (battery devices: press its button) and \
                             try again, or use force remove"
                        )
                    } else {
                        e
                    }
                })?;
            zdo::parse_status_rsp(&rsp)
                .map_err(|e| anyhow!("the device refused to leave ({e:#}); use force remove to delete it anyway"))?;
            info!("{ieee} left the network");
        }
        self.forget(ieee);
        info!("{ieee} removed{}", if force { " (forced)" } else { "" });
        Ok(())
    }

    // ---- Groups ----

    pub fn groups(&self) -> Vec<store::Group> {
        self.data.lock().unwrap().groups.clone()
    }

    fn groups_changed(&self) {
        self.dirty.notify_one();
        let _ = self.events.send(Event::Groups { groups: self.groups() });
    }

    fn group_name_check(&self, name: &str, except: Option<u16>) -> Result<String> {
        let name = name.trim();
        if name.is_empty() || name.chars().count() > 64 {
            bail!("the group name must be 1-64 characters");
        }
        let data = self.data.lock().unwrap();
        if data.groups.iter().any(|g| g.name == name && Some(g.id) != except) {
            bail!("a group named \"{name}\" already exists");
        }
        Ok(name.to_owned())
    }

    /// A new, empty group; ids count up from 1 as in Zigbee2MQTT
    pub fn create_group(&self, name: &str) -> Result<u16> {
        let name = self.group_name_check(name, None)?;
        let id = {
            let mut data = self.data.lock().unwrap();
            let id = (1..=0xFFF7).find(|id| !data.groups.iter().any(|g| g.id == *id)).context("no free group id")?;
            data.groups.push(store::Group {
                id,
                name: name.clone(),
                members: Vec::new(),
                matter: false,
            });
            id
        };
        info!("Group {id} \"{name}\" created");
        self.groups_changed();
        Ok(id)
    }

    pub fn rename_group(&self, id: u16, name: &str) -> Result<()> {
        let name = self.group_name_check(name, Some(id))?;
        {
            let mut data = self.data.lock().unwrap();
            let group = data.groups.iter_mut().find(|g| g.id == id).context("group not found")?;
            group.name = name;
        }
        self.groups_changed();
        Ok(())
    }

    pub fn set_group_matter(&self, id: u16, enabled: bool) -> Result<()> {
        {
            let mut data = self.data.lock().unwrap();
            let group = data.groups.iter_mut().find(|g| g.id == id).context("group not found")?;
            group.matter = enabled;
        }
        self.groups_changed();
        Ok(())
    }

    /// Asks every member to leave the group, then forgets it (a member that does not
    /// answer keeps the membership, which is harmless without the group)
    pub async fn delete_group(&self, id: u16) -> Result<()> {
        let members = {
            let data = self.data.lock().unwrap();
            data.groups.iter().find(|g| g.id == id).context("group not found")?.members.clone()
        };
        for member in members {
            if let Err(e) = self.group_membership(id, &member, false).await {
                warn!("{} did not leave group {id}: {e:#}", member.ieee);
            }
        }
        self.data.lock().unwrap().groups.retain(|g| g.id != id);
        info!("Group {id} deleted");
        self.groups_changed();
        Ok(())
    }

    /// ZCL Groups cluster: Add Group / Remove Group on the member's endpoint
    async fn group_membership(&self, id: u16, member: &store::GroupMember, add: bool) -> Result<()> {
        let device = self.device(&member.ieee).context("device not found")?;
        let serves_groups = device
            .endpoints
            .iter()
            .any(|ep| ep.id == member.ep && ep.in_clusters.contains(&cluster::GROUPS));
        if !serves_groups {
            bail!("{} endpoint {} has no Groups cluster", device.name, member.ep);
        }
        let mut payload = id.to_le_bytes().to_vec();
        if add {
            payload.push(0); // empty group name
        }
        let command = if add { 0x00 } else { 0x03 };
        let (header, body) = self
            .zcl_request(device.nwk, member.ep, cluster::GROUPS, zcl::FC_CLUSTER_SPECIFIC, command, &payload)
            .await?;
        // The response carries the status first; an existing membership on add and a
        // missing one on remove are what was asked for
        let status = if header.command == zcl::cmd::DEFAULT_RSP {
            zcl::default_response_status(&body)
        } else {
            body.first().copied()
        };
        match status {
            Some(zcl::STATUS_SUCCESS) => Ok(()),
            Some(0x8A) if add => Ok(()),
            Some(0x8B) if !add => Ok(()),
            Some(status) => bail!("the device refused (status 0x{status:02x})"),
            None => bail!("malformed response"),
        }
    }

    pub async fn set_group_member(&self, id: u16, ieee: &str, ep: u8, add: bool) -> Result<()> {
        if !self.data.lock().unwrap().groups.iter().any(|g| g.id == id) {
            bail!("group not found");
        }
        let member = store::GroupMember { ieee: ieee.to_owned(), ep };
        self.group_membership(id, &member, add).await?;
        {
            let mut data = self.data.lock().unwrap();
            let group = data.groups.iter_mut().find(|g| g.id == id).context("group not found")?;
            group.members.retain(|m| *m != member);
            if add {
                group.members.push(member);
            }
        }
        info!("{ieee} ep {ep} {} group {id}", if add { "joined" } else { "left" });
        self.groups_changed();
        Ok(())
    }

    /// One multicast frame for the whole group: on/off, brightness or color temperature.
    /// The members' states are updated at once; their reports confirm shortly.
    pub fn set_group(&self, id: u16, key: &str, value: &Json) -> Result<()> {
        let transition: u16 = 5;
        let members = {
            let data = self.data.lock().unwrap();
            data.groups.iter().find(|g| g.id == id).context("group not found")?.members.clone()
        };
        let member_devices: Vec<Device> = members.iter().filter_map(|m| self.device(&m.ieee)).collect();
        // The narrowest limits of the members (e.g. Hue lights start brightness at 2)
        let limits = |feature_key: &str| {
            let mut low = f64::MIN;
            let mut high = f64::MAX;
            for device in &member_devices {
                if let Some(feature) = devices::features(device).into_iter().find(|f| f.key == feature_key) {
                    let (min, max) = feature.range(device);
                    low = low.max(min.unwrap_or(f64::MIN));
                    high = high.min(max.unwrap_or(f64::MAX));
                }
            }
            (low, high)
        };
        let number = || value.as_f64().filter(|v| v.is_finite()).context("expected a number");
        let (cluster_id, command, payload, state): (u16, u8, Vec<u8>, Vec<(&str, Json)>) = match key {
            "on" => {
                let on = value.as_bool().context("expected true or false")?;
                (cluster::ON_OFF, u8::from(on), vec![], vec![("on", json!(on))])
            }
            "level" => {
                let (low, high) = limits("level");
                let level = number()?.round().clamp(low.max(1.0), high.min(254.0));
                let mut payload = vec![level as u8];
                payload.extend_from_slice(&transition.to_le_bytes());
                (cluster::LEVEL, 0x04, payload, vec![("level", json!(level)), ("on", json!(true))])
            }
            "color_temp" => {
                let (low, high) = limits("color_temp");
                let mired = number()?.round().clamp(low.max(0.0), high.min(65_279.0));
                let mut payload = (mired as u16).to_le_bytes().to_vec();
                payload.extend_from_slice(&transition.to_le_bytes());
                (cluster::COLOR, 0x0A, payload, vec![("color_temp", json!(mired))])
            }
            _ => bail!("unknown group control: {key}"),
        };
        let frame = zcl::frame(
            zcl::FC_CLUSTER_SPECIFIC | zcl::FC_DISABLE_DEFAULT_RESPONSE,
            self.next_seq(),
            command,
            &payload,
        );
        self.stack
            .send_aps_groupcast(id, HA_PROFILE, cluster_id, HUB_ENDPOINT, RADIUS, frame, TxPriority::UserNormal)
            .map_err(|e| anyhow!("cannot send to the group: {e:?}"))?;
        info!("Group {id} {key} = {value}");
        for device in member_devices {
            // Only members that have the feature
            let features = devices::features(&device);
            self.update_device(&device.ieee, |d| {
                for (k, v) in &state {
                    if features.iter().any(|f| f.key == *k) {
                        d.state.insert((*k).to_owned(), v.clone());
                    }
                }
            });
        }
        Ok(())
    }

    /// A ZDO request that a sleepy device may answer only once a button press wakes it:
    /// repeated for a while, each attempt waiting as long as the parent keeps the frame
    async fn zdo_patient(&self, device: &Device, cluster_id: u16, payload: Vec<u8>) -> Result<Vec<u8>> {
        let (window, wait) = if device.rx_on_when_idle {
            (Duration::ZERO, None)
        } else {
            (SLEEPY_REQUEST_WINDOW, Some(EAGER_WRITE_WAIT))
        };
        let started = Instant::now();
        loop {
            match self.zdo_request_within(device.nwk, cluster_id, payload.clone(), wait).await {
                Err(e) if e.is::<NoReply>() && started.elapsed() < window => {}
                answer => return answer,
            }
        }
    }

    /// A cluster-specific command, repeated for a sleepy device like `zdo_patient`
    async fn zcl_patient(&self, device: &Device, ep: u8, cluster_id: u16, command: u8, payload: &[u8]) -> Result<(zcl::Header, Vec<u8>)> {
        let (window, wait) = if device.rx_on_when_idle {
            (Duration::ZERO, None)
        } else {
            (SLEEPY_REQUEST_WINDOW, Some(EAGER_WRITE_WAIT))
        };
        let started = Instant::now();
        loop {
            match self
                .zcl_request_within(device.nwk, ep, cluster_id, zcl::FC_CLUSTER_SPECIFIC, None, command, payload, wait)
                .await
            {
                Err(e) if e.is::<NoReply>() && started.elapsed() < window => {}
                answer => return answer,
            }
        }
    }

    /// The device's binding table, destinations named where known
    pub async fn bindings(&self, ieee: &str) -> Result<Vec<BindingView>> {
        let device = self.device(ieee).context("device not found")?;
        let mut all = Vec::new();
        // The table comes in pages; a page holds a few entries
        for _ in 0..16 {
            let rsp = self
                .zdo_patient(&device, zdo::cluster::MGMT_BIND_REQ, zdo::mgmt_bind_req(all.len() as u8))
                .await
                .map_err(|e| no_reply_hint(e, "read the bindings"))?;
            let (total, page) = zdo::parse_mgmt_bind_rsp(&rsp)
                .map_err(|e| anyhow!("the device does not report its bindings ({e:#})"))?;
            let empty = page.is_empty();
            all.extend(page);
            if empty || all.len() >= usize::from(total) {
                break;
            }
        }
        let hub = eui64_to_string(&self.ieee);
        let data = self.data.lock().unwrap();
        Ok(all
            .into_iter()
            .map(|binding| {
                let dst_name = match (binding.group, binding.dst_ieee.as_deref()) {
                    (Some(group), _) => data.groups.iter().find(|g| g.id == group).map(|g| g.name.clone()),
                    (None, Some(dst)) if dst == hub => Some("Hub".to_owned()),
                    (None, Some(dst)) => data.devices.get(dst).map(|d| d.name.clone()),
                    (None, None) => None,
                };
                BindingView { binding, dst_name }
            })
            .collect())
    }

    /// Binds (or unbinds) a cluster of the device to the hub or another device; the
    /// binding lives in the device's own table, so it works without the hub
    pub async fn set_binding(&self, ieee: &str, src_ep: u8, cluster_id: u16, dst: &str, dst_ep: u8, unbind: bool) -> Result<()> {
        let device = self.device(ieee).context("device not found")?;
        let src = parse_eui64(ieee)?;
        let payload = if let Some(group) = dst.strip_prefix("group:") {
            let group: u16 = group.parse().context("invalid group id")?;
            zdo::bind_req_group(src.0, src_ep, cluster_id, group)
        } else if dst == "hub" {
            zdo::bind_req(src.0, src_ep, cluster_id, self.ieee.0, HUB_ENDPOINT)
        } else {
            zdo::bind_req(src.0, src_ep, cluster_id, parse_eui64(dst)?.0, dst_ep)
        };
        let request = if unbind { zdo::cluster::UNBIND_REQ } else { zdo::cluster::BIND_REQ };
        let what = if unbind { "unbind" } else { "bind" };
        let rsp = self
            .zdo_patient(&device, request, payload)
            .await
            .map_err(|e| no_reply_hint(e, what))?;
        zdo::parse_status_rsp(&rsp).map_err(|e| anyhow!("the device refused to {what} cluster 0x{cluster_id:04x} ({e:#})"))?;
        info!("{ieee} {what} ep {src_ep} cluster 0x{cluster_id:04x} → {dst} ep {dst_ep}");
        Ok(())
    }

    fn forget(&self, ieee: &str) {
        {
            let mut data = self.data.lock().unwrap();
            data.devices.remove(ieee);
            data.link_keys.retain(|k| k.ieee != ieee);
            data.children.retain(|c| c.ieee != ieee);
            for group in &mut data.groups {
                group.members.retain(|m| m.ieee != ieee);
            }
        }
        self.pending_writes.lock().unwrap().remove(ieee);
        let _ = self.events.send(Event::Removed {
            ieee: ieee.to_owned(),
        });
        self.dirty.notify_one();
    }

    // ---- Persistence ----

    async fn saver(self: Arc<Self>) {
        loop {
            self.dirty.notified().await;
            tokio::time::sleep(SAVE_DELAY).await;
            if let Err(e) = self.save_now().await {
                warn!("State not saved: {e:#}");
            }
        }
    }

    pub async fn save_now(&self) -> Result<()> {
        let mut snapshot = self.data.lock().unwrap().clone();
        snapshot.pending_writes = self
            .pending_writes
            .lock()
            .unwrap()
            .iter()
            .map(|(ieee, values)| (ieee.clone(), values.clone()))
            .collect();
        let path = self.state_path.clone();
        tokio::task::spawn_blocking(move || snapshot.save(&path)).await??;
        Ok(())
    }

    async fn table_snapshots(self: Arc<Self>) {
        loop {
            tokio::time::sleep(TABLE_SNAPSHOT_INTERVAL).await;
            if self.refresh_tables() {
                self.dirty.notify_one();
            }
        }
    }

    /// Takes the link keys and the child table from the stack; `true` if they changed
    pub fn refresh_tables(&self) -> bool {
        let link_keys: Vec<LinkKey> = proto::key_entries(&self.stack)
            .into_iter()
            .map(|entry| LinkKey {
                ieee: eui64_to_string(&entry.partner_ieee),
                key: key_to_string(&entry.key),
                rx_counter: entry.rx_counter,
            })
            .collect();
        let children: Vec<Child> = proto::child_entries(&self.stack)
            .into_iter()
            .map(|entry| Child {
                ieee: eui64_to_string(&entry.ieee),
                nwk: entry.nwk.0,
                rx_on_when_idle: entry.flags.rx_on_when_idle,
                router: matches!(entry.flags.device_type, proto::ChildDeviceType::Router),
            })
            .collect();

        let mut data = self.data.lock().unwrap();
        let changed = data.link_keys != link_keys || data.children != children;
        data.link_keys = link_keys;
        data.children = children;
        changed
    }
}

/// Restores the link keys, children and address mappings of the previous run into the
/// stack. Without the child table, sleepy sensors drop off on restart.
fn restore_tables(stack: &ZigbeeStack<SpinelPhy>, data: &Persisted) {
    let keys: Vec<proto::KeyEntry> = data
        .link_keys
        .iter()
        .filter_map(|k| {
            Some(proto::KeyEntry {
                key: parse_key(&k.key).ok()?,
                tx_counter: 0,
                rx_counter: k.rx_counter,
                seq: 0,
                partner_ieee: parse_eui64(&k.ieee).ok()?,
            })
        })
        .collect();
    let children: Vec<proto::ChildEntry> = data
        .children
        .iter()
        .filter_map(|c| {
            Some(proto::ChildEntry {
                ieee: parse_eui64(&c.ieee).ok()?,
                nwk: Nwk(c.nwk),
                flags: proto::ChildFlags {
                    rx_on_when_idle: c.rx_on_when_idle,
                    device_type: if c.router {
                        proto::ChildDeviceType::Router
                    } else {
                        proto::ChildDeviceType::EndDevice
                    },
                },
            })
        })
        .collect();
    let addresses: Vec<proto::AddressEntry> = data
        .devices
        .values()
        .filter_map(|d| {
            Some(proto::AddressEntry {
                ieee: parse_eui64(&d.ieee).ok()?,
                nwk: Nwk(d.nwk),
            })
        })
        .collect();

    info!(
        "Restoring {} link keys, {} children, {} addresses",
        keys.len(),
        children.len(),
        addresses.len()
    );
    proto::apply_key_table(
        stack,
        proto::LoadKeyTablePayload {
            entries: keys,
        },
    );
    proto::apply_children(
        stack,
        proto::LoadChildrenPayload {
            entries: children,
        },
    );
    proto::apply_address_cache(
        stack,
        proto::LoadAddressCachePayload {
            entries: addresses,
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Endpoint;

    fn device_with(clusters: &[u16]) -> Device {
        let mut device = Device::new("00:17:88:01:02:03:04:05".into(), 0x1234);
        device.endpoints.push(Endpoint {
            id: 11,
            profile: HA_PROFILE,
            device_id: 0x010C,
            in_clusters: clusters.to_vec(),
            out_clusters: vec![],
        });
        device
    }

    fn record(id: u16, value: Value) -> zcl::AttributeRecord {
        zcl::AttributeRecord {
            id,
            status: zcl::STATUS_SUCCESS,
            value: Some(value),
        }
    }

    #[test]
    fn applies_standard_sensor_attributes() {
        let mut device = device_with(&[0x0001, 0x0402, 0x0405, 0x0500]);
        apply_zcl_records(&mut device, cluster::TEMPERATURE, None, &[record(0, Value::Int(2153))]);
        apply_zcl_records(&mut device, cluster::HUMIDITY, None, &[record(0, Value::Uint(4512))]);
        apply_zcl_records(&mut device, cluster::POWER_CONFIG, None, &[record(0x0021, Value::Uint(200))]);
        apply_zcl_records(&mut device, cluster::IAS_ZONE, None, &[record(0x0002, Value::Uint(0x0009))]);
        assert_eq!(device.state["temperature"], json!(21.53));
        assert_eq!(device.state["humidity"], json!(45.12));
        assert_eq!(device.state["battery"], json!(100));
        assert_eq!(device.state["alarm"], json!(true));
        assert_eq!(device.state["battery_low"], json!(true));
    }

    #[test]
    fn manufacturer_attributes_need_their_code() {
        let mut device = device_with(&[0x0000, 0x0001]);
        device.model = Some("RDM001".into());
        // genBasic 0x0034 without Signify's code is some other attribute
        apply_zcl_records(&mut device, cluster::BASIC, None, &[record(0x0034, Value::Uint(2))]);
        assert_eq!(device.state.get("device_mode"), None);
        apply_zcl_records(&mut device, cluster::BASIC, Some(0x100B), &[record(0x0034, Value::Uint(2))]);
        assert_eq!(device.state["device_mode"], json!("dual_rocker"));
    }

    #[test]
    fn ignores_invalid_temperature() {
        let mut device = device_with(&[0x0402]);
        apply_zcl_records(&mut device, cluster::TEMPERATURE, None, &[record(0, Value::Int(-0x8000))]);
        assert!(!device.state.contains_key("temperature"));
    }

    #[test]
    fn learning_the_model_switches_to_its_definition() {
        let mut device = device_with(&[0x0000, 0x0001, 0x0006, 0x0201, 0xFC11]);
        let basic = [record(0x0004, Value::Str("SONOFF".into())), record(0x0005, Value::Str("TRVZB".into()))];
        apply_zcl_records(&mut device, cluster::BASIC, None, &basic);
        apply_zcl_records(&mut device, cluster::THERMOSTAT, None, &[record(0x0012, Value::Int(2150)), record(0x001C, Value::Uint(4))]);
        apply_zcl_records(&mut device, 0xFC11, None, &[record(0x0000, Value::Bool(true))]);
        assert_eq!(device.state["setpoint"], json!(21.5));
        assert_eq!(device.state["system_mode"], json!("heat"));
        assert_eq!(device.state["child_lock"], json!(true));
        // The On/Off cluster, absent from the definition, is no longer a feature
        apply_zcl_records(&mut device, cluster::ON_OFF, None, &[record(0, Value::Bool(true))]);
        assert!(!device.state.contains_key("on"));
    }

    #[test]
    fn applies_tuya_datapoints_and_reports_unknown_ones() {
        let mut device = device_with(&[0xEF00]);
        device.manufacturer = Some("_TZE200_g9a3awaj".into());
        let dps = [tuya::Datapoint::value(16, 225), tuya::Datapoint::bool(1, true), tuya::Datapoint::bool(99, true)];
        let unknown = apply_tuya_datapoints(&mut device, &dps);
        assert_eq!(device.state["setpoint"], json!(22.5));
        assert_eq!(device.state["system_mode"], json!("heat"));
        assert_eq!(unknown.iter().map(|d| d.id).collect::<Vec<_>>(), [99]);
    }

    #[test]
    fn tuya_writes_use_the_datapoint_type() {
        assert_eq!(tuya_datapoint(1, tuya::dp_type::BOOL, 1).unwrap(), tuya::Datapoint::bool(1, true));
        assert_eq!(tuya_datapoint(2, tuya::dp_type::ENUM, 0).unwrap(), tuya::Datapoint::enumeration(2, 0));
        assert_eq!(tuya_datapoint(16, tuya::dp_type::VALUE, 215).unwrap(), tuya::Datapoint::value(16, 215));
        assert!(tuya_datapoint(45, tuya::dp_type::BITMAP, 0).is_err());
    }

    #[test]
    fn time_response_supports_both_epochs() {
        // 2026-01-01 00:00:00 UTC, UTC+3
        let now = 1_767_225_600;
        let standard = time_read_response(&[0x0000, 0x0007, 0x0005], now, 10_800, TimeBase::Zigbee2000);
        let since_2000 = (now - SECONDS_1970_TO_2000) as u32;
        let mut expected = vec![0x00, 0x00, 0x00, types::UTC_TIME];
        expected.extend_from_slice(&since_2000.to_le_bytes());
        expected.extend_from_slice(&[0x07, 0x00, 0x00, types::UINT32]);
        expected.extend_from_slice(&(since_2000 + 10_800).to_le_bytes());
        expected.extend_from_slice(&[0x05, 0x00, 0x86]);
        assert_eq!(standard, expected);

        let unix = time_read_response(&[0x0000], now, 0, TimeBase::Unix1970);
        assert_eq!(&unix[4..], &(now as u32).to_le_bytes());
    }

    #[test]
    fn default_names_come_from_definitions_and_stay_unique() {
        let mut sensor = device_with(&[0x0000]);
        sensor.model = Some("SNZB-02P".into());
        let base = devices::default_name(&sensor).unwrap();
        assert_eq!(base, "SNZB-02P");
        assert_eq!(unique_name(base, &sensor.ieee, &["SNZB-02P".into()]), "SNZB-02P 0405");

        let mut thermostat = device_with(&[0x0000, 0xEF00]);
        thermostat.manufacturer = Some("_TZE200_g9a3awaj".into());
        thermostat.model = Some("TS0601".into());
        assert_eq!(devices::default_name(&thermostat).as_deref(), Some("Thermostat"));
    }

    #[test]
    fn model_named_devices_get_their_definition_name_and_stale_keys_go() {
        let mut device = device_with(&[0x0000, 0x0201, 0xFC11]);
        device.model = Some("TRVZB".into());
        device.name = "TRVZB".into();
        device.interviewed = true;
        device.state.insert("on".into(), json!(false));
        device.state.insert("setpoint".into(), json!(19.0));
        let mut data = Persisted::default();
        data.devices.insert(device.ieee.clone(), device.clone());
        apply_default_names(&mut data);
        let device = &data.devices[&device.ieee];
        assert_eq!(device.name, "Radiator valve");
        assert!(!device.state.contains_key("on"));
        assert_eq!(device.state["setpoint"], json!(19.0));

        let mut renamed = device.clone();
        renamed.name = "Living room".into();
        assert!(!untouched_name(&renamed));
        renamed.name = "Termostat".into();
        assert!(untouched_name(&renamed));
        renamed.name = format!("Cihaz {}", &renamed.ieee.replace(':', "")[12..]);
        assert!(untouched_name(&renamed));
    }

    #[test]
    fn settled_write_leaves_newer_pending_value() {
        let mut pending: HashMap<String, BTreeMap<String, Json>> = HashMap::new();
        let ieee = "38:39:8f:ff:fe:de:0d:70";
        pending.entry(ieee.into()).or_default().insert("temperature_calibration".into(), json!(1.0));

        // The user changed the value during the write: the old write must not clear it
        pending.get_mut(ieee).unwrap().insert("temperature_calibration".into(), json!(-0.5));
        settle_pending(&mut pending, ieee, "temperature_calibration", &json!(1.0));
        assert_eq!(pending[ieee]["temperature_calibration"], json!(-0.5));

        settle_pending(&mut pending, ieee, "temperature_calibration", &json!(-0.5));
        assert!(!pending.contains_key(ieee));
    }

    #[test]
    fn a_settings_patch_keeps_what_it_leaves_out() {
        let mut current = json!({"availability_mains_minutes": 10, "mqtt": {"enabled": true, "host": "127.0.0.1", "port": 1883}});
        merge_json(&mut current, json!({"availability_mains_minutes": 1, "mqtt": {"port": 1884}}));
        assert_eq!(current, json!({"availability_mains_minutes": 1, "mqtt": {"enabled": true, "host": "127.0.0.1", "port": 1884}}));
    }
}
