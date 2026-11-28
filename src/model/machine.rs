//! Which machine recorded a ledger node, without naming the machine.
//!
//! Ledger nodes record `origin.hostname` so handoff can tell machines apart
//! when `KNIT_ENVIRONMENT_ID` is unset. Bundle artifacts are uploaded to sync
//! remotes, so new nodes record a random label kept on this machine instead
//! of its real hostname. The real hostname is only read to recognise nodes
//! that older Knit versions recorded here; it is never written.

use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

/// File in the global Knit config directory holding this machine's random id.
const MACHINE_ID_FILE: &str = "machine-id";
/// Environment variable that names this machine explicitly.
pub const MACHINE_LABEL_ENV: &str = "KNIT_MACHINE_LABEL";

fn non_empty_env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// The label new ledger nodes record as `origin.hostname`: `KNIT_MACHINE_LABEL`
/// when set, otherwise `machine-` and the start of a random id created once and
/// kept in the global Knit config directory.
pub fn label() -> String {
    if let Some(label) = non_empty_env(MACHINE_LABEL_ENV) {
        return label;
    }
    let id = crate::store::global_config_path()
        .ok()
        .and_then(|config| config.parent().map(Path::to_path_buf))
        .and_then(|dir| stored_or_new_id(&dir));
    match id {
        Some(id) => label_from_id(&id),
        // No writable config directory: a stable pseudonym derived from the
        // hostname, so this machine still matches itself across runs.
        None => label_from_id(&hex(&Sha256::digest(
            format!("knit-machine\0{}", legacy_hostname().unwrap_or_default()).as_bytes(),
        ))),
    }
}

fn label_from_id(id: &str) -> String {
    format!("machine-{}", &id[..12])
}

fn valid_id(id: &str) -> bool {
    id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit())
}

fn read_id(path: &Path) -> Option<String> {
    let id = std::fs::read_to_string(path)
        .ok()?
        .trim()
        .to_ascii_lowercase();
    valid_id(&id).then_some(id)
}

/// Read the stored id, creating it when missing. Creation goes through a
/// private temporary file and a hard link, so two Knit processes starting at
/// once agree on the same id and nobody reads a half-written file.
fn stored_or_new_id(dir: &Path) -> Option<String> {
    let path = dir.join(MACHINE_ID_FILE);
    if let Some(id) = read_id(&path) {
        return Some(id);
    }
    std::fs::create_dir_all(dir).ok()?;
    let id = random_id();
    let temporary: PathBuf = dir.join(format!(
        ".{MACHINE_ID_FILE}.{}.{}",
        std::process::id(),
        &id[..8]
    ));
    std::fs::write(&temporary, format!("{id}\n")).ok()?;
    let linked = std::fs::hard_link(&temporary, &path);
    if linked.is_err() && !path.exists() {
        // File systems without hard links: a rename is atomic too, at the
        // cost of a last-writer-wins race on the very first run.
        let _ = std::fs::rename(&temporary, &path);
    }
    let _ = std::fs::remove_file(&temporary);
    read_id(&path)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn random_id() -> String {
    let mut bytes = [0u8; 16];
    #[cfg(unix)]
    {
        use std::io::Read;
        if std::fs::File::open("/dev/urandom")
            .and_then(|mut file| file.read_exact(&mut bytes))
            .is_ok()
        {
            return hex(&bytes);
        }
    }
    // The standard library seeds RandomState from the operating system's
    // random source; mix in the clock and process id for good measure.
    use std::hash::{BuildHasher, Hasher};
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    for chunk in bytes.chunks_mut(8) {
        let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
        hasher.write_u128(nanos);
        hasher.write_u32(std::process::id());
        chunk.copy_from_slice(&hasher.finish().to_le_bytes()[..chunk.len()]);
    }
    hex(&bytes)
}

/// The hostname older Knit versions recorded: `HOSTNAME`, else
/// `COMPUTERNAME`, else the output of `hostname`. Only used to recognise
/// nodes recorded on this machine before the label replaced it.
pub fn legacy_hostname() -> Option<String> {
    non_empty_env("HOSTNAME")
        .or_else(|| non_empty_env("COMPUTERNAME"))
        .or_else(|| {
            std::process::Command::new("hostname")
                .output()
                .ok()
                .filter(|o| o.status.success())
                .and_then(|o| String::from_utf8(o.stdout).ok())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        })
}

/// Whether a recorded origin hostname names this machine: its current label,
/// or the real hostname older Knit versions recorded here.
pub fn is_this_machine_name(recorded: &str) -> bool {
    recorded == label() || legacy_hostname().is_some_and(|hostname| hostname == recorded)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_stored_id_is_created_once_and_reused() {
        let dir = std::env::temp_dir().join(format!(
            "knit-machine-id-{}-{}",
            std::process::id(),
            random_id()
        ));
        let first = stored_or_new_id(&dir).unwrap();
        assert!(valid_id(&first));
        assert_eq!(stored_or_new_id(&dir).unwrap(), first);
        // Only the id file remains; the temporary file is cleaned up.
        let entries: Vec<_> = std::fs::read_dir(&dir).unwrap().collect();
        assert_eq!(entries.len(), 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_corrupt_id_file_is_not_trusted() {
        let dir = std::env::temp_dir().join(format!(
            "knit-machine-id-bad-{}-{}",
            std::process::id(),
            random_id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(MACHINE_ID_FILE), "not-an-id").unwrap();
        // The bad file stays (a hard link cannot replace it) and no id is
        // invented that a later run would disagree with.
        assert_eq!(stored_or_new_id(&dir), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn random_ids_differ_and_labels_do_not_contain_the_hostname() {
        let a = random_id();
        let b = random_id();
        assert!(valid_id(&a) && valid_id(&b));
        assert_ne!(a, b);
        let label = label_from_id(&a);
        assert!(label.starts_with("machine-") && label.len() == "machine-".len() + 12);
    }
}
