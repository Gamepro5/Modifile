//! What is on disk, and who still wants it.
//!
//! The store is content-addressed, so updating a mod leaves the previous
//! version sitting there under its own hash. Keeping those forever is waste:
//! nothing points at them and re-downloading is one request. But a download is
//! *not* waste just because its profile is switched off — that is the whole
//! point of profiles, so an inactive profile's mods are kept.
//!
//! The distinction this module draws is therefore "referenced by some profile"
//! versus "referenced by nobody", not "active" versus "inactive".

use std::collections::BTreeMap;

use crate::error::{Context, Result};
use crate::paths::Paths;
use crate::profile::{Lock, Profile};
use crate::source::ModId;

/// One thing that still wants a stored download.
#[derive(Debug, Clone)]
pub struct StoreUse {
    pub profile: String,
    pub mod_id: ModId,
    pub version: String,
    /// The profile's mods are in a game folder right now.
    pub active: bool,
}

#[derive(Debug, Clone)]
pub struct StoredItem {
    pub sha256: String,
    pub size: u64,
    /// Empty means nothing references it — safe to delete.
    pub used_by: Vec<StoreUse>,
}

impl StoredItem {
    pub fn is_orphan(&self) -> bool {
        self.used_by.is_empty()
    }

    /// Best label we have: whatever mod claims it, else the bare hash.
    pub fn name(&self) -> String {
        match self.used_by.first() {
            Some(u) => format!("{} {}", u.mod_id.short(), u.version),
            None => format!("unused ({})", &self.sha256[..12.min(self.sha256.len())]),
        }
    }

    /// Wanted only by profiles that are switched off.
    pub fn only_inactive(&self) -> bool {
        !self.used_by.is_empty() && self.used_by.iter().all(|u| !u.active)
    }
}

#[derive(Debug, Clone, Default)]
pub struct StorageReport {
    pub items: Vec<StoredItem>,
}

impl StorageReport {
    pub fn total_bytes(&self) -> u64 {
        self.items.iter().map(|i| i.size).sum()
    }

    pub fn orphan_bytes(&self) -> u64 {
        self.items.iter().filter(|i| i.is_orphan()).map(|i| i.size).sum()
    }

    pub fn orphans(&self) -> impl Iterator<Item = &StoredItem> {
        self.items.iter().filter(|i| i.is_orphan())
    }
}

/// Read every lockfile and work out who references what.
///
/// A lockfile that will not parse is an error rather than an empty answer: the
/// caller may be about to delete "unreferenced" downloads, and a file we could
/// not read is not evidence that nothing needs them.
pub fn report(
    paths: &Paths,
    store: &crate::store::Store,
    active_by_game: &BTreeMap<String, Vec<String>>,
) -> Result<StorageReport> {
    let mut uses: BTreeMap<String, Vec<StoreUse>> = BTreeMap::new();

    // Profiles live one directory per game, so this walks games and then
    // their lockfiles. A stray file at the top level is ignored rather than
    // guessed at; the startup migration moves real ones down.
    for game_entry in std::fs::read_dir(&paths.profiles)
        .ctx(format!("reading {}", paths.profiles.display()))?
        .flatten()
    {
        let game_dir = game_entry.path();
        if !game_dir.is_dir() {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&game_dir) else {
            continue;
        };

        for entry in entries.flatten() {
            let path = entry.path();
            let Some(file_name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            let Some(name) = file_name.strip_suffix(".lock.json") else {
                continue;
            };

            let lock = Lock::load(&path)?;
            // The profile beside the lock names the game authoritatively; the
            // directory name is only a sanitised version of it.
            let game = Profile::load(&game_dir.join(format!("{name}.toml")))
                .map(|p| p.game)
                .unwrap_or_default();
            let active = active_by_game
                .get(&game)
                .map(|names| names.iter().any(|n| n == name))
                .unwrap_or(false);

            for locked in &lock.mods {
                uses.entry(locked.sha256.clone()).or_default().push(StoreUse {
                    profile: name.to_string(),
                    mod_id: locked.id.clone(),
                    version: locked.version.clone(),
                    active,
                });
            }
        }
    }

    let mut items: Vec<StoredItem> = store
        .entries()?
        .into_iter()
        .map(|(sha256, size)| StoredItem {
            used_by: uses.remove(&sha256).unwrap_or_default(),
            sha256,
            size,
        })
        .collect();

    // Biggest first: the thing worth deleting is usually the thing worth seeing.
    items.sort_by(|a, b| b.size.cmp(&a.size));
    Ok(StorageReport { items })
}
