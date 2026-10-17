//! mDNS for Matter through the router's own Avahi daemon. Keenetic runs `avahi-daemon`
//! on port 5353 without D-Bus, so services are published as static service files in its
//! service directory, which it watches. The files are rewritten whenever the Matter stack
//! changes its services and checked periodically, since the OS recreates the directory
//! (it lives in tmpfs) on reboot.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use embassy_futures::select::select;
use embassy_time::{Duration, Timer};
use rs_matter::Matter;
use rs_matter::error::Error;
use tracing::{debug, warn};

/// Where Keenetic's Avahi reads static services from
pub const SERVICE_DIR: &str = "/tmp/avahi/services";
const FILE_PREFIX: &str = "zhub-matter-";
const RECHECK: Duration = Duration::from_secs(60);

/// One service in Avahi's XML form
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Service {
    pub name: String,
    pub service_protocol: String,
    pub port: u16,
    pub subtypes: Vec<String>,
    pub txt: Vec<(String, String)>,
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

impl Service {
    pub fn xml(&self) -> String {
        let mut out = String::from(
            "<?xml version=\"1.0\" standalone=\"no\"?>\n\
             <!DOCTYPE service-group SYSTEM \"avahi-service.dtd\">\n\
             <!-- Written by zhub (Matter bridge); removed when Matter stops -->\n\
             <service-group>\n",
        );
        let _ = writeln!(out, "  <name>{}</name>", escape(&self.name));
        out.push_str("  <service>\n");
        let _ = writeln!(out, "    <type>{}</type>", escape(&self.service_protocol));
        for subtype in &self.subtypes {
            let _ = writeln!(out, "    <subtype>{}._sub.{}</subtype>", escape(subtype), escape(&self.service_protocol));
        }
        let _ = writeln!(out, "    <port>{}</port>", self.port);
        for (key, value) in &self.txt {
            let _ = writeln!(out, "    <txt-record>{}={}</txt-record>", escape(key), escape(value));
        }
        out.push_str("  </service>\n</service-group>\n");
        out
    }
}

/// The services the Matter stack currently announces
fn current_services(matter: &Matter<'_>) -> Result<Vec<Service>, Error> {
    // Collected first: building a service reads state the closure holds locked
    let mut local = Vec::new();
    matter.mdns_services(|service| {
        local.push(service);
        Ok(())
    })?;
    let mut services = Vec::new();
    let mut buf = [0u8; 512];
    for service in local {
        let (s, _) = service.service(matter, &mut buf)?;
        services.push(Service {
            name: s.name.to_owned(),
            service_protocol: s.service_protocol.to_owned(),
            port: s.port,
            subtypes: s.service_subtypes.map(str::to_owned).collect(),
            txt: s.txt_kvs.map(|(k, v)| (k.to_owned(), v.to_owned())).collect(),
        });
    }
    Ok(services)
}

/// File name per service, stable while the service is the same
fn file_name(index: usize, service: &Service) -> String {
    let kind = if service.service_protocol.starts_with("_matterc") { "commissionable" } else { "operational" };
    format!("{FILE_PREFIX}{kind}-{index}.service")
}

/// Brings the service directory in line with `services`; returns whether anything changed
pub fn sync(dir: &Path, services: &[Service]) -> std::io::Result<bool> {
    if !dir.is_dir() {
        // Avahi not running (or not Keenetic): nothing to publish into
        return Ok(false);
    }
    let wanted: BTreeMap<String, String> =
        services.iter().enumerate().map(|(i, s)| (file_name(i, s), s.xml())).collect();
    let mut changed = false;
    for entry in std::fs::read_dir(dir)? {
        let name = entry?.file_name().to_string_lossy().into_owned();
        if name.starts_with(FILE_PREFIX) && !wanted.contains_key(&name) {
            std::fs::remove_file(dir.join(&name))?;
            changed = true;
        }
    }
    for (name, xml) in &wanted {
        let path = dir.join(name);
        if std::fs::read_to_string(&path).ok().as_deref() != Some(xml.as_str()) {
            // Avahi picks up a complete file only
            let tmp: PathBuf = dir.join(format!(".{name}.tmp"));
            std::fs::write(&tmp, xml)?;
            std::fs::rename(&tmp, &path)?;
            changed = true;
        }
    }
    Ok(changed)
}

/// Removes every service this module published
pub fn clear(dir: &Path) {
    if let Err(e) = sync(dir, &[]) {
        warn!("Matter mDNS services not removed: {e}");
    }
}

/// Keeps Avahi's service files in line with the Matter stack until dropped
pub async fn run(matter: &Matter<'_>) -> Result<(), Error> {
    let dir = Path::new(SERVICE_DIR);
    loop {
        let services = current_services(matter)?;
        match sync(dir, &services) {
            Ok(true) => debug!("Matter mDNS services: {:?}", services.iter().map(|s| &s.name).collect::<Vec<_>>()),
            Ok(false) => {}
            Err(e) => warn!("Matter mDNS services not written: {e}"),
        }
        select(matter.transport().wait_mdns(), Timer::after(RECHECK)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn commissionable() -> Service {
        Service {
            name: "1A2B3C4D5E6F7A8B".into(),
            service_protocol: "_matterc._udp".into(),
            port: 5540,
            subtypes: vec!["_L3840".into(), "_S15".into(), "_CM".into()],
            txt: vec![("D".into(), "3840".into()), ("CM".into(), "1".into()), ("DN".into(), "zhub <bridge>".into())],
        }
    }

    #[test]
    fn writes_avahi_service_xml() {
        let xml = commissionable().xml();
        assert!(xml.contains("<type>_matterc._udp</type>"));
        assert!(xml.contains("<subtype>_L3840._sub._matterc._udp</subtype>"));
        assert!(xml.contains("<port>5540</port>"));
        assert!(xml.contains("<txt-record>DN=zhub &lt;bridge&gt;</txt-record>"));
    }

    #[test]
    fn sync_adds_updates_and_removes_only_its_files() {
        let dir = std::env::temp_dir().join(format!("zhub-avahi-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("Http.service"), "keep").unwrap();
        let service = commissionable();
        assert!(sync(&dir, std::slice::from_ref(&service)).unwrap());
        assert!(!sync(&dir, std::slice::from_ref(&service)).unwrap(), "unchanged → no rewrite");
        assert!(dir.join("zhub-matter-commissionable-0.service").exists());
        assert!(sync(&dir, &[]).unwrap());
        let names: Vec<_> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(names, ["Http.service"]);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
