//! The bridged devices: one Matter endpoint per Zigbee device, under the aggregator.
//!
//! The endpoint list follows the hub (devices joining, leaving, changing), so the node
//! metadata is built on each access instead of being a constant. Reads come from the
//! hub's device state; commands go to the device as the ZCL command with the same id
//! (Matter's lighting clusters are ZCL's), so transitions run on the light itself.
//! Changes reported over Zigbee are pushed to subscribers.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};

use rs_matter::dm::clusters::decl::bridged_device_basic_information as bridged;
use rs_matter::dm::clusters::decl::color_control;
use rs_matter::dm::clusters::decl::level_control;
use rs_matter::dm::clusters::decl::on_off;
use rs_matter::dm::clusters::decl::power_source;
use rs_matter::dm::clusters::decl::relative_humidity_measurement as humidity;
use rs_matter::dm::clusters::decl::switch;
use rs_matter::dm::clusters::decl::temperature_measurement as temperature;
use rs_matter::dm::clusters::decl::thermostat;
use rs_matter::dm::clusters::desc::{self, ClusterHandler as _};
use rs_matter::dm::clusters::groups::{self, ClusterHandler as _};
use rs_matter::dm::devices::{
    DEV_TYPE_AGGREGATOR, DEV_TYPE_BRIDGED_NODE, DEV_TYPE_DIMMABLE_LIGHT, DEV_TYPE_GENERIC_SWITCH, DEV_TYPE_ON_OFF_LIGHT,
    DEV_TYPE_THERMOSTAT,
};
use rs_matter::dm::{
    ArrayAttributeRead, AsyncHandler, AttrChangeNotifier, Cluster, ClusterId, Dataver, DeviceType, EndptId, Endpoint,
    EventEmitter, InvokeContext, InvokeReply, MatchContext, Metadata, Node, ReadContext, ReadReply, WriteContext,
};
use rs_matter::im::encoding::EventPriority;
use rs_matter::im::events::EVENT_DATA_TAG;
use rs_matter::error::{Error, ErrorCode};
use rs_matter::tlv::{Nullable, TLVBuilderParent, ToTLVArrayBuilder, ToTLVBuilder, Utf8StrBuilder};
use rs_matter::{clusters, devices, root_endpoint, with};
use serde_json::{Value as Json, json};
use tracing::{debug, info, warn};

use crate::devices::{Access, Card, Kind as FeatureKind};
use crate::hub::{DeviceView, Hub};

pub const AGGREGATOR_ENDPOINT: EndptId = 1;

const ON_OFF_ID: ClusterId = on_off::FULL_CLUSTER.id;
const LEVEL_ID: ClusterId = level_control::FULL_CLUSTER.id;
const COLOR_ID: ClusterId = color_control::FULL_CLUSTER.id;
const BRIDGED_ID: ClusterId = bridged::FULL_CLUSTER.id;
const TEMPERATURE_ID: ClusterId = temperature::FULL_CLUSTER.id;
const HUMIDITY_ID: ClusterId = humidity::FULL_CLUSTER.id;
const POWER_ID: ClusterId = power_source::FULL_CLUSTER.id;
const THERMOSTAT_ID: ClusterId = thermostat::FULL_CLUSTER.id;
const SWITCH_ID: ClusterId = switch::FULL_CLUSTER.id;

const DEV_TYPE_COLOR_TEMPERATURE_LIGHT: DeviceType = DeviceType { dtype: 0x010C, drev: 4 };
const DEV_TYPE_TEMPERATURE_SENSOR: DeviceType = DeviceType { dtype: 0x0302, drev: 2 };
const DEV_TYPE_HUMIDITY_SENSOR: DeviceType = DeviceType { dtype: 0x0307, drev: 2 };
const DEV_TYPE_POWER_SOURCE: DeviceType = DeviceType { dtype: 0x0011, drev: 1 };

const ON_OFF_CLUSTER: Cluster<'static> = on_off::FULL_CLUSTER
    .with_revision(6)
    .with_features(on_off::Feature::LIGHTING.bits())
    .with_attrs(with!(
        required;
        on_off::AttributeId::OnOff
            | on_off::AttributeId::GlobalSceneControl
            | on_off::AttributeId::OnTime
            | on_off::AttributeId::OffWaitTime
            | on_off::AttributeId::StartUpOnOff
    ))
    .with_cmds(with!(
        on_off::CommandId::Off
            | on_off::CommandId::On
            | on_off::CommandId::Toggle
            | on_off::CommandId::OffWithEffect
            | on_off::CommandId::OnWithRecallGlobalScene
            | on_off::CommandId::OnWithTimedOff
    ));

const LEVEL_CLUSTER: Cluster<'static> = level_control::FULL_CLUSTER
    .with_revision(6)
    .with_features(level_control::Feature::ON_OFF.bits() | level_control::Feature::LIGHTING.bits())
    .with_attrs(with!(
        required;
        level_control::AttributeId::CurrentLevel
            | level_control::AttributeId::RemainingTime
            | level_control::AttributeId::MinLevel
            | level_control::AttributeId::MaxLevel
            | level_control::AttributeId::OnLevel
            | level_control::AttributeId::Options
            | level_control::AttributeId::StartUpCurrentLevel
    ))
    .with_cmds(with!(
        level_control::CommandId::MoveToLevel
            | level_control::CommandId::Move
            | level_control::CommandId::Step
            | level_control::CommandId::Stop
            | level_control::CommandId::MoveToLevelWithOnOff
            | level_control::CommandId::MoveWithOnOff
            | level_control::CommandId::StepWithOnOff
            | level_control::CommandId::StopWithOnOff
    ));

const COLOR_CLUSTER: Cluster<'static> = color_control::FULL_CLUSTER
    .with_revision(7)
    .with_features(color_control::Feature::COLOR_TEMPERATURE.bits())
    .with_attrs(with!(
        required;
        color_control::AttributeId::RemainingTime
            | color_control::AttributeId::ColorTemperatureMireds
            | color_control::AttributeId::ColorMode
            | color_control::AttributeId::Options
            | color_control::AttributeId::NumberOfPrimaries
            | color_control::AttributeId::EnhancedColorMode
            | color_control::AttributeId::ColorCapabilities
            | color_control::AttributeId::ColorTempPhysicalMinMireds
            | color_control::AttributeId::ColorTempPhysicalMaxMireds
            | color_control::AttributeId::CoupleColorTempToLevelMinMireds
            | color_control::AttributeId::StartUpColorTemperatureMireds
    ))
    .with_cmds(with!(
        color_control::CommandId::MoveToColorTemperature
            | color_control::CommandId::MoveColorTemperature
            | color_control::CommandId::StepColorTemperature
            | color_control::CommandId::StopMoveStep
    ));

const BRIDGED_CLUSTER: Cluster<'static> = bridged::FULL_CLUSTER
    .with_features(0)
    .with_attrs(with!(
        required;
        bridged::AttributeId::VendorName
            | bridged::AttributeId::ProductName
            | bridged::AttributeId::NodeLabel
            | bridged::AttributeId::SerialNumber
            | bridged::AttributeId::Reachable
            | bridged::AttributeId::UniqueID
    ))
    .with_cmds(with!());

const TEMPERATURE_CLUSTER: Cluster<'static> = temperature::FULL_CLUSTER
    .with_revision(4)
    .with_attrs(with!(required; temperature::AttributeId::MeasuredValue | temperature::AttributeId::MinMeasuredValue | temperature::AttributeId::MaxMeasuredValue))
    .with_cmds(with!());

const HUMIDITY_CLUSTER: Cluster<'static> = humidity::FULL_CLUSTER
    .with_revision(3)
    .with_attrs(with!(required; humidity::AttributeId::MeasuredValue | humidity::AttributeId::MinMeasuredValue | humidity::AttributeId::MaxMeasuredValue))
    .with_cmds(with!());

const POWER_CLUSTER: Cluster<'static> = power_source::FULL_CLUSTER
    .with_revision(2)
    .with_features(power_source::Feature::BATTERY.bits())
    .with_attrs(with!(
        required;
        power_source::AttributeId::Status
            | power_source::AttributeId::Order
            | power_source::AttributeId::Description
            | power_source::AttributeId::BatPercentRemaining
            | power_source::AttributeId::BatChargeLevel
            | power_source::AttributeId::BatReplacementNeeded
            | power_source::AttributeId::BatReplaceability
            | power_source::AttributeId::EndpointList
    ))
    .with_cmds(with!());

const THERMOSTAT_CLUSTER: Cluster<'static> = thermostat::FULL_CLUSTER
    .with_revision(7)
    .with_features(thermostat::Feature::HEATING.bits())
    .with_attrs(with!(
        required;
        thermostat::AttributeId::LocalTemperature
            | thermostat::AttributeId::AbsMinHeatSetpointLimit
            | thermostat::AttributeId::AbsMaxHeatSetpointLimit
            | thermostat::AttributeId::OccupiedHeatingSetpoint
            | thermostat::AttributeId::ControlSequenceOfOperation
            | thermostat::AttributeId::SystemMode
            | thermostat::AttributeId::ThermostatRunningState
    ))
    .with_cmds(with!(thermostat::CommandId::SetpointRaiseLower));

const SWITCH_CLUSTER: Cluster<'static> = switch::FULL_CLUSTER
    .with_revision(2)
    .with_features(
        switch::Feature::MOMENTARY_SWITCH.bits()
            | switch::Feature::MOMENTARY_SWITCH_RELEASE.bits()
            | switch::Feature::MOMENTARY_SWITCH_LONG_PRESS.bits(),
    )
    .with_attrs(with!(required; switch::AttributeId::NumberOfPositions | switch::AttributeId::CurrentPosition))
    .with_cmds(with!())
    .with_events(with!(
        switch::EventId::InitialPress | switch::EventId::LongPress | switch::EventId::ShortRelease | switch::EventId::LongRelease
    ));

/// The kinds of bridged devices, by what the Matter side offers
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    OnOffLight,
    DimmableLight,
    ColorTemperatureLight,
    TemperatureSensor { humidity: bool, battery: bool },
    Thermostat { battery: bool },
    /// One button of a remote or switch module
    Switch { battery: bool },
}

impl Kind {
    /// A light kind from which features a device or the members of a group have
    fn light(level: bool, color_temp: bool) -> Self {
        match (level, color_temp) {
            (_, true) => Self::ColorTemperatureLight,
            (true, false) => Self::DimmableLight,
            _ => Self::OnOffLight,
        }
    }

    /// What a hub device becomes; `None` for devices not bridged (yet)
    pub fn of(view: &DeviceView) -> Option<Self> {
        let writable = |key: &str| view.features.iter().any(|f| f.key == key && f.access != Access::Read);
        let has = |key: &str| view.features.iter().any(|f| f.key == key);
        let battery = has("battery");
        match view.card {
            Card::Light if writable("on") => Some(match (writable("level"), writable("color_temp")) {
                (true, true) => Self::ColorTemperatureLight,
                (true, false) => Self::DimmableLight,
                _ => Self::OnOffLight,
            }),
            Card::Thermostat if writable("setpoint") && has("temperature") => Some(Self::Thermostat { battery }),
            Card::Sensor if has("temperature") => Some(Self::TemperatureSensor {
                humidity: has("humidity"),
                battery,
            }),
            _ => None,
        }
    }

    fn device_types(self) -> &'static [DeviceType] {
        match self {
            Self::OnOffLight => devices!(DEV_TYPE_ON_OFF_LIGHT, DEV_TYPE_BRIDGED_NODE),
            Self::DimmableLight => devices!(DEV_TYPE_DIMMABLE_LIGHT, DEV_TYPE_BRIDGED_NODE),
            Self::ColorTemperatureLight => devices!(DEV_TYPE_COLOR_TEMPERATURE_LIGHT, DEV_TYPE_BRIDGED_NODE),
            Self::TemperatureSensor { humidity: false, battery: false } => devices!(DEV_TYPE_TEMPERATURE_SENSOR, DEV_TYPE_BRIDGED_NODE),
            Self::TemperatureSensor { humidity: true, battery: false } => {
                devices!(DEV_TYPE_TEMPERATURE_SENSOR, DEV_TYPE_HUMIDITY_SENSOR, DEV_TYPE_BRIDGED_NODE)
            }
            Self::TemperatureSensor { humidity: false, battery: true } => {
                devices!(DEV_TYPE_TEMPERATURE_SENSOR, DEV_TYPE_POWER_SOURCE, DEV_TYPE_BRIDGED_NODE)
            }
            Self::TemperatureSensor { humidity: true, battery: true } => devices!(
                DEV_TYPE_TEMPERATURE_SENSOR,
                DEV_TYPE_HUMIDITY_SENSOR,
                DEV_TYPE_POWER_SOURCE,
                DEV_TYPE_BRIDGED_NODE
            ),
            Self::Thermostat { battery: false } => devices!(DEV_TYPE_THERMOSTAT, DEV_TYPE_BRIDGED_NODE),
            Self::Thermostat { battery: true } => devices!(DEV_TYPE_THERMOSTAT, DEV_TYPE_POWER_SOURCE, DEV_TYPE_BRIDGED_NODE),
            Self::Switch { battery: false } => devices!(DEV_TYPE_GENERIC_SWITCH, DEV_TYPE_BRIDGED_NODE),
            Self::Switch { battery: true } => devices!(DEV_TYPE_GENERIC_SWITCH, DEV_TYPE_POWER_SOURCE, DEV_TYPE_BRIDGED_NODE),
        }
    }

    fn clusters(self) -> &'static [Cluster<'static>] {
        match self {
            Self::OnOffLight => clusters!(desc::DescHandler::CLUSTER, groups::GroupsHandler::CLUSTER, BRIDGED_CLUSTER, ON_OFF_CLUSTER),
            Self::DimmableLight => clusters!(
                desc::DescHandler::CLUSTER,
                groups::GroupsHandler::CLUSTER,
                BRIDGED_CLUSTER,
                ON_OFF_CLUSTER,
                LEVEL_CLUSTER
            ),
            Self::ColorTemperatureLight => clusters!(
                desc::DescHandler::CLUSTER,
                groups::GroupsHandler::CLUSTER,
                BRIDGED_CLUSTER,
                ON_OFF_CLUSTER,
                LEVEL_CLUSTER,
                COLOR_CLUSTER
            ),
            Self::TemperatureSensor { humidity: false, battery: false } => {
                clusters!(desc::DescHandler::CLUSTER, BRIDGED_CLUSTER, TEMPERATURE_CLUSTER)
            }
            Self::TemperatureSensor { humidity: true, battery: false } => {
                clusters!(desc::DescHandler::CLUSTER, BRIDGED_CLUSTER, TEMPERATURE_CLUSTER, HUMIDITY_CLUSTER)
            }
            Self::TemperatureSensor { humidity: false, battery: true } => {
                clusters!(desc::DescHandler::CLUSTER, BRIDGED_CLUSTER, TEMPERATURE_CLUSTER, POWER_CLUSTER)
            }
            Self::TemperatureSensor { humidity: true, battery: true } => clusters!(
                desc::DescHandler::CLUSTER,
                BRIDGED_CLUSTER,
                TEMPERATURE_CLUSTER,
                HUMIDITY_CLUSTER,
                POWER_CLUSTER
            ),
            Self::Thermostat { battery: false } => clusters!(desc::DescHandler::CLUSTER, BRIDGED_CLUSTER, THERMOSTAT_CLUSTER),
            Self::Thermostat { battery: true } => {
                clusters!(desc::DescHandler::CLUSTER, BRIDGED_CLUSTER, THERMOSTAT_CLUSTER, POWER_CLUSTER)
            }
            Self::Switch { battery: false } => clusters!(desc::DescHandler::CLUSTER, BRIDGED_CLUSTER, SWITCH_CLUSTER),
            Self::Switch { battery: true } => {
                clusters!(desc::DescHandler::CLUSTER, BRIDGED_CLUSTER, SWITCH_CLUSTER, POWER_CLUSTER)
            }
        }
    }
}

/// What an endpoint stands for
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Device(String),
    /// One button of a remote or switch module, by its action prefix ("left", "right")
    Button { ieee: String, button: String },
    /// A Zigbee group of lights, controlled as one light
    Group(u16),
}

impl Target {
    /// The key the endpoint id is allocated under
    fn key(&self) -> String {
        match self {
            Self::Device(ieee) => ieee.clone(),
            Self::Button { ieee, button } => format!("{ieee}#{button}"),
            Self::Group(id) => format!("group:{id}"),
        }
    }

    fn ieee(&self) -> Option<&str> {
        match self {
            Self::Device(ieee) | Self::Button { ieee, .. } => Some(ieee),
            Self::Group(_) => None,
        }
    }
}

/// One bridged endpoint
pub struct Bridged {
    pub endpoint: EndptId,
    pub target: Target,
    pub kind: Kind,
    /// The name as last pushed (the node label)
    name: String,
    /// Switch: 1 while the button is held
    position: AtomicU8,
    datavers: HashMap<ClusterId, Dataver>,
}

impl Bridged {
    fn metadata(&self) -> Endpoint<'static> {
        Endpoint::new(self.endpoint, self.kind.device_types(), self.kind.clusters())
    }

    /// Clusters whose attributes come from the device state (pushed on changes)
    fn state_clusters(&self) -> impl Iterator<Item = ClusterId> + '_ {
        self.kind.clusters().iter().map(|c| c.id).filter(|id| *id != desc::DescHandler::CLUSTER.id && *id != groups::GroupsHandler::CLUSTER.id)
    }
}

/// What the Matter thread shares with the cluster handlers
pub struct Bridge {
    pub hub: Arc<Hub>,
    /// The hub's runtime: Zigbee commands run there and are awaited from Matter's thread
    pub rt: tokio::runtime::Handle,
    pub bridged: Mutex<Vec<Bridged>>,
}

/// Something that wants to hear that attributes changed (the interaction model)
pub trait Notify {
    fn cluster_changed(&self, endpoint: EndptId, cluster: ClusterId);
    fn endpoint_changed(&self, endpoint: EndptId);
}

/// The names and identity a bridged endpoint shows
struct Info {
    name: String,
    vendor: String,
    product: String,
    serial: String,
    reachable: bool,
}

/// The buttons a device's action feature names: "left_press" → "left"
fn buttons(view: &DeviceView) -> Vec<String> {
    let Some(feature) = view.features.iter().find(|f| f.key == "action") else {
        return Vec::new();
    };
    let FeatureKind::Enum { values } = feature.kind else {
        return Vec::new();
    };
    values.iter().filter_map(|v| v.strip_suffix("_press")).map(str::to_owned).collect()
}

impl Bridge {
    pub fn new(hub: Arc<Hub>, rt: tokio::runtime::Handle) -> Self {
        Self {
            hub,
            rt,
            bridged: Mutex::new(Vec::new()),
        }
    }

    fn target(&self, endpoint: EndptId) -> Result<Target, Error> {
        let bridged = self.bridged.lock().unwrap();
        bridged
            .iter()
            .find(|b| b.endpoint == endpoint)
            .map(|b| b.target.clone())
            .ok_or_else(|| ErrorCode::EndpointNotFound.into())
    }

    fn device(&self, ieee: &str) -> Result<DeviceView, Error> {
        self.hub.device_view(ieee).ok_or_else(|| ErrorCode::EndpointNotFound.into())
    }

    fn members(&self, group: u16) -> Vec<DeviceView> {
        let groups = self.hub.groups();
        let Some(group) = groups.iter().find(|g| g.id == group) else {
            return Vec::new();
        };
        group.members.iter().filter_map(|m| self.hub.device_view(&m.ieee)).collect()
    }

    /// The device behind the endpoint (a device or one of its buttons)
    fn view(&self, endpoint: EndptId) -> Result<DeviceView, Error> {
        match self.target(endpoint)? {
            Target::Device(ieee) | Target::Button { ieee, .. } => self.device(&ieee),
            Target::Group(_) => Err(ErrorCode::AttributeNotFound.into()),
        }
    }

    /// A state value; for a group the members' combined light state
    fn state(&self, endpoint: EndptId, key: &str) -> Result<Option<Json>, Error> {
        match self.target(endpoint)? {
            Target::Device(ieee) | Target::Button { ieee, .. } => Ok(self.device(&ieee)?.device.state.get(key).cloned()),
            Target::Group(id) => {
                let members = self.members(id);
                let values = || members.iter().filter_map(|m| m.device.state.get(key));
                Ok(match key {
                    "on" => Some(json!(values().any(|v| v.as_bool() == Some(true)))),
                    "level" => values().filter_map(Json::as_f64).reduce(f64::max).map(|v| json!(v)),
                    _ => values().next().cloned(),
                })
            }
        }
    }

    /// The valid range of a numeric feature; for a group the narrowest of its members
    fn range(&self, endpoint: EndptId, key: &str) -> Result<(Option<f64>, Option<f64>), Error> {
        let views = match self.target(endpoint)? {
            Target::Device(ieee) | Target::Button { ieee, .. } => vec![self.device(&ieee)?],
            Target::Group(id) => self.members(id),
        };
        let mut range: (Option<f64>, Option<f64>) = (None, None);
        for view in &views {
            if let Some(feature) = view.features.iter().find(|f| f.key == key) {
                let (min, max) = feature.range(&view.device);
                range.0 = match (range.0, min) {
                    (Some(a), Some(b)) => Some(a.max(b)),
                    (a, b) => a.or(b),
                };
                range.1 = match (range.1, max) {
                    (Some(a), Some(b)) => Some(a.min(b)),
                    (a, b) => a.or(b),
                };
            }
        }
        Ok(range)
    }

    fn info(&self, endpoint: EndptId) -> Result<Info, Error> {
        match self.target(endpoint)? {
            Target::Group(id) => {
                let name = self.hub.groups().into_iter().find(|g| g.id == id).map(|g| g.name).unwrap_or_default();
                Ok(Info {
                    name,
                    vendor: "zhub".into(),
                    product: "Zigbee group".into(),
                    serial: format!("group-{id}"),
                    reachable: true,
                })
            }
            target => {
                let view = self.view(endpoint)?;
                let mut name = view.device.name.clone();
                if let Target::Button { button, .. } = &target {
                    name = format!("{name} {button}");
                }
                Ok(Info {
                    name,
                    vendor: view.definition.map(|d| d.vendor.to_owned()).or(view.device.manufacturer.clone()).unwrap_or_default(),
                    product: view.device.model.clone().unwrap_or_default(),
                    serial: target.key(),
                    reachable: !self.hub.is_offline(&view.device),
                })
            }
        }
    }

    fn dataver(&self, endpoint: EndptId, cluster: ClusterId) -> u32 {
        let bridged = self.bridged.lock().unwrap();
        bridged
            .iter()
            .find(|b| b.endpoint == endpoint)
            .and_then(|b| b.datavers.get(&cluster))
            .map_or(0, Dataver::get)
    }

    fn dataver_changed(&self, endpoint: EndptId, cluster: ClusterId) {
        let bridged = self.bridged.lock().unwrap();
        if let Some(dataver) = bridged.iter().find(|b| b.endpoint == endpoint).and_then(|b| b.datavers.get(&cluster)) {
            dataver.changed();
        }
    }

    /// Sends the ZCL command (to the device, or to the group in one multicast frame) and
    /// sets the state the command leads to, so the controller sees the result before the
    /// devices report it
    async fn command(&self, endpoint: EndptId, cluster: u16, command: u8, payload: Vec<u8>, state: Vec<(&'static str, Json)>) -> Result<(), Error> {
        match self.target(endpoint)? {
            Target::Group(id) => self.hub.send_group_zcl(id, cluster, command, &payload, state).map_err(|e| {
                warn!("Matter → group {id} cluster 0x{cluster:04x} command 0x{command:02x}: {e:#}");
                ErrorCode::Failure.into()
            }),
            Target::Device(ieee) | Target::Button { ieee, .. } => {
                let hub = self.hub.clone();
                let target = ieee.clone();
                let sent = self
                    .rt
                    .spawn(async move { hub.send_zcl_command(&target, cluster, command, payload).await })
                    .await;
                match sent {
                    Ok(Ok(())) => {
                        if !state.is_empty() {
                            self.hub.set_state_values(&ieee, state);
                        }
                        Ok(())
                    }
                    Ok(Err(e)) => {
                        warn!("Matter → {ieee} cluster 0x{cluster:04x} command 0x{command:02x}: {e:#}");
                        Err(ErrorCode::Failure.into())
                    }
                    Err(_) => Err(ErrorCode::Failure.into()),
                }
            }
        }
    }

    /// Writes a feature value through the hub, as the UI does (sleepy devices included)
    async fn set(&self, endpoint: EndptId, key: &'static str, value: Json) -> Result<(), Error> {
        let Target::Device(ieee) = self.target(endpoint)? else {
            return Err(ErrorCode::UnsupportedAccess.into());
        };
        let hub = self.hub.clone();
        let target = ieee.clone();
        match self.rt.spawn(async move { hub.set(&target, key, value).await }).await {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(e)) => {
                warn!("Matter → {ieee} {key}: {e:#}");
                Err(ErrorCode::ConstraintError.into())
            }
            Err(_) => Err(ErrorCode::Failure.into()),
        }
    }

    /// The endpoints the hub's devices and groups call for, with their names
    fn wanted(&self) -> Vec<(Target, Kind, String)> {
        let devices = self.hub.devices();
        let mut wanted = Vec::new();
        for view in &devices {
            if let Some(kind) = Kind::of(view) {
                wanted.push((Target::Device(view.device.ieee.clone()), kind, view.device.name.clone()));
            }
            let has_battery = view.features.iter().any(|f| f.key == "battery");
            for (i, button) in buttons(view).into_iter().enumerate() {
                let name = format!("{} {button}", view.device.name);
                let target = Target::Button {
                    ieee: view.device.ieee.clone(),
                    button,
                };
                // The battery once, on the first button
                wanted.push((target, Kind::Switch { battery: i == 0 && has_battery }, name));
            }
        }
        for group in self.hub.groups() {
            let members: Vec<&DeviceView> = group.members.iter().filter_map(|m| devices.iter().find(|d| d.device.ieee == m.ieee)).collect();
            let writable = |key: &str| members.iter().any(|m| m.features.iter().any(|f| f.key == key && f.access != Access::Read));
            if writable("on") {
                wanted.push((Target::Group(group.id), Kind::light(writable("level"), writable("color_temp")), group.name.clone()));
            }
        }
        wanted
    }

    /// Brings the bridged endpoints in line with the hub's devices and groups; tells
    /// subscribers what changed
    pub fn sync(&self, notify: &impl Notify) {
        let wanted = self.wanted();
        let mut parts_changed = false;
        let mut changed = Vec::new();
        {
            let mut bridged = self.bridged.lock().unwrap();
            let before = bridged.len();
            bridged.retain(|b| wanted.iter().any(|(t, k, _)| *t == b.target && *k == b.kind));
            parts_changed |= bridged.len() != before;
            for (target, kind, name) in wanted {
                if let Some(existing) = bridged.iter_mut().find(|b| b.target == target) {
                    if existing.name != name {
                        existing.name = name;
                        changed.push((existing.endpoint, BRIDGED_ID));
                    }
                    continue;
                }
                let endpoint = self.hub.matter_endpoint(&target.key());
                let datavers = kind.clusters().iter().map(|c| (c.id, Dataver::new(rand::random()))).collect();
                info!("Matter: {name} on endpoint {endpoint} as {kind:?}");
                bridged.push(Bridged {
                    endpoint,
                    target,
                    kind,
                    name,
                    position: AtomicU8::new(0),
                    datavers,
                });
                parts_changed = true;
            }
            bridged.sort_by_key(|b| b.endpoint);
        }
        for (endpoint, cluster) in changed {
            notify.cluster_changed(endpoint, cluster);
        }
        if parts_changed {
            notify.endpoint_changed(AGGREGATOR_ENDPOINT);
        }
    }

    /// A device's state changed: its endpoints and the groups it is in may have
    pub fn device_changed(&self, ieee: &str, notify: &impl Notify) {
        let groups: Vec<u16> = self.hub.groups().iter().filter(|g| g.members.iter().any(|m| m.ieee == ieee)).map(|g| g.id).collect();
        let targets: Vec<(EndptId, Vec<ClusterId>)> = {
            let bridged = self.bridged.lock().unwrap();
            bridged
                .iter()
                .filter(|b| b.target.ieee() == Some(ieee) || matches!(b.target, Target::Group(id) if groups.contains(&id)))
                .map(|b| (b.endpoint, b.state_clusters().collect()))
                .collect()
        };
        for (endpoint, clusters) in targets {
            for cluster in clusters {
                notify.cluster_changed(endpoint, cluster);
            }
        }
    }

    /// A button press: the Switch cluster's position and events
    pub fn action<E: EventEmitter + AttrChangeNotifier>(&self, ieee: &str, action: &str, im: &E) {
        let found = {
            let bridged = self.bridged.lock().unwrap();
            bridged.iter().find_map(|b| match &b.target {
                Target::Button { ieee: i, button } if i == ieee => action
                    .strip_prefix(button.as_str())
                    .and_then(|rest| rest.strip_prefix('_'))
                    .map(|kind| (b.endpoint, kind.to_owned())),
                _ => None,
            })
        };
        let Some((endpoint, kind)) = found else {
            return;
        };
        let (event, position) = match kind.as_str() {
            "press" => (switch::EventId::InitialPress, 1),
            "hold" => (switch::EventId::LongPress, 1),
            "press_release" => (switch::EventId::ShortRelease, 0),
            "hold_release" => (switch::EventId::LongRelease, 0),
            _ => return,
        };
        if let Some(b) = self.bridged.lock().unwrap().iter().find(|b| b.endpoint == endpoint) {
            b.position.store(position, Ordering::Relaxed);
        }
        im.notify_attr_changed(endpoint, SWITCH_ID, switch::AttributeId::CurrentPosition as _);
        let emitted = im.emit_event(endpoint, SWITCH_ID, event as _, EventPriority::Info, |w| match event {
            switch::EventId::InitialPress => switch::InitialPressBuilder::new(w, &EVENT_DATA_TAG)?.new_position(1)?.end().map(drop),
            switch::EventId::LongPress => switch::LongPressBuilder::new(w, &EVENT_DATA_TAG)?.new_position(1)?.end().map(drop),
            switch::EventId::ShortRelease => {
                switch::ShortReleaseBuilder::new(w, &EVENT_DATA_TAG)?.previous_position(1)?.end().map(drop)
            }
            _ => switch::LongReleaseBuilder::new(w, &EVENT_DATA_TAG)?.previous_position(1)?.end().map(drop),
        });
        match emitted {
            Ok(number) => debug!("Matter event {event:?} on endpoint {endpoint} (#{number})"),
            Err(e) => warn!("Matter event {event:?} on endpoint {endpoint} not emitted: {e:?}"),
        }
    }

    fn position(&self, endpoint: EndptId) -> u8 {
        let bridged = self.bridged.lock().unwrap();
        bridged.iter().find(|b| b.endpoint == endpoint).map_or(0, |b| b.position.load(Ordering::Relaxed))
    }
}

// ---- Node metadata ----

/// The node: root, aggregator, and the bridged endpoints as they are now
pub struct BridgeNode<'a>(pub &'a Bridge);

const ROOT: Endpoint<'static> = root_endpoint!(eth);
const AGGREGATOR: Endpoint<'static> = Endpoint::new(AGGREGATOR_ENDPOINT, devices!(DEV_TYPE_AGGREGATOR), clusters!(desc::DescHandler::CLUSTER));

impl Metadata for BridgeNode<'_> {
    fn access<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&Node<'_>) -> R,
    {
        let mut endpoints = vec![ROOT, AGGREGATOR];
        endpoints.extend(self.0.bridged.lock().unwrap().iter().map(Bridged::metadata));
        f(&Node { endpoints: &endpoints })
    }
}

// ---- Dispatch to the bridged device's cluster ----

/// Handles every cluster of the bridged endpoints except Descriptor and Groups
pub struct BridgedHandler<'a>(pub &'a Bridge);

impl BridgedHandler<'_> {
    pub fn handles(endpoint: EndptId, cluster: ClusterId) -> bool {
        endpoint > AGGREGATOR_ENDPOINT && cluster != desc::DescHandler::CLUSTER.id && cluster != groups::GroupsHandler::CLUSTER.id
    }
}

impl AsyncHandler for BridgedHandler<'_> {
    async fn read(&self, ctx: impl ReadContext, reply: impl ReadReply) -> Result<(), Error> {
        let (endpoint, cluster) = (ctx.attr().endpoint_id, ctx.attr().cluster_id);
        let c = Cl { bridge: self.0, endpoint };
        match cluster {
            ON_OFF_ID => on_off::HandlerAsyncAdaptor(&c).read(ctx, reply).await,
            LEVEL_ID => level_control::HandlerAsyncAdaptor(&c).read(ctx, reply).await,
            COLOR_ID => color_control::HandlerAsyncAdaptor(&c).read(ctx, reply).await,
            BRIDGED_ID => bridged::HandlerAsyncAdaptor(&c).read(ctx, reply).await,
            TEMPERATURE_ID => temperature::HandlerAsyncAdaptor(&c).read(ctx, reply).await,
            HUMIDITY_ID => humidity::HandlerAsyncAdaptor(&c).read(ctx, reply).await,
            POWER_ID => power_source::HandlerAsyncAdaptor(&c).read(ctx, reply).await,
            THERMOSTAT_ID => thermostat::HandlerAsyncAdaptor(&c).read(ctx, reply).await,
            SWITCH_ID => switch::HandlerAsyncAdaptor(&c).read(ctx, reply).await,
            _ => Err(ErrorCode::ClusterNotFound.into()),
        }
    }

    async fn write(&self, ctx: impl WriteContext) -> Result<(), Error> {
        let (endpoint, cluster) = (ctx.attr().endpoint_id, ctx.attr().cluster_id);
        let c = Cl { bridge: self.0, endpoint };
        match cluster {
            ON_OFF_ID => on_off::HandlerAsyncAdaptor(&c).write(ctx).await,
            LEVEL_ID => level_control::HandlerAsyncAdaptor(&c).write(ctx).await,
            COLOR_ID => color_control::HandlerAsyncAdaptor(&c).write(ctx).await,
            BRIDGED_ID => bridged::HandlerAsyncAdaptor(&c).write(ctx).await,
            TEMPERATURE_ID => temperature::HandlerAsyncAdaptor(&c).write(ctx).await,
            HUMIDITY_ID => humidity::HandlerAsyncAdaptor(&c).write(ctx).await,
            POWER_ID => power_source::HandlerAsyncAdaptor(&c).write(ctx).await,
            THERMOSTAT_ID => thermostat::HandlerAsyncAdaptor(&c).write(ctx).await,
            SWITCH_ID => switch::HandlerAsyncAdaptor(&c).write(ctx).await,
            _ => Err(ErrorCode::ClusterNotFound.into()),
        }
    }

    async fn invoke(&self, ctx: impl InvokeContext, reply: impl InvokeReply) -> Result<(), Error> {
        let (endpoint, cluster) = (ctx.cmd().endpoint_id, ctx.cmd().cluster_id);
        let c = Cl { bridge: self.0, endpoint };
        debug!("Matter invoke ep {endpoint} cluster 0x{cluster:04x} cmd 0x{:02x}", ctx.cmd().cmd_id);
        match cluster {
            ON_OFF_ID => on_off::HandlerAsyncAdaptor(&c).invoke(ctx, reply).await,
            LEVEL_ID => level_control::HandlerAsyncAdaptor(&c).invoke(ctx, reply).await,
            COLOR_ID => color_control::HandlerAsyncAdaptor(&c).invoke(ctx, reply).await,
            BRIDGED_ID => bridged::HandlerAsyncAdaptor(&c).invoke(ctx, reply).await,
            TEMPERATURE_ID => temperature::HandlerAsyncAdaptor(&c).invoke(ctx, reply).await,
            HUMIDITY_ID => humidity::HandlerAsyncAdaptor(&c).invoke(ctx, reply).await,
            POWER_ID => power_source::HandlerAsyncAdaptor(&c).invoke(ctx, reply).await,
            THERMOSTAT_ID => thermostat::HandlerAsyncAdaptor(&c).invoke(ctx, reply).await,
            SWITCH_ID => switch::HandlerAsyncAdaptor(&c).invoke(ctx, reply).await,
            _ => Err(ErrorCode::ClusterNotFound.into()),
        }
    }

    fn bump_dataver(&self, ctx: impl MatchContext) {
        let bridged = self.0.bridged.lock().unwrap();
        for b in bridged.iter() {
            if ctx.endpt().is_some_and(|e| e != b.endpoint) {
                continue;
            }
            for (cluster, dataver) in &b.datavers {
                if ctx.cluster().is_none_or(|c| c == *cluster) {
                    dataver.changed();
                }
            }
        }
    }
}

/// One bridged endpoint, as seen by one cluster handler
struct Cl<'a> {
    bridge: &'a Bridge,
    endpoint: EndptId,
}

impl Cl<'_> {
    fn number(&self, key: &str) -> Result<Option<f64>, Error> {
        Ok(self.bridge.state(self.endpoint, key)?.and_then(|v| v.as_f64()))
    }
}

fn nullable_u16(value: Nullable<u16>) -> u16 {
    value.into_option().unwrap_or(0xFFFF)
}

fn le16(value: u16) -> [u8; 2] {
    value.to_le_bytes()
}

// ---- On/Off ----

impl on_off::ClusterAsyncHandler for &Cl<'_> {
    const CLUSTER: Cluster<'static> = ON_OFF_CLUSTER;

    fn dataver(&self) -> u32 {
        self.bridge.dataver(self.endpoint, ON_OFF_ID)
    }

    fn dataver_changed(&self) {
        self.bridge.dataver_changed(self.endpoint, ON_OFF_ID);
    }

    async fn on_off(&self, _ctx: impl ReadContext) -> Result<bool, Error> {
        Ok(self.bridge.state(self.endpoint, "on")?.and_then(|v| v.as_bool()).unwrap_or(false))
    }

    async fn global_scene_control(&self, _ctx: impl ReadContext) -> Result<bool, Error> {
        Ok(true)
    }

    async fn on_time(&self, _ctx: impl ReadContext) -> Result<u16, Error> {
        Ok(0)
    }

    async fn off_wait_time(&self, _ctx: impl ReadContext) -> Result<u16, Error> {
        Ok(0)
    }

    async fn start_up_on_off(&self, _ctx: impl ReadContext) -> Result<Nullable<on_off::StartUpOnOffEnum>, Error> {
        Ok(Nullable::none())
    }

    async fn handle_off(&self, _ctx: impl InvokeContext) -> Result<(), Error> {
        self.bridge.command(self.endpoint, 0x0006, 0x00, vec![], vec![("on", json!(false))]).await
    }

    async fn handle_on(&self, _ctx: impl InvokeContext) -> Result<(), Error> {
        self.bridge.command(self.endpoint, 0x0006, 0x01, vec![], vec![("on", json!(true))]).await
    }

    async fn handle_toggle(&self, _ctx: impl InvokeContext) -> Result<(), Error> {
        let on = self.bridge.state(self.endpoint, "on")?.and_then(|v| v.as_bool()).unwrap_or(false);
        self.bridge.command(self.endpoint, 0x0006, 0x02, vec![], vec![("on", json!(!on))]).await
    }

    async fn handle_off_with_effect(&self, _ctx: impl InvokeContext, request: on_off::OffWithEffectRequest<'_>) -> Result<(), Error> {
        let payload = vec![request.effect_identifier()? as u8, request.effect_variant()?];
        self.bridge.command(self.endpoint, 0x0006, 0x40, payload, vec![("on", json!(false))]).await
    }

    async fn handle_on_with_recall_global_scene(&self, _ctx: impl InvokeContext) -> Result<(), Error> {
        self.bridge.command(self.endpoint, 0x0006, 0x41, vec![], vec![("on", json!(true))]).await
    }

    async fn handle_on_with_timed_off(&self, _ctx: impl InvokeContext, request: on_off::OnWithTimedOffRequest<'_>) -> Result<(), Error> {
        let mut payload = vec![request.on_off_control()?.bits()];
        payload.extend(le16(request.on_time()?));
        payload.extend(le16(request.off_wait_time()?));
        self.bridge.command(self.endpoint, 0x0006, 0x42, payload, vec![("on", json!(true))]).await
    }
}

// ---- Level Control ----

impl Cl<'_> {
    async fn level_command(&self, command: u8, payload: Vec<u8>, level: Option<u8>, with_on_off: bool) -> Result<(), Error> {
        let mut state = Vec::new();
        if let Some(level) = level {
            state.push(("level", json!(level)));
            if with_on_off {
                state.push(("on", json!(level > 0)));
            }
        }
        self.bridge.command(self.endpoint, 0x0008, command, payload, state).await
    }
}

impl level_control::ClusterAsyncHandler for &Cl<'_> {
    const CLUSTER: Cluster<'static> = LEVEL_CLUSTER;

    fn dataver(&self) -> u32 {
        self.bridge.dataver(self.endpoint, LEVEL_ID)
    }

    fn dataver_changed(&self) {
        self.bridge.dataver_changed(self.endpoint, LEVEL_ID);
    }

    async fn current_level(&self, _ctx: impl ReadContext) -> Result<Nullable<u8>, Error> {
        Ok(match self.number("level")? {
            Some(level) => Nullable::some(level.clamp(1.0, 254.0) as u8),
            None => Nullable::none(),
        })
    }

    async fn remaining_time(&self, _ctx: impl ReadContext) -> Result<u16, Error> {
        Ok(0)
    }

    async fn min_level(&self, _ctx: impl ReadContext) -> Result<u8, Error> {
        Ok(1)
    }

    async fn max_level(&self, _ctx: impl ReadContext) -> Result<u8, Error> {
        Ok(254)
    }

    async fn options(&self, _ctx: impl ReadContext) -> Result<level_control::OptionsBitmap, Error> {
        Ok(level_control::OptionsBitmap::empty())
    }

    async fn on_level(&self, _ctx: impl ReadContext) -> Result<Nullable<u8>, Error> {
        Ok(Nullable::none())
    }

    async fn start_up_current_level(&self, _ctx: impl ReadContext) -> Result<Nullable<u8>, Error> {
        Ok(match self.number("current_level_startup")? {
            Some(level) => Nullable::some(level as u8),
            None => Nullable::none(),
        })
    }

    async fn set_options(&self, _ctx: impl WriteContext, _value: level_control::OptionsBitmap) -> Result<(), Error> {
        Ok(())
    }

    async fn set_on_level(&self, _ctx: impl WriteContext, _value: Nullable<u8>) -> Result<(), Error> {
        Ok(())
    }

    async fn handle_move_to_level(&self, _ctx: impl InvokeContext, r: level_control::MoveToLevelRequest<'_>) -> Result<(), Error> {
        let level = r.level()?;
        let mut payload = vec![level];
        payload.extend(le16(nullable_u16(r.transition_time()?)));
        payload.extend([r.options_mask()?.bits(), r.options_override()?.bits()]);
        self.level_command(0x00, payload, Some(level), false).await
    }

    async fn handle_move(&self, _ctx: impl InvokeContext, r: level_control::MoveRequest<'_>) -> Result<(), Error> {
        let payload = vec![
            r.move_mode()? as u8,
            r.rate()?.into_option().unwrap_or(0xFF),
            r.options_mask()?.bits(),
            r.options_override()?.bits(),
        ];
        self.level_command(0x01, payload, None, false).await
    }

    async fn handle_step(&self, _ctx: impl InvokeContext, r: level_control::StepRequest<'_>) -> Result<(), Error> {
        let mut payload = vec![r.step_mode()? as u8, r.step_size()?];
        payload.extend(le16(nullable_u16(r.transition_time()?)));
        payload.extend([r.options_mask()?.bits(), r.options_override()?.bits()]);
        self.level_command(0x02, payload, None, false).await
    }

    async fn handle_stop(&self, _ctx: impl InvokeContext, r: level_control::StopRequest<'_>) -> Result<(), Error> {
        let payload = vec![r.options_mask()?.bits(), r.options_override()?.bits()];
        self.level_command(0x03, payload, None, false).await
    }

    async fn handle_move_to_level_with_on_off(
        &self,
        _ctx: impl InvokeContext,
        r: level_control::MoveToLevelWithOnOffRequest<'_>,
    ) -> Result<(), Error> {
        let level = r.level()?;
        let mut payload = vec![level];
        payload.extend(le16(nullable_u16(r.transition_time()?)));
        payload.extend([r.options_mask()?.bits(), r.options_override()?.bits()]);
        self.level_command(0x04, payload, Some(level), true).await
    }

    async fn handle_move_with_on_off(&self, _ctx: impl InvokeContext, r: level_control::MoveWithOnOffRequest<'_>) -> Result<(), Error> {
        let payload = vec![
            r.move_mode()? as u8,
            r.rate()?.into_option().unwrap_or(0xFF),
            r.options_mask()?.bits(),
            r.options_override()?.bits(),
        ];
        self.level_command(0x05, payload, None, true).await
    }

    async fn handle_step_with_on_off(&self, _ctx: impl InvokeContext, r: level_control::StepWithOnOffRequest<'_>) -> Result<(), Error> {
        let mut payload = vec![r.step_mode()? as u8, r.step_size()?];
        payload.extend(le16(nullable_u16(r.transition_time()?)));
        payload.extend([r.options_mask()?.bits(), r.options_override()?.bits()]);
        self.level_command(0x06, payload, None, true).await
    }

    async fn handle_stop_with_on_off(&self, _ctx: impl InvokeContext, r: level_control::StopWithOnOffRequest<'_>) -> Result<(), Error> {
        let payload = vec![r.options_mask()?.bits(), r.options_override()?.bits()];
        self.level_command(0x07, payload, None, true).await
    }

    async fn handle_move_to_closest_frequency(
        &self,
        _ctx: impl InvokeContext,
        _r: level_control::MoveToClosestFrequencyRequest<'_>,
    ) -> Result<(), Error> {
        Err(ErrorCode::CommandNotFound.into())
    }
}

// ---- Color Control (color temperature only) ----

impl Cl<'_> {
    fn mireds(&self, key: &str) -> Result<Option<u16>, Error> {
        Ok(self.number(key)?.map(|v| v.clamp(0.0, 65_279.0) as u16))
    }

    /// The light's range; the definition's limits when it did not report its own
    fn mired_range(&self) -> Result<(u16, u16), Error> {
        let (min, max) = self.bridge.range(self.endpoint, "color_temp")?;
        Ok((min.unwrap_or(153.0) as u16, max.unwrap_or(500.0) as u16))
    }
}

fn unsupported() -> Result<(), Error> {
    Err(ErrorCode::CommandNotFound.into())
}

impl color_control::ClusterAsyncHandler for &Cl<'_> {
    const CLUSTER: Cluster<'static> = COLOR_CLUSTER;

    fn dataver(&self) -> u32 {
        self.bridge.dataver(self.endpoint, COLOR_ID)
    }

    fn dataver_changed(&self) {
        self.bridge.dataver_changed(self.endpoint, COLOR_ID);
    }

    async fn remaining_time(&self, _ctx: impl ReadContext) -> Result<u16, Error> {
        Ok(0)
    }

    async fn color_temperature_mireds(&self, _ctx: impl ReadContext) -> Result<u16, Error> {
        let (min, max) = self.mired_range()?;
        Ok(self.mireds("color_temp")?.unwrap_or(max).clamp(min, max))
    }

    async fn color_mode(&self, _ctx: impl ReadContext) -> Result<color_control::ColorModeEnum, Error> {
        Ok(color_control::ColorModeEnum::ColorTemperatureMireds)
    }

    async fn enhanced_color_mode(&self, _ctx: impl ReadContext) -> Result<color_control::EnhancedColorModeEnum, Error> {
        Ok(color_control::EnhancedColorModeEnum::ColorTemperatureMireds)
    }

    async fn color_capabilities(&self, _ctx: impl ReadContext) -> Result<color_control::ColorCapabilitiesBitmap, Error> {
        Ok(color_control::ColorCapabilitiesBitmap::COLOR_TEMPERATURE)
    }

    async fn options(&self, _ctx: impl ReadContext) -> Result<color_control::OptionsBitmap, Error> {
        Ok(color_control::OptionsBitmap::empty())
    }

    async fn number_of_primaries(&self, _ctx: impl ReadContext) -> Result<Nullable<u8>, Error> {
        Ok(Nullable::some(0))
    }

    async fn color_temp_physical_min_mireds(&self, _ctx: impl ReadContext) -> Result<u16, Error> {
        Ok(self.mired_range()?.0)
    }

    async fn color_temp_physical_max_mireds(&self, _ctx: impl ReadContext) -> Result<u16, Error> {
        Ok(self.mired_range()?.1)
    }

    async fn couple_color_temp_to_level_min_mireds(&self, _ctx: impl ReadContext) -> Result<u16, Error> {
        Ok(self.mired_range()?.0)
    }

    async fn start_up_color_temperature_mireds(&self, _ctx: impl ReadContext) -> Result<Nullable<u16>, Error> {
        // 0xFFFF ("previous") shows as the preset name in the hub's state
        Ok(match self.mireds("color_temp_startup")? {
            Some(mireds) => Nullable::some(mireds),
            None => Nullable::none(),
        })
    }

    async fn set_options(&self, _ctx: impl WriteContext, _value: color_control::OptionsBitmap) -> Result<(), Error> {
        Ok(())
    }

    async fn handle_move_to_color_temperature(
        &self,
        _ctx: impl InvokeContext,
        r: color_control::MoveToColorTemperatureRequest<'_>,
    ) -> Result<(), Error> {
        let (min, max) = self.mired_range()?;
        let mireds = r.color_temperature_mireds()?.clamp(min, max);
        let mut payload = le16(mireds).to_vec();
        payload.extend(le16(r.transition_time()?));
        payload.extend([r.options_mask()?.bits(), r.options_override()?.bits()]);
        self.bridge
            .command(self.endpoint, 0x0300, 0x0A, payload, vec![("color_temp", json!(mireds))])
            .await
    }

    async fn handle_move_color_temperature(
        &self,
        _ctx: impl InvokeContext,
        r: color_control::MoveColorTemperatureRequest<'_>,
    ) -> Result<(), Error> {
        let mut payload = vec![r.move_mode()? as u8];
        payload.extend(le16(r.rate()?));
        payload.extend(le16(r.color_temperature_minimum_mireds()?));
        payload.extend(le16(r.color_temperature_maximum_mireds()?));
        payload.extend([r.options_mask()?.bits(), r.options_override()?.bits()]);
        self.bridge.command(self.endpoint, 0x0300, 0x4B, payload, vec![]).await
    }

    async fn handle_step_color_temperature(
        &self,
        _ctx: impl InvokeContext,
        r: color_control::StepColorTemperatureRequest<'_>,
    ) -> Result<(), Error> {
        let mut payload = vec![r.step_mode()? as u8];
        payload.extend(le16(r.step_size()?));
        payload.extend(le16(r.transition_time()?));
        payload.extend(le16(r.color_temperature_minimum_mireds()?));
        payload.extend(le16(r.color_temperature_maximum_mireds()?));
        payload.extend([r.options_mask()?.bits(), r.options_override()?.bits()]);
        self.bridge.command(self.endpoint, 0x0300, 0x4C, payload, vec![]).await
    }

    async fn handle_stop_move_step(&self, _ctx: impl InvokeContext, r: color_control::StopMoveStepRequest<'_>) -> Result<(), Error> {
        let payload = vec![r.options_mask()?.bits(), r.options_override()?.bits()];
        self.bridge.command(self.endpoint, 0x0300, 0x47, payload, vec![]).await
    }

    async fn handle_move_to_hue(&self, _ctx: impl InvokeContext, _r: color_control::MoveToHueRequest<'_>) -> Result<(), Error> {
        unsupported()
    }
    async fn handle_move_hue(&self, _ctx: impl InvokeContext, _r: color_control::MoveHueRequest<'_>) -> Result<(), Error> {
        unsupported()
    }
    async fn handle_step_hue(&self, _ctx: impl InvokeContext, _r: color_control::StepHueRequest<'_>) -> Result<(), Error> {
        unsupported()
    }
    async fn handle_move_to_saturation(&self, _ctx: impl InvokeContext, _r: color_control::MoveToSaturationRequest<'_>) -> Result<(), Error> {
        unsupported()
    }
    async fn handle_move_saturation(&self, _ctx: impl InvokeContext, _r: color_control::MoveSaturationRequest<'_>) -> Result<(), Error> {
        unsupported()
    }
    async fn handle_step_saturation(&self, _ctx: impl InvokeContext, _r: color_control::StepSaturationRequest<'_>) -> Result<(), Error> {
        unsupported()
    }
    async fn handle_move_to_hue_and_saturation(
        &self,
        _ctx: impl InvokeContext,
        _r: color_control::MoveToHueAndSaturationRequest<'_>,
    ) -> Result<(), Error> {
        unsupported()
    }
    async fn handle_move_to_color(&self, _ctx: impl InvokeContext, _r: color_control::MoveToColorRequest<'_>) -> Result<(), Error> {
        unsupported()
    }
    async fn handle_move_color(&self, _ctx: impl InvokeContext, _r: color_control::MoveColorRequest<'_>) -> Result<(), Error> {
        unsupported()
    }
    async fn handle_step_color(&self, _ctx: impl InvokeContext, _r: color_control::StepColorRequest<'_>) -> Result<(), Error> {
        unsupported()
    }
    async fn handle_enhanced_move_to_hue(&self, _ctx: impl InvokeContext, _r: color_control::EnhancedMoveToHueRequest<'_>) -> Result<(), Error> {
        unsupported()
    }
    async fn handle_enhanced_move_hue(&self, _ctx: impl InvokeContext, _r: color_control::EnhancedMoveHueRequest<'_>) -> Result<(), Error> {
        unsupported()
    }
    async fn handle_enhanced_step_hue(&self, _ctx: impl InvokeContext, _r: color_control::EnhancedStepHueRequest<'_>) -> Result<(), Error> {
        unsupported()
    }
    async fn handle_enhanced_move_to_hue_and_saturation(
        &self,
        _ctx: impl InvokeContext,
        _r: color_control::EnhancedMoveToHueAndSaturationRequest<'_>,
    ) -> Result<(), Error> {
        unsupported()
    }
    async fn handle_color_loop_set(&self, _ctx: impl InvokeContext, _r: color_control::ColorLoopSetRequest<'_>) -> Result<(), Error> {
        unsupported()
    }
}

// ---- Bridged Device Basic Information ----

impl bridged::ClusterAsyncHandler for &Cl<'_> {
    const CLUSTER: Cluster<'static> = BRIDGED_CLUSTER;

    fn dataver(&self) -> u32 {
        self.bridge.dataver(self.endpoint, BRIDGED_ID)
    }

    fn dataver_changed(&self) {
        self.bridge.dataver_changed(self.endpoint, BRIDGED_ID);
    }

    async fn vendor_name<P: TLVBuilderParent>(&self, _ctx: impl ReadContext, builder: Utf8StrBuilder<P>) -> Result<P, Error> {
        builder.set(&truncate(&self.bridge.info(self.endpoint)?.vendor, 32))
    }

    async fn product_name<P: TLVBuilderParent>(&self, _ctx: impl ReadContext, builder: Utf8StrBuilder<P>) -> Result<P, Error> {
        builder.set(&truncate(&self.bridge.info(self.endpoint)?.product, 32))
    }

    async fn node_label<P: TLVBuilderParent>(&self, _ctx: impl ReadContext, builder: Utf8StrBuilder<P>) -> Result<P, Error> {
        builder.set(&truncate(&self.bridge.info(self.endpoint)?.name, 32))
    }

    async fn serial_number<P: TLVBuilderParent>(&self, _ctx: impl ReadContext, builder: Utf8StrBuilder<P>) -> Result<P, Error> {
        builder.set(&truncate(&self.bridge.info(self.endpoint)?.serial, 32))
    }

    async fn unique_id<P: TLVBuilderParent>(&self, _ctx: impl ReadContext, builder: Utf8StrBuilder<P>) -> Result<P, Error> {
        builder.set(&truncate(&self.bridge.info(self.endpoint)?.serial.replace(':', ""), 32))
    }

    async fn reachable(&self, _ctx: impl ReadContext) -> Result<bool, Error> {
        Ok(self.bridge.info(self.endpoint)?.reachable)
    }

    async fn handle_keep_active(&self, _ctx: impl InvokeContext, _request: bridged::KeepActiveRequest<'_>) -> Result<(), Error> {
        Err(ErrorCode::CommandNotFound.into())
    }
}

/// Matter strings here are at most 32 bytes
fn truncate(text: &str, max: usize) -> String {
    let mut end = text.len().min(max);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

// ---- Temperature and humidity ----

impl temperature::ClusterAsyncHandler for &Cl<'_> {
    const CLUSTER: Cluster<'static> = TEMPERATURE_CLUSTER;

    fn dataver(&self) -> u32 {
        self.bridge.dataver(self.endpoint, TEMPERATURE_ID)
    }

    fn dataver_changed(&self) {
        self.bridge.dataver_changed(self.endpoint, TEMPERATURE_ID);
    }

    async fn measured_value(&self, _ctx: impl ReadContext) -> Result<Nullable<i16>, Error> {
        Ok(match self.number("temperature")? {
            Some(celsius) => Nullable::some((celsius * 100.0).round() as i16),
            None => Nullable::none(),
        })
    }

    async fn min_measured_value(&self, _ctx: impl ReadContext) -> Result<Nullable<i16>, Error> {
        Ok(Nullable::none())
    }

    async fn max_measured_value(&self, _ctx: impl ReadContext) -> Result<Nullable<i16>, Error> {
        Ok(Nullable::none())
    }
}

impl humidity::ClusterAsyncHandler for &Cl<'_> {
    const CLUSTER: Cluster<'static> = HUMIDITY_CLUSTER;

    fn dataver(&self) -> u32 {
        self.bridge.dataver(self.endpoint, HUMIDITY_ID)
    }

    fn dataver_changed(&self) {
        self.bridge.dataver_changed(self.endpoint, HUMIDITY_ID);
    }

    async fn measured_value(&self, _ctx: impl ReadContext) -> Result<Nullable<u16>, Error> {
        Ok(match self.number("humidity")? {
            Some(percent) => Nullable::some((percent.clamp(0.0, 100.0) * 100.0).round() as u16),
            None => Nullable::none(),
        })
    }

    async fn min_measured_value(&self, _ctx: impl ReadContext) -> Result<Nullable<u16>, Error> {
        Ok(Nullable::none())
    }

    async fn max_measured_value(&self, _ctx: impl ReadContext) -> Result<Nullable<u16>, Error> {
        Ok(Nullable::none())
    }
}

// ---- Battery ----

impl Cl<'_> {
    fn battery(&self) -> Result<Option<f64>, Error> {
        self.number("battery")
    }
}

impl power_source::ClusterAsyncHandler for &Cl<'_> {
    const CLUSTER: Cluster<'static> = POWER_CLUSTER;

    fn dataver(&self) -> u32 {
        self.bridge.dataver(self.endpoint, POWER_ID)
    }

    fn dataver_changed(&self) {
        self.bridge.dataver_changed(self.endpoint, POWER_ID);
    }

    async fn status(&self, _ctx: impl ReadContext) -> Result<power_source::PowerSourceStatusEnum, Error> {
        Ok(power_source::PowerSourceStatusEnum::Active)
    }

    async fn order(&self, _ctx: impl ReadContext) -> Result<u8, Error> {
        Ok(0)
    }

    async fn description<P: TLVBuilderParent>(&self, _ctx: impl ReadContext, builder: Utf8StrBuilder<P>) -> Result<P, Error> {
        builder.set("Battery")
    }

    async fn endpoint_list<P: TLVBuilderParent>(
        &self,
        _ctx: impl ReadContext,
        builder: ArrayAttributeRead<ToTLVArrayBuilder<P, EndptId>, ToTLVBuilder<P, EndptId>>,
    ) -> Result<P, Error> {
        match builder {
            ArrayAttributeRead::ReadAll(builder) => builder.push(&self.endpoint)?.end(),
            ArrayAttributeRead::ReadOne(0, builder) => builder.set(&self.endpoint),
            ArrayAttributeRead::ReadOne(_, _) => Err(ErrorCode::ConstraintError.into()),
            ArrayAttributeRead::ReadNone(builder) => builder.end(),
        }
    }

    async fn bat_percent_remaining(&self, _ctx: impl ReadContext) -> Result<Nullable<u8>, Error> {
        // Half percent units
        Ok(match self.battery()? {
            Some(percent) => Nullable::some((percent.clamp(0.0, 100.0) * 2.0).round() as u8),
            None => Nullable::none(),
        })
    }

    async fn bat_charge_level(&self, _ctx: impl ReadContext) -> Result<power_source::BatChargeLevelEnum, Error> {
        Ok(match self.battery()? {
            Some(p) if p < 10.0 => power_source::BatChargeLevelEnum::Critical,
            Some(p) if p < 20.0 => power_source::BatChargeLevelEnum::Warning,
            _ => power_source::BatChargeLevelEnum::OK,
        })
    }

    async fn bat_replacement_needed(&self, _ctx: impl ReadContext) -> Result<bool, Error> {
        Ok(self.battery()?.is_some_and(|p| p < 10.0))
    }

    async fn bat_replaceability(&self, _ctx: impl ReadContext) -> Result<power_source::BatReplaceabilityEnum, Error> {
        Ok(power_source::BatReplaceabilityEnum::UserReplaceable)
    }
}

// ---- Thermostat (heating only) ----

impl Cl<'_> {
    /// The setpoint's range in hundredths of a degree
    fn setpoint_range(&self) -> Result<(i16, i16), Error> {
        let (min, max) = self.bridge.range(self.endpoint, "setpoint")?;
        Ok(((min.unwrap_or(5.0) * 100.0) as i16, (max.unwrap_or(35.0) * 100.0) as i16))
    }

    fn setpoint(&self) -> Result<i16, Error> {
        let (min, max) = self.setpoint_range()?;
        Ok(self.number("setpoint")?.map_or(min, |c| (c * 100.0).round() as i16).clamp(min, max))
    }
}

impl thermostat::ClusterAsyncHandler for &Cl<'_> {
    const CLUSTER: Cluster<'static> = THERMOSTAT_CLUSTER;

    fn dataver(&self) -> u32 {
        self.bridge.dataver(self.endpoint, THERMOSTAT_ID)
    }

    fn dataver_changed(&self) {
        self.bridge.dataver_changed(self.endpoint, THERMOSTAT_ID);
    }

    async fn local_temperature(&self, _ctx: impl ReadContext) -> Result<Nullable<i16>, Error> {
        Ok(match self.number("temperature")? {
            Some(celsius) => Nullable::some((celsius * 100.0).round() as i16),
            None => Nullable::none(),
        })
    }

    async fn abs_min_heat_setpoint_limit(&self, _ctx: impl ReadContext) -> Result<i16, Error> {
        Ok(self.setpoint_range()?.0)
    }

    async fn abs_max_heat_setpoint_limit(&self, _ctx: impl ReadContext) -> Result<i16, Error> {
        Ok(self.setpoint_range()?.1)
    }

    async fn occupied_heating_setpoint(&self, _ctx: impl ReadContext) -> Result<i16, Error> {
        self.setpoint()
    }

    async fn set_occupied_heating_setpoint(&self, _ctx: impl WriteContext, value: i16) -> Result<(), Error> {
        let (min, max) = self.setpoint_range()?;
        if !(min..=max).contains(&value) {
            return Err(ErrorCode::ConstraintError.into());
        }
        self.bridge.set(self.endpoint, "setpoint", json!(f64::from(value) / 100.0)).await
    }

    async fn control_sequence_of_operation(&self, _ctx: impl ReadContext) -> Result<thermostat::ControlSequenceOfOperationEnum, Error> {
        Ok(thermostat::ControlSequenceOfOperationEnum::HeatingOnly)
    }

    async fn set_control_sequence_of_operation(
        &self,
        _ctx: impl WriteContext,
        value: thermostat::ControlSequenceOfOperationEnum,
    ) -> Result<(), Error> {
        match value {
            thermostat::ControlSequenceOfOperationEnum::HeatingOnly => Ok(()),
            _ => Err(ErrorCode::ConstraintError.into()),
        }
    }

    /// The device's own "auto" (its schedule) still heats: Matter's Auto means heat and
    /// cool, which these devices cannot
    async fn system_mode(&self, _ctx: impl ReadContext) -> Result<thermostat::SystemModeEnum, Error> {
        Ok(match self.bridge.state(self.endpoint, "system_mode")?.as_ref().and_then(Json::as_str) {
            Some("off") => thermostat::SystemModeEnum::Off,
            _ => thermostat::SystemModeEnum::Heat,
        })
    }

    async fn set_system_mode(&self, _ctx: impl WriteContext, value: thermostat::SystemModeEnum) -> Result<(), Error> {
        let mode = match value {
            thermostat::SystemModeEnum::Off => "off",
            thermostat::SystemModeEnum::Heat => "heat",
            _ => return Err(ErrorCode::ConstraintError.into()),
        };
        self.bridge.set(self.endpoint, "system_mode", json!(mode)).await
    }

    async fn thermostat_running_state(&self, _ctx: impl ReadContext) -> Result<thermostat::RelayStateBitmap, Error> {
        let heating = self.bridge.state(self.endpoint, "heating")?.and_then(|v| v.as_bool()).unwrap_or(false);
        Ok(if heating { thermostat::RelayStateBitmap::HEAT } else { thermostat::RelayStateBitmap::empty() })
    }

    async fn handle_setpoint_raise_lower(&self, _ctx: impl InvokeContext, request: thermostat::SetpointRaiseLowerRequest<'_>) -> Result<(), Error> {
        // Amount in tenths of a degree
        let (min, max) = self.setpoint_range()?;
        let target = (self.setpoint()? + i16::from(request.amount()?) * 10).clamp(min, max);
        self.bridge.set(self.endpoint, "setpoint", json!(f64::from(target) / 100.0)).await
    }

    async fn handle_set_weekly_schedule(&self, _ctx: impl InvokeContext, _r: thermostat::SetWeeklyScheduleRequest<'_>) -> Result<(), Error> {
        unsupported()
    }

    async fn handle_get_weekly_schedule<P: TLVBuilderParent>(
        &self,
        _ctx: impl InvokeContext,
        _r: thermostat::GetWeeklyScheduleRequest<'_>,
        _response: thermostat::GetWeeklyScheduleResponseBuilder<P>,
    ) -> Result<P, Error> {
        Err(ErrorCode::CommandNotFound.into())
    }

    async fn handle_clear_weekly_schedule(&self, _ctx: impl InvokeContext) -> Result<(), Error> {
        unsupported()
    }

    async fn handle_set_active_schedule_request(
        &self,
        _ctx: impl InvokeContext,
        _r: thermostat::SetActiveScheduleRequestRequest<'_>,
    ) -> Result<(), Error> {
        unsupported()
    }

    async fn handle_set_active_preset_request(
        &self,
        _ctx: impl InvokeContext,
        _r: thermostat::SetActivePresetRequestRequest<'_>,
    ) -> Result<(), Error> {
        unsupported()
    }

    async fn handle_add_thermostat_suggestion<P: TLVBuilderParent>(
        &self,
        _ctx: impl InvokeContext,
        _r: thermostat::AddThermostatSuggestionRequest<'_>,
        _response: thermostat::AddThermostatSuggestionResponseBuilder<P>,
    ) -> Result<P, Error> {
        Err(ErrorCode::CommandNotFound.into())
    }

    async fn handle_remove_thermostat_suggestion(
        &self,
        _ctx: impl InvokeContext,
        _r: thermostat::RemoveThermostatSuggestionRequest<'_>,
    ) -> Result<(), Error> {
        unsupported()
    }

    async fn handle_atomic_request<P: TLVBuilderParent>(
        &self,
        _ctx: impl InvokeContext,
        _r: thermostat::AtomicRequestRequest<'_>,
        _response: thermostat::AtomicResponseBuilder<P>,
    ) -> Result<P, Error> {
        Err(ErrorCode::CommandNotFound.into())
    }
}

// ---- Switch (one button) ----

impl switch::ClusterAsyncHandler for &Cl<'_> {
    const CLUSTER: Cluster<'static> = SWITCH_CLUSTER;

    fn dataver(&self) -> u32 {
        self.bridge.dataver(self.endpoint, SWITCH_ID)
    }

    fn dataver_changed(&self) {
        self.bridge.dataver_changed(self.endpoint, SWITCH_ID);
    }

    async fn number_of_positions(&self, _ctx: impl ReadContext) -> Result<u8, Error> {
        Ok(2)
    }

    async fn current_position(&self, _ctx: impl ReadContext) -> Result<u8, Error> {
        Ok(self.bridge.position(self.endpoint))
    }
}
