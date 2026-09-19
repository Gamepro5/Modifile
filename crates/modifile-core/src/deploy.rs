//! Deployment: linking store entries into a game directory, and taking them
//! back out again without ever eating a file we did not put there.
//!
//! Hard links are the whole trick. A deployed file is the *same inode* as the
//! store copy, so a profile with 200 mods adds 200 directory entries and zero
//! bytes. Switching profiles unlinks and relinks — no copying, no duplicated
//! game install, and nothing left running while you play.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Context, Error, Result};
use crate::pack::{CompiledPack, Target};
use crate::paths::write_atomic;
use crate::profile::{Lock, Profile};
use crate::store::Store;

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LinkMode {
    Hardlink,
    Symlink,
    Copy,
}

impl LinkMode {
    pub fn label(self) -> &'static str {
        match self {
            LinkMode::Hardlink => "hardlink",
            LinkMode::Symlink => "symlink",
            LinkMode::Copy => "copy",
        }
    }

    pub fn explain(self) -> &'static str {
        match self {
            LinkMode::Hardlink => "zero extra disk; store and game share one inode",
            LinkMode::Symlink => "zero extra disk; some anti-cheat and older games dislike symlinks",
            LinkMode::Copy => "duplicates bytes; used when the store and game are on different volumes",
        }
    }
}

/// One file that will be placed in the game directory.
#[derive(Debug, Clone)]
pub struct PlannedFile {
    /// Destination relative to the target root.
    pub rel: PathBuf,
    /// Absolute path inside the store.
    pub src: PathBuf,
    pub size: u64,
    pub mod_id: String,
    pub store_sha: String,
    /// A default for something the user will edit: copy it, only if absent,
    /// and never track it. See `pack::StateRules`.
    pub mutable: bool,
}

/// Two mods claiming the same destination path.
#[derive(Debug, Clone)]
pub struct Conflict {
    pub rel: PathBuf,
    pub winner: String,
    pub losers: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct Plan {
    pub game: String,
    pub target: String,
    pub root: PathBuf,
    pub files: Vec<PlannedFile>,
    pub conflicts: Vec<Conflict>,
    /// Mods with nothing to install here — usually a pack missing a rule.
    pub empty_mods: Vec<String>,
    /// The game needs a mod loader and does not have one. Everything installs
    /// correctly and the game ignores all of it, which is the single most
    /// confusing way for this to fail.
    pub missing_loader: Option<String>,
}

impl Plan {
    pub fn total_bytes(&self) -> u64 {
        self.files.iter().map(|f| f.size).sum()
    }
}

// ---------------------------------------------------------------------------
// Manifest
// ---------------------------------------------------------------------------

/// What we put in a game directory, so we can take exactly that back out.
///
/// Stored outside the game directory so a game patch or a Steam file
/// verification cannot destroy our bookkeeping along with the mods.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Manifest {
    pub game: String,
    pub target: String,
    pub profile: String,
    pub root: PathBuf,
    pub mode: Option<LinkMode>,
    pub deployed_ms: u64,
    /// False once the profile has been deactivated. The record survives
    /// deactivation only to remember files we could not remove — the profile is
    /// no longer installed, so it must not count as active.
    #[serde(default = "yes")]
    pub active: bool,
    pub files: Vec<ManifestFile>,
    /// Directories we created, deepest first, so cleanup can unwind them.
    #[serde(default)]
    pub created_dirs: Vec<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestFile {
    pub rel: PathBuf,
    pub store_sha: String,
    pub size: u64,
    /// Modification time as observed right after we created the file. Together
    /// with size this is our proof that the file on disk is still ours.
    pub mtime_ms: u64,
    pub mod_id: String,
}

impl Manifest {
    pub fn load(path: &Path) -> Result<Option<Self>> {
        if !path.exists() {
            return Ok(None);
        }
        let raw = std::fs::read(path).ctx(format!("reading {}", path.display()))?;
        Ok(Some(
            serde_json::from_slice(&raw).ctx(format!("parsing {}", path.display()))?,
        ))
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        write_atomic(path, &serde_json::to_vec_pretty(self)?)
    }
}

#[derive(Debug, Clone, Default)]
pub struct DeployReport {
    pub mode: Option<LinkMode>,
    pub linked: usize,
    pub bytes: u64,
    /// Default config files copied in because nothing was there yet.
    pub seeded: usize,
    /// Profile state files restored into the game directory.
    pub restored: usize,
    /// Profile state files captured out of it before switching away.
    pub captured: usize,
    /// Settings that were already in the game folder and now belong to this
    /// profile. Only happens on a profile's first activation.
    pub adopted: usize,
    /// Destination paths we refused to touch, with the reason.
    pub skipped: Vec<(PathBuf, String)>,
    pub removed: usize,
}

#[derive(Debug, Clone, Default)]
pub struct VerifyReport {
    pub ok: usize,
    pub missing: Vec<PathBuf>,
    pub modified: Vec<PathBuf>,
}

impl VerifyReport {
    pub fn is_clean(&self) -> bool {
        self.missing.is_empty() && self.modified.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Planning
// ---------------------------------------------------------------------------

/// Turn a resolved lockfile into a concrete list of files to place.
///
/// Later mods win conflicts, so profile order is load order.
pub fn plan(
    pack: &CompiledPack,
    target: &Target,
    root: &Path,
    profile: &Profile,
    lock: &Lock,
    store: &Store,
) -> Result<Plan> {
    let mut claims: BTreeMap<PathBuf, PlannedFile> = BTreeMap::new();
    let mut contested: BTreeMap<PathBuf, Vec<String>> = BTreeMap::new();
    let mut empty_mods = Vec::new();

    for entry in &profile.mods {
        if !entry.applies_to(&target.id) {
            continue;
        }
        let Some(locked) = lock.get(&entry.id) else {
            continue;
        };
        if !store.contains(&locked.sha256) {
            return Err(Error::NotFound(format!(
                "{} {} is locked but not in the store — run `modifile sync` first",
                entry.id, locked.version
            )));
        }

        let mod_id = entry.id.to_string();
        let mut placed = 0usize;

        for file in store.files(&locked.sha256)? {
            let Some(rule) = pack.rule_for(&file.rel, &target.id) else {
                continue;
            };
            let Some(rel) = pack.destination_for(&file.rel, target, rule) else {
                continue;
            };

            if let Some(previous) = claims.insert(
                rel.clone(),
                PlannedFile {
                    rel: rel.clone(),
                    src: file.abs,
                    size: file.size,
                    mod_id: mod_id.clone(),
                    store_sha: locked.sha256.clone(),
                    mutable: rule.mutable,
                },
            ) {
                if previous.mod_id != mod_id {
                    contested.entry(rel).or_default().push(previous.mod_id);
                }
            }
            placed += 1;
        }

        if placed == 0 {
            empty_mods.push(mod_id);
        }
    }

    let conflicts = contested
        .into_iter()
        .map(|(rel, losers)| Conflict {
            winner: claims
                .get(&rel)
                .map(|f| f.mod_id.clone())
                .unwrap_or_default(),
            rel,
            losers,
        })
        .collect();

    // A Unity game without BepInEx reads none of this. Say so at plan time
    // rather than letting someone launch a game that ignores every mod.
    let missing_loader = pack
        .pack
        .loaders
        .iter()
        .find(|l| {
            l.kind == crate::pack::LoaderKind::Archive
                && !crate::loader::markers_present(root, &l.markers)
        })
        .map(|l| l.name.clone());

    Ok(Plan {
        game: pack.id().to_string(),
        target: target.id.clone(),
        root: root.to_path_buf(),
        files: claims.into_values().collect(),
        conflicts,
        empty_mods,
        missing_loader,
    })
}

// ---------------------------------------------------------------------------
// Link mode probing
// ---------------------------------------------------------------------------

/// Work out the cheapest linking method that actually works between the store
/// and this game directory, by trying it rather than guessing from the path.
pub fn probe_mode(store_root: &Path, game_root: &Path) -> LinkMode {
    let probe_dir = game_root.join(".modifile-probe");
    if std::fs::create_dir_all(&probe_dir).is_err() {
        return LinkMode::Copy;
    }
    let src = store_root.join(".modifile-probe-src");
    let _ = std::fs::write(&src, b"uml");
    let dst = probe_dir.join("link");

    let mut mode = LinkMode::Copy;
    if std::fs::hard_link(&src, &dst).is_ok() {
        mode = LinkMode::Hardlink;
    } else {
        let _ = std::fs::remove_file(&dst);
        // Windows needs Developer Mode or elevation for symlinks, so this
        // frequently fails there and we fall through to copying.
        #[cfg(unix)]
        if std::os::unix::fs::symlink(&src, &dst).is_ok() {
            mode = LinkMode::Symlink;
        }
    }

    let _ = std::fs::remove_file(&dst);
    let _ = std::fs::remove_file(&src);
    let _ = std::fs::remove_dir_all(&probe_dir);
    mode
}

fn link_file(mode: LinkMode, src: &Path, dst: &Path) -> std::io::Result<()> {
    match mode {
        LinkMode::Hardlink => std::fs::hard_link(src, dst),
        #[cfg(unix)]
        LinkMode::Symlink => std::os::unix::fs::symlink(src, dst),
        #[cfg(not(unix))]
        LinkMode::Symlink => std::fs::copy(src, dst).map(|_| ()),
        LinkMode::Copy => std::fs::copy(src, dst).map(|_| ()),
    }
}

fn mtime_ms(path: &Path) -> u64 {
    std::fs::symlink_metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Is this file still exactly what we deployed? Size and mtime together are
/// enough to catch a game patch overwriting a mod file, without rehashing
/// hundreds of megabytes on every profile switch.
fn still_ours(path: &Path, record: &ManifestFile) -> bool {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return false;
    };
    if meta.is_symlink() {
        return true;
    }
    meta.len() == record.size && mtime_ms(path) == record.mtime_ms
}

// ---------------------------------------------------------------------------
// Apply / revert
// ---------------------------------------------------------------------------

/// Remove a previous deployment, then place the new one.
///
/// `force` overwrites foreign files that sit where a mod file should go.
/// Without it those destinations are skipped and reported, because an
/// unexpected file there is usually a manually-installed mod or a game file.
pub fn apply(
    plan: &Plan,
    previous: Option<&Manifest>,
    store_root: &Path,
    profile_name: &str,
    force: bool,
) -> Result<(Manifest, DeployReport)> {
    let mut report = DeployReport::default();

    if let Some(previous) = previous {
        report.removed = revert(previous, &mut report, force)?;
    }

    let mode = probe_mode(store_root, &plan.root);
    report.mode = Some(mode);

    let mut created_dirs: BTreeSet<PathBuf> = BTreeSet::new();
    let mut files = Vec::with_capacity(plan.files.len());

    for planned in &plan.files {
        let dst = plan.root.join(&planned.rel);

        if let Some(parent) = dst.parent() {
            if !parent.exists() {
                std::fs::create_dir_all(parent)
                    .ctx(format!("creating {}", parent.display()))?;
                // Record every level we brought into existence so revert can
                // unwind exactly those and leave the game's own dirs alone.
                let mut cursor = parent.to_path_buf();
                while cursor.starts_with(&plan.root) && cursor != plan.root {
                    created_dirs.insert(cursor.clone());
                    let Some(up) = cursor.parent().map(|p| p.to_path_buf()) else {
                        break;
                    };
                    cursor = up;
                }
            }
        }

        // A default config is seeded once and then belongs to the profile. It
        // is copied rather than linked, because the game will rewrite it and a
        // hard link would push that edit back into the shared store.
        if planned.mutable {
            if !dst.exists() {
                if let Err(e) = std::fs::copy(&planned.src, &dst) {
                    report
                        .skipped
                        .push((planned.rel.clone(), format!("could not seed default: {e}")));
                } else {
                    report.seeded += 1;
                }
            }
            continue;
        }

        if dst.exists() {
            if !force {
                report.skipped.push((
                    planned.rel.clone(),
                    "a file is already there and we did not put it there".to_string(),
                ));
                continue;
            }
            std::fs::remove_file(&dst).ctx(format!("replacing {}", dst.display()))?;
        }

        if let Err(e) = link_file(mode, &planned.src, &dst) {
            report
                .skipped
                .push((planned.rel.clone(), format!("{mode:?} failed: {e}")));
            continue;
        }

        report.linked += 1;
        report.bytes += planned.size;
        files.push(ManifestFile {
            rel: planned.rel.clone(),
            store_sha: planned.store_sha.clone(),
            size: planned.size,
            mtime_ms: mtime_ms(&dst),
            mod_id: planned.mod_id.clone(),
        });
    }

    let mut created_dirs: Vec<PathBuf> = created_dirs.into_iter().collect();
    // Deepest first, so unwinding removes children before parents.
    created_dirs.sort_by_key(|p| std::cmp::Reverse(p.components().count()));

    Ok((
        Manifest {
            game: plan.game.clone(),
            target: plan.target.clone(),
            profile: profile_name.to_string(),
            root: plan.root.clone(),
            mode: Some(mode),
            deployed_ms: crate::paths::now_millis(),
            active: true,
            files,
            created_dirs,
        },
        report,
    ))
}

/// Take a deployment back out.
///
/// A file that no longer matches what we recorded is left alone and reported:
/// a game patch may have replaced it, or the user may have dropped their own
/// build over the top, and deleting either would be destroying something we did
/// not create. `force` overrides that, for when the user says so explicitly.
pub fn revert(manifest: &Manifest, report: &mut DeployReport, force: bool) -> Result<usize> {
    let mut removed = 0;
    for record in &manifest.files {
        let path = manifest.root.join(&record.rel);
        if !path.exists() && std::fs::symlink_metadata(&path).is_err() {
            continue;
        }
        if !still_ours(&path, record) && !force {
            report.skipped.push((
                record.rel.clone(),
                "changed since it was installed, so it was left in place".to_string(),
            ));
            continue;
        }
        match std::fs::remove_file(&path) {
            Ok(()) => removed += 1,
            Err(e) => report
                .skipped
                .push((record.rel.clone(), format!("could not remove: {e}"))),
        }
    }

    for dir in &manifest.created_dirs {
        // Only succeeds when empty, which is exactly the condition we want.
        let _ = std::fs::remove_dir(dir);
    }

    Ok(removed)
}

// ---------------------------------------------------------------------------
// Scanning the game folder
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct ScanEntry {
    /// Path relative to the game root.
    pub rel: PathBuf,
    pub size: u64,
    /// A mod in this profile wants to install to this exact path.
    pub conflicts: bool,
    /// Which mod put it here, when we know.
    pub mod_id: Option<String>,
}

/// What is actually sitting in a game's mod folders right now.
#[derive(Debug, Clone, Default)]
pub struct FolderScan {
    pub target: String,
    pub root: PathBuf,
    /// Files we placed that are still exactly as we left them.
    pub intact: usize,
    /// Files we placed that have since changed — a game patch, or a hand edit.
    pub modified: Vec<ScanEntry>,
    /// Files we placed that are gone.
    pub missing: Vec<ScanEntry>,
    /// Files in a mod folder that Modifile did not put there. Usually mods
    /// installed by hand before, or left behind by another manager.
    pub foreign: Vec<ScanEntry>,
}

impl FolderScan {
    pub fn is_clean(&self) -> bool {
        self.modified.is_empty() && self.missing.is_empty() && self.foreign.is_empty()
    }

    /// Foreign files that would stop a mod installing, because something we
    /// want to place is already sitting at that path.
    pub fn blocking(&self) -> impl Iterator<Item = &ScanEntry> {
        self.foreign.iter().filter(|e| e.conflicts)
    }
}

/// Inspect a game's mod folders and say what is ours, what changed, and what
/// arrived from somewhere else.
///
/// `planned` is the set of destinations the current profile wants to occupy,
/// used to tell a harmless leftover apart from one that will actively block an
/// install.
pub fn scan(
    pack: &CompiledPack,
    target: &Target,
    root: &Path,
    manifest: Option<&Manifest>,
    planned: &BTreeSet<PathBuf>,
) -> FolderScan {
    let mut result = FolderScan {
        target: target.id.clone(),
        root: root.to_path_buf(),
        ..Default::default()
    };

    // Everything we believe we placed, by relative path.
    let ours: BTreeMap<PathBuf, &ManifestFile> = manifest
        .map(|m| m.files.iter().map(|f| (f.rel.clone(), f)).collect())
        .unwrap_or_default();

    let mut seen_on_disk: BTreeSet<PathBuf> = BTreeSet::new();

    for (dir, is_state) in pack.managed_dirs(target, root) {
        let mut files = Vec::new();
        collect_files(&dir, &dir, &mut files);

        for (abs, rel_to_dir, size) in files {
            let Ok(rel) = abs.strip_prefix(root) else {
                continue;
            };
            let rel = rel.to_path_buf();
            seen_on_disk.insert(rel.clone());
            let _ = rel_to_dir;

            match ours.get(&rel) {
                Some(record) => {
                    if still_ours(&abs, record) {
                        result.intact += 1;
                    } else {
                        result.modified.push(ScanEntry {
                            rel,
                            size,
                            conflicts: false,
                            mod_id: Some(record.mod_id.clone()),
                        });
                    }
                }
                // Config directories belong to the profile, so an unfamiliar
                // file there is expected, not something to raise.
                None if is_state => {}
                None => result.foreign.push(ScanEntry {
                    conflicts: planned.contains(&rel),
                    rel,
                    size,
                    mod_id: None,
                }),
            }
        }
    }

    for (rel, record) in &ours {
        if !seen_on_disk.contains(rel) && !root.join(rel).exists() {
            result.missing.push(ScanEntry {
                rel: rel.clone(),
                size: record.size,
                conflicts: false,
                mod_id: Some(record.mod_id.clone()),
            });
        }
    }

    result
}

fn collect_files(root: &Path, dir: &Path, out: &mut Vec<(PathBuf, PathBuf, u64)>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        match entry.metadata() {
            Ok(m) if m.is_dir() => collect_files(root, &path, out),
            Ok(m) if m.is_file() || m.is_symlink() => {
                let rel = path.strip_prefix(root).unwrap_or(&path).to_path_buf();
                out.push((path, rel, m.len()));
            }
            _ => {}
        }
    }
}

/// Check a deployment is still intact. This is how you find out that last
/// Tuesday's game patch quietly clobbered half your mods.
pub fn verify(manifest: &Manifest) -> VerifyReport {
    let mut report = VerifyReport::default();
    for record in &manifest.files {
        let path = manifest.root.join(&record.rel);
        if std::fs::symlink_metadata(&path).is_err() {
            report.missing.push(record.rel.clone());
        } else if !still_ours(&path, record) {
            report.modified.push(record.rel.clone());
        } else {
            report.ok += 1;
        }
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conflicts_are_won_by_the_later_mod() {
        // Planning is exercised end-to-end in the engine tests; this pins the
        // ordering rule that makes profile order mean load order.
        let mut claims: BTreeMap<PathBuf, &str> = BTreeMap::new();
        claims.insert(PathBuf::from("a.dll"), "first");
        let previous = claims.insert(PathBuf::from("a.dll"), "second");
        assert_eq!(previous, Some("first"));
        assert_eq!(claims[&PathBuf::from("a.dll")], "second");
    }

    /// A minimal Valheim-shaped pack: plugins are managed, config is state.
    fn scan_pack() -> CompiledPack {
        use crate::pack::{GameMeta, InstallRule, Pack, StateRules, TargetKind};
        let mut paths = BTreeMap::new();
        paths.insert("plugins".to_string(), "BepInEx/plugins".to_string());
        paths.insert("config".to_string(), "BepInEx/config".to_string());

        CompiledPack::new(Pack {
            schema: 1,
            game: GameMeta {
                id: "scantest".into(),
                name: "Scan Test".into(),
                description: String::new(),
                maintainers: vec![],
            },
            targets: vec![Target {
                id: "client".into(),
                name: "Client".into(),
                kind: TargetKind::Client,
                flavor: None,
                asset_reject: vec![],
                processes: vec![],
                markers: vec!["game.exe".into()],
                paths: BTreeMap::new(),
                steam: None,
                candidates: vec![],
            }],
            paths,
            assets: Default::default(),
            state: StateRules {
                paths: vec!["config".into()],
            },
            versions: Default::default(),
            loaders: Vec::new(),
            search: Default::default(),
            running: Default::default(),
            install: vec![
                InstallRule {
                    pattern: "**/*.dll".into(),
                    into: "plugins".into(),
                    strip: None,
                    flatten: true,
                    targets: None,
                    mutable: false,
                },
                InstallRule {
                    pattern: "**/*.cfg".into(),
                    into: "config".into(),
                    strip: None,
                    flatten: true,
                    targets: None,
                    mutable: true,
                },
            ],
            readable_extensions: vec!["lua".into()],
            executable_extensions: vec!["dll".into()],
        })
        .unwrap()
    }

    #[test]
    fn scan_tells_our_files_from_everyone_elses() {
        let root = std::env::temp_dir().join(format!("modifile-scan-{}", crate::paths::now_millis()));
        let plugins = root.join("BepInEx/plugins");
        let config = root.join("BepInEx/config");
        std::fs::create_dir_all(&plugins).unwrap();
        std::fs::create_dir_all(&config).unwrap();

        // One we placed and left alone, one we placed and then something
        // changed, one we placed that vanished, one we never placed, and a
        // config file (profile-owned, so not "foreign").
        std::fs::write(plugins.join("Ours.dll"), b"ours").unwrap();
        std::fs::write(plugins.join("Edited.dll"), b"original").unwrap();
        std::fs::write(plugins.join("HandInstalled.dll"), b"someone else").unwrap();
        std::fs::write(config.join("whatever.cfg"), b"settings").unwrap();

        let record = |rel: &str, size: u64| ManifestFile {
            rel: PathBuf::from("BepInEx/plugins").join(rel),
            store_sha: "0".into(),
            size,
            mtime_ms: mtime_ms(&plugins.join(rel)),
            mod_id: "github:test/mod".into(),
        };
        let manifest = Manifest {
            game: "scantest".into(),
            target: "client".into(),
            profile: "main".into(),
            root: root.clone(),
            mode: Some(LinkMode::Hardlink),
            deployed_ms: 0,
            active: true,
            files: vec![
                record("Ours.dll", 4),
                record("Edited.dll", 8),
                record("Vanished.dll", 10),
            ],
            created_dirs: vec![],
        };

        // Now make Edited.dll differ from what the manifest remembers.
        std::fs::write(plugins.join("Edited.dll"), b"tampered with").unwrap();

        let pack = scan_pack();
        let target = pack.target("client").unwrap();
        let mut planned = BTreeSet::new();
        planned.insert(PathBuf::from("BepInEx/plugins").join("HandInstalled.dll"));

        let scan = scan(&pack, target, &root, Some(&manifest), &planned);

        assert_eq!(scan.intact, 1, "Ours.dll should be intact");
        assert_eq!(scan.modified.len(), 1, "Edited.dll should be modified");
        assert_eq!(scan.missing.len(), 1, "Vanished.dll should be missing");
        assert_eq!(
            scan.foreign.len(),
            1,
            "only HandInstalled.dll is foreign; the config file is profile-owned"
        );
        assert_eq!(
            scan.blocking().count(),
            1,
            "the hand-installed file sits where a mod wants to go"
        );
        assert!(!scan.is_clean());

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn revert_leaves_a_replaced_file_but_reports_it() {
        // The real case: the mod's author rebuilds their DLL and drops it over
        // the installed one. Deactivating must not silently delete their build,
        // and must not silently pretend it removed it either.
        let root = std::env::temp_dir().join(format!("modifile-revert-{}", crate::paths::now_millis()));
        let plugins = root.join("BepInEx/plugins");
        std::fs::create_dir_all(&plugins).unwrap();

        let ours = plugins.join("Ours.dll");
        let theirs = plugins.join("Replaced.dll");
        std::fs::write(&ours, b"installed by modifile").unwrap();
        std::fs::write(&theirs, b"installed by modifile").unwrap();

        let record = |name: &str, path: &std::path::Path| ManifestFile {
            rel: PathBuf::from("BepInEx/plugins").join(name),
            store_sha: "0".into(),
            size: std::fs::metadata(path).unwrap().len(),
            mtime_ms: mtime_ms(path),
            mod_id: "github:test/mod".into(),
        };
        let manifest = Manifest {
            game: "t".into(),
            target: "client".into(),
            profile: "main".into(),
            root: root.clone(),
            mode: Some(LinkMode::Copy),
            deployed_ms: 0,
            active: true,
            files: vec![record("Ours.dll", &ours), record("Replaced.dll", &theirs)],
            created_dirs: vec![],
        };

        // The author drops in their own build.
        std::fs::write(&theirs, b"my own freshly compiled build, different size").unwrap();

        let mut report = DeployReport::default();
        let removed = revert(&manifest, &mut report, false).unwrap();

        assert_eq!(removed, 1, "only the untouched file is removed");
        assert!(!ours.exists());
        assert!(theirs.exists(), "the author's own build must survive");
        assert_eq!(report.skipped.len(), 1, "and it must be reported, not silent");

        // Now they say "delete it anyway".
        let mut forced = DeployReport::default();
        revert(&manifest, &mut forced, true).unwrap();
        assert!(!theirs.exists(), "force removes it");

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn scan_of_an_untouched_folder_is_clean() {
        let root = std::env::temp_dir().join(format!("modifile-clean-{}", crate::paths::now_millis()));
        std::fs::create_dir_all(root.join("BepInEx/plugins")).unwrap();

        let pack = scan_pack();
        let target = pack.target("client").unwrap();
        let scan = scan(&pack, target, &root, None, &BTreeSet::new());

        assert!(scan.is_clean());
        assert_eq!(scan.intact, 0);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn foreign_files_are_not_ours() {
        let dir = std::env::temp_dir().join(format!("uml-test-{}", crate::paths::now_millis()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("x.txt");
        std::fs::write(&file, b"hello").unwrap();

        let record = ManifestFile {
            rel: PathBuf::from("x.txt"),
            store_sha: "0".into(),
            size: 5,
            mtime_ms: mtime_ms(&file),
            mod_id: "test".into(),
        };
        assert!(still_ours(&file, &record));

        std::fs::write(&file, b"tampered with").unwrap();
        assert!(!still_ours(&file, &record));

        std::fs::remove_dir_all(&dir).ok();
    }
}
