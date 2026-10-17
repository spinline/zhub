//! The bridged devices: one Matter endpoint per Zigbee device, under the aggregator.
//!
//! The endpoint list follows the hub (devices joining, leaving, changing), so the node
//! metadata is built on each access instead of being a constant. Reads come from the
//! hub's device state; commands go to the device as the ZCL command with the same id
//! (Matter's lighting clusters are ZCL's), so transitions run on the light itself.
//! Changes reported over Zigbee are pushed to subscribers.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use rs_matter::dm::clusters::decl::bridged_device_basic_information as bridged;
use rs_matter::dm::clusters::decl::color_control;
use rs_matter::dm::clusters::decl::level_control;
use rs_matter::dm::clusters::decl::on_off;
use rs_matter::dm::clusters::desc::{self, ClusterHandler as _};
use rs_matter::dm::clusters::groups::{self, ClusterHandler as _};
use rs_matter::dm::devices::{DEV_TYPE_AGGREGATOR, DEV_TYPE_BRIDGED_NODE, DEV_TYPE_DIMMABLE_LIGHT, DEV_TYPE_ON_OFF_LIGHT};
use rs_matter::dm::{
    AsyncHandler, Cluster, ClusterId, Dataver, DeviceType, EndptId, Endpoint, InvokeContext, InvokeReply, MatchContext,
    Metadata, Node, ReadContext, ReadReply, WriteContext,
};
use rs_matter::error::{Error, ErrorCode};
use rs_matter::tlv::{Nullable, TLVBuilderParent, Utf8StrBuilder};
use rs_matter::{clusters, devices, root_endpoint, with};
use serde_json::{Value as Json, json};
use tracing::{debug, info, warn};

use crate::devices::{Access, Card};
use crate::hub::{DeviceView, Hub};

pub const AGGREGATOR_ENDPOINT: EndptId = 1;

const ON_OFF_ID: ClusterId = on_off::FULL_CLUSTER.id;
const LEVEL_ID: ClusterId = level_control::FULL_CLUSTER.id;
const COLOR_ID: ClusterId = color_control::FULL_CLUSTER.id;
const BRIDGED_ID: ClusterId = bridged::FULL_CLUSTER.id;

const DEV_TYPE_COLOR_TEMPERATURE_LIGHT: DeviceType = DeviceType { dtype: 0x010C, drev: 4 };

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

/// The kinds of bridged devices, by what the Matter side offers
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    OnOffLight,
    DimmableLight,
    ColorTemperatureLight,
}

impl Kind {
    /// What a hub device becomes; `None` for devices not bridged (yet)
    pub fn of(view: &DeviceView) -> Option<Self> {
        let writable = |key: &str| view.features.iter().any(|f| f.key == key && f.access != Access::Read);
        if view.card != Card::Light || !writable("on") {
            return None;
        }
        Some(match (writable("level"), writable("color_temp")) {
            (true, true) => Self::ColorTemperatureLight,
            (true, false) => Self::DimmableLight,
            _ => Self::OnOffLight,
        })
    }

    fn device_types(self) -> &'static [DeviceType] {
        match self {
            Self::OnOffLight => devices!(DEV_TYPE_ON_OFF_LIGHT, DEV_TYPE_BRIDGED_NODE),
            Self::DimmableLight => devices!(DEV_TYPE_DIMMABLE_LIGHT, DEV_TYPE_BRIDGED_NODE),
            Self::ColorTemperatureLight => devices!(DEV_TYPE_COLOR_TEMPERATURE_LIGHT, DEV_TYPE_BRIDGED_NODE),
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
        }
    }
}

/// One bridged device
pub struct Bridged {
    pub endpoint: EndptId,
    pub ieee: String,
    pub kind: Kind,
    /// The device's name as last pushed (the node label)
    name: String,
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

impl Bridge {
    pub fn new(hub: Arc<Hub>, rt: tokio::runtime::Handle) -> Self {
        Self {
            hub,
            rt,
            bridged: Mutex::new(Vec::new()),
        }
    }

    fn ieee_of(&self, endpoint: EndptId) -> Result<String, Error> {
        let bridged = self.bridged.lock().unwrap();
        bridged
            .iter()
            .find(|b| b.endpoint == endpoint)
            .map(|b| b.ieee.clone())
            .ok_or_else(|| ErrorCode::EndpointNotFound.into())
    }

    fn view(&self, endpoint: EndptId) -> Result<DeviceView, Error> {
        let ieee = self.ieee_of(endpoint)?;
        self.hub.device_view(&ieee).ok_or_else(|| ErrorCode::EndpointNotFound.into())
    }

    fn state(&self, endpoint: EndptId, key: &str) -> Result<Option<Json>, Error> {
        Ok(self.view(endpoint)?.device.state.get(key).cloned())
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

    /// Sends the ZCL command and sets the state the command leads to, so the controller
    /// sees the result before the device reports it
    async fn command(&self, endpoint: EndptId, cluster: u16, command: u8, payload: Vec<u8>, state: Vec<(&'static str, Json)>) -> Result<(), Error> {
        let ieee = self.ieee_of(endpoint)?;
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

    /// Brings the bridged endpoints in line with the hub's devices; tells subscribers
    /// what changed
    pub fn sync(&self, notify: &impl Notify) {
        let devices = self.hub.devices();
        let mut parts_changed = false;
        let mut changed = Vec::new();
        {
            let mut bridged = self.bridged.lock().unwrap();
            let before = bridged.len();
            bridged.retain(|b| devices.iter().any(|d| d.device.ieee == b.ieee && Kind::of(d) == Some(b.kind)));
            parts_changed |= bridged.len() != before;
            for view in &devices {
                let Some(kind) = Kind::of(view) else {
                    continue;
                };
                if let Some(existing) = bridged.iter_mut().find(|b| b.ieee == view.device.ieee) {
                    if existing.name != view.device.name {
                        existing.name = view.device.name.clone();
                        changed.push((existing.endpoint, BRIDGED_ID));
                    }
                    continue;
                }
                let endpoint = self.hub.matter_endpoint(&view.device.ieee);
                let mut datavers = HashMap::new();
                for cluster in kind.clusters() {
                    datavers.insert(cluster.id, Dataver::new(rand::random()));
                }
                info!("Matter: {} ({}) on endpoint {endpoint} as {kind:?}", view.device.name, view.device.ieee);
                bridged.push(Bridged {
                    endpoint,
                    ieee: view.device.ieee.clone(),
                    kind,
                    name: view.device.name.clone(),
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

    /// A device's state changed: its clusters may have
    pub fn device_changed(&self, ieee: &str, notify: &impl Notify) {
        let targets: Vec<(EndptId, Vec<ClusterId>)> = {
            let bridged = self.bridged.lock().unwrap();
            bridged
                .iter()
                .filter(|b| b.ieee == ieee)
                .map(|b| (b.endpoint, b.state_clusters().collect()))
                .collect()
        };
        for (endpoint, clusters) in targets {
            for cluster in clusters {
                notify.cluster_changed(endpoint, cluster);
            }
        }
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
        let view = self.bridge.view(self.endpoint)?;
        let (min, max) = view
            .features
            .iter()
            .find(|f| f.key == "color_temp")
            .map(|f| f.range(&view.device))
            .unwrap_or((None, None));
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
        let view = self.bridge.view(self.endpoint)?;
        let vendor = view.definition.map(|d| d.vendor.to_owned()).or(view.device.manufacturer).unwrap_or_default();
        builder.set(&truncate(&vendor, 32))
    }

    async fn product_name<P: TLVBuilderParent>(&self, _ctx: impl ReadContext, builder: Utf8StrBuilder<P>) -> Result<P, Error> {
        let view = self.bridge.view(self.endpoint)?;
        builder.set(&truncate(view.device.model.as_deref().unwrap_or_default(), 32))
    }

    async fn node_label<P: TLVBuilderParent>(&self, _ctx: impl ReadContext, builder: Utf8StrBuilder<P>) -> Result<P, Error> {
        let view = self.bridge.view(self.endpoint)?;
        builder.set(&truncate(&view.device.name, 32))
    }

    async fn serial_number<P: TLVBuilderParent>(&self, _ctx: impl ReadContext, builder: Utf8StrBuilder<P>) -> Result<P, Error> {
        builder.set(&self.bridge.ieee_of(self.endpoint)?)
    }

    async fn unique_id<P: TLVBuilderParent>(&self, _ctx: impl ReadContext, builder: Utf8StrBuilder<P>) -> Result<P, Error> {
        builder.set(&self.bridge.ieee_of(self.endpoint)?.replace(':', ""))
    }

    async fn reachable(&self, _ctx: impl ReadContext) -> Result<bool, Error> {
        let view = self.bridge.view(self.endpoint)?;
        Ok(!self.bridge.hub.is_offline(&view.device))
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
