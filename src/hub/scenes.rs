//! Device-side scene operations. Management uses acknowledged unicasts so partial
//! installation is visible; recalling a group sends one Scenes multicast frame.
use super::*;
use crate::scenes::{self, Operation, StoredScene};
use serde_json::Map;

#[derive(Clone)]
struct Target {
    group: Option<u16>,
    group_id: u16,
    members: Vec<(Device, u8)>,
}
impl Target {
    fn contains(&self, s: &StoredScene) -> bool {
        s.group_id == self.group_id
            && self
                .members
                .iter()
                .any(|(d, ep)| d.ieee == s.ieee && *ep == s.ep)
    }
}
impl Hub {
    fn scene_target(&self, name: &str, ep: Option<u8>, group_id: Option<u16>) -> Result<Target> {
        let data = self.data.lock().unwrap();
        let device = data
            .devices
            .values()
            .find(|d| d.ieee == name || crate::mqtt::topic_name(&d.name) == name);
        if let Some(d) = device {
            let endpoint = d
                .endpoints
                .iter()
                .find(|e| {
                    ep.is_none_or(|ep| e.id == ep) && e.in_clusters.contains(&cluster::SCENES)
                })
                .context("device endpoint does not support Scenes")?;
            let gid = group_id.unwrap_or(0);
            if gid != 0 {
                ensure_scene(
                    data.groups.iter().any(|g| {
                        g.id == gid
                            && g.members
                                .iter()
                                .any(|m| m.ieee == d.ieee && m.ep == endpoint.id)
                    }),
                    "device endpoint is not a member of this group",
                )?;
            }
            return Ok(Target {
                group: None,
                group_id: gid,
                members: vec![(d.clone(), endpoint.id)],
            });
        }
        let g = data
            .groups
            .iter()
            .find(|g| crate::mqtt::topic_name(&g.name) == name || format!("group:{}", g.id) == name)
            .context("no such device or group")?;
        ensure_scene(ep.is_none(), "choose an endpoint only for a device")?;
        ensure_scene(
            group_id.is_none_or(|id| id == g.id),
            "group_id does not match the target group",
        )?;
        ensure_scene(!g.members.is_empty(), "group has no members")?;
        let mut members = Vec::new();
        for m in &g.members {
            let d = data
                .devices
                .get(&m.ieee)
                .context("group contains a missing device")?;
            ensure_scene(
                d.endpoints
                    .iter()
                    .any(|e| e.id == m.ep && e.in_clusters.contains(&cluster::SCENES)),
                &format!("{} endpoint {} does not support Scenes", d.name, m.ep),
            )?;
            members.push((d.clone(), m.ep));
        }
        Ok(Target {
            group: Some(g.id),
            group_id: g.id,
            members,
        })
    }
    /// Lists known, device-confirmed entries, including incomplete group installs.
    pub fn scene_list(
        &self,
        name: &str,
        ep: Option<u8>,
        group_id: Option<u16>,
    ) -> Result<Vec<Json>> {
        let target = self.scene_target(name, ep, group_id)?;
        let data = self.data.lock().unwrap();
        Ok(summaries(&target, &data.scenes))
    }
    fn scenes_changed(&self) {
        self.dirty.notify_one();
        let _ = self.events.send(Event::Scenes);
    }
    fn remember_scene(&self, entry: StoredScene) {
        let mut data = self.data.lock().unwrap();
        data.scenes.retain(|s| {
            !(s.ieee == entry.ieee
                && s.ep == entry.ep
                && s.group_id == entry.group_id
                && s.id == entry.id)
        });
        data.scenes.push(entry);
        drop(data);
        self.scenes_changed();
    }
    fn forget_scene(&self, ieee: &str, ep: u8, group: u16, id: Option<u8>) {
        self.data.lock().unwrap().scenes.retain(|s| {
            !(s.ieee == ieee && s.ep == ep && s.group_id == group && id.is_none_or(|id| id == s.id))
        });
        self.scenes_changed();
    }
    async fn scene_reply(
        &self,
        d: &Device,
        ep: u8,
        command: u8,
        payload: &[u8],
    ) -> Result<Vec<u8>> {
        let (h, body) = self
            .zcl_request(
                d.nwk,
                ep,
                cluster::SCENES,
                zcl::FC_CLUSTER_SPECIFIC,
                command,
                payload,
            )
            .await?;
        if h.command == zcl::cmd::DEFAULT_RSP {
            bail!(
                "{} rejected Scenes command 0x{command:02x} (default response {:?})",
                d.name,
                zcl::default_response_status(&body)
            );
        }
        ensure_scene(
            h.cluster_specific() && h.server_to_client() && h.command == command,
            "unexpected Scenes response command",
        )?;
        Ok(body)
    }
    async fn scene_view(
        &self,
        d: &Device,
        ep: u8,
        group: u16,
        id: u8,
    ) -> Result<(f64, String, Map<String, Json>)> {
        let payload = scenes::identity(group, id);
        match self.scene_reply(d, ep, 0x41, &payload).await {
            Ok(body) => scenes::view(&body, group, id, true),
            Err(_) => {
                let body = self.scene_reply(d, ep, 1, &payload).await?;
                scenes::view(&body, group, id, false)
            }
        }
    }
    pub async fn scene_refresh(
        self: &Arc<Self>,
        name: &str,
        ep: Option<u8>,
        group_id: Option<u16>,
    ) -> Result<Vec<Json>> {
        let _lock = self.scene_lock.lock().await;
        let target = self.scene_target(name, ep, group_id)?;
        let mut errors = Vec::new();
        for (d, ep) in &target.members {
            let result = async {
                let body = self
                    .scene_reply(d, *ep, 6, &target.group_id.to_le_bytes())
                    .await?;
                let ids = scenes::membership(&body, target.group_id)?;
                self.data.lock().unwrap().scenes.retain(|s| {
                    !(s.ieee == d.ieee
                        && s.ep == *ep
                        && s.group_id == target.group_id
                        && !ids.contains(&s.id))
                });
                self.scenes_changed();
                for id in ids {
                    let (transition, wire_name, values) =
                        self.scene_view(d, *ep, target.group_id, id).await?;
                    let old = self
                        .data
                        .lock()
                        .unwrap()
                        .scenes
                        .iter()
                        .find(|s| {
                            s.ieee == d.ieee
                                && s.ep == *ep
                                && s.group_id == target.group_id
                                && s.id == id
                        })
                        .cloned();
                    let name = old
                        .map(|s| s.name)
                        .filter(|n| !n.is_empty())
                        .unwrap_or_else(|| {
                            if wire_name.is_empty() {
                                format!("Scene {id}")
                            } else {
                                wire_name
                            }
                        });
                    self.remember_scene(StoredScene {
                        ieee: d.ieee.clone(),
                        ep: *ep,
                        group_id: target.group_id,
                        id,
                        name,
                        transition,
                        values,
                    });
                }
                Ok::<_, anyhow::Error>(())
            }
            .await;
            if let Err(e) = result {
                errors.push(format!("{}: {e:#}", d.name));
            }
        }
        ensure_scene(errors.is_empty(), &errors.join("; "))?;
        self.scene_list(name, ep, group_id)
    }
    pub async fn scene_apply(self: &Arc<Self>, name: &str, key: &str, value: &Json) -> Result<()> {
        let op = Operation::parse(key, value)?;
        let _lock = self.scene_lock.lock().await;
        let target = self.scene_target(name, op.ep, op.group_id)?;
        op.validate_scope(target.group_id)?;
        if key == "scene_rename" {
            let mut data = self.data.lock().unwrap();
            let mut count = 0;
            for s in data
                .scenes
                .iter_mut()
                .filter(|s| target.contains(s) && s.id == op.id)
            {
                s.name = op.name.clone().unwrap();
                count += 1;
            }
            ensure_scene(
                count > 0,
                "scene is not known; refresh scenes from the devices first",
            )?;
            drop(data);
            self.scenes_changed();
            return Ok(());
        }
        if key == "scene_recall" {
            let known = self
                .data
                .lock()
                .unwrap()
                .scenes
                .iter()
                .filter(|s| target.contains(s) && s.id == op.id)
                .count();
            ensure_scene(
                target.group.is_none() || known == 0 || known == target.members.len(),
                "scene is only installed on part of this group; store/add it on all members before recalling",
            )?;
            let mut payload = scenes::identity(target.group_id, op.id);
            payload.extend(0xffffu16.to_le_bytes()); // use each device's stored transition
            if let Some(id) = target.group {
                // No optimistic shared state: members can have different scene values.
                self.send_group_zcl(id, cluster::SCENES, 5, &payload, Vec::new())?;
            } else {
                let (d, ep) = &target.members[0];
                let (h, body) = self
                    .zcl_request(
                        d.nwk,
                        *ep,
                        cluster::SCENES,
                        zcl::FC_CLUSTER_SPECIFIC,
                        5,
                        &payload,
                    )
                    .await?;
                ensure_scene(
                    h.command == zcl::cmd::DEFAULT_RSP
                        && body.len() == 2
                        && body[0] == 5
                        && body[1] == 0,
                    "device did not acknowledge Recall Scene",
                )?;
            }
            let entries: Vec<_> = self
                .data
                .lock()
                .unwrap()
                .scenes
                .iter()
                .filter(|s| target.contains(s) && s.id == op.id)
                .cloned()
                .collect();
            for entry in &entries {
                self.update_device(&entry.ieee, |d| d.state.extend(entry.values.clone()));
            }
            // Known scenes use the cached per-member state plus normal reports. Only
            // unknown scenes need explicit reads; normal recalls stay one group frame.
            let unknown: Vec<_> = target
                .members
                .into_iter()
                .filter(|(d, ep)| !entries.iter().any(|s| s.ieee == d.ieee && s.ep == *ep))
                .collect();
            let hub = self.clone();
            tokio::spawn(async move {
                let seconds = entries
                    .iter()
                    .map(|s| s.transition)
                    .fold(0.0, f64::max)
                    .clamp(0.0, 65534.0);
                tokio::time::sleep(Duration::from_secs_f64(seconds + 0.5)).await;
                for (d, ep) in unknown {
                    for (cluster, attrs) in [
                        (cluster::ON_OFF, vec![0]),
                        (cluster::LEVEL, vec![0]),
                        (cluster::COLOR, vec![7]),
                    ] {
                        if d.endpoints
                            .iter()
                            .any(|e| e.id == ep && e.in_clusters.contains(&cluster))
                        {
                            if let Err(e) =
                                hub.read_attributes(d.nwk, ep, cluster, None, &attrs).await
                            {
                                debug!("scene readback {}: {e:#}", d.name);
                            }
                        }
                    }
                }
            });
            return Ok(());
        }
        // Preflight all members before any destructive replace. In particular, do not
        // save a brightness/color extension on a device lacking that cluster.
        let mut plans = Vec::new();
        for (d, ep) in &target.members {
            let mut fields = op.fields.clone();
            if key == "scene_add" {
                let clusters = &d
                    .endpoints
                    .iter()
                    .find(|e| e.id == *ep)
                    .unwrap()
                    .in_clusters;
                for (key, cluster) in [
                    ("state", 6),
                    ("brightness", 8),
                    ("color_temp", 0x0300),
                    ("color", 0x0300),
                ] {
                    ensure_scene(
                        !fields.contains_key(key) || clusters.contains(&cluster),
                        &format!("{} does not support {key}", d.name),
                    )?;
                }
                if let Some(value) = fields.get("color_temp").and_then(Json::as_f64) {
                    let range = devices::features(d)
                        .iter()
                        .find(|f| f.key == "color_temp")
                        .map(|f| f.range(d))
                        .unwrap_or((None, None));
                    fields.insert(
                        "color_temp".into(),
                        json!(value.clamp(range.0.unwrap_or(153.0), range.1.unwrap_or(500.0))),
                    );
                }
            }
            plans.push(if key == "scene_add" {
                Some(scenes::add_payload(target.group_id, op.id, &fields)?)
            } else {
                None
            });
        }
        let mut errors = Vec::new();
        for ((d, ep), plan) in target.members.iter().zip(plans) {
            let result = async {
                let old = self
                    .data
                    .lock()
                    .unwrap()
                    .scenes
                    .iter()
                    .find(|s| {
                        s.ieee == d.ieee
                            && s.ep == *ep
                            && s.group_id == target.group_id
                            && s.id == op.id
                    })
                    .cloned();
                let payload = scenes::identity(target.group_id, op.id);
                match key {
                    "scene_add" => {
                        // Devices may merge omitted attributes otherwise. Forget the old
                        // metadata immediately after confirmed removal, even if add fails.
                        let body = self.scene_reply(d, *ep, 2, &payload).await?;
                        scenes::response(&body, target.group_id, Some(op.id), true)?;
                        self.forget_scene(&d.ieee, *ep, target.group_id, Some(op.id));
                        let (command, payload, values, transition) = plan.unwrap();
                        let body = self.scene_reply(d, *ep, command, &payload).await?;
                        scenes::response(&body, target.group_id, Some(op.id), false)?;
                        self.remember_scene(StoredScene {
                            ieee: d.ieee.clone(),
                            ep: *ep,
                            group_id: target.group_id,
                            id: op.id,
                            name: op
                                .name
                                .clone()
                                .or(old.map(|s| s.name))
                                .unwrap_or_else(|| format!("Scene {}", op.id)),
                            transition,
                            values,
                        });
                    }
                    "scene_store" => {
                        let body = self.scene_reply(d, *ep, 4, &payload).await?;
                        scenes::response(&body, target.group_id, Some(op.id), false)?;
                        let values: Map<_, _> = d
                            .state
                            .iter()
                            .filter(|(k, _)| {
                                matches!(k.as_str(), "on" | "level" | "color_temp" | "color")
                            })
                            .map(|(k, v)| (k.clone(), v.clone()))
                            .collect();
                        let mut entry = StoredScene {
                            ieee: d.ieee.clone(),
                            ep: *ep,
                            group_id: target.group_id,
                            id: op.id,
                            name: op
                                .name
                                .clone()
                                .or(old.as_ref().map(|s| s.name.clone()))
                                .unwrap_or_else(|| format!("Scene {}", op.id)),
                            transition: old.map_or(0.0, |s| s.transition),
                            values,
                        };
                        // View proves the stored attributes and retained transition. A
                        // server lacking View still confirmed the Store above.
                        if let Ok((time, _, values)) =
                            self.scene_view(d, *ep, target.group_id, op.id).await
                        {
                            entry.transition = time;
                            entry.values = values;
                        }
                        self.remember_scene(entry);
                    }
                    "scene_remove" | "scene_remove_all" => {
                        let all = key == "scene_remove_all";
                        let body = self
                            .scene_reply(
                                d,
                                *ep,
                                if all { 3 } else { 2 },
                                if all { &payload[..2] } else { &payload },
                            )
                            .await?;
                        scenes::response(
                            &body,
                            target.group_id,
                            if all { None } else { Some(op.id) },
                            true,
                        )?;
                        self.forget_scene(
                            &d.ieee,
                            *ep,
                            target.group_id,
                            if all { None } else { Some(op.id) },
                        );
                    }
                    _ => bail!("unknown scene operation"),
                }
                Ok::<_, anyhow::Error>(())
            }
            .await;
            if let Err(e) = result {
                errors.push(format!("{}: {e:#}", d.name));
            }
        }
        ensure_scene(
            errors.is_empty(),
            &format!("scene operation incomplete: {}", errors.join("; ")),
        )
    }
}
fn ensure_scene(ok: bool, message: &str) -> Result<()> {
    if ok { Ok(()) } else { bail!("{message}") }
}

fn summaries(target: &Target, scenes: &[StoredScene]) -> Vec<Json> {
    let mut list: BTreeMap<u8, Vec<&StoredScene>> = BTreeMap::new();
    for scene in scenes.iter().filter(|s| target.contains(s)) {
        list.entry(scene.id).or_default().push(scene);
    }
    list.into_iter()
        .map(|(id, entries)| {
            json!({
                "id": id, "name": entries[0].name, "group_id": target.group_id,
                "installed": entries.len(), "members": target.members.len(),
                "complete": entries.len() == target.members.len(), "entries": entries
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    fn scene(ieee: &str, ep: u8, group_id: u16, level: u8) -> StoredScene {
        StoredScene {
            ieee: ieee.into(),
            ep,
            group_id,
            id: 1,
            name: "Night".into(),
            transition: 0.2,
            values: json!({"level":level}).as_object().unwrap().clone(),
        }
    }
    #[test]
    fn group_summary_reports_partial_install_and_keeps_individual_values() {
        let target = Target {
            group: Some(3),
            group_id: 3,
            members: vec![
                (Device::new("a".into(), 1), 11),
                (Device::new("b".into(), 2), 11),
            ],
        };
        let mut scenes = vec![
            scene("a", 11, 3, 38),
            scene("b", 11, 1, 204),
            scene("b", 12, 3, 99),
        ];
        let list = summaries(&target, &scenes);
        assert_eq!(list.len(), 1);
        assert_eq!(list[0]["installed"], 1);
        assert_eq!(list[0]["complete"], false);
        scenes.push(scene("b", 11, 3, 80));
        let list = summaries(&target, &scenes);
        assert_eq!(list[0]["complete"], true);
        let entries = list[0]["entries"].as_array().unwrap();
        assert_eq!(entries[0]["values"]["level"], 38);
        assert_eq!(entries[1]["values"]["level"], 80);
        // Removing/replacing a member makes the old endpoint irrelevant, not complete.
        let target = Target {
            members: vec![(Device::new("c".into(), 3), 11)],
            ..target
        };
        assert!(summaries(&target, &scenes).is_empty());
    }
    #[test]
    fn standalone_and_group_scenes_never_share_a_scope() {
        let target = Target {
            group: None,
            group_id: 0,
            members: vec![(Device::new("a".into(), 1), 11)],
        };
        assert!(target.contains(&scene("a", 11, 0, 38)));
        assert!(!target.contains(&scene("a", 11, 3, 38)));
        assert!(!target.contains(&scene("a", 12, 0, 38)));
        assert!(!target.contains(&scene("b", 11, 0, 38)));
    }
}

impl Hub {
    /// Complete scenes for the MQTT-only automation engine. No current device
    /// values are included: these are exclusively the installed scene values.
    pub fn automation_scenes(&self) -> Vec<Json> {
        let data = self.data.lock().unwrap();
        let mut result = Vec::new();
        for device in data.devices.values() {
            let Some(ep) = device
                .endpoints
                .iter()
                .find(|e| e.in_clusters.contains(&cluster::SCENES))
            else {
                continue;
            };
            for scene in data
                .scenes
                .iter()
                .filter(|s| s.ieee == device.ieee && s.ep == ep.id && s.group_id == 0)
            {
                result.push(scenes::automation_descriptor(
                    &crate::mqtt::topic_name(&device.name),
                    &[scene],
                ));
            }
        }
        for group in &data.groups {
            if group.members.is_empty() {
                continue;
            }
            let ids: std::collections::BTreeSet<_> = data
                .scenes
                .iter()
                .filter(|s| s.group_id == group.id)
                .map(|s| s.id)
                .collect();
            for id in ids {
                let entries: Vec<_> = group
                    .members
                    .iter()
                    .filter_map(|m| {
                        data.scenes.iter().find(|s| {
                            s.group_id == group.id && s.id == id && s.ieee == m.ieee && s.ep == m.ep
                        })
                    })
                    .collect();
                if entries.len() == group.members.len() {
                    result.push(scenes::automation_descriptor(
                        &crate::mqtt::topic_name(&group.name),
                        &entries,
                    ));
                }
            }
        }
        result
    }
}
