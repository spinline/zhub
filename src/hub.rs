//! Zigbee merkezi: yığını ayağa kaldırır, gelen trafiği işler, katılan cihazları tanır
//! ve kullanıcının komutlarını gönderir.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use tokio::sync::{Notify, broadcast, oneshot};
use tracing::{debug, info, warn};

use ziggurat_driver::runtime::TokioSpawner;
use ziggurat_driver::zigbee_stack::{
    ApsAck, DeviceLeaveReason, NetworkConfig, NwkDeviceType, RouteDirective, Tunables,
    TxPriority, ZigbeeNotification, ZigbeeStack,
};
use ziggurat_driver::ziggurat_ieee_802154::types::{Eui64, Nwk, PanId};
use ziggurat_driver::ziggurat_zigbee::constants::WELL_KNOWN_LINK_KEY;
use ziggurat_phy_spinel::SpinelPhy;
use ziggurat_protocol as proto;

use crate::store::{
    self, Child, Device, LinkKey, NetworkData, Persisted, eui64_to_string, key_to_string,
    parse_eui64, parse_key,
};
use crate::zcl::{self, ReportConfig, Value, cluster, types};
use crate::{tuya, zdo};

/// Koordinatörün kendi uç noktası; bağlamalar ve raporlar buraya gelir
pub const HUB_ENDPOINT: u8 = 1;
const HA_PROFILE: u16 = 0x0104;
const ZDO_PROFILE: u16 = 0x0000;
const GREEN_POWER_PROFILE: u16 = 0xA1E0;
const GREEN_POWER_ENDPOINT: u8 = 242;
/// NWK yarıçapı; 0 vermek çerçeveyi ilk aktarıcıda düşürür
const RADIUS: u8 = 30;
/// Yığın çerçeve sayacını her 100 çerçevede bildiriyor. Yeniden başlatmada sayacın
/// geri gitmemesi için kayıtlı değerin üstüne bu kadar ekliyoruz.
const FRAME_COUNTER_MARGIN: u32 = 1000;
const SAVE_DELAY: Duration = Duration::from_secs(5);
const TABLE_SNAPSHOT_INTERVAL: Duration = Duration::from_secs(300);
const REPLY_TIMEOUT: Duration = Duration::from_secs(10);
/// Uyuyan cihaz isteği ancak bir sonraki yoklamada alır
const SLEEPY_REPLY_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_PERMIT_SECONDS: u8 = 254;

pub struct Options {
    pub device: String,
    pub baudrate: u32,
    pub state_path: PathBuf,
    pub channel: u8,
    pub tx_power: i8,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    Device { device: DeviceView },
    Removed { ieee: String },
    PermitJoin { remaining: u64 },
}

#[derive(Debug, Clone, Serialize)]
pub struct DeviceView {
    #[serde(flatten)]
    pub device: Device,
    pub capabilities: Vec<&'static str>,
    pub interviewing: bool,
    /// Termostat hedef sıcaklığı için [en az, en çok, adım]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub setpoint_range: Option<[f64; 3]>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "action", content = "value", rename_all = "snake_case")]
pub enum Action {
    On,
    Off,
    Toggle,
    Level(u8),
    ColorTemp(u16),
    Setpoint(f64),
    /// "heat" ya da "off"
    SystemMode(String),
    /// "manual" ya da "program"
    Preset(String),
    Frost(bool),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum PendingKey {
    Zdo { cluster: u16, tsn: u8 },
    Zcl { nwk: u16, seq: u8 },
}

pub struct Hub {
    stack: Arc<ZigbeeStack<SpinelPhy>>,
    ieee: Eui64,
    state_path: PathBuf,
    data: Mutex<Persisted>,
    dirty: Notify,
    pending: Mutex<HashMap<PendingKey, oneshot::Sender<Vec<u8>>>>,
    seq: AtomicU8,
    events: broadcast::Sender<Event>,
    permit_until: Mutex<Option<Instant>>,
    interviews: Mutex<HashSet<String>>,
}

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

/// Cihazın sunduğu kümelerden arayüzün kullanacağı yetenek listesi
pub fn capabilities(device: &Device) -> Vec<&'static str> {
    let mut caps = Vec::new();
    if device.has_cluster(cluster::ON_OFF) {
        caps.push("on_off");
    }
    if device.has_cluster(cluster::LEVEL) {
        caps.push("level");
    }
    if device.has_cluster(cluster::COLOR) {
        caps.push("color_temp");
    }
    if device.has_cluster(cluster::TEMPERATURE) {
        caps.push("temperature");
    }
    if device.has_cluster(cluster::HUMIDITY) {
        caps.push("humidity");
    }
    if device.has_cluster(cluster::OCCUPANCY) {
        caps.push("occupancy");
    }
    if device.has_cluster(cluster::IAS_ZONE) {
        caps.push(match device.zone_type {
            Some(0x0015) => "contact",
            Some(0x000D) => "motion",
            Some(0x002A) => "water_leak",
            Some(0x0028) => "smoke",
            _ => "alarm",
        });
    }
    if device.has_cluster(cluster::POWER_CONFIG) && !device.router {
        caps.push("battery");
    }
    if device.has_cluster(tuya::CLUSTER) && tuya::thermostat_profile(device.manufacturer.as_deref()).is_some() {
        caps.push("thermostat");
    }
    caps
}

/// Kullanıcı adı vermediyse modelden anlamlı bir ad; Tuya modelleri (TS0601 gibi)
/// hiçbir şey söylemediği için profil adı tercih edilir
fn default_name(device: &Device, taken: &[String]) -> Option<String> {
    let base = match tuya::thermostat_profile(device.manufacturer.as_deref()) {
        Some(profile) => profile.name.to_owned(),
        None => device.model.clone()?,
    };
    if !taken.contains(&base) {
        return Some(base);
    }
    let suffix = device.ieee.replace(':', "");
    Some(format!("{base} {}", &suffix[suffix.len().saturating_sub(4)..]))
}

/// Bir özniteliği cihaz durumuna yansıtır
fn apply_attribute(device: &mut Device, cluster_id: u16, attribute: u16, value: &Value) {
    let state = &mut device.state;
    match (cluster_id, attribute) {
        (cluster::BASIC, 0x0004) => device.manufacturer = value.as_str().map(str::to_owned),
        (cluster::BASIC, 0x0005) => device.model = value.as_str().map(str::to_owned),
        (cluster::ON_OFF, 0x0000) => state.on = value.as_u64().map(|v| v != 0),
        (cluster::LEVEL, 0x0000) => state.level = value.as_u64().and_then(|v| u8::try_from(v).ok()),
        (cluster::COLOR, 0x0007) => {
            state.color_temp = value.as_u64().and_then(|v| u16::try_from(v).ok());
        }
        (cluster::COLOR, 0x400B) => {
            device.color_temp_min = value.as_u64().and_then(|v| u16::try_from(v).ok());
        }
        (cluster::COLOR, 0x400C) => {
            device.color_temp_max = value.as_u64().and_then(|v| u16::try_from(v).ok());
        }
        (cluster::TEMPERATURE, 0x0000) => {
            // 0x8000 "ölçüm yok" demek
            if let Some(v) = value.as_i64().filter(|&v| v != -0x8000) {
                state.temperature = Some(v as f64 / 100.0);
            }
        }
        (cluster::HUMIDITY, 0x0000) => {
            if let Some(v) = value.as_u64().filter(|&v| v != 0xFFFF) {
                state.humidity = Some(v as f64 / 100.0);
            }
        }
        (cluster::OCCUPANCY, 0x0000) => state.occupancy = value.as_u64().map(|v| v & 0x01 != 0),
        (cluster::POWER_CONFIG, 0x0020) => {
            if let Some(v) = value.as_u64().filter(|&v| v != 0xFF) {
                state.voltage = Some(v as f64 / 10.0);
            }
        }
        (cluster::POWER_CONFIG, 0x0021) => {
            // Yarım yüzde birimiyle; 0xFF bilinmiyor
            if let Some(v) = value.as_u64().filter(|&v| v != 0xFF) {
                state.battery = Some((v / 2).min(100) as u8);
            }
        }
        (cluster::IAS_ZONE, 0x0001) => {
            device.zone_type = value.as_u64().and_then(|v| u16::try_from(v).ok());
        }
        (cluster::IAS_ZONE, 0x0002) => {
            if let Some(v) = value.as_u64() {
                apply_zone_status(state, v as u16);
            }
        }
        _ => {}
    }
}

fn apply_zone_status(state: &mut store::DeviceState, status: u16) {
    state.alarm = Some(status & 0x0001 != 0);
    state.battery_low = Some(status & 0x0008 != 0);
}

/// Her kümenin bağlanıp raporlanacak özniteliği
fn report_config(cluster_id: u16) -> Option<ReportConfig> {
    let (attribute, ty, min_interval, max_interval, reportable_change) = match cluster_id {
        cluster::ON_OFF => (0x0000, types::BOOL, 0, 300, 0),
        cluster::LEVEL => (0x0000, types::UINT8, 1, 300, 1),
        cluster::COLOR => (0x0007, types::UINT16, 1, 300, 1),
        cluster::TEMPERATURE => (0x0000, types::INT16, 30, 1800, 20),
        cluster::HUMIDITY => (0x0000, types::UINT16, 30, 1800, 100),
        cluster::OCCUPANCY => (0x0000, types::BITMAP8, 0, 1800, 0),
        cluster::POWER_CONFIG => (0x0021, types::UINT8, 3600, 21600, 2),
        _ => return None,
    };
    Some(ReportConfig {
        attribute,
        ty,
        min_interval,
        max_interval,
        reportable_change,
    })
}

/// Raporlamadan sonra güncel değerleri okumak için öznitelikler
fn initial_reads(cluster_id: u16) -> &'static [u16] {
    match cluster_id {
        cluster::COLOR => &[0x0007, 0x400B, 0x400C],
        cluster::POWER_CONFIG => &[0x0020, 0x0021],
        cluster::ON_OFF | cluster::LEVEL | cluster::TEMPERATURE | cluster::HUMIDITY
        | cluster::OCCUPANCY => &[0x0000],
        _ => &[],
    }
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
                debug!("{what} denemesi {n} başarısız: {e:#}");
                last_error = Some(e);
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
    }
    Err(last_error.unwrap_or_else(|| anyhow!("{what} başarısız")))
}

impl Hub {
    pub async fn start(opts: Options) -> Result<Arc<Self>> {
        let mut data = Persisted::load(&opts.state_path)?;

        let phy = crate::radio::open(&opts.device, opts.baudrate).await?;
        let radio_ieee = phy
            .hw_address()
            .await
            .map_err(|e| anyhow!("radyonun adresi okunamadı: {e}"))?;
        info!("Radyo hazır: {}", eui64_to_string(&radio_ieee));

        let mut network = match data.network.take() {
            Some(network) => network,
            None => {
                let network = store::new_network(&radio_ieee, opts.channel, opts.tx_power);
                info!(
                    "Kayıtlı ağ yok; yeni ağ kuruluyor (kanal {}, PAN 0x{:04x})",
                    network.channel, network.pan_id
                );
                network
            }
        };
        network.nwk_frame_counter = network.nwk_frame_counter.saturating_add(FRAME_COUNTER_MARGIN);
        network.aps_frame_counter = network.aps_frame_counter.saturating_add(FRAME_COUNTER_MARGIN);
        data.network = Some(network.clone());
        // Sayaçlar kullanılmadan önce diske: çökme döngüsünde bile geri gitmesinler
        data.save(&opts.state_path)?;

        let ieee = parse_eui64(&network.ieee)?;
        let stack = ZigbeeStack::new(
            phy,
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
            .map_err(|e| anyhow!("ağ başlatılamadı: {e}"))?;
        stack.spawn_tracked(|stack| async move {
            stack.run().await;
        });
        info!(
            "Ağ çalışıyor: kanal {}, PAN 0x{:04x}, {} cihaz",
            network.channel,
            network.pan_id,
            data.devices.len()
        );

        let (events, _) = broadcast::channel(256);
        let hub = Arc::new(Self {
            stack,
            ieee,
            state_path: opts.state_path,
            data: Mutex::new(data),
            dirty: Notify::new(),
            pending: Mutex::new(HashMap::new()),
            seq: AtomicU8::new(store::random_bytes::<1>()[0]),
            events,
            permit_until: Mutex::new(None),
            interviews: Mutex::new(HashSet::new()),
        });

        tokio::spawn(hub.clone().notification_loop());
        tokio::spawn(hub.clone().saver());
        tokio::spawn(hub.clone().table_snapshots());

        // Önceki çalışmada tanıma yarıda kaldıysa sürdür
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

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.events.subscribe()
    }

    pub fn devices(&self) -> Vec<DeviceView> {
        let data = self.data.lock().unwrap();
        data.devices.values().map(|d| self.view(d)).collect()
    }

    fn view(&self, device: &Device) -> DeviceView {
        DeviceView {
            capabilities: capabilities(device),
            interviewing: self.interviews.lock().unwrap().contains(&device.ieee),
            setpoint_range: tuya::thermostat_profile(device.manufacturer.as_deref())
                .map(|p| [p.setpoint_min, p.setpoint_max, p.setpoint_step]),
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

    /// Cihazı değiştirir; değişiklik olduysa yayınlar ve kaydı tetikler
    fn update_device(&self, ieee: &str, change: impl FnOnce(&mut Device)) {
        let view = {
            let mut data = self.data.lock().unwrap();
            let Some(device) = data.devices.get_mut(ieee) else {
                return;
            };
            let before = serde_json::to_value(&*device).ok();
            change(device);
            if serde_json::to_value(&*device).ok() == before {
                return;
            }
            self.view(device)
        };
        let _ = self.events.send(Event::Device { device: view });
        self.dirty.notify_one();
    }

    fn emit_device(&self, ieee: &str) {
        if let Some(device) = self.device(ieee) {
            let _ = self.events.send(Event::Device {
                device: self.view(&device),
            });
        }
    }

    fn next_seq(&self) -> u8 {
        self.seq.fetch_add(1, Ordering::Relaxed)
    }

    // ---- Gelen trafik ----

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
                lqi,
                data,
                ..
            } => {
                self.touch(source.0, lqi);
                if profile_id == ZDO_PROFILE {
                    self.handle_zdo(source.0, destination.0, cluster_id, &data);
                } else {
                    self.handle_zcl(source.0, src_ep, cluster_id, &data);
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
                info!("Cihaz ayrıldı: {ieee:?} ({reason:?})");
                // Yeniden katılacağını söylemeden ayrılan cihazı listeden düşür; zaman
                // aşımına uğrayan uyuyan çocuklar ise yeniden katılır
                if let (Some(ieee), DeviceLeaveReason::Announced { rejoin: false }) = (ieee, reason) {
                    self.forget(&ieee);
                }
            }
            ZigbeeNotification::ApsDecryptionFailure { source_ieee, .. } => {
                warn!(
                    "{} cihazının APS çerçevesi çözülemedi; cihazı yeniden eşleştirmek gerekebilir",
                    eui64_to_string(&source_ieee)
                );
            }
            ZigbeeNotification::RouteRecord { .. } => {}
        }
    }

    fn touch(&self, nwk: u16, lqi: u8) {
        let mut data = self.data.lock().unwrap();
        if let Some(device) = data.devices.values_mut().find(|d| d.nwk == nwk) {
            device.last_seen = Some(now_unix());
            device.lqi = Some(lqi);
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
        // Katılım, Update-Device ve Device_annce aynı olayı üç kez bildirebiliyor
        if changed {
            info!("Cihaz katıldı: {ieee} (nwk 0x{nwk:04x})");
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
                // IAS sensörleri merkezi bununla arar; yanıtsız kalırlarsa kayıt olmazlar
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
                debug!("ZDO yanıtı gönderilemedi: {e:#}");
            }
        }
    }

    fn handle_zcl(self: &Arc<Self>, source: u16, src_ep: u8, cluster_id: u16, data: &[u8]) {
        let Some((header, payload)) = zcl::Header::parse(data) else {
            return;
        };
        let ieee = self.ieee_for_nwk(source);

        if !header.cluster_specific() {
            match header.command {
                zcl::cmd::REPORT_ATTRIBUTES => {
                    if let Some(ieee) = &ieee {
                        self.apply_records(ieee, cluster_id, &zcl::parse_report(payload));
                    }
                    self.default_response(source, src_ep, cluster_id, &header);
                }
                zcl::cmd::READ_ATTRIBUTES_RSP => {
                    if let Some(ieee) = &ieee {
                        self.apply_records(ieee, cluster_id, &zcl::parse_read_response(payload));
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

        if cluster_id == cluster::IAS_ZONE && header.server_to_client() {
            match header.command {
                // Zone Status Change Notification
                0x00 => {
                    if let (Some(ieee), Some(bytes)) = (&ieee, payload.get(..2)) {
                        let status = u16::from_le_bytes([bytes[0], bytes[1]]);
                        self.update_device(ieee, |d| apply_zone_status(&mut d.state, status));
                    }
                    self.default_response(source, src_ep, cluster_id, &header);
                }
                // Zone Enroll Request
                0x01 => {
                    if let (Some(ieee), Some(bytes)) = (&ieee, payload.get(..2)) {
                        let zone_type = u16::from_le_bytes([bytes[0], bytes[1]]);
                        self.update_device(ieee, |d| d.zone_type = Some(zone_type));
                    }
                    self.send_enroll_response(source, src_ep);
                }
                _ => {}
            }
            return;
        }

        if cluster_id == tuya::CLUSTER && header.server_to_client() {
            self.handle_tuya(source, src_ep, ieee.as_deref(), &header, payload);
            return;
        }

        debug!(
            "İşlenmeyen ZCL komutu: nwk 0x{source:04x} küme 0x{cluster_id:04x} komut 0x{:02x}",
            header.command
        );
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
                let profile = self
                    .device(ieee)
                    .and_then(|d| tuya::thermostat_profile(d.manufacturer.as_deref()));
                self.update_device(ieee, |device| {
                    for dp in &datapoints {
                        let known = profile.is_some_and(|p| p.apply(&mut device.state, dp));
                        if !known {
                            // Yeni bir model eklerken DP haritasını çıkarmak için
                            info!(
                                "{ieee} Tuya DP {} (tür {}): {:02x?}",
                                dp.id, dp.ty, dp.data
                            );
                        }
                    }
                });
            }
            tuya::cmd::MCU_SYNC_TIME => {
                let since_2000 = ieee
                    .and_then(|ieee| self.device(ieee))
                    .and_then(|d| tuya::thermostat_profile(d.manufacturer.as_deref()))
                    .is_none_or(|p| p.time_since_2000);
                let body = tuya::time_sync_payload(now_unix(), tuya::utc_offset_seconds(), since_2000);
                self.send_tuya(nwk, ep, tuya::cmd::MCU_SYNC_TIME, &body);
            }
            tuya::cmd::GATEWAY_CONNECTION_STATUS => {
                // Ağ geçidi internete bağlı: bazı cihazlar bunu görmeden rapor göndermiyor
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
            warn!("Tuya komutu 0x{command:02x} gönderilemedi: {e:#}");
        }
    }

    fn apply_records(&self, ieee: &str, cluster_id: u16, records: &[zcl::AttributeRecord]) {
        self.update_device(ieee, |device| {
            for record in records {
                if let Some(value) = &record.value {
                    apply_attribute(device, cluster_id, record.id, value);
                }
            }
        });
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
            debug!("Default Response gönderilemedi: {e:#}");
        }
    }

    fn send_enroll_response(&self, nwk: u16, ep: u8) {
        // Bölge kimliği yalnızca bu merkez içinde anlamlı
        let zone_id = (nwk & 0x7F) as u8;
        let frame = zcl::frame(
            zcl::FC_CLUSTER_SPECIFIC | zcl::FC_DISABLE_DEFAULT_RESPONSE,
            self.next_seq(),
            0x00,
            &[zcl::STATUS_SUCCESS, zone_id],
        );
        if let Err(e) = self.send(nwk, HA_PROFILE, cluster::IAS_ZONE, HUB_ENDPOINT, ep, frame, true) {
            warn!("IAS kayıt yanıtı gönderilemedi: {e:#}");
        }
    }

    // ---- Giden istekler ----

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
            .map_err(|e| anyhow!("gönderim kuyruğa alınamadı: {e:?}"))
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
    ) -> Result<Vec<u8>> {
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(key, tx);
        if let Err(e) = self.send(nwk, profile, cluster_id, src_ep, dst_ep, data, true) {
            self.pending.lock().unwrap().remove(&key);
            return Err(e);
        }
        let wait = if self.is_sleepy(nwk) {
            SLEEPY_REPLY_TIMEOUT
        } else {
            REPLY_TIMEOUT
        };
        match tokio::time::timeout(wait, rx).await {
            Ok(Ok(response)) => Ok(response),
            _ => {
                self.pending.lock().unwrap().remove(&key);
                bail!("cihaz yanıt vermedi")
            }
        }
    }

    async fn zdo_request(&self, nwk: u16, cluster_id: u16, payload: Vec<u8>) -> Result<Vec<u8>> {
        let tsn = self.next_seq();
        let mut data = vec![tsn];
        data.extend(payload);
        let key = PendingKey::Zdo {
            cluster: cluster_id | zdo::cluster::RESPONSE,
            tsn,
        };
        self.request(key, nwk, ZDO_PROFILE, cluster_id, 0, 0, data)
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
        let seq = self.next_seq();
        let frame = zcl::frame(frame_control, seq, command, payload);
        let key = PendingKey::Zcl { nwk, seq };
        let response = self
            .request(key, nwk, HA_PROFILE, cluster_id, HUB_ENDPOINT, ep, frame)
            .await?;
        let (header, body) =
            zcl::Header::parse(&response).ok_or_else(|| anyhow!("bozuk ZCL yanıtı"))?;
        Ok((header, body.to_vec()))
    }

    async fn read_attributes(
        &self,
        nwk: u16,
        ep: u8,
        cluster_id: u16,
        ids: &[u16],
    ) -> Result<Vec<zcl::AttributeRecord>> {
        let (header, body) = self
            .zcl_request(
                nwk,
                ep,
                cluster_id,
                0x00,
                zcl::cmd::READ_ATTRIBUTES,
                &zcl::read_attributes_payload(ids),
            )
            .await?;
        if header.command != zcl::cmd::READ_ATTRIBUTES_RSP {
            bail!("öznitelik okuma reddedildi (komut 0x{:02x})", header.command);
        }
        Ok(zcl::parse_read_response(&body))
    }

    // ---- Cihaz tanıma ----

    pub fn spawn_interview(self: &Arc<Self>, ieee: String) {
        if !self.interviews.lock().unwrap().insert(ieee.clone()) {
            return;
        }
        self.emit_device(&ieee);
        let hub = self.clone();
        tokio::spawn(async move {
            let result = hub.interview(&ieee).await;
            hub.interviews.lock().unwrap().remove(&ieee);
            match result {
                Ok(()) => info!("{ieee} tanındı"),
                Err(e) => warn!("{ieee} tanınamadı: {e:#}"),
            }
            hub.emit_device(&ieee);
        });
    }

    async fn interview(self: &Arc<Self>, ieee: &str) -> Result<()> {
        // Cihaz katıldıktan sonra anahtar alışverişini bitirsin
        tokio::time::sleep(Duration::from_secs(2)).await;
        let nwk = self.device(ieee).context("cihaz listede yok")?.nwk;
        info!("{ieee} tanınıyor");

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

        let device = self.device(ieee).context("cihaz listede yok")?;
        if let Some(ep) = device.endpoint_with(cluster::BASIC) {
            // Tuya cihazları bu öznitelikleri tek istekte okumadan veri göndermiyor;
            // diğer cihazlar için de zararsız
            if let Err(e) = self
                .read_attributes(nwk, ep, cluster::BASIC, &[0x0004, 0x0000, 0x0001, 0x0005, 0x0007, 0xFFFE])
                .await
            {
                warn!("{ieee} temel bilgileri okunamadı: {e:#}");
            }
        }

        self.configure(ieee).await;

        if let Some(ep) = device.endpoint_with(tuya::CLUSTER) {
            // Bütün DP'leri bir kez iste; sonrası cihazın kendi raporlarıyla gelir
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
            let untouched = d.name == Device::new(d.ieee.clone(), d.nwk).name;
            if untouched && let Some(name) = default_name(d, &taken) {
                d.name = name;
            }
        });
        Ok(())
    }

    /// Kümeleri merkeze bağlar, raporlamayı ayarlar ve güncel değerleri okur. Tek tek
    /// hatalar tanımayı durdurmaz: bazı cihazlar bazı adımları desteklemiyor.
    async fn configure(self: &Arc<Self>, ieee: &str) {
        let Some(device) = self.device(ieee) else {
            return;
        };
        let Ok(device_ieee) = parse_eui64(ieee) else {
            return;
        };
        let nwk = device.nwk;

        for endpoint in &device.endpoints {
            for &cluster_id in &endpoint.in_clusters {
                if cluster_id == cluster::IAS_ZONE {
                    self.enroll(ieee, nwk, endpoint.id).await;
                    continue;
                }
                let Some(report) = report_config(cluster_id) else {
                    continue;
                };

                let bind = retry("Bind", || async move {
                    let rsp = self
                        .zdo_request(
                            nwk,
                            zdo::cluster::BIND_REQ,
                            zdo::bind_req(device_ieee.0, endpoint.id, cluster_id, self.ieee.0, HUB_ENDPOINT),
                        )
                        .await?;
                    zdo::parse_status_rsp(&rsp)
                })
                .await;
                if let Err(e) = bind {
                    warn!("{ieee} küme 0x{cluster_id:04x} bağlanamadı: {e:#}");
                }

                match self
                    .zcl_request(
                        nwk,
                        endpoint.id,
                        cluster_id,
                        0x00,
                        zcl::cmd::CONFIGURE_REPORTING,
                        &zcl::configure_reporting_payload(&[report]),
                    )
                    .await
                {
                    Ok((header, body))
                        if header.command == zcl::cmd::CONFIGURE_REPORTING_RSP
                            && zcl::all_success(&body) => {}
                    Ok((header, body)) => warn!(
                        "{ieee} küme 0x{cluster_id:04x} raporlama ayarı reddedildi (komut 0x{:02x}, {body:02x?})",
                        header.command
                    ),
                    Err(e) => warn!("{ieee} küme 0x{cluster_id:04x} raporlama ayarlanamadı: {e:#}"),
                }

                let reads = initial_reads(cluster_id);
                if !reads.is_empty()
                    && let Err(e) = self.read_attributes(nwk, endpoint.id, cluster_id, reads).await
                {
                    debug!("{ieee} küme 0x{cluster_id:04x} okunamadı: {e:#}");
                }
            }
        }
    }

    /// IAS bölgesini merkeze kaydeder: CIE adresini yazar ve kayıt yanıtını önceden
    /// gönderir (bazı sensörler kayıt isteği göndermeden bekler)
    async fn enroll(&self, ieee: &str, nwk: u16, ep: u8) {
        let payload = zcl::write_attribute_payload(0x0010, types::EUI64, &self.ieee.0);
        match self
            .zcl_request(nwk, ep, cluster::IAS_ZONE, 0x00, zcl::cmd::WRITE_ATTRIBUTES, &payload)
            .await
        {
            Ok((header, body))
                if header.command == zcl::cmd::WRITE_ATTRIBUTES_RSP && zcl::all_success(&body) => {}
            Ok((_, body)) => warn!("{ieee} IAS merkez adresi reddedildi: {body:02x?}"),
            Err(e) => warn!("{ieee} IAS merkez adresi yazılamadı: {e:#}"),
        }
        if let Err(e) = self
            .read_attributes(nwk, ep, cluster::IAS_ZONE, &[0x0001, 0x0002])
            .await
        {
            debug!("{ieee} IAS durumu okunamadı: {e:#}");
        }
        self.send_enroll_response(nwk, ep);
    }

    // ---- Kullanıcı işlemleri ----

    pub async fn command(&self, ieee: &str, action: Action) -> Result<()> {
        let device = self.device(ieee).context("cihaz bulunamadı")?;
        if matches!(
            action,
            Action::Setpoint(_) | Action::SystemMode(_) | Action::Preset(_) | Action::Frost(_)
        ) {
            return self.thermostat_command(&device, action);
        }
        let (cluster_id, command, payload): (u16, u8, Vec<u8>) = match &action {
            Action::On => (cluster::ON_OFF, 0x01, vec![]),
            Action::Off => (cluster::ON_OFF, 0x00, vec![]),
            Action::Toggle => (cluster::ON_OFF, 0x02, vec![]),
            // Move to Level (with On/Off), yarım saniyelik geçiş
            Action::Level(level) => (cluster::LEVEL, 0x04, vec![(*level).min(254), 5, 0]),
            // Move to Color Temperature
            Action::ColorTemp(mireds) => {
                let mut payload = mireds.to_le_bytes().to_vec();
                payload.extend_from_slice(&5u16.to_le_bytes());
                (cluster::COLOR, 0x0A, payload)
            }
            Action::Setpoint(_) | Action::SystemMode(_) | Action::Preset(_) | Action::Frost(_) => {
                unreachable!("termostat komutları yukarıda ayrıldı")
            }
        };
        let ep = device
            .endpoint_with(cluster_id)
            .context("cihaz bu komutu desteklemiyor")?;

        let (header, body) = self
            .zcl_request(device.nwk, ep, cluster_id, zcl::FC_CLUSTER_SPECIFIC, command, &payload)
            .await?;
        if header.command == zcl::cmd::DEFAULT_RSP
            && let Some(status) = zcl::default_response_status(&body)
            && status != zcl::STATUS_SUCCESS
        {
            bail!("cihaz komutu reddetti (durum 0x{status:02x})");
        }

        // Raporlar birazdan gelecek; arayüz beklemesin
        self.update_device(ieee, |d| match action {
            Action::On => d.state.on = Some(true),
            Action::Off => d.state.on = Some(false),
            Action::Toggle => d.state.on = d.state.on.map(|on| !on),
            Action::Level(level) => {
                d.state.level = Some(level);
                d.state.on = Some(level > 0);
            }
            Action::ColorTemp(mireds) => d.state.color_temp = Some(mireds),
            _ => {}
        });
        Ok(())
    }

    /// Tuya termostatına DP yazar. Cihaz uyuyan uç cihaz olduğu için komut bir sonraki
    /// yoklamasında teslim edilir; onayı cihazın kendi DP raporu getirir.
    fn thermostat_command(&self, device: &Device, action: Action) -> Result<()> {
        let profile = tuya::thermostat_profile(device.manufacturer.as_deref())
            .context("bu cihaz için termostat tanımı yok")?;
        let ep = device
            .endpoint_with(tuya::CLUSTER)
            .context("cihazda Tuya kümesi yok")?;
        let dp = match &action {
            Action::Setpoint(celsius) => profile.setpoint_dp(*celsius).with_context(|| {
                format!(
                    "hedef sıcaklık {}-{} °C arasında olmalı",
                    profile.setpoint_min, profile.setpoint_max
                )
            })?,
            Action::SystemMode(mode) => match mode.as_str() {
                "heat" => profile.system_mode_dp(true),
                "off" => profile.system_mode_dp(false),
                _ => bail!("geçersiz mod: {mode}"),
            },
            Action::Preset(preset) => match preset.as_str() {
                "manual" => profile.preset_dp(false),
                "program" => profile.preset_dp(true),
                _ => bail!("geçersiz ön ayar: {preset}"),
            }
            .context("cihazda ön ayar yok")?,
            Action::Frost(on) => profile.frost_dp(*on).context("cihazda donma koruması yok")?,
            _ => bail!("termostat komutu değil"),
        };

        let seq = u16::from(self.next_seq());
        self.send_tuya(device.nwk, ep, tuya::cmd::DATA_REQUEST, &tuya::data_request(seq, &[dp.clone()]));
        self.update_device(&device.ieee, |d| {
            profile.apply(&mut d.state, &dp);
        });
        Ok(())
    }

    pub fn reinterview(self: &Arc<Self>, ieee: &str) -> Result<()> {
        if self.device(ieee).is_none() {
            bail!("cihaz bulunamadı");
        }
        self.spawn_interview(ieee.to_owned());
        Ok(())
    }

    pub fn permit_join(self: &Arc<Self>, seconds: u8) -> Result<()> {
        let seconds = seconds.min(MAX_PERMIT_SECONDS);
        self.stack.permit_joins(u64::from(seconds), true);

        // Yönlendiricilere de duyur: cihazlar en yakın Hue lambası üzerinden katılabilsin
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
            .map_err(|e| anyhow!("eşleştirme izni yayınlanamadı: {e:?}"))?;

        let until = (seconds > 0).then(|| Instant::now() + Duration::from_secs(u64::from(seconds)));
        *self.permit_until.lock().unwrap() = until;
        info!("Eşleştirme izni: {seconds} sn");
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

    pub fn rename(&self, ieee: &str, name: &str) -> Result<()> {
        let name = name.trim();
        if name.is_empty() || name.chars().count() > 64 {
            bail!("ad 1-64 karakter olmalı");
        }
        if self.device(ieee).is_none() {
            bail!("cihaz bulunamadı");
        }
        self.update_device(ieee, |d| d.name = name.to_owned());
        Ok(())
    }

    /// Cihazdan ağdan ayrılmasını ister ve kayıtlarını siler
    pub async fn remove(&self, ieee: &str) -> Result<()> {
        let device = self.device(ieee).context("cihaz bulunamadı")?;
        let device_ieee = parse_eui64(ieee)?;
        if let Err(e) = self
            .zdo_request(device.nwk, zdo::cluster::MGMT_LEAVE_REQ, zdo::mgmt_leave_req(device_ieee.0))
            .await
        {
            warn!("{ieee} ayrılma isteğine yanıt vermedi: {e:#}");
        }
        self.forget(ieee);
        Ok(())
    }

    fn forget(&self, ieee: &str) {
        {
            let mut data = self.data.lock().unwrap();
            data.devices.remove(ieee);
            data.link_keys.retain(|k| k.ieee != ieee);
            data.children.retain(|c| c.ieee != ieee);
        }
        let _ = self.events.send(Event::Removed {
            ieee: ieee.to_owned(),
        });
        self.dirty.notify_one();
    }

    // ---- Kalıcılık ----

    async fn saver(self: Arc<Self>) {
        loop {
            self.dirty.notified().await;
            tokio::time::sleep(SAVE_DELAY).await;
            if let Err(e) = self.save_now().await {
                warn!("Durum kaydedilemedi: {e:#}");
            }
        }
    }

    pub async fn save_now(&self) -> Result<()> {
        let snapshot = self.data.lock().unwrap().clone();
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

    /// Link anahtarlarını ve çocuk tablosunu yığından alır; değiştiyse `true`
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

/// Önceki çalışmadan kalan link anahtarlarını, çocukları ve adres eşlemelerini yığına
/// geri yükler. Çocuk tablosu olmadan uyuyan sensörler yeniden başlatmada kopar.
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
        "Geri yükleniyor: {} link anahtarı, {} çocuk, {} adres",
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

    #[test]
    fn hue_white_ambiance_capabilities() {
        let mut device = device_with(&[0x0000, 0x0006, 0x0008, 0x0300]);
        device.router = true;
        assert_eq!(capabilities(&device), ["on_off", "level", "color_temp"]);
    }

    #[test]
    fn contact_sensor_capabilities() {
        let mut device = device_with(&[0x0000, 0x0001, 0x0500]);
        device.zone_type = Some(0x0015);
        assert_eq!(capabilities(&device), ["contact", "battery"]);
    }

    #[test]
    fn applies_sensor_attributes() {
        let mut device = device_with(&[]);
        apply_attribute(&mut device, cluster::TEMPERATURE, 0, &Value::Int(2153));
        apply_attribute(&mut device, cluster::HUMIDITY, 0, &Value::Uint(4512));
        apply_attribute(&mut device, cluster::POWER_CONFIG, 0x0021, &Value::Uint(200));
        apply_attribute(&mut device, cluster::IAS_ZONE, 0x0002, &Value::Uint(0x0009));
        assert_eq!(device.state.temperature, Some(21.53));
        assert_eq!(device.state.humidity, Some(45.12));
        assert_eq!(device.state.battery, Some(100));
        assert_eq!(device.state.alarm, Some(true));
        assert_eq!(device.state.battery_low, Some(true));
    }

    #[test]
    fn ignores_invalid_temperature() {
        let mut device = device_with(&[]);
        apply_attribute(&mut device, cluster::TEMPERATURE, 0, &Value::Int(-0x8000));
        assert_eq!(device.state.temperature, None);
    }

    #[test]
    fn action_json_shape() {
        let action: Action = serde_json::from_str(r#"{"action":"level","value":128}"#).unwrap();
        assert!(matches!(action, Action::Level(128)));
        let action: Action = serde_json::from_str(r#"{"action":"on"}"#).unwrap();
        assert!(matches!(action, Action::On));
    }
}
