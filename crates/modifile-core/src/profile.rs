use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Context, Result};
use crate::paths::write_atomic;
use crate::source::ModId;
use crate::trust::TrustReport;

fn yes() -> bool {
    true
}

/// A named set of mods for one game. Profiles are cheap: they hold references
/// into the store, never copies, so having twenty of them costs twenty small
/// text files.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Profile {
    pub name: String,
    /// Game pack id.
    pub game: String,
    /// Target ids this profile deploys to. Empty means every target that has a
    /// known root — which is how a profile covers client and dedicated server
    /// at once.
    #[serde(default)]
    pub targets: Vec<String>,
    /// Explicit game roots per target id, for installs autodetection misses.
    #[serde(default)]
    pub roots: BTreeMap<String, PathBuf>,
    /// For games where a mod is built against one game version, e.g. `1.20.1`.
    /// Sources that publish per-version builds use this to pick the right one.
    #[serde(default)]
    pub game_version: Option<String>,
    /// For games with competing mod loaders, e.g. `fabric`, `forge`, `neoforge`.
    #[serde(default)]
    pub loader: Option<String>,
    #[serde(default)]
    pub mods: Vec<ModEntry>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ModEntry {
    pub id: ModId,
    #[serde(default = "yes")]
    pub enabled: bool,
    /// Restrict this mod to some targets. This is how a profile says "the
    /// minimap addon is client-only, the anti-cheat plugin is server-only".
    #[serde(default)]
    pub targets: Option<Vec<String>>,
    /// Hold at an exact release tag; updates will not move it.
    #[serde(default)]
    pub pin: Option<String>,
    /// Consider prereleases when resolving.
    #[serde(default)]
    pub prerelease: bool,
    /// Supplied as a file rather than fetched. Update checks skip it and keep
    /// its existing lock entry, because there is no API to ask.
    #[serde(default)]
    pub manual: bool,
}

impl ModEntry {
    pub fn new(id: ModId) -> Self {
        Self {
            id,
            enabled: true,
            targets: None,
            pin: None,
            prerelease: false,
            manual: false,
        }
    }

    pub fn applies_to(&self, target_id: &str) -> bool {
        self.enabled
            && self
                .targets
                .as_ref()
                .map(|t| t.iter().any(|id| id == target_id))
                .unwrap_or(true)
    }
}

impl Profile {
    pub fn new(name: impl Into<String>, game: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            game: game.into(),
            targets: Vec::new(),
            roots: BTreeMap::new(),
            game_version: None,
            loader: None,
            mods: Vec::new(),
        }
    }

    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).ctx(format!("reading {}", path.display()))?;
        toml::from_str(&text).ctx(format!("parsing {}", path.display()))
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let text = toml::to_string_pretty(self)
            .map_err(|e| crate::error::Error::other(format!("serializing profile: {e}")))?;
        write_atomic(path, text.as_bytes())
    }

    pub fn find(&self, id: &ModId) -> Option<&ModEntry> {
        self.mods.iter().find(|m| &m.id == id)
    }

    /// Add a mod, or return false if it is already listed.
    pub fn add(&mut self, entry: ModEntry) -> bool {
        if self.find(&entry.id).is_some() {
            return false;
        }
        self.mods.push(entry);
        true
    }

    pub fn remove(&mut self, id: &ModId) -> bool {
        let before = self.mods.len();
        self.mods.retain(|m| &m.id != id);
        self.mods.len() != before
    }
}

// ---------------------------------------------------------------------------
// Lockfile
// ---------------------------------------------------------------------------

/// Exactly what a profile resolved to, pinned by content hash.
///
/// The hash is the point. Re-resolving is what makes "update" work; the lock is
/// what makes "install the same thing on the other machine" and "detect that an
/// asset was re-uploaded under the same tag" work.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct Lock {
    #[serde(default)]
    pub profile: String,
    #[serde(default)]
    pub generated_ms: u64,
    #[serde(default)]
    pub mods: Vec<LockEntry>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LockEntry {
    pub id: ModId,
    /// Release tag.
    pub version: String,
    pub asset: String,
    pub url: String,
    /// SHA-256 of the downloaded artifact; also its store key.
    pub sha256: String,
    pub size: u64,
    #[serde(default)]
    pub published_at: String,
    /// The newest version the source was advertising when we last looked, set
    /// only when it is not the version we are on. Two cases produce that:
    /// a manually supplied mod, which nothing can download for you, and a
    /// pinned one, which is deliberately not being moved. Both need saying;
    /// silence in either case reads as "you are up to date".
    #[serde(default)]
    pub upstream: Option<String>,
    pub trust: TrustReport,
}

impl Lock {
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = std::fs::read(path).ctx(format!("reading {}", path.display()))?;
        serde_json::from_slice(&raw).ctx(format!("parsing {}", path.display()))
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        write_atomic(path, &serde_json::to_vec_pretty(self)?)
    }

    pub fn get(&self, id: &ModId) -> Option<&LockEntry> {
        self.mods.iter().find(|m| &m.id == id)
    }

    pub fn store_keys(&self) -> std::collections::HashSet<String> {
        self.mods.iter().map(|m| m.sha256.clone()).collect()
    }
}
