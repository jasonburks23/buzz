//! Persisted record of the Buzz events this bot has already shown to any of
//! its ACP sessions. buzz#29, from opeff#1197.
//!
//! Each live session keeps its own `delivered_event_ids`, and that set is
//! cleared on every session invalidation and lost on every restart, on
//! purpose: a new session has no memory, so it must receive thread context
//! again to understand a new reply. The cost was that an hours-old dispatch
//! came back as thread context looking brand new, and a bot acted on it a
//! second time. MP-CC did this three times on 2026-09-18.
//!
//! This store does not suppress anything. It only lets the prompt say which
//! context events an earlier session already saw, so the agent checks before
//! acting. Every failure here fails open to today's behavior: no store, no
//! labels, a warning in the log, and the bot keeps running.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

/// An entry older than this is forgotten.
pub const MAX_AGE_SECS: u64 = 7 * 24 * 60 * 60;
/// At most this many event IDs are kept. The oldest go first.
pub const MAX_ENTRIES: usize = 20_000;

pub type SharedDeliveredStore = Arc<Mutex<DeliveredStore>>;

#[derive(Debug, Default)]
pub struct DeliveredStore {
    /// Where the store is saved. `None` keeps it in memory only, for tests.
    path: Option<PathBuf>,
    /// Event ID, in Buzz's canonical lowercase hex, to the unix second it was
    /// first delivered.
    entries: HashMap<String, u64>,
}

impl DeliveredStore {
    /// A store that is never written to disk.
    pub fn in_memory() -> Self {
        Self::default()
    }

    /// Load the store at `path`. A missing file is an empty store. A file that
    /// cannot be read or parsed is also an empty store, with a warning; it will
    /// be overwritten on the next successful delivery.
    pub fn load(path: PathBuf, now: u64) -> Self {
        let entries = match std::fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice::<HashMap<String, u64>>(&bytes) {
                Ok(map) => map,
                Err(e) => {
                    tracing::warn!(path = %path.display(), "delivered store unreadable, starting empty: {e}");
                    HashMap::new()
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => HashMap::new(),
            Err(e) => {
                tracing::warn!(path = %path.display(), "delivered store unreadable, starting empty: {e}");
                HashMap::new()
            }
        };
        let mut store = Self { path: Some(path), entries };
        store.prune(now);
        store
    }

    /// True when some session of this bot was already shown this event.
    pub fn contains(&self, event_id: &str) -> bool {
        !event_id.is_empty() && self.entries.contains_key(event_id)
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Record events a session was shown, then prune and save. An event keeps
    /// the time it was FIRST delivered, so re-delivery does not extend its life.
    pub fn record(&mut self, event_ids: impl IntoIterator<Item = String>, now: u64) {
        let mut changed = false;
        for id in event_ids.into_iter().filter(|id| !id.is_empty()) {
            self.entries.entry(id).or_insert_with(|| {
                changed = true;
                now
            });
        }
        if changed {
            self.prune(now);
            self.save();
        }
    }

    /// Drop entries past `MAX_AGE_SECS`, then the oldest until at most
    /// `MAX_ENTRIES` remain.
    fn prune(&mut self, now: u64) {
        self.entries
            .retain(|_, first_seen| now.saturating_sub(*first_seen) <= MAX_AGE_SECS);
        if self.entries.len() > MAX_ENTRIES {
            let mut by_age: Vec<(String, u64)> = self.entries.drain().collect();
            by_age.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            by_age.truncate(MAX_ENTRIES);
            self.entries = by_age.into_iter().collect();
        }
    }

    /// Write to a temporary file beside the store, then rename it into place,
    /// so a crash mid-write never leaves a half-written store.
    fn save(&self) {
        let Some(path) = &self.path else { return };
        if let Err(e) = write_atomically(path, &self.entries) {
            tracing::warn!(path = %path.display(), "delivered store not saved: {e}");
        }
    }
}

fn write_atomically(path: &Path, entries: &HashMap<String, u64>) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec(entries)?)?;
    std::fs::rename(&tmp, path)
}

/// The store file for one bot. `BUZZ_ACP_STATE_DIR` overrides the folder.
/// It is never under /tmp, which the host clears by access time.
pub fn store_path_for(pubkey_hex: &str) -> Option<PathBuf> {
    let dir = match std::env::var_os("BUZZ_ACP_STATE_DIR") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => PathBuf::from(std::env::var_os("HOME")?).join(".buzz").join("acp-state"),
    };
    Some(dir.join(format!("delivered-{pubkey_hex}.json")))
}

static PROCESS_STORE: OnceLock<SharedDeliveredStore> = OnceLock::new();

/// The one store this process shares across all of its agents. It is opened
/// on first use for this bot's key, so every agent in the pool, including a
/// respawned one, labels against the same record.
pub fn for_bot(pubkey_hex: &str) -> SharedDeliveredStore {
    PROCESS_STORE
        .get_or_init(|| {
            let store = match store_path_for(pubkey_hex) {
                Some(path) => DeliveredStore::load(path, unix_now()),
                None => {
                    tracing::warn!("no HOME and no BUZZ_ACP_STATE_DIR, delivered store kept in memory only");
                    DeliveredStore::in_memory()
                }
            };
            Arc::new(Mutex::new(store))
        })
        .clone()
}

pub fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_store(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("buzz29-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir.join("delivered.json")
    }

    #[test]
    fn a_recorded_event_survives_a_reload() {
        let path = tmp_store("reload");
        let mut store = DeliveredStore::load(path.clone(), 1_000);
        store.record(["aa".to_string()], 1_000);
        let reloaded = DeliveredStore::load(path, 1_001);
        assert!(reloaded.contains("aa"), "a restart must keep what an earlier session saw");
        assert!(!reloaded.contains("bb"));
    }

    #[test]
    fn entries_past_seven_days_expire_on_load() {
        let path = tmp_store("expire");
        let mut store = DeliveredStore::load(path.clone(), 1_000);
        store.record(["old".to_string()], 1_000);
        let later = 1_000 + MAX_AGE_SECS + 1;
        let reloaded = DeliveredStore::load(path, later);
        assert!(!reloaded.contains("old"), "an entry older than 7 days must be forgotten");
    }

    #[test]
    fn re_delivery_keeps_the_first_time() {
        let mut store = DeliveredStore::in_memory();
        store.record(["aa".to_string()], 1_000);
        store.record(["aa".to_string()], 1_000 + MAX_AGE_SECS);
        store.record(["bb".to_string()], 1_000 + MAX_AGE_SECS + 1);
        assert!(!store.contains("aa"), "re-delivery must not extend an entry's life");
        assert!(store.contains("bb"));
    }

    #[test]
    fn the_count_cap_keeps_the_newest() {
        let mut store = DeliveredStore::in_memory();
        store.record((0..MAX_ENTRIES).map(|i| format!("e{i}")), 100);
        store.record(["newest".to_string()], 200);
        assert_eq!(store.len(), MAX_ENTRIES);
        assert!(store.contains("newest"));
    }

    #[test]
    fn a_corrupt_store_fails_open_to_empty() {
        let path = tmp_store("corrupt");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"{not json").unwrap();
        let mut store = DeliveredStore::load(path.clone(), 1_000);
        assert_eq!(store.len(), 0, "a corrupt store must read as empty, never block the bot");
        store.record(["aa".to_string()], 1_000);
        assert!(DeliveredStore::load(path, 1_000).contains("aa"), "the next save repairs it");
    }

    #[test]
    fn empty_ids_are_never_recorded_or_matched() {
        let mut store = DeliveredStore::in_memory();
        store.record([String::new()], 1_000);
        assert_eq!(store.len(), 0);
        assert!(!store.contains(""));
    }
}
