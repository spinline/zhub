//! Matter bridge (feature `matter`): the hub's devices for Matter controllers such as
//! Apple Home, Google Home or Home Assistant, built on rs-matter. Like the MQTT bridge it
//! is an adapter on the hub's event bus; it runs on its own thread with rs-matter's
//! executor, apart from the hub's tokio runtime.
//!
//! The node is a Matter bridge: the root endpoint, an aggregator (endpoint 1) and one
//! bridged endpoint per device. mDNS goes through the router's Avahi (see `avahi`).
//! Uncertified: it uses the Matter test attestation (vendor 0xFFF1), so controllers warn
//! when adding it.

mod avahi;
mod bridge;

use std::net::UdpSocket;
use std::path::PathBuf;
use std::pin::pin;
use std::sync::Arc;
use std::time::Duration;

use embassy_futures::select::{Either, Either3, select, select3, select4};
use rs_matter::crypto::{Crypto, default_crypto};
use rs_matter::dm::clusters::basic_info::BasicInfoConfig;
use rs_matter::dm::clusters::desc::{self, ClusterHandler as _};
use rs_matter::dm::devices::test::{DAC_PRIVKEY, TEST_DEV_ATT, TEST_PID, TEST_VID};
use rs_matter::dm::endpoints;
use rs_matter::dm::networks::SysNetifs;
use rs_matter::dm::networks::eth::EthNetwork;
use rs_matter::dm::clusters::groups::{self, ClusterHandler as _};
use rs_matter::dm::{AttrChangeNotifier, Async, DataModel, Dataver};
use rs_matter::error::Error;
use rs_matter::im::{EthInteractionModelState, InteractionModel};
use rs_matter::pairing::DiscoveryCapabilities;
use rs_matter::pairing::qr::{CommFlowType, NoOptionalData, Qr, QrPayload, no_optional_data};
use rs_matter::persist::DirKvBlobStore;
use rs_matter::respond::DefaultResponder;
use rs_matter::sc::pase::{CommWindowState, Spake2pVerifierPassword, Spake2pVerifierPasswordRef};
use rs_matter::transport::MATTER_SOCKET_BIND_ADDR;
use rs_matter::transport::exchange::MatterBuffers;
use rs_matter::utils::select::Coalesce;
use rs_matter::{BasicCommData, MATTER_PORT, Matter};
use serde::Serialize;
use tokio::sync::{broadcast, mpsc, watch};
use tracing::{info, warn};

use crate::hub::{Event, Hub, MatterCommand};
use crate::store::MatterSettings;

/// How long a commissioning window opened from the UI stays open (seconds)
const COMMISSIONING_WINDOW: u16 = 15 * 60;
/// How often the status shown in the UI is refreshed
const STATUS_INTERVAL: Duration = Duration::from_secs(3);

/// What the settings page shows
#[derive(Debug, Clone, Default, Serialize)]
pub struct MatterStatus {
    pub supported: bool,
    pub state: String,
    /// The `MT:` text the QR code carries
    #[serde(skip_serializing_if = "Option::is_none")]
    pub qr: Option<String>,
    /// The 11-digit manual pairing code
    #[serde(skip_serializing_if = "Option::is_none")]
    pub manual_code: Option<String>,
    /// The QR code as an SVG image
    #[serde(skip_serializing_if = "Option::is_none")]
    pub qr_svg: Option<String>,
    pub controllers: usize,
    pub commissioning_open: bool,
}

/// Starts the supervisor thread: it runs the stack while Matter is enabled and restarts
/// it when the settings change
pub fn start(hub: Arc<Hub>, data_dir: PathBuf, commands: mpsc::UnboundedReceiver<MatterCommand>) {
    let settings = hub.matter_settings();
    // Zigbee commands are run on the hub's runtime
    let rt = tokio::runtime::Handle::current();
    let spawned = std::thread::Builder::new()
        .name("matter".into())
        // rs-matter keeps its buffers and state on the stack of the running future
        .stack_size(8 * 1024 * 1024)
        .spawn(move || supervise(hub, rt, data_dir, settings, commands));
    if let Err(e) = spawned {
        warn!("Matter not started: {e}");
    }
}

fn supervise(
    hub: Arc<Hub>,
    rt: tokio::runtime::Handle,
    data_dir: PathBuf,
    mut settings: watch::Receiver<MatterSettings>,
    mut commands: mpsc::UnboundedReceiver<MatterCommand>,
) {
    let dir = std::path::Path::new(avahi::SERVICE_DIR);
    loop {
        let current = settings.borrow_and_update().clone();
        if !current.enabled || current.passcode == 0 {
            hub.set_matter_status(MatterStatus {
                supported: true,
                state: "disabled".into(),
                ..MatterStatus::default()
            });
            avahi::clear(dir);
            if futures_lite::future::block_on(settings.changed()).is_err() {
                return;
            }
            continue;
        }
        let outcome = futures_lite::future::block_on(run(&hub, &rt, &data_dir, &current, &mut settings, &mut commands));
        avahi::clear(dir);
        match outcome {
            Ok(Stop::SettingsChanged) => info!("Matter settings changed, restarting the Matter stack"),
            Ok(Stop::Reset) => {
                info!("Matter reset: forgetting every controller");
                if let Err(e) = std::fs::remove_dir_all(&data_dir) {
                    warn!("Matter data not removed: {e}");
                }
            }
            Ok(Stop::Closed) => return,
            Err(e) => {
                warn!("Matter stopped: {e}");
                hub.set_matter_status(MatterStatus {
                    supported: true,
                    state: format!("error: {e}"),
                    ..MatterStatus::default()
                });
                // Retry later, or as soon as the settings change
                let wait = async {
                    async_io::Timer::after(Duration::from_secs(30)).await;
                };
                futures_lite::future::block_on(select(pin!(wait), pin!(settings.changed())));
            }
        }
    }
}

enum Stop {
    SettingsChanged,
    Reset,
    Closed,
}

fn leak(text: String) -> &'static str {
    Box::leak(text.into_boxed_str())
}

fn group_code(code: &str) -> String {
    // 1234-567-8901, as printed on Matter devices
    if code.len() == 11 {
        format!("{}-{}-{}", &code[..4], &code[4..7], &code[7..])
    } else {
        code.to_owned()
    }
}

/// The QR code as a small SVG (a 2-module quiet zone around it)
fn qr_svg(text: &str) -> Result<String, Error> {
    let mut tmp = [0u8; 4096];
    let mut out = [0u8; 4096];
    let qr = Qr::compute(text, &mut tmp, &mut out)?;
    let size = qr.size() as i32;
    let mut path = String::new();
    for y in 0..size {
        for x in 0..size {
            if qr.get_module(x, y) {
                path.push_str(&format!("M{} {}h1v1h-1z", x + 2, y + 2));
            }
        }
    }
    let full = size + 4;
    Ok(format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {full} {full}" shape-rendering="crispEdges"><rect width="{full}" height="{full}" fill="#fff"/><path d="{path}" fill="#000"/></svg>"##
    ))
}

/// The data model: root endpoint, the aggregator and the bridged devices (see `bridge`)
fn data_model<'a>(bridge: &'a bridge::Bridge, mut rand: impl rand::Rng + Copy) -> impl DataModel + 'a {
    (
        bridge::BridgeNode(bridge),
        endpoints::EthSysHandlerBuilder::new()
            .netif_diag(&SysNetifs)
            .build(rand)
            .chain(
                |e, c| e == bridge::AGGREGATOR_ENDPOINT && c == desc::DescHandler::CLUSTER.id,
                Async(desc::DescHandler::new_aggregator(Dataver::new_rand(&mut rand)).adapt()),
            )
            .chain(
                |e, c| e > bridge::AGGREGATOR_ENDPOINT && c == desc::DescHandler::CLUSTER.id,
                Async(desc::DescHandler::new(Dataver::new_rand(&mut rand)).adapt()),
            )
            .chain(
                |e, c| e > bridge::AGGREGATOR_ENDPOINT && c == groups::GroupsHandler::CLUSTER.id,
                Async(groups::GroupsHandler::new(Dataver::new_rand(&mut rand)).adapt()),
            )
            .chain(bridge::BridgedHandler::handles, bridge::BridgedHandler(bridge)),
    )
}

/// Attribute change notifications into the interaction model
struct ImNotify<'a, T: AttrChangeNotifier>(&'a T);

impl<T: AttrChangeNotifier> bridge::Notify for ImNotify<'_, T> {
    fn cluster_changed(&self, endpoint: u16, cluster: u32) {
        self.0.notify_cluster_changed(endpoint, cluster);
    }

    fn endpoint_changed(&self, endpoint: u16) {
        self.0.notify_endpoint_changed(endpoint);
    }
}

/// Before the interaction model exists there is nobody to tell
struct NoNotify;

impl bridge::Notify for NoNotify {
    fn cluster_changed(&self, _endpoint: u16, _cluster: u32) {}
    fn endpoint_changed(&self, _endpoint: u16) {}
}

async fn run(
    hub: &Arc<Hub>,
    rt: &tokio::runtime::Handle,
    data_dir: &std::path::Path,
    cfg: &MatterSettings,
    settings: &mut watch::Receiver<MatterSettings>,
    commands: &mut mpsc::UnboundedReceiver<MatterCommand>,
) -> Result<Stop, Error> {
    hub.set_matter_status(MatterStatus {
        supported: true,
        state: "starting".into(),
        ..MatterStatus::default()
    });
    let ieee = hub.hub_ieee().replace(':', "");
    let dev_det: &'static BasicInfoConfig<'static> = Box::leak(Box::new(BasicInfoConfig {
        vid: TEST_VID,
        pid: TEST_PID,
        hw_ver: 1,
        hw_ver_str: "1",
        sw_ver: 1,
        sw_ver_str: leak(env!("CARGO_PKG_VERSION").to_owned()),
        serial_no: leak(ieee.clone()),
        unique_id: leak(format!("zhub-{ieee}")),
        manufacturing_date: "20261017",
        product_name: "Zigbee bridge",
        vendor_name: "zhub",
        product_label: "zhub",
        ..BasicInfoConfig::new()
    }));
    let comm = BasicCommData {
        password: Spake2pVerifierPassword::new_from_ref(Spake2pVerifierPasswordRef::new(&cfg.passcode.to_le_bytes())),
        discriminator: cfg.discriminator,
    };

    let matter = Matter::new(dev_det, comm.clone(), &TEST_DEV_ATT, MATTER_PORT);
    std::fs::create_dir_all(data_dir).map_err(|_| rs_matter::error::ErrorCode::StdIoError)?;
    let kv = matter.kv(DirKvBlobStore::new(data_dir.to_path_buf()));
    matter.startup(&kv)?;
    let buffers: MatterBuffers = MatterBuffers::new();
    let state: EthInteractionModelState = EthInteractionModelState::new(EthNetwork::new_default());
    let crypto = default_crypto(rand::rng(), DAC_PRIVKEY);
    let rand = crypto.rand()?;
    let bridge = bridge::Bridge::new(hub.clone(), rt.clone());
    bridge.sync(&NoNotify);
    let im = InteractionModel::new(&matter, &crypto, &buffers, data_model(&bridge, rand), &kv, &state);
    im.startup().await?;

    let responder = DefaultResponder::new(&im);
    let mut respond = pin!(responder.run::<4, 4>());
    let mut im_job = pin!(im.run());
    let socket = async_io::Async::<UdpSocket>::bind(MATTER_SOCKET_BIND_ADDR)?;
    let mut mdns = pin!(avahi::run(&matter));
    let mut transport = pin!(matter.run(&crypto, &socket, &socket, &socket));

    if !matter.has_fabrics() {
        im.open_basic_comm_window(COMMISSIONING_WINDOW)?;
    }

    // Pairing codes, computed once
    let mut buf = [0u8; 128];
    let payload: QrPayload<'_, NoOptionalData> =
        QrPayload::new_from_basic_info(DiscoveryCapabilities::IP, CommFlowType::Standard, comm.clone(), dev_det, no_optional_data as _);
    let qr = payload.as_str(&mut buf)?.0.to_owned();
    let qr_svg = qr_svg(&qr)?;
    let manual_code = group_code(&comm.compute_pairing_code());
    info!("Matter bridge up on UDP {MATTER_PORT}; pairing code {manual_code}");

    let status = async {
        loop {
            let controllers = matter.with_state(|s| s.fabrics.iter().count());
            let open = !matches!(matter.comm_window_state(), CommWindowState::Closed);
            hub.set_matter_status(MatterStatus {
                supported: true,
                state: "running".into(),
                qr: Some(qr.clone()),
                manual_code: Some(manual_code.clone()),
                qr_svg: Some(qr_svg.clone()),
                controllers,
                commissioning_open: open,
            });
            async_io::Timer::after(STATUS_INTERVAL).await;
        }
    };

    let control = async {
        loop {
            match select(commands.recv(), settings.changed()).await {
                Either::First(Some(MatterCommand::OpenCommissioning)) => {
                    match im.open_basic_comm_window(COMMISSIONING_WINDOW) {
                        Ok(()) => info!("Matter commissioning window open for {} min", COMMISSIONING_WINDOW / 60),
                        Err(e) => warn!("Matter commissioning window not opened: {e:?}"),
                    }
                }
                Either::First(Some(MatterCommand::Reset)) => return Stop::Reset,
                Either::First(None) | Either::Second(Err(_)) => return Stop::Closed,
                Either::Second(Ok(())) => return Stop::SettingsChanged,
            }
        }
    };

    // Devices joining, leaving or changing state
    let follow = async {
        let notify = ImNotify(&im);
        let mut events = hub.subscribe();
        bridge.sync(&notify);
        loop {
            match events.recv().await {
                Ok(Event::Device { device }) => {
                    bridge.sync(&notify);
                    bridge.device_changed(&device.device.ieee, &notify);
                }
                Ok(Event::Removed { .. } | Event::Groups { .. }) => bridge.sync(&notify),
                Ok(_) => {}
                // Missed changes: everything may have changed
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    bridge.sync(&notify);
                    for view in hub.devices() {
                        bridge.device_changed(&view.device.ieee, &notify);
                    }
                }
                Err(broadcast::error::RecvError::Closed) => core::future::pending::<()>().await,
            }
        }
    };

    let stack = select4(&mut transport, &mut mdns, &mut respond, &mut im_job).coalesce();
    let (control, status, follow) = (pin!(control), pin!(status), pin!(follow));
    let side = select3(control, status, follow);
    match select(pin!(stack), side).await {
        Either::First(result) => result.map(|()| Stop::Closed),
        Either::Second(Either3::First(stop)) => Ok(stop),
        Either::Second(Either3::Second(()) | Either3::Third(())) => Ok(Stop::Closed),
    }
}
