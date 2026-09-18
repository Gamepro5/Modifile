//! Orchestration: resolve what a profile means, fetch what is missing, and put
//! it in the game directory.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use futures::StreamExt;

use crate::deploy::{self, DeployReport, Manifest, Plan};
use crate::error::{Error, Result};
use crate::hash::normalize_digest;
use crate::http::Http;
use crate::pack::{load_dir, CompiledPack, Target};
use crate::paths::Paths;
use crate::profile::{Lock, LockEntry, Profile};
use crate::source::github::GitHub;
use crate::source::{Asset, ModId, Release, RepoInfo};
use crate::state;
use crate::store::Store;
use crate::trust::{self, TrustPolicy, TrustReport};

/// How many repositories to query at once. GitHub tolerates this comfortably
/// and it turns a 40-mod update check from a minute into a couple of seconds.
const RESOLVE_CONCURRENCY: usize = 8;
/// Downloads are heavier, so fewer at a time.
const DOWNLOAD_CONCURRENCY: usize = 4;

/// Knobs for a deploy. Two booleans in an argument list is a bug waiting to
/// happen, so they get names at the call site.
#[derive(Debug, Clone, Copy, Default)]
pub struct DeployOptions {
    /// Overwrite files sitting where a mod file should go.
    pub force: bool,
    /// The user states the game is stopped. Only consulted for a game
    /// directory on another machine, where we genuinely cannot check.
    pub assume_stopped: bool,
}

/// Where imported configs come from.
#[derive(Debug, Clone)]
pub enum ConfigSource {
    /// Whatever is sitting in the game folder right now — for adopting an
    /// install you modded by hand before using Modifile.
    Game,
    /// Another profile's saved configs.
    Profile(String),
    /// A folder on disk, e.g. a backup.
    Folder(PathBuf),
}

#[derive(Debug, Clone)]
pub enum Event {
    Resolving(ModId),
    Resolved { id: ModId, version: String },
    Downloading { id: ModId, asset: String, size: u64 },
    Cached { id: ModId, version: String },
    Installed { id: ModId, version: String, trust: TrustReport },
    Failed { id: ModId, error: String },
}

pub type Reporter = Arc<dyn Fn(Event) + Send + Sync>;

fn silent() -> Reporter {
    Arc::new(|_| {})
}

pub struct Engine {
    pub paths: Paths,
    pub store: Store,
    pub github: GitHub,
    pub packs: Vec<CompiledPack>,
    pub pack_errors: Vec<(PathBuf, Error)>,
    pub policy: TrustPolicy,
    pub roots: crate::roots::GlobalRoots,
}

/// A mod resolved to a concrete downloadable artifact, before we have it.
#[derive(Debug, Clone)]
struct Resolution {
    id: ModId,
    release: Release,
    asset: Asset,
    repo: Option<RepoInfo>,
}

impl Engine {
    pub fn open(paths: Paths, token: Option<String>) -> Result<Self> {
        paths.ensure()?;
        let http = Http::new(paths.http_cache(), token)?;
        let (packs, pack_errors) = load_dir(&paths.packs);
        let roots = crate::roots::GlobalRoots::load(&paths.roots_file()).unwrap_or_default();
        Ok(Self {
            store: Store::new(paths.store.clone()),
            github: GitHub::new(http),
            packs,
            pack_errors,
            policy: TrustPolicy::default(),
            roots,
            paths,
        })
    }

    pub fn pack(&self, id: &str) -> Option<&CompiledPack> {
        self.packs.iter().find(|p| p.id() == id)
    }

    pub fn pack_for(&self, profile: &Profile) -> Result<&CompiledPack> {
        self.pack(&profile.game).ok_or_else(|| {
            Error::NotFound(format!(
                "game pack `{}` (drop its .toml in {})",
                profile.game,
                self.paths.packs.display()
            ))
        })
    }

    /// Targets for this profile paired with the directory we will deploy into.
    ///
    /// Precedence: a root set on the profile, then one the user pointed us at
    /// for this game, then autodetection.
    pub fn targets(&self, pack: &CompiledPack, profile: &Profile) -> Vec<(Target, Option<PathBuf>)> {
        pack.pack
            .targets
            .iter()
            .filter(|t| profile.targets.is_empty() || profile.targets.iter().any(|id| id == &t.id))
            .map(|t| {
                let root = profile
                    .roots
                    .get(&t.id)
                    .filter(|p| p.is_dir())
                    .cloned()
                    .or_else(|| {
                        self.roots
                            .get(pack.id(), &t.id)
                            .filter(|p| p.is_dir())
                            .cloned()
                    })
                    .or_else(|| pack.detect(t).into_iter().next());
                (t.clone(), root)
            })
            .collect()
    }

    /// Remember a game directory for every profile of this game.
    pub fn set_root(&mut self, game: &str, target: &str, path: PathBuf) -> Result<()> {
        self.roots.set(game, target, path);
        self.roots.save(&self.paths.roots_file())
    }

    /// Forget a remembered directory, falling back to autodetection.
    pub fn clear_root(&mut self, game: &str, target: &str) -> Result<()> {
        self.roots.clear(game, target);
        self.roots.save(&self.paths.roots_file())
    }

    /// Save a GitHub token and rebuild the HTTP client that uses it.
    pub fn set_token(&mut self, token: &str) -> Result<()> {
        let token = token.trim();
        let path = self.paths.token_file();
        if token.is_empty() {
            std::fs::remove_file(&path).ok();
        } else {
            crate::paths::write_atomic(&path, token.as_bytes())?;
        }
        let http = Http::new(
            self.paths.http_cache(),
            (!token.is_empty()).then(|| token.to_string()),
        )?;
        self.github = GitHub::new(http);
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Resolve + install
    // -----------------------------------------------------------------------

    /// Resolve every mod in the profile, download whatever the store is missing,
    /// and return a fresh lockfile.
    ///
    /// `keep` is the previous lock: entries whose resolved artifact hash is
    /// unchanged are carried over untouched, so a no-op sync downloads nothing.
    pub async fn sync(
        &self,
        pack: &CompiledPack,
        profile: &Profile,
        previous: &Lock,
        report: Option<Reporter>,
    ) -> Result<(Lock, Vec<(ModId, String)>)> {
        let report = report.unwrap_or_else(silent);
        let enabled: Vec<_> = profile.mods.iter().filter(|m| m.enabled).collect();
        let mut failures = Vec::new();

        // --- resolve, concurrently -----------------------------------------
        // Asset choice is per target: a profile covering retail and Classic
        // needs a release that can satisfy at least one of them.
        let targets: Vec<Target> = pack
            .pack
            .targets
            .iter()
            .filter(|t| profile.targets.is_empty() || profile.targets.iter().any(|id| id == &t.id))
            .cloned()
            .collect();

        let resolutions: Vec<std::result::Result<Resolution, (ModId, String)>> =
            futures::stream::iter(enabled.iter().map(|entry| {
                let report = report.clone();
                let targets = targets.clone();
                async move {
                    report(Event::Resolving(entry.id.clone()));
                    match self.resolve_one(pack, &entry.id, entry.pin.as_deref(), entry.prerelease, &targets).await {
                        Ok(res) => {
                            report(Event::Resolved {
                                id: res.id.clone(),
                                version: res.release.tag.clone(),
                            });
                            Ok(res)
                        }
                        Err(e) => {
                            report(Event::Failed {
                                id: entry.id.clone(),
                                error: e.to_string(),
                            });
                            Err((entry.id.clone(), e.to_string()))
                        }
                    }
                }
            }))
            .buffer_unordered(RESOLVE_CONCURRENCY)
            .collect()
            .await;

        let mut resolved = Vec::new();
        for outcome in resolutions {
            match outcome {
                Ok(r) => resolved.push(r),
                Err(f) => failures.push(f),
            }
        }

        // --- fetch what we do not already have ------------------------------
        let fetched: Vec<std::result::Result<LockEntry, (ModId, String)>> =
            futures::stream::iter(resolved.into_iter().map(|res| {
                let report = report.clone();
                let previous_entry = previous.get(&res.id).cloned();
                async move {
                    let id = res.id.clone();
                    match self.acquire(pack, res, previous_entry, &report).await {
                        Ok(entry) => Ok(entry),
                        Err(e) => {
                            report(Event::Failed {
                                id: id.clone(),
                                error: e.to_string(),
                            });
                            Err((id, e.to_string()))
                        }
                    }
                }
            }))
            .buffer_unordered(DOWNLOAD_CONCURRENCY)
            .collect()
            .await;

        let mut mods = Vec::new();
        for outcome in fetched {
            match outcome {
                Ok(entry) => mods.push(entry),
                Err(f) => failures.push(f),
            }
        }

        // Preserve profile order so conflict resolution stays predictable.
        mods.sort_by_key(|entry| {
            profile
                .mods
                .iter()
                .position(|m| m.id == entry.id)
                .unwrap_or(usize::MAX)
        });

        Ok((
            Lock {
                profile: profile.name.clone(),
                generated_ms: crate::paths::now_millis(),
                mods,
            },
            failures,
        ))
    }

    async fn resolve_one(
        &self,
        pack: &CompiledPack,
        id: &ModId,
        pin: Option<&str>,
        allow_prerelease: bool,
        targets: &[Target],
    ) -> Result<Resolution> {
        let releases = self.github.releases(id).await?;
        if releases.is_empty() {
            return Err(Error::NotFound(format!(
                "{id} has no GitHub releases — this loader installs release assets, not source checkouts"
            )));
        }

        // Walk back through releases until one carries an asset we can use.
        // A tag with no build attached is common and should not be fatal.
        for release in releases
            .iter()
            .filter(|r| pin.map(|p| r.tag == p).unwrap_or(allow_prerelease || !r.prerelease))
        {
            let names: Vec<String> = release.assets.iter().map(|a| a.name.clone()).collect();
            for target in targets {
                if let Some(idx) = pack.select_asset(&names, target) {
                    return Ok(Resolution {
                        id: id.clone(),
                        release: release.clone(),
                        asset: release.assets[idx].clone(),
                        repo: self.github.repo(id).await.unwrap_or(None),
                    });
                }
            }
        }

        Err(Error::NotFound(match pin {
            Some(p) => format!("{id} has no usable asset on pinned release {p}"),
            None => format!(
                "{id} has releases but none carry an asset this game pack accepts"
            ),
        }))
    }

    /// Ensure the artifact is in the store, then assess trust.
    async fn acquire(
        &self,
        pack: &CompiledPack,
        res: Resolution,
        previous: Option<LockEntry>,
        report: &Reporter,
    ) -> Result<LockEntry> {
        let upstream_digest = res.asset.digest.as_deref().map(normalize_digest);

        // Three ways to avoid a download, cheapest first:
        //   1. the lock already pinned this exact tag+asset and the store has it
        //   2. GitHub told us the digest and the store already has that content
        if let Some(prev) = &previous {
            if prev.version == res.release.tag
                && prev.asset == res.asset.name
                && self.store.contains(&prev.sha256)
            {
                report(Event::Cached {
                    id: res.id.clone(),
                    version: res.release.tag.clone(),
                });
                return Ok(prev.clone());
            }
        }
        if let Some(digest) = &upstream_digest {
            if self.store.contains(digest) {
                report(Event::Cached {
                    id: res.id.clone(),
                    version: res.release.tag.clone(),
                });
                return self
                    .finish_entry(pack, &res, digest.clone(), res.asset.size)
                    .await;
            }
        }

        report(Event::Downloading {
            id: res.id.clone(),
            asset: res.asset.name.clone(),
            size: res.asset.size,
        });

        let tmp = self.paths.downloads().join(format!(
            "{}-{}-{}",
            res.id.owner,
            res.id.repo,
            crate::paths::now_millis()
        ));
        let (size, sha256) = self
            .github
            .http()
            .download_to(&res.asset.download_url, &tmp)
            .await?;

        // If GitHub published a digest, the bytes must match it.
        if let Some(expected) = &upstream_digest {
            if expected != &sha256 {
                let _ = std::fs::remove_file(&tmp);
                return Err(Error::Integrity {
                    name: res.asset.name.clone(),
                    expected: expected.clone(),
                    actual: sha256,
                });
            }
        }

        let unpack = pack.should_unpack(&res.asset.name);
        let stored = self
            .store
            .insert(&sha256, &tmp, &res.asset.name, unpack);
        let _ = std::fs::remove_file(&tmp);
        stored?;

        self.finish_entry(pack, &res, sha256, size).await
    }

    async fn finish_entry(
        &self,
        pack: &CompiledPack,
        res: &Resolution,
        sha256: String,
        size: u64,
    ) -> Result<LockEntry> {
        let attested = self
            .github
            .has_attestation(&res.id, &sha256)
            .await
            .unwrap_or(false);
        let files = self.store.files(&sha256)?;
        let trust = trust::assess(pack, &files, res.repo.as_ref(), attested);

        if !self.policy.permits(&trust) {
            return Err(Error::other(format!(
                "{} is {} ({}) and your policy refuses it",
                res.id,
                trust.level.label(),
                trust.level.explain()
            )));
        }

        Ok(LockEntry {
            id: res.id.clone(),
            version: res.release.tag.clone(),
            asset: res.asset.name.clone(),
            url: res.asset.download_url.clone(),
            sha256,
            size,
            published_at: res.release.published_at.clone(),
            trust,
        })
    }

    // -----------------------------------------------------------------------
    // Deploy
    // -----------------------------------------------------------------------

    pub fn plan(
        &self,
        pack: &CompiledPack,
        profile: &Profile,
        lock: &Lock,
        target: &Target,
        root: &std::path::Path,
    ) -> Result<Plan> {
        deploy::plan(pack, target, root, profile, lock, &self.store)
    }

    pub fn manifest(&self, game: &str, target: &str) -> Result<Option<Manifest>> {
        Manifest::load(&self.paths.manifest_file(game, target))
    }

    /// Is this game running? Cheap enough to call from a UI, and the single
    /// source of truth for the running-game guard.
    pub fn running(
        target: &Target,
        root: &std::path::Path,
    ) -> Option<crate::process::RunningGame> {
        crate::process::find_running(root, &target.processes)
    }

    /// Every path that changes files in a game directory goes through here.
    ///
    /// Locally there is no override: we can see the process list, so a running
    /// game is a hard stop. Remotely we can see nothing, so the only honest
    /// options are to refuse or to let the user assert it — never to assume.
    fn require_closed(
        target: &Target,
        root: &std::path::Path,
        options: DeployOptions,
    ) -> Result<()> {
        if crate::paths::is_network_path(root) {
            return if options.assume_stopped {
                Ok(())
            } else {
                Err(Error::RemoteUnverifiable {
                    target: target.name.clone(),
                    path: root.display().to_string(),
                })
            };
        }
        match Self::running(target, root) {
            Some(found) => Err(Error::GameRunning {
                game: target.name.clone(),
                detail: found.to_string(),
            }),
            None => Ok(()),
        }
    }

    /// Deploy a plan, replacing whatever profile currently occupies the target.
    ///
    /// Config files and other profile-owned state are carried across the swap:
    /// the outgoing profile's edits are captured before its files are removed,
    /// and the incoming profile's saved copies are put back.
    pub fn deploy(
        &self,
        pack: &CompiledPack,
        target: &Target,
        plan: &Plan,
        profile_name: &str,
        options: DeployOptions,
    ) -> Result<DeployReport> {
        Self::require_closed(target, &plan.root, options)?;
        let previous = self.manifest(&plan.game, &plan.target)?;
        let state_dirs = pack.state_dirs(target, &plan.root);

        // 1. The outgoing profile keeps whatever it changed.
        let mut captured = 0;
        let switching = previous
            .as_ref()
            .map(|m| m.profile != profile_name)
            .unwrap_or(false);
        if switching {
            let outgoing = previous.as_ref().expect("checked above").profile.clone();
            for (name, live) in &state_dirs {
                let saved = state::profile_state_dir(&self.paths.profiles, &outgoing, &plan.target)
                    .join(name);
                captured += state::capture(live, &saved)?.files;
                state::clear(live)?;
            }
        }

        // 2. The incoming profile gets its own back.
        let mut restored = 0;
        for (name, live) in &state_dirs {
            let saved =
                state::profile_state_dir(&self.paths.profiles, profile_name, &plan.target)
                    .join(name);
            restored += state::restore(&saved, live)?.files;
        }

        let (manifest, mut report) = deploy::apply(
            plan,
            previous.as_ref(),
            self.store.root(),
            profile_name,
            options.force,
        )?;
        report.captured = captured;
        report.restored = restored;

        manifest.save(&self.paths.manifest_file(&plan.game, &plan.target))?;
        Ok(report)
    }

    /// Remove a deployment entirely, leaving a clean game directory.
    ///
    /// Config edits are captured into the profile first, so reinstalling later
    /// brings your settings back rather than starting from the mod's defaults.
    pub fn undeploy(
        &self,
        game: &str,
        target: &str,
        options: DeployOptions,
    ) -> Result<DeployReport> {
        let path = self.paths.manifest_file(game, target);
        let Some(manifest) = Manifest::load(&path)? else {
            return Ok(DeployReport::default());
        };
        let mut report = DeployReport::default();

        if let Some(pack) = self.pack(game) {
            if let Some(target_def) = pack.target(target) {
                Self::require_closed(target_def, &manifest.root, options)?;
                for (name, live) in pack.state_dirs(target_def, &manifest.root) {
                    let saved = state::profile_state_dir(
                        &self.paths.profiles,
                        &manifest.profile,
                        target,
                    )
                    .join(&name);
                    report.captured += state::capture(&live, &saved)?.files;
                    state::clear(&live)?;
                }
            }
        }

        report.removed = deploy::revert(&manifest, &mut report)?;
        std::fs::remove_file(&path).ok();
        Ok(report)
    }

    // -----------------------------------------------------------------------
    // Profile configs
    // -----------------------------------------------------------------------

    /// Where a profile keeps its saved state for one target, per state path.
    pub fn config_dirs(
        &self,
        pack: &CompiledPack,
        profile: &str,
        target: &Target,
    ) -> Vec<(String, PathBuf)> {
        pack.pack
            .state
            .paths
            .iter()
            .map(|name| {
                (
                    name.clone(),
                    state::profile_state_dir(&self.paths.profiles, profile, &target.id)
                        .join(name),
                )
            })
            .collect()
    }

    /// Config files this profile has saved, as display paths.
    pub fn saved_configs(
        &self,
        pack: &CompiledPack,
        profile: &str,
        target: &Target,
    ) -> Vec<PathBuf> {
        self.config_dirs(pack, profile, target)
            .into_iter()
            .flat_map(|(name, dir)| {
                state::list_files(&dir)
                    .into_iter()
                    .map(move |rel| PathBuf::from(&name).join(rel))
            })
            .collect()
    }

    /// Throw away a profile's saved configs so the next deploy re-seeds the
    /// mods' shipped defaults.
    ///
    /// The defaults are never lost: they live in the content-addressed store,
    /// immutable and keyed by the artifact hash, which is what makes this safe
    /// to offer at all.
    pub fn reset_configs(
        &self,
        pack: &CompiledPack,
        profile: &str,
        target: &Target,
        root: Option<&std::path::Path>,
    ) -> Result<usize> {
        let mut removed = 0;
        for (_, dir) in self.config_dirs(pack, profile, target) {
            removed += state::list_files(&dir).len();
            std::fs::remove_dir_all(&dir).ok();
        }

        // If this profile is the one currently installed, clear the live copies
        // too, otherwise the stale files would just be captured straight back.
        if let Some(root) = root {
            let deployed_here = self
                .manifest(pack.id(), &target.id)?
                .map(|m| m.profile == profile)
                .unwrap_or(false);
            // Only touch the live folder when we can be sure it is safe. When
            // we cannot — the game is running, or it is on another machine —
            // the profile's copy is still reset and the next deploy applies it.
            if deployed_here
                && Self::require_closed(target, root, DeployOptions::default()).is_ok()
            {
                for (_, live) in pack.state_dirs(target, root) {
                    state::clear(&live)?;
                }
            }
        }
        Ok(removed)
    }

    /// Bring configs in from somewhere else.
    pub fn import_configs(
        &self,
        pack: &CompiledPack,
        profile: &str,
        target: &Target,
        source: &ConfigSource,
        root: Option<&std::path::Path>,
    ) -> Result<usize> {
        let mut copied = 0;
        for (name, dest) in self.config_dirs(pack, profile, target) {
            let from = match source {
                ConfigSource::Profile(other) => {
                    state::profile_state_dir(&self.paths.profiles, other, &target.id).join(&name)
                }
                ConfigSource::Game => {
                    let Some(root) = root else {
                        return Err(Error::NotFound(
                            "the game folder for this target".to_string(),
                        ));
                    };
                    match pack.resolve_path(target, &name, root) {
                        Some(path) => path,
                        None => continue,
                    }
                }
                ConfigSource::Folder(path) => path.clone(),
            };
            copied += state::restore(&from, &dest)?.files;
        }

        // Put them into play immediately when this profile is the live one.
        if let (Some(root), true) = (
            root,
            self.manifest(pack.id(), &target.id)?
                .map(|m| m.profile == profile)
                .unwrap_or(false),
        ) {
            if !matches!(source, ConfigSource::Game)
                && Self::require_closed(target, root, DeployOptions::default()).is_ok()
            {
                for (name, live) in pack.state_dirs(target, root) {
                    let saved =
                        state::profile_state_dir(&self.paths.profiles, profile, &target.id)
                            .join(&name);
                    state::restore(&saved, &live)?;
                }
            }
        }
        Ok(copied)
    }

    /// Store entries no profile references any more.
    pub fn gc(&self) -> Result<(usize, u64)> {
        let mut keep: HashSet<String> = HashSet::new();
        if let Ok(entries) = std::fs::read_dir(&self.paths.profiles) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) == Some("json") {
                    if let Ok(lock) = Lock::load(&path) {
                        keep.extend(lock.store_keys());
                    }
                }
            }
        }
        self.store.gc(&keep)
    }
}

/// Human-sized bytes. Used by both front ends.
pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::format_bytes;

    #[test]
    fn scales_units() {
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(2048), "2.0 KB");
        assert_eq!(format_bytes(5 * 1024 * 1024), "5.0 MB");
    }
}
