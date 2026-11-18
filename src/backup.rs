//! Portable copies of zhub's Zigbee state and optional Matter records.
//! Restore runs before the radio and the Matter persistence tasks start.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions, Permissions};
use std::io::{Cursor, ErrorKind, Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};
use zip::{CompressionMethod, ZipArchive, ZipWriter, write::SimpleFileOptions};

use crate::store::{self, Persisted};

pub const MAX_ARCHIVE_BYTES: usize = 2 * 1024 * 1024;
const MAX_ENTRIES: usize = 128;
const MANIFEST: &str = "manifest.json";
const STATE: &str = "state.json";
const ARCHIVE: &str = "restore.zip";
const ROLLBACK: &str = "before-restore.zip";
const PENDING: &str = ".restore-pending";
const PREVIOUS: &str = ".last-restore";
// zigpy uses the same reserve when the previous host's counter is unavailable.
const RESTORE_COUNTER_INCREMENT: u32 = 10_000;

#[derive(Deserialize, Serialize)]
struct Manifest {
    format: String,
    version: u8,
    created_at: u64,
}

pub struct Backup {
    manifest: Manifest,
    state: Persisted,
    matter: BTreeMap<String, Vec<u8>>,
}

#[derive(Serialize)]
pub struct BackupSummary {
    created_at: u64,
    devices: usize,
    groups: usize,
    scenes: usize,
    includes_matter: bool,
}

impl Backup {
    fn snapshot(state: Persisted, data_dir: &Path) -> Result<Self> {
        let matter_dir = data_dir.join("matter");
        let matter = read_matter(&matter_dir)?;
        ensure!(
            matter == read_matter(&matter_dir)?,
            "Matter records changed while creating the backup"
        );
        Ok(Self {
            manifest: Manifest {
                format: "zhub".into(),
                version: 1,
                created_at: SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs(),
            },
            state,
            matter,
        })
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        let backup = Self::decode(bytes)?;
        backup.validate_state()?;
        Ok(backup)
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        ensure!(bytes.len() <= MAX_ARCHIVE_BYTES, "backup is too large");
        let mut zip = ZipArchive::new(Cursor::new(bytes)).context("invalid backup ZIP")?;
        ensure!(zip.len() <= MAX_ENTRIES, "too many entries in backup");
        let mut remaining = MAX_ARCHIVE_BYTES;
        let mut manifest = None;
        let mut state = None;
        let mut matter = BTreeMap::new();
        for index in 0..zip.len() {
            let mut entry = zip.by_index(index)?;
            let name = entry.name()?.into_owned();
            ensure!(
                entry.is_file() && !entry.is_symlink(),
                "backup entry must be a regular file"
            );
            ensure!(entry.size() <= remaining as u64, "backup is too large");
            let mut data = Vec::new();
            (&mut entry)
                .take(remaining as u64 + 1)
                .read_to_end(&mut data)?;
            remaining = remaining
                .checked_sub(data.len())
                .context("backup is too large")?;
            match name.as_str() {
                MANIFEST if manifest.is_none() => {
                    manifest = Some(serde_json::from_slice::<Manifest>(&data)?)
                }
                STATE if state.is_none() => {
                    state = Some(serde_json::from_slice::<Persisted>(&data)?)
                }
                _ => {
                    let key = name
                        .strip_prefix("matter/")
                        .filter(|key| is_matter_key(key))
                        .context("unexpected or duplicate backup entry")?;
                    ensure!(
                        matter.insert(key.to_owned(), data).is_none(),
                        "duplicate Matter record"
                    );
                }
            }
        }
        let manifest = manifest.context("missing backup manifest")?;
        ensure!(
            manifest.format == "zhub" && manifest.version == 1,
            "unsupported backup format"
        );
        Ok(Self {
            manifest,
            state: state.context("missing Zigbee state")?,
            matter,
        })
    }

    fn validate_state(&self) -> Result<()> {
        let network = self
            .state
            .network
            .as_ref()
            .context("backup has no Zigbee network")?;
        ensure!(
            (11..=26).contains(&network.channel),
            "invalid Zigbee channel"
        );
        store::parse_eui64(&network.ieee).context("invalid coordinator address")?;
        store::parse_eui64(&network.extended_pan_id).context("invalid extended PAN ID")?;
        store::parse_key(&network.network_key).context("invalid Zigbee network key")?;
        for device in self.state.devices.values() {
            store::parse_eui64(&device.ieee).context("invalid device address")?;
        }
        for key in &self.state.link_keys {
            store::parse_eui64(&key.ieee).context("invalid link-key device address")?;
            store::parse_key(&key.key).context("invalid Zigbee link key")?;
        }
        for child in &self.state.children {
            store::parse_eui64(&child.ieee).context("invalid child device address")?;
        }
        self.state.settings.validate()?;
        Ok(())
    }

    pub fn summary(&self) -> BackupSummary {
        BackupSummary {
            created_at: self.manifest.created_at,
            devices: self.state.devices.len(),
            groups: self.state.groups.len(),
            scenes: self.state.scenes.len(),
            includes_matter: !self.matter.is_empty(),
        }
    }

    fn encode(&self) -> Result<Vec<u8>> {
        let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
        let options = SimpleFileOptions::default()
            .compression_method(CompressionMethod::Stored)
            .unix_permissions(0o600);
        zip.start_file(MANIFEST, options)?;
        zip.write_all(&serde_json::to_vec(&self.manifest)?)?;
        zip.start_file(STATE, options)?;
        zip.write_all(&serde_json::to_vec_pretty(&self.state)?)?;
        ensure!(
            self.matter.len() + 2 <= MAX_ENTRIES,
            "too many Matter records"
        );
        for (key, bytes) in &self.matter {
            zip.start_file(format!("matter/{key}"), options)?;
            zip.write_all(bytes)?;
        }
        let bytes = zip.finish()?.into_inner();
        ensure!(bytes.len() <= MAX_ARCHIVE_BYTES, "backup is too large");
        Ok(bytes)
    }
}

pub fn download(state: Persisted, data_dir: &Path) -> Result<Vec<u8>> {
    let backup = Backup::snapshot(state, data_dir)?;
    backup.validate_state()?;
    backup.encode()
}

fn is_matter_key(name: &str) -> bool {
    name.strip_prefix("k_").is_some_and(|key| {
        key.len() == 4
            && key
                .bytes()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
    })
}

fn read_matter(path: &Path) -> Result<BTreeMap<String, Vec<u8>>> {
    let entries = match fs::read_dir(path) {
        Ok(entries) => entries,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(err) => return Err(err.into()),
    };
    let mut records = BTreeMap::new();
    let mut remaining = MAX_ARCHIVE_BYTES;
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str().filter(|name| is_matter_key(name)) else {
            continue;
        };
        ensure!(
            entry.file_type()?.is_file(),
            "Matter record must be a regular file"
        );
        let mut data = Vec::new();
        File::open(entry.path())?
            .take(remaining as u64 + 1)
            .read_to_end(&mut data)?;
        remaining = remaining
            .checked_sub(data.len())
            .context("Matter records are too large")?;
        records.insert(name.to_owned(), data);
        ensure!(records.len() + 2 <= MAX_ENTRIES, "too many Matter records");
    }
    Ok(records)
}

fn private_dir(path: &Path) -> std::io::Result<()> {
    fs::DirBuilder::new().mode(0o700).create(path)
}

fn write_private(path: &Path, data: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(data)?;
    file.sync_all()?;
    Ok(())
}

fn remove_dir(path: &Path) -> Result<()> {
    match fs::remove_dir_all(path) {
        Err(err) if err.kind() != ErrorKind::NotFound => Err(err.into()),
        _ => Ok(()),
    }
}

pub fn stage_restore(data_dir: &Path, bytes: &[u8]) -> Result<()> {
    Backup::from_bytes(bytes)?;
    let pending = data_dir.join(PENDING);
    ensure!(!pending.try_exists()?, "a restore is already pending");
    let name: String = store::random_bytes::<8>()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let upload = data_dir.join(format!(".restore-upload-{name}"));
    private_dir(&upload)?;
    let result = (|| -> Result<()> {
        write_private(&upload.join(ARCHIVE), bytes)?;
        File::open(&upload)?.sync_all()?;
        let parent = File::open(data_dir)?;
        fs::rename(&upload, &pending).context("cannot schedule restore")?;
        if let Err(err) = parent.sync_all() {
            remove_dir(&pending)?;
            return Err(err.into());
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = remove_dir(&upload);
    }
    result
}

pub fn apply_pending(state_path: &Path) -> Result<()> {
    let data_dir = state_path
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let pending = data_dir.join(PENDING);
    if !pending.try_exists()? {
        return Ok(());
    }
    let uploaded = fs::read(pending.join(ARCHIVE))
        .context("cannot open staged backup")
        .and_then(|bytes| Backup::from_bytes(&bytes));
    let rollback = pending.join(ROLLBACK);
    let old = if rollback.try_exists()? {
        Backup::decode(&fs::read(&rollback)?).context("cannot open the recovery snapshot")?
    } else {
        // Nothing in the live directory has changed at this stage.
        let prepare = || -> Result<Backup> {
            uploaded
                .as_ref()
                .map_err(|err| anyhow::anyhow!("{err:#}"))?;
            let old = Backup::snapshot(Persisted::load(state_path)?, data_dir)?;
            let partial = pending.join("before-restore.tmp");
            if partial.exists() {
                fs::remove_file(&partial)?;
            }
            write_private(&partial, &old.encode()?)?;
            remove_dir(&data_dir.join(PREVIOUS))?;
            fs::rename(partial, &rollback)?;
            File::open(&pending)?.sync_all()?;
            Ok(old)
        };
        match prepare() {
            Ok(old) => old,
            Err(err) => {
                warn!("Restore canceled before changing the current state: {err:#}");
                remove_dir(&pending)?;
                return Ok(());
            }
        }
    };
    let restored = uploaded.and_then(|mut backup| {
        prepare_restore(&mut backup, &old);
        replace_state(state_path, data_dir, &pending, &backup)?;
        info!(
            "Restored backup with {} Zigbee devices and {} Matter records",
            backup.state.devices.len(),
            backup.matter.len()
        );
        Ok(())
    });
    if let Err(err) = restored {
        warn!("Backup restore failed, recovering the previous state: {err:#}");
        replace_state(state_path, data_dir, &pending, &old)
            .context("restore and recovery both failed")?;
    }
    let previous = data_dir.join(PREVIOUS);
    remove_dir(&previous)?;
    fs::rename(&pending, &previous)?;
    File::open(data_dir)?.sync_all()?;
    // Keep the previous archive for recovery. The upload and staging files can go.
    if let Err(err) = fs::remove_file(previous.join(ARCHIVE)) {
        warn!("Cannot remove the completed restore upload: {err}");
    }
    Ok(())
}

fn prepare_restore(backup: &mut Backup, old: &Backup) {
    // The live serial settings belong to the connected radio, even when restoring another hub's network.
    backup.state.radio_serial.clone_from(&old.state.radio_serial);
    let mut same_coordinator = false;
    let mut same_network = false;
    if let Some(network) = backup.state.network.as_mut() {
        if let Some(previous) = &old.state.network {
            same_coordinator =
                store::parse_eui64(&network.ieee).ok() == store::parse_eui64(&previous.ieee).ok();
            same_network = same_coordinator
                && network.extended_pan_id == previous.extended_pan_id
                && network.network_key == previous.network_key;
            if same_coordinator {
                network.nwk_frame_counter =
                    network.nwk_frame_counter.max(previous.nwk_frame_counter);
                network.aps_frame_counter =
                    network.aps_frame_counter.max(previous.aps_frame_counter);
            }
        }
        if !same_network {
            network.nwk_frame_counter = network
                .nwk_frame_counter
                .saturating_add(RESTORE_COUNTER_INCREMENT);
            network.aps_frame_counter = network
                .aps_frame_counter
                .saturating_add(RESTORE_COUNTER_INCREMENT);
        }
    }
    // Keep existing Matter identities on a Zigbee-only restore of this coordinator.
    if backup.matter.is_empty() && same_coordinator && !old.matter.is_empty() {
        backup.matter.clone_from(&old.matter);
        backup
            .state
            .matter_endpoints
            .clone_from(&old.state.matter_endpoints);
        backup.state.settings.matter = old.state.settings.matter.clone();
    }
    backup.state.settings.matter.enabled =
        old.state.settings.matter.enabled && !backup.matter.is_empty();
}

fn replace_state(
    state_path: &Path,
    data_dir: &Path,
    pending: &Path,
    backup: &Backup,
) -> Result<()> {
    let staged_state = pending.join(STATE);
    let staged_matter = pending.join("matter");
    backup.state.save(&staged_state)?;
    remove_dir(&staged_matter)?;
    private_dir(&staged_matter)?;
    for (name, bytes) in &backup.matter {
        write_private(&staged_matter.join(name), bytes)?;
    }
    File::open(&staged_matter)?.sync_all()?;
    let matter = data_dir.join("matter");
    remove_dir(&matter)?;
    if !backup.matter.is_empty() {
        fs::rename(staged_matter, &matter)?;
    }
    fs::rename(staged_state, state_path)?;
    fs::set_permissions(state_path, Permissions::from_mode(0o600))?;
    File::open(data_dir)?.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestDir(std::path::PathBuf);

    impl TestDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "zhub-backup-test-{:016x}",
                u64::from_le_bytes(store::random_bytes())
            ));
            private_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn state(counter: u32) -> Persisted {
        let mut network = store::new_network(
            &crate::store::parse_eui64("00:00:00:00:00:00:00:01").unwrap(),
            15,
            10,
        );
        network.extended_pan_id = "00:00:00:00:00:00:00:01".into();
        network.network_key = "00".repeat(16);
        network.nwk_frame_counter = counter;
        network.aps_frame_counter = counter;
        Persisted {
            network: Some(network),
            ..Default::default()
        }
    }

    #[test]
    fn backup_and_restore_work_without_matter_files() {
        let dir = TestDir::new();
        let path = dir.0.join(STATE);
        let archived = state(500);
        let zip = download(archived, &dir.0).unwrap();
        assert!(!Backup::from_bytes(&zip).unwrap().summary().includes_matter);

        state(800).save(&path).unwrap();
        stage_restore(&dir.0, &zip).unwrap();
        assert!(stage_restore(&dir.0, &zip).is_err());
        apply_pending(&path).unwrap();
        let restored = Persisted::load(&path).unwrap();
        assert!(!restored.settings.matter.enabled);
        assert_eq!(restored.network.unwrap().nwk_frame_counter, 800);
        assert!(!dir.0.join("matter").exists());
        assert!(dir.0.join(PREVIOUS).join(ROLLBACK).is_file());
        apply_pending(&path).unwrap();
    }

    #[test]
    fn matter_files_can_be_imported_into_a_disabled_installation() {
        let source = TestDir::new();
        let target = TestDir::new();
        private_dir(&source.0.join("matter")).unwrap();
        write_private(&source.0.join("matter").join("k_0001"), b"fabric record").unwrap();
        let mut source_state = state(100);
        source_state.settings.matter.enabled = true;
        source_state.matter_endpoints.insert("sensor".into(), 20);
        let zip = download(source_state, &source.0).unwrap();
        assert!(Backup::from_bytes(&zip).unwrap().summary().includes_matter);

        let path = target.0.join(STATE);
        stage_restore(&target.0, &zip).unwrap();
        apply_pending(&path).unwrap();
        let restored = Persisted::load(&path).unwrap();
        assert!(!restored.settings.matter.enabled);
        assert_eq!(restored.matter_endpoints["sensor"], 20);
        assert_eq!(restored.network.unwrap().nwk_frame_counter, 10_100);
        assert_eq!(
            fs::read(target.0.join("matter/k_0001")).unwrap(),
            b"fabric record"
        );
    }

    #[test]
    fn invalid_backup_is_rejected_before_restore() {
        let dir = TestDir::new();
        assert!(stage_restore(&dir.0, b"invalid ZIP").is_err());
        let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
        zip.start_file("matter/../../state.json", SimpleFileOptions::default())
            .unwrap();
        zip.write_all(b"unsafe path").unwrap();
        let bytes = zip.finish().unwrap().into_inner();
        assert!(stage_restore(&dir.0, &bytes).is_err());
        assert!(!dir.0.join(PENDING).exists());
    }

    #[test]
    fn damaged_upload_after_interruption_recovers_the_previous_snapshot() {
        let dir = TestDir::new();
        let path = dir.0.join(STATE);
        let old = download(state(600), &dir.0).unwrap();
        stage_restore(&dir.0, &download(state(100), &dir.0).unwrap()).unwrap();
        write_private(&dir.0.join(PENDING).join(ROLLBACK), &old).unwrap();
        fs::write(dir.0.join(PENDING).join(ARCHIVE), b"damaged").unwrap();
        state(100).save(&path).unwrap();
        apply_pending(&path).unwrap();
        assert_eq!(
            Persisted::load(&path)
                .unwrap()
                .network
                .unwrap()
                .nwk_frame_counter,
            600
        );
    }
}
