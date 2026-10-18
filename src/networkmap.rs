//! Network map, as Zigbee2MQTT's: every router (and the hub) is asked for its neighbor
//! table (Mgmt_Lqi) and, optionally, its routing table (Mgmt_Rtg); the answers become
//! nodes and links in Zigbee2MQTT's `raw` format, also rendered as Graphviz or PlantUML.

use anyhow::{Result, bail};
use serde::Serialize;

pub const FORMATS: &[&str] = &["raw", "graphviz", "plantuml"];

/// Zigbee2MQTT's `bridge/response/networkmap` data: the map in the asked format
pub fn render(map: &NetworkMap, kind: &str, routes: bool, now_ms: u64) -> Result<serde_json::Value> {
    let value = match kind {
        "raw" => serde_json::to_value(map)?,
        "graphviz" => serde_json::Value::String(graphviz(map, now_ms)),
        "plantuml" => serde_json::Value::String(plantuml(map, now_ms)),
        other => bail!("Type '{other}' not supported, allowed are: {}", FORMATS.join(",")),
    };
    Ok(serde_json::json!({ "type": kind, "routes": routes, "value": value }))
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

/// One neighbor table record (Zigbee spec 2.4.4.3.2), in zigbee-herdsman's names
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Neighbor {
    pub eui64: String,
    pub nwk_address: u16,
    /// 0 coordinator, 1 router, 2 end device
    pub device_type: u8,
    pub rx_on_when_idle: u8,
    /// 0 parent, 1 child, 2 sibling, 3 none of these, 4 previous child
    pub relationship: u8,
    pub permit_joining: u8,
    pub depth: u8,
    pub lqi: u8,
}

/// One routing table record (Zigbee spec 2.4.4.3.3)
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Route {
    pub destination_address: u16,
    pub status: &'static str,
    pub next_hop: u16,
}

const ROUTE_STATUS: [&str; 8] = [
    "ACTIVE",
    "DISCOVERY_UNDERWAY",
    "DISCOVERY_FAILED",
    "INACTIVE",
    "VALIDATION_UNDERWAY",
    "RESERVED",
    "RESERVED",
    "RESERVED",
];

pub fn route_status(code: u8) -> &'static str {
    ROUTE_STATUS[usize::from(code & 0x07)]
}

/// "00:17:88:01:0b:1d:bc:29" → "0x001788010b1dbc29", as Zigbee2MQTT writes addresses
pub fn eui64_hex(ieee: &str) -> String {
    format!("0x{}", ieee.replace(':', ""))
}

fn eui64_from_le(bytes: &[u8]) -> String {
    let mut out = String::from("0x");
    for b in bytes.iter().rev() {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

fn header(payload: &[u8], what: &str) -> Result<(u8, u8, usize)> {
    let [status, total, start, count, ..] = payload else {
        bail!("{what} response too short");
    };
    if *status != 0 {
        bail!("{what} refused (status 0x{status:02x})");
    }
    Ok((*total, *start, usize::from(*count)))
}

/// Mgmt_Lqi_rsp without the TSN: (total entries, entries in this page)
pub fn parse_lqi_rsp(payload: &[u8]) -> Result<(u8, Vec<Neighbor>)> {
    let (total, _start, count) = header(payload, "neighbor table")?;
    let records = &payload[4..];
    if records.len() < count * 22 {
        bail!("neighbor table response truncated");
    }
    let neighbors = records
        .chunks_exact(22)
        .take(count)
        .map(|r| Neighbor {
            eui64: eui64_from_le(&r[8..16]),
            nwk_address: u16::from_le_bytes([r[16], r[17]]),
            device_type: r[18] & 0x03,
            rx_on_when_idle: (r[18] >> 2) & 0x03,
            relationship: (r[18] >> 4) & 0x07,
            permit_joining: r[19] & 0x03,
            depth: r[20],
            lqi: r[21],
        })
        .collect();
    Ok((total, neighbors))
}

/// Mgmt_Rtg_rsp without the TSN: (total entries, entries in this page)
pub fn parse_rtg_rsp(payload: &[u8]) -> Result<(u8, Vec<Route>)> {
    let (total, _start, count) = header(payload, "routing table")?;
    let records = &payload[4..];
    if records.len() < count * 5 {
        bail!("routing table response truncated");
    }
    let routes = records
        .chunks_exact(5)
        .take(count)
        .map(|r| Route {
            destination_address: u16::from_le_bytes([r[0], r[1]]),
            status: route_status(r[2]),
            next_hop: u16::from_le_bytes([r[3], r[4]]),
        })
        .collect();
    Ok((total, routes))
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Definition {
    pub model: String,
    pub vendor: String,
    pub description: String,
    pub supports: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Node {
    pub ieee_addr: String,
    pub friendly_name: String,
    /// Coordinator, Router or EndDevice
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub network_address: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub manufacturer_name: Option<String>,
    #[serde(rename = "modelID", skip_serializing_if = "Option::is_none")]
    pub model_id: Option<String>,
    /// What could not be read from this device ("lqi", "routingTable")
    pub failed: Vec<&'static str>,
    /// Milliseconds since 1970
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_seen: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub definition: Option<Definition>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Address {
    pub ieee_addr: String,
    pub network_address: u16,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Link {
    pub source: Address,
    pub target: Address,
    pub device_type: u8,
    pub rx_on_when_idle: u8,
    pub relationship: u8,
    pub permit_joining: u8,
    pub depth: u8,
    pub lqi: u8,
    /// The target's routes whose next hop is the source
    pub routes: Vec<Route>,
    // Zigbee2MQTT's deprecated names, kept for tools that still read them
    pub source_ieee_addr: String,
    pub target_ieee_addr: String,
    pub source_nwk_addr: u16,
    pub linkquality: u8,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct NetworkMap {
    pub nodes: Vec<Node>,
    pub links: Vec<Link>,
}

/// What one device reported
pub struct Scanned {
    pub target: Address,
    pub neighbors: Vec<Neighbor>,
    pub routes: Vec<Route>,
}

/// Zigbee2MQTT's links: one per active neighbor (relationship 0-3), from the neighbor to
/// the device that reported it
pub fn links(scans: &[Scanned], nodes: &[Node]) -> Vec<Link> {
    let mut links = Vec::new();
    for scan in scans {
        for n in scan.neighbors.iter().filter(|n| n.relationship <= 3) {
            // Some devices report an all-zero address: take it from the network address
            let eui64 = if n.eui64 == "0x0000000000000000" {
                nodes.iter().find(|d| d.network_address == n.nwk_address).map_or(n.eui64.clone(), |d| d.ieee_addr.clone())
            } else {
                n.eui64.clone()
            };
            links.push(Link {
                source: Address {
                    ieee_addr: eui64.clone(),
                    network_address: n.nwk_address,
                },
                target: scan.target.clone(),
                device_type: n.device_type,
                rx_on_when_idle: n.rx_on_when_idle,
                relationship: n.relationship,
                permit_joining: n.permit_joining,
                depth: n.depth,
                lqi: n.lqi,
                routes: scan.routes.iter().filter(|r| r.next_hop == n.nwk_address).cloned().collect(),
                source_ieee_addr: eui64,
                target_ieee_addr: scan.target.ieee_addr.clone(),
                source_nwk_addr: n.nwk_address,
                linkquality: n.lqi,
            });
        }
    }
    links
}

/// Zigbee2MQTT's relative last-seen text
fn relative(last_seen: Option<u64>, now_ms: u64) -> String {
    let Some(seen) = last_seen else {
        return "unknown".into();
    };
    let s = now_ms.saturating_sub(seen) / 1000;
    match s {
        0..45 => "a few seconds ago".into(),
        45..90 => "a minute ago".into(),
        90..2700 => format!("{} minutes ago", s.div_ceil(60)),
        2700..5400 => "an hour ago".into(),
        5400..79200 => format!("{} hours ago", (s + 1800) / 3600),
        79200..129600 => "a day ago".into(),
        _ => format!("{} days ago", (s + 43200) / 86400),
    }
}

fn hex16(nwk: u16) -> String {
    format!("0x{nwk:04x}")
}

fn node_labels(node: &Node, now_ms: u64) -> Vec<String> {
    let failed = if node.failed.is_empty() { String::new() } else { format!("failed: {}", node.failed.join(",")) };
    let mut labels = vec![node.friendly_name.clone(), format!("{} ({}){failed}", node.ieee_addr, hex16(node.network_address))];
    if node.kind != "Coordinator" {
        let d = node.definition.as_ref();
        labels.push(format!(
            "{} {} ({})",
            d.map_or("undefined", |d| &d.vendor),
            d.map_or("undefined", |d| &d.description),
            d.map_or("undefined", |d| &d.model)
        ));
    }
    labels.push(relative(if node.kind == "Coordinator" { Some(now_ms) } else { node.last_seen }, now_ms));
    labels
}

/// Zigbee2MQTT's Graphviz output with its default colors
pub fn graphviz(map: &NetworkMap, now_ms: u64) -> String {
    let mut text = String::from("digraph G {\nnode[shape=record];\n");
    for node in &map.nodes {
        let labels: Vec<String> = node_labels(node, now_ms).iter().map(|l| l.replace('\\', "\\\\").replace('"', "\\\"")).collect();
        let style = match node.kind {
            "Coordinator" => r##"style="bold, filled", fillcolor="#e04e5d", fontcolor="#ffffff""##,
            "Router" => r##"style="rounded, filled", fillcolor="#4ea3e0", fontcolor="#ffffff""##,
            _ => r##"style="rounded, dashed, filled", fillcolor="#fff8ce", fontcolor="#000000""##,
        };
        text.push_str(&format!("  \"{}\" [{style}, label=\"{{{}}}\"];\n", node.ieee_addr, labels.join("|")));
        for link in map.links.iter().filter(|l| l.source.ieee_addr == node.ieee_addr) {
            let line_style = if node.kind == "EndDevice" {
                "penwidth=1, "
            } else if link.routes.is_empty() {
                "penwidth=0.5, "
            } else {
                "penwidth=2, "
            };
            let weight = if link.routes.is_empty() { r##"weight=0, color="#994444", "## } else { r##"weight=1, color="#009900", "## };
            let label = if link.routes.is_empty() {
                format!("label=\"{}\"", link.linkquality)
            } else {
                let routes: Vec<String> = link.routes.iter().map(|r| hex16(r.destination_address)).collect();
                format!("label=\"{} (routes: {})\"", link.linkquality, routes.join(","))
            };
            text.push_str(&format!("  \"{}\" -> \"{}\" [{line_style}{weight}{label}]\n", node.ieee_addr, link.target.ieee_addr));
        }
    }
    text.push('}');
    text.replace('\0', "")
}

/// Zigbee2MQTT's PlantUML output
pub fn plantuml(map: &NetworkMap, now_ms: u64) -> String {
    let mut text = vec!["' paste into: https://www.planttext.com/".to_owned(), String::new(), "@startuml".to_owned()];
    let mut nodes: Vec<&Node> = map.nodes.iter().collect();
    nodes.sort_by(|a, b| a.friendly_name.cmp(&b.friendly_name));
    for node in nodes {
        let labels = node_labels(node, now_ms);
        text.push(format!("card {} [", node.ieee_addr));
        text.push(labels[0].clone());
        text.push("---".into());
        text.push(labels[1].clone().replace(")failed", ") failed"));
        if node.kind != "Coordinator" {
            text.push("---".into());
            text.push(labels[2].clone());
        }
        text.push("---".into());
        text.push(labels.last().cloned().unwrap_or_default());
        text.push("]".into());
        text.push(String::new());
    }
    for link in &map.links {
        text.push(format!("{} --> {}: {}", link.source_ieee_addr, link.target_ieee_addr, link.lqi));
    }
    text.push(String::new());
    text.push("@enduml".into());
    text.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_neighbor_table_page() {
        // status, 3 entries, start 0, 1 in this page; then one 22-byte record
        let mut rsp = vec![0x00, 3, 0, 1];
        rsp.extend([0xb7, 0x4a, 0x00, 0xc7, 0xa9, 0x26, 0x68, 0xca]); // extended PAN id
        rsp.extend([0x29, 0xbc, 0x1d, 0x0b, 0x01, 0x88, 0x17, 0x00]); // 00:17:88:01:0b:1d:bc:29
        rsp.extend([0x97, 0xb7]); // nwk 0xb797
        rsp.push(0b0010_0101); // router, rx on when idle, sibling
        rsp.push(0x02); // permit joining unknown
        rsp.push(2); // depth
        rsp.push(180); // lqi
        let (total, page) = parse_lqi_rsp(&rsp).unwrap();
        assert_eq!(total, 3);
        assert_eq!(
            page,
            [Neighbor {
                eui64: "0x001788010b1dbc29".into(),
                nwk_address: 0xb797,
                device_type: 1,
                rx_on_when_idle: 1,
                relationship: 2,
                permit_joining: 2,
                depth: 2,
                lqi: 180,
            }]
        );
        assert!(parse_lqi_rsp(&[0x84, 0, 0, 0]).is_err());
        assert!(parse_lqi_rsp(&[0, 1, 0, 1, 0]).is_err());
    }

    #[test]
    fn parses_a_routing_table_page() {
        let rsp = [0x00, 2, 0, 2, 0x4a, 0x16, 0x00, 0x4a, 0x16, 0x5b, 0x6b, 0x03, 0x97, 0xb7];
        let (total, routes) = parse_rtg_rsp(&rsp).unwrap();
        assert_eq!(total, 2);
        assert_eq!(routes[0], Route { destination_address: 0x164a, status: "ACTIVE", next_hop: 0x164a });
        assert_eq!(routes[1], Route { destination_address: 0x6b5b, status: "INACTIVE", next_hop: 0xb797 });
    }

    fn node(ieee: &str, nwk: u16, kind: &'static str) -> Node {
        Node {
            ieee_addr: ieee.into(),
            friendly_name: format!("n{nwk:x}"),
            kind,
            network_address: nwk,
            manufacturer_name: None,
            model_id: None,
            failed: vec![],
            last_seen: Some(1_000_000),
            definition: None,
        }
    }

    #[test]
    fn links_follow_zigbee2mqtt() {
        let hub = node("0x0000000000000001", 0, "Coordinator");
        let lamp = node("0x0000000000000002", 0x1234, "Router");
        let scans = [Scanned {
            target: Address { ieee_addr: hub.ieee_addr.clone(), network_address: 0 },
            neighbors: vec![
                // A zero address resolves through the network address
                Neighbor { eui64: "0x0000000000000000".into(), nwk_address: 0x1234, device_type: 1, rx_on_when_idle: 1, relationship: 2, permit_joining: 0, depth: 1, lqi: 200 },
                // Previous children are left out
                Neighbor { eui64: "0x0000000000000003".into(), nwk_address: 0x5555, device_type: 2, rx_on_when_idle: 0, relationship: 4, permit_joining: 0, depth: 1, lqi: 90 },
            ],
            routes: vec![Route { destination_address: 0x7777, status: "ACTIVE", next_hop: 0x1234 }],
        }];
        let nodes = [hub, lamp];
        let links = links(&scans, &nodes);
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].source.ieee_addr, "0x0000000000000002");
        assert_eq!(links[0].routes.len(), 1);
        let map = NetworkMap { nodes: nodes.to_vec(), links };
        let dot = graphviz(&map, 1_000_000);
        assert!(dot.starts_with("digraph G {"));
        assert!(dot.contains(r##""0x0000000000000002" -> "0x0000000000000001" [penwidth=2, weight=1, color="#009900", label="200 (routes: 0x7777)"]"##));
        let uml = plantuml(&map, 1_000_000);
        assert!(uml.contains("0x0000000000000002 --> 0x0000000000000001: 200"));
        let raw = serde_json::to_value(&map).unwrap();
        assert_eq!(raw["nodes"][0]["type"], "Coordinator");
        assert_eq!(raw["links"][0]["linkquality"], 200);
    }
}
