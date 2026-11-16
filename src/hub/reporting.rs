use super::*;
use serde::Deserialize;

pub(super) const REPORTING_WRITE_PREFIX: &str = "reporting:";
pub(super) const REPORTING_READ_PREFIX: &str = "reporting_read:";
const POLL_OPTION: &str = "measurement_poll_interval";

fn can_poll_measurements(device: &Device) -> bool {
    device.model.as_deref() == Some("TS011F")
        && device
            .endpoint_with(cluster::ELECTRICAL_MEASUREMENT)
            .is_some()
}

pub(super) fn polling_profile(device: &Device) -> bool {
    can_poll_measurements(device)
        && match (device.application_version, device.manufacturer.as_deref()) {
            (
                _,
                Some(
                    "_TZ3008_tary5dvv" | "_TZ3210_iooniers" | "_TZ3008_iooniers"
                    | "_TZ3008_xvfd3nkp" | "_TZ3008_qziabvzj",
                ),
            ) => false,
            (Some(192), Some("_TZ3000_2uollq9d")) | (Some(67), Some("_TZ3000_9ni6xxld")) => true,
            (Some(64 | 65 | 68 | 69 | 100 | 160), _) => true,
            (_, Some("_TZ3000_cehuw1lw")) => device.firmware.as_deref() == Some("1.0.5"),
            _ => false,
        }
}

fn poll_interval(device: &Device) -> Option<Duration> {
    if !polling_profile(device) {
        return None;
    }
    let seconds = if let Some(value) = device.options.get(POLL_OPTION) {
        value.as_u64()?
    } else {
        60
    };
    (seconds > 0).then(|| Duration::from_secs(seconds))
}

pub(super) fn uses_polling(device: &Device, cluster_id: u16) -> bool {
    let profile = polling_profile(device);
    let metering = profile
        && (matches!(device.application_version, Some(100 | 160 | 192))
            || device.firmware.as_deref() == Some("1.0.5"));
    (cluster_id == cluster::ELECTRICAL_MEASUREMENT && profile)
        || (cluster_id == cluster::METERING && metering)
}

pub(super) fn queue_missing_reporting_reads(
    device: &Device,
    pending: &mut HashMap<String, BTreeMap<String, Json>>,
) -> bool {
    if !device.interviewed || device.rx_on_when_idle {
        return false;
    }
    let mut added = false;
    for feature in devices::features(device) {
        let Source::Attr {
            cluster,
            report: Some(_),
            ..
        } = feature.source
        else {
            continue;
        };
        if uses_polling(device, cluster)
            || device.endpoint_with(cluster).is_none()
            || device.active_reporting.contains_key(feature.key)
            || device.unavailable_reporting.contains(feature.key)
        {
            continue;
        }
        let key = format!("{REPORTING_READ_PREFIX}{}", feature.key);
        if pending
            .entry(device.ieee.clone())
            .or_default()
            .insert(key, Json::Null)
            .is_none()
        {
            added = true;
        }
    }
    added
}

pub(super) fn measurement_poll_setting(device: &Device) -> Option<i64> {
    polling_profile(device)
        .then(|| poll_interval(device).map_or(-1, |interval| interval.as_secs() as i64))
}

pub(super) fn reporting_fields(
    device: &Device,
    features: &[Feature],
    pending: Option<&BTreeMap<String, Json>>,
) -> Vec<Json> {
    features.iter().filter_map(|feature| {
        let Source::Attr { cluster, ty, conv, report: Some(_), .. } = feature.source else {
            return None;
        };
        if uses_polling(device, cluster) {
            return None;
        }
        let endpoint = device.endpoint_with(cluster)?;
        let scale = zcl::is_analog(ty).then(|| match conv {
            devices::Conv::Number { divisor, .. } => divisor,
            devices::Conv::BatteryHalfPercent => 2.0,
            _ => 1.0,
        });
        Some(json!({
            "key": feature.key, "label": feature.label, "endpoint": endpoint, "scale": scale, "kind": feature.kind,
            "configuration": device.active_reporting.get(feature.key),
            "initial": device.initial_reporting.get(feature.key),
            "pending": pending.and_then(|entries| entries.get(&format!("{REPORTING_WRITE_PREFIX}{}", feature.key)))
                .and_then(|value| value.get("configuration")),
        }))
    }).collect()
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
pub struct ReportingIntervals {
    pub minimum_report_interval: u16,
    pub maximum_report_interval: u16,
    #[serde(default)]
    pub reportable_change: u64,
}

impl ReportingIntervals {
    pub(super) fn from_response(response: &zcl::ReportingResponse) -> Option<Self> {
        Some(Self {
            minimum_report_interval: response.min_rep_intval?,
            maximum_report_interval: response.max_rep_intval?,
            reportable_change: response.rep_change.unwrap_or(0),
        })
    }

    pub(super) fn from_config(config: &ReportConfig) -> Self {
        Self {
            minimum_report_interval: config.min_interval,
            maximum_report_interval: config.max_interval,
            reportable_change: config.reportable_change,
        }
    }
}

#[derive(Deserialize)]
pub struct ReportingRequest {
    pub key: String,
    pub configuration: Option<ReportingIntervals>,
}

#[derive(Deserialize, Serialize)]
struct ReportingWrite {
    configuration: ReportingIntervals,
    keep_on_interview: bool,
}

#[derive(Debug)]
pub(super) struct ReportingRejected(String);

impl std::fmt::Display for ReportingRejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ReportingRejected {}

#[derive(Serialize)]
pub struct ReportingWriteResult {
    #[serde(flatten)]
    configuration: ReportingIntervals,
    pending: bool,
}

impl Hub {
    pub(super) async fn poll_electrical_measurements(self: Arc<Self>) {
        // Older state files did not keep the Basic cluster's application version.
        let unknown: Vec<_> = self
            .data
            .lock()
            .unwrap()
            .devices
            .values()
            .filter(|d| {
                d.interviewed && can_poll_measurements(d) && d.application_version.is_none()
            })
            .map(|d| d.ieee.clone())
            .collect();
        for ieee in unknown {
            if let Err(e) = self.read_attrs(&ieee, cluster::BASIC, &[0x0001]).await {
                debug!("{ieee} application version unavailable: {e:#}");
            }
        }
        let mut last = HashMap::new();
        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;
            let devices: Vec<_> = self
                .data
                .lock()
                .unwrap()
                .devices
                .values()
                .filter(|device| device.interviewed && poll_interval(device).is_some())
                .cloned()
                .collect();
            last.retain(|ieee, _| devices.iter().any(|device| &device.ieee == ieee));
            for device in devices {
                let sent = last.entry(device.ieee.clone()).or_insert_with(Instant::now);
                let Some(period) = poll_interval(&device) else {
                    continue;
                };
                if sent.elapsed() < period {
                    continue;
                }
                *sent = Instant::now();
                if let Err(e) = self
                    .read_attrs(
                        &device.ieee,
                        cluster::ELECTRICAL_MEASUREMENT,
                        &[0x0505, 0x0508, 0x050B],
                    )
                    .await
                {
                    debug!("{} measurement poll failed: {e:#}", device.name);
                }
                if uses_polling(&device, cluster::METERING)
                    && device.endpoint_with(cluster::METERING).is_some()
                    && let Err(e) = self.read_attrs(&device.ieee, cluster::METERING, &[0]).await
                {
                    debug!("{} energy poll failed: {e:#}", device.name);
                }
            }
        }
    }

    pub async fn read_reporting(&self, ieee: &str, key: &str) -> Result<Json> {
        let (device, _, _) = self.reporting_feature(ieee, key)?;
        if let Some(config) = device.active_reporting.get(key) {
            return Ok(json!(config));
        }
        if device.rx_on_when_idle {
            return Ok(json!(self.read_current_reporting(ieee, key, None).await?));
        }
        if device.unavailable_reporting.contains(key) {
            bail!("Reporting settings unavailable");
        }
        Ok(json!({ "pending": true }))
    }

    pub(super) async fn read_pending_reporting(
        &self,
        ieee: &str,
        key: &str,
        wait: Option<Duration>,
    ) -> Result<()> {
        let Some(device) = self.device(ieee) else {
            return Ok(());
        };
        if device.active_reporting.contains_key(key) || device.unavailable_reporting.contains(key) {
            return Ok(());
        }
        let result = self
            .read_current_reporting(ieee, key, wait.or(Some(EAGER_WRITE_WAIT)))
            .await
            .map(drop);
        if result
            .as_ref()
            .is_err_and(|error| error.is::<ReportingRejected>())
        {
            self.update_device(ieee, |d| {
                d.unavailable_reporting.insert(key.to_owned());
            });
        }
        result
    }

    async fn read_current_reporting(
        &self,
        ieee: &str,
        key: &str,
        wait: Option<Duration>,
    ) -> Result<ReportingIntervals> {
        let response = self.read_reporting_response(ieee, key, wait).await?;
        ReportingIntervals::from_response(&response).ok_or_else(|| {
            ReportingRejected(format!(
                "reporting configuration unavailable (status 0x{:02x})",
                response.status
            ))
            .into()
        })
    }

    async fn read_reporting_response(
        &self,
        ieee: &str,
        key: &str,
        wait: Option<Duration>,
    ) -> Result<zcl::ReportingResponse> {
        let _guard = self.reporting_write.lock().await;
        let (device, ep, feature) = self.reporting_feature(ieee, key)?;
        let Source::Attr {
            cluster,
            attr,
            manufacturer,
            ..
        } = feature.source
        else {
            unreachable!()
        };
        let [lo, hi] = attr.to_le_bytes();
        let (header, body) = self
            .zcl_request_within(
                device.nwk,
                ep,
                cluster,
                0,
                manufacturer,
                zcl::cmd::READ_REPORTING,
                &[0, lo, hi],
                wait,
            )
            .await?;
        if header.command != zcl::cmd::READ_REPORTING_RSP {
            return Err(
                ReportingRejected("device rejected Read Reporting Configuration".into()).into(),
            );
        }
        let response = zcl::parse_reporting_response(&body, attr)?;
        if let Some(config) = ReportingIntervals::from_response(&response) {
            self.update_device(ieee, |d| {
                d.unavailable_reporting.remove(key);
                d.active_reporting.insert(key.to_owned(), config);
            });
        }
        Ok(response)
    }

    pub(super) async fn read_initial_reporting(
        &self,
        device: &Device,
        ep: u8,
        cluster: u16,
        manufacturer: Option<u16>,
        attributes: &[u16],
    ) -> Result<Vec<zcl::ReportingResponse>> {
        let mut payload = Vec::new();
        for id in attributes {
            let [lo, hi] = id.to_le_bytes();
            payload.extend([0, lo, hi]);
        }
        // The interview still needs to bind and configure the device if it
        // does not answer Read Reporting Configuration.
        let (header, body) = self
            .zcl_request_within(
                device.nwk,
                ep,
                cluster,
                0,
                manufacturer,
                zcl::cmd::READ_REPORTING,
                &payload,
                Some(Duration::from_secs(4)),
            )
            .await?;
        if header.command != zcl::cmd::READ_REPORTING_RSP {
            bail!("device rejected Read Reporting Configuration");
        }
        zcl::parse_reporting_responses(&body)
    }

    pub async fn configure_reporting(
        self: &Arc<Self>,
        ieee: &str,
        request: ReportingRequest,
    ) -> Result<ReportingWriteResult> {
        let (device, _, feature) = self.reporting_feature(ieee, &request.key)?;
        let Source::Attr {
            ty,
            report: Some(defaults),
            ..
        } = feature.source
        else {
            unreachable!()
        };
        let initial = device.initial_reporting.get(feature.key).copied();
        let config = request
            .configuration
            .or(initial)
            .unwrap_or(ReportingIntervals {
                minimum_report_interval: defaults.min,
                maximum_report_interval: defaults.max,
                reportable_change: defaults.change,
            });
        if config.maximum_report_interval != 0
            && config.maximum_report_interval < config.minimum_report_interval
        {
            bail!("maximum interval must be at least the minimum interval");
        }
        if config.reportable_change > zcl::max_reportable_change(ty) {
            bail!("reportable change is out of range for this attribute");
        }
        let write = ReportingWrite {
            configuration: config,
            keep_on_interview: request.configuration.is_some() || initial.is_some(),
        };
        let pending = !device.rx_on_when_idle;
        if pending {
            self.pending_writes
                .lock()
                .unwrap()
                .entry(ieee.to_owned())
                .or_default()
                .insert(
                    format!("{REPORTING_WRITE_PREFIX}{}", feature.key),
                    serde_json::to_value(write)?,
                );
            self.dirty.notify_one();
            self.emit_device(ieee);
            self.spawn_eager_writes(ieee.to_owned());
        } else {
            self.write_reporting(ieee, feature.key, write, None).await?;
        }
        Ok(ReportingWriteResult {
            configuration: config,
            pending,
        })
    }

    pub(super) async fn write_pending_reporting(
        &self,
        ieee: &str,
        key: &str,
        value: &Json,
        wait: Option<Duration>,
    ) -> Result<()> {
        let write = serde_json::from_value(value.clone())
            .context("invalid queued reporting configuration")?;
        self.write_reporting(ieee, key, write, wait).await
    }

    async fn write_reporting(
        &self,
        ieee: &str,
        key: &str,
        write: ReportingWrite,
        wait: Option<Duration>,
    ) -> Result<()> {
        let _guard = self.reporting_write.lock().await;
        let (device, ep, feature) = self.reporting_feature(ieee, key)?;
        let Source::Attr {
            cluster,
            attr,
            ty,
            manufacturer,
            ..
        } = feature.source
        else {
            unreachable!()
        };
        let config = write.configuration;
        let payload = zcl::configure_reporting_payload(&[ReportConfig {
            attribute: attr,
            ty,
            min_interval: config.minimum_report_interval,
            max_interval: config.maximum_report_interval,
            reportable_change: config.reportable_change,
        }]);
        let (header, body) = self
            .zcl_request_within(
                device.nwk,
                ep,
                cluster,
                0,
                manufacturer,
                zcl::cmd::CONFIGURE_REPORTING,
                &payload,
                wait,
            )
            .await?;
        if header.command != zcl::cmd::CONFIGURE_REPORTING_RSP || !zcl::all_success(&body) {
            return Err(ReportingRejected(format!(
                "device rejected Configure Reporting (0x{:02x}, {body:02x?})",
                header.command
            ))
            .into());
        }
        self.update_device(ieee, |d| {
            d.unavailable_reporting.remove(key);
            d.active_reporting.insert(key.to_owned(), config);
            if write.keep_on_interview {
                d.reporting.insert(key.to_owned(), config);
            } else {
                d.reporting.remove(key);
            }
        });
        Ok(())
    }

    fn reporting_feature(&self, ieee: &str, key: &str) -> Result<(Device, u8, &'static Feature)> {
        let device = self.device(ieee).context("device not found")?;
        let feature = devices::features(&device)
            .into_iter()
            .find(|f| f.key == key)
            .context("unknown reporting attribute")?;
        let Source::Attr {
            cluster,
            report: Some(_),
            ..
        } = feature.source
        else {
            bail!("attribute does not support configurable reporting");
        };
        if uses_polling(&device, cluster) {
            bail!("this measurement uses periodic reads");
        }
        let ep = device
            .endpoint_with(cluster)
            .context("the device does not have this cluster")?;
        Ok((device, ep, feature))
    }

    pub async fn mqtt_reporting(
        self: &Arc<Self>,
        operation: &str,
        mut payload: Json,
    ) -> Result<Json> {
        let id = payload["id"].as_str().context("id required")?;
        let device = self
            .data
            .lock()
            .unwrap()
            .devices
            .values()
            .find(|d| {
                d.name == id || d.ieee == id || format!("0x{}", d.ieee.replace(':', "")) == id
            })
            .cloned()
            .context("device not found")?;
        if operation == "options" {
            let options = payload["options"]
                .as_object()
                .context("options required")?
                .clone();
            self.set_device_options(&device.ieee, options)?;
            let updated = self.device(&device.ieee).context("device not found")?;
            return Ok(
                json!({"id": id, "from": device.options, "to": updated.options, "restart_required": false}),
            );
        }
        let manufacturer = if operation == "read" {
            payload["manufacturer_code"].as_u64()
        } else {
            payload["options"]["manufacturerCode"].as_u64()
        };
        let find = |attribute: &Json| -> Result<&'static Feature> {
            devices::features(&device)
                .into_iter()
                .find(|f| {
                    let Source::Attr {
                        cluster,
                        attr,
                        report: Some(_),
                        manufacturer: code,
                        ..
                    } = f.source
                    else {
                        return false;
                    };
                    let (cluster_name, attribute_name) = reporting_names(cluster, attr);
                    let matches = |value: &Json, number, name: &str| {
                        value.as_u64() == Some(number)
                            || (!name.is_empty() && value.as_str() == Some(name))
                    };
                    matches(&payload["cluster"], u64::from(cluster), cluster_name)
                        && matches(attribute, u64::from(attr), attribute_name)
                        && manufacturer == code.map(u64::from)
                        && payload.get("endpoint").is_none_or(|endpoint| {
                            endpoint.as_u64() == device.endpoint_with(cluster).map(u64::from)
                        })
                })
                .context("unknown reporting attribute or endpoint")
        };
        if operation == "read" {
            let entries = payload["configs"]
                .as_array()
                .filter(|a| !a.is_empty() && a.len() <= 16)
                .context("configs must contain 1 to 16 attributes")?;
            let mut configs = Vec::with_capacity(entries.len());
            for entry in entries {
                let attribute = &entry["attribute"];
                let feature = find(attribute)?;
                if entry
                    .get("direction")
                    .is_some_and(|direction| direction.as_u64() != Some(0))
                {
                    bail!("only reporting from the device can be read");
                }
                configs.push(
                    self.read_reporting_response(&device.ieee, feature.key, None)
                        .await?,
                );
            }
            payload["configs"] = json!(configs);
        } else {
            let feature = find(&payload["attribute"])?;
            let configuration = serde_json::from_value(payload.clone())
                .context("invalid reporting configuration")?;
            let result = self
                .configure_reporting(
                    &device.ieee,
                    ReportingRequest {
                        key: feature.key.into(),
                        configuration: Some(configuration),
                    },
                )
                .await?;
            if result.pending {
                payload["pending"] = json!(true);
            }
        }
        payload
            .as_object_mut()
            .context("expected object")?
            .remove("transaction");
        Ok(payload)
    }
}

fn reporting_names(cluster: u16, attribute: u16) -> (&'static str, &'static str) {
    match (cluster, attribute) {
        (cluster::TEMPERATURE, 0) => ("msTemperatureMeasurement", "measuredValue"),
        (cluster::HUMIDITY, 0) => ("msRelativeHumidity", "measuredValue"),
        (cluster::POWER_CONFIG, 0x21) => ("genPowerCfg", "batteryPercentageRemaining"),
        (cluster::ON_OFF, 0) => ("genOnOff", "onOff"),
        (cluster::LEVEL, 0) => ("genLevelCtrl", "currentLevel"),
        (cluster::ELECTRICAL_MEASUREMENT, 0x0505) => ("haElectricalMeasurement", "rmsVoltage"),
        (cluster::ELECTRICAL_MEASUREMENT, 0x0508) => ("haElectricalMeasurement", "rmsCurrent"),
        (cluster::ELECTRICAL_MEASUREMENT, 0x050B) => ("haElectricalMeasurement", "activePower"),
        (cluster::METERING, 0) => ("seMetering", "currentSummDelivered"),
        _ => ("", ""),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reporting_fields_contain_values_already_known_to_the_hub() {
        let mut device = Device::new("00:00:00:00:00:00:00:01".into(), 1);
        device.model = Some("SNZB-02P".into());
        device.endpoints.push(store::Endpoint {
            id: 1,
            in_clusters: vec![cluster::TEMPERATURE],
            ..Default::default()
        });
        let config = ReportingIntervals {
            minimum_report_interval: 30,
            maximum_report_interval: 1800,
            reportable_change: 20,
        };
        device.active_reporting.insert("temperature".into(), config);
        let features: Vec<_> = devices::features(&device).into_iter().copied().collect();
        let fields = reporting_fields(&device, &features, None);
        assert_eq!(fields.len(), 1);
        assert_eq!(fields[0]["key"], "temperature");
        assert_eq!(fields[0]["scale"], 100.0);
        assert_eq!(fields[0]["configuration"], json!(config));
    }

    #[test]
    fn startup_queues_each_missing_read_for_paired_sleepy_devices() {
        let ieee = "00:00:00:00:00:00:00:01";
        let mut device = Device::new(ieee.into(), 1);
        device.model = Some("SNZB-02P".into());
        device.endpoints.push(store::Endpoint {
            id: 1,
            in_clusters: vec![
                cluster::TEMPERATURE,
                cluster::HUMIDITY,
                cluster::POWER_CONFIG,
            ],
            ..Default::default()
        });
        device.rx_on_when_idle = false;
        let mut pending = HashMap::new();
        assert!(!queue_missing_reporting_reads(&device, &mut pending));
        device.interviewed = true;
        assert!(queue_missing_reporting_reads(&device, &mut pending));
        assert_eq!(pending[ieee].len(), 3);
        assert!(!queue_missing_reporting_reads(&device, &mut pending));

        pending.clear();
        device.active_reporting.insert(
            "temperature".into(),
            ReportingIntervals {
                minimum_report_interval: 5,
                maximum_report_interval: 3600,
                reportable_change: 50,
            },
        );
        device.unavailable_reporting.insert("battery".into());
        assert!(queue_missing_reporting_reads(&device, &mut pending));
        assert_eq!(
            pending[ieee],
            BTreeMap::from([(format!("{REPORTING_READ_PREFIX}humidity"), Json::Null)])
        );
    }

    #[test]
    fn queued_reporting_survives_reload_and_an_older_acknowledgement() {
        let ieee = "00:00:00:00:00:00:00:01";
        let key = format!("{REPORTING_WRITE_PREFIX}temperature");
        let read = format!("{REPORTING_READ_PREFIX}temperature");
        let mut request = ReportingWrite {
            configuration: ReportingIntervals {
                minimum_report_interval: 30,
                maximum_report_interval: 600,
                reportable_change: 50,
            },
            keep_on_interview: true,
        };
        let older = serde_json::to_value(&request).unwrap();
        request.configuration.reportable_change = 100;
        let newer = serde_json::to_value(request).unwrap();
        let snapshot = Persisted {
            pending_writes: BTreeMap::from([(
                ieee.to_owned(),
                BTreeMap::from([(key.clone(), newer.clone()), (read.clone(), Json::Null)]),
            )]),
            ..Default::default()
        };
        let restored: Persisted =
            serde_json::from_slice(&serde_json::to_vec(&snapshot).unwrap()).unwrap();
        let mut pending = restored.pending_writes.into_iter().collect();
        settle_pending(&mut pending, ieee, &key, &older);
        assert_eq!(pending[ieee][&key], newer);
        settle_pending(&mut pending, ieee, &key, &newer);
        assert_eq!(pending[ieee].get(&read), Some(&Json::Null));
        settle_pending(&mut pending, ieee, &read, &Json::Null);
        assert!(!pending.contains_key(ieee));
    }

    #[test]
    fn ts011f_polling_depends_on_the_device_fingerprint() {
        let mut device = Device::new("00:00:00:00:00:00:00:01".into(), 1);
        device.model = Some("TS011F".into());
        device.manufacturer = Some("_TZ3210_5ct6e7ye".into());
        device.endpoints.push(store::Endpoint {
            id: 1,
            in_clusters: vec![cluster::ELECTRICAL_MEASUREMENT, cluster::METERING],
            ..Default::default()
        });
        assert_eq!(poll_interval(&device), None);
        device.application_version = Some(64);
        assert_eq!(poll_interval(&device), Some(Duration::from_secs(60)));
        assert!(uses_polling(&device, cluster::ELECTRICAL_MEASUREMENT));
        assert!(!uses_polling(&device, cluster::METERING));
        device.options.insert(POLL_OPTION.into(), json!(30));
        assert_eq!(poll_interval(&device), Some(Duration::from_secs(30)));
        device.options.insert(POLL_OPTION.into(), json!(-1));
        assert_eq!(poll_interval(&device), None);
        assert!(uses_polling(&device, cluster::ELECTRICAL_MEASUREMENT));
        device.application_version = Some(100);
        assert!(uses_polling(&device, cluster::METERING));
        device.manufacturer = Some("_TZ3210_iooniers".into());
        assert!(!uses_polling(&device, cluster::ELECTRICAL_MEASUREMENT));
    }
}
