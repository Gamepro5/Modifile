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

/// A profile's identity: a name, scoped to the game it is for.
///
/// Names used to be global, because profiles were one flat directory of TOML
/// files. That made "main" a resource you could only spend once across every
/// game you own, which is exactly the name everybody wants first. The game is
/// part of the identity now, so a Valheim `main` and a Minecraft `main` are
/// two different profiles and neither has to be called `valheim-main`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ProfileId {
    pub game: String,
    pub name: String,
}

impl ProfileId {
    pub fn new(game: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            game: game.into(),
            name: name.into(),
        }
    }

    /// `game/name`, the form that is never ambiguous.
    pub fn qualified(&self) -> String {
        format!("{}/{}", self.game, self.name)
    }
}

impl std::fmt::Display for ProfileId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.qualified())
    }
}

impl Profile {
    pub fn id(&self) -> ProfileId {
        ProfileId::new(&self.game, &self.name)
    }
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
    /// Obsolete, and read only so an older profile still loads.
    ///
    /// Play used to install into the game folder and undo it afterwards, which
    /// meant a crash or a power cut left the install modified. Play now hands
    /// the game a directory of its own instead, so there is nothing to revert
    /// and nothing to lose. The field is ignored.
    #[serde(default, skip_serializing)]
    pub revert_on_exit: bool,
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
    /// The source's own handle for one exact file, when a tag is not enough to
    /// name it. Today that means a CurseForge `fileID`.
    ///
    /// This is separate from `pin` because the two answer different questions.
    /// `pin` is a release tag, which for CurseForge is a human display name
    /// like `Sodium 0.5.8` — that is what the mod list should show. A modpack
    /// pins a numeric file id, and the project's newest fifty files (all the
    /// API will list) may no longer include it. Keeping both means a pack can
    /// be resolved exactly while still reading like a version.
    #[serde(default)]
    pub file: Option<String>,
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
            file: None,
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
            revert_on_exit: false,
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

/// Move profiles from the old flat layout into a directory per game.
///
/// Profiles used to live as `profiles/<name>.toml`, which made names globally
/// unique whether you wanted that or not. They now live under
/// `profiles/<game>/<name>.toml`. This runs at startup and is a no-op once
/// there is nothing left at the top level.
///
/// No name can collide during the move: the old layout could not hold two
/// profiles with the same name in the first place.
pub fn migrate_flat_layout(profiles_dir: &Path) -> Vec<ProfileId> {
    let Ok(entries) = std::fs::read_dir(profiles_dir) else {
        return Vec::new();
    };

    let mut moved = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("toml") {
            continue;
        }
        let Ok(profile) = Profile::load(&path) else {
            continue;
        };
        let id = profile.id();

        let game_dir = profiles_dir.join(crate::engine::sanitize_name(&id.game));
        if std::fs::create_dir_all(&game_dir).is_err() {
            continue;
        }

        // The profile, its lock and its saved settings travel together. A
        // half-moved profile would lose its configs, so the TOML moves last:
        // if anything fails before that, the old layout is still intact and
        // the next start tries again.
        let stem = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| id.name.clone());

        let lock = profiles_dir.join(format!("{stem}.lock.json"));
        if lock.exists() {
            let _ = std::fs::rename(&lock, game_dir.join(format!("{stem}.lock.json")));
        }
        let state = profiles_dir.join(format!("{stem}.state"));
        if state.is_dir() {
            let _ = std::fs::rename(&state, game_dir.join(format!("{stem}.state")));
        }
        if std::fs::rename(&path, game_dir.join(format!("{stem}.toml"))).is_ok() {
            moved.push(id);
        }
    }
    moved
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
