//! Orchestration: resolve what a profile means, fetch what is missing, and put
//! it in the game directory.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use futures::StreamExt;

use crate::deploy::{self, DeployReport, Manifest, Plan};
use crate::error::{Context, Error, Result};
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

/// A mod that did not end up in the lock, and why.
#[derive(Debug, Clone)]
pub struct SyncIssue {
    pub id: ModId,
    pub message: String,
    /// True when the mod is fine and simply has no build for this profile's
    /// game version or loader yet. It stays in the profile, is skipped on
    /// activation, and picks itself up once a compatible build appears.
    pub waiting: bool,
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
    /// A manually supplied mod whose source now advertises something newer.
    /// Nothing can fetch it for you, but you can be told.
    UpdateAvailable {
        id: ModId,
        have: String,
        latest: String,
        page: String,
    },
    /// A pinned mod that a newer release has overtaken. The pin is doing its
    /// job, so this is not an error — but an update check that stayed silent
    /// here is the reason someone can sit on a stale version for months
    /// believing they are current.
    HeldBack {
        id: ModId,
        have: String,
        latest: String,
    },
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
    pub modrinth: crate::source::modrinth::Modrinth,
    /// GitLab, Gitea and Forgejo, including self-hosted instances.
    pub forge: crate::source::forge::Forge,
    /// Present only when the user has supplied their own CurseForge key.
    pub curseforge: Option<crate::source::curseforge::CurseForge>,
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
    /// The newest release we could have used had the entry not been pinned.
    /// `None` when that is the one we picked, so `Some` always means "this is
    /// being held back". Costs no extra request: the release list was already
    /// fetched to satisfy the pin.
    newer: Option<String>,
}

/// One release a mod could be set to, for choosing a version by hand.
#[derive(Debug, Clone)]
pub struct VersionOption {
    pub tag: String,
    pub name: String,
    pub published_at: String,
    pub prerelease: bool,
    /// The asset this game's pack would install from this release. `None`
    /// means the release exists but carries nothing usable here — shown, and
    /// not selectable, because "that version is not installable" is an answer.
    pub asset: Option<String>,
    pub size: u64,
}

impl Engine {
    pub fn open(paths: Paths, token: Option<String>) -> Result<Self> {
        paths.ensure()?;
        let http = Http::new(paths.http_cache(), token)?;
        // Modrinth and CurseForge take no bearer token, so they get their own
        // client without GitHub's Authorization header attached.
        let plain = Http::new(paths.http_cache(), None)?;
        let curseforge_key = std::fs::read_to_string(paths.curseforge_key_file())
            .ok()
            .map(|k| k.trim().to_string())
            .filter(|k| !k.is_empty())
            .or_else(|| std::env::var("CURSEFORGE_API_KEY").ok())
            .filter(|k| !k.trim().is_empty());

        // Opt-in, stored as a plain marker file so it is obvious and revocable.
        let cf_direct = paths.curseforge_direct_file().exists();

        let (packs, pack_errors) = load_dir(&paths.packs);
        let roots = crate::roots::GlobalRoots::load(&paths.roots_file()).unwrap_or_default();
        Ok(Self {
            store: Store::new(paths.store.clone()),
            modrinth: crate::source::modrinth::Modrinth::new(plain.clone()),
            forge: crate::source::forge::Forge::new(plain.clone()),
            curseforge: curseforge_key
                .map(|key| crate::source::curseforge::CurseForge::new(plain, key, cf_direct)),
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

    /// Save the user's own CurseForge API key.
    ///
    /// Theirs, not ours: Overwolf issues keys after a human review and forbids
    /// sharing them, so one cannot be shipped inside the binary.
    pub fn set_curseforge_key(&mut self, key: &str) -> Result<()> {
        let key = key.trim();
        let path = self.paths.curseforge_key_file();
        if key.is_empty() {
            std::fs::remove_file(&path).ok();
            self.curseforge = None;
            return Ok(());
        }
        crate::paths::write_atomic(&path, key.as_bytes())?;
        let http = Http::new(self.paths.http_cache(), None)?;
        self.curseforge = Some(crate::source::curseforge::CurseForge::new(
            http,
            key.to_string(),
            self.paths.curseforge_direct_file().exists(),
        ));
        Ok(())
    }

    pub fn has_curseforge_key(&self) -> bool {
        self.curseforge.is_some()
    }

    /// Whether blocked CurseForge files are fetched from the CDN.
    pub fn curseforge_direct(&self) -> bool {
        self.paths.curseforge_direct_file().exists()
    }

    /// Turn that on or off. Stored as a marker file so the setting is obvious
    /// on disk and trivially undone.
    pub fn set_curseforge_direct(&mut self, on: bool) -> Result<()> {
        let marker = self.paths.curseforge_direct_file();
        if on {
            crate::paths::write_atomic(&marker, b"on")?;
        } else {
            std::fs::remove_file(&marker).ok();
        }
        // Rebuild the client so the change takes effect without a restart.
        if let Some(key) = std::fs::read_to_string(self.paths.curseforge_key_file())
            .ok()
            .map(|k| k.trim().to_string())
            .filter(|k| !k.is_empty())
        {
            let http = Http::new(self.paths.http_cache(), None)?;
            self.curseforge = Some(crate::source::curseforge::CurseForge::new(http, key, on));
        }
        Ok(())
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
    ) -> Result<(Lock, Vec<SyncIssue>)> {
        let report = report.unwrap_or_else(silent);
        let mut failures: Vec<SyncIssue> = Vec::new();

        // Manually supplied mods have no API to ask. Their lock entry is
        // carried straight through, so an update check neither loses them nor
        // reports them as failures.
        let mut carried: Vec<LockEntry> = Vec::new();
        for entry in profile.mods.iter().filter(|m| m.enabled && m.manual) {
            match previous.get(&entry.id) {
                Some(locked) if self.store.contains(&locked.sha256) => {
                    let mut locked = locked.clone();

                    // Nothing can download it, but the source will still say
                    // what the newest version is — so you do not need their app
                    // running just to learn there is an update.
                    if entry.id.kind == crate::source::SourceKind::CurseForge {
                        if let Some(cf) = &self.curseforge {
                            if let Ok(Some(latest)) = cf
                                .latest_version(
                                    &entry.id,
                                    pack.pack.search.curseforge_game_id,
                                    profile.game_version.as_deref(),
                                )
                                .await
                            {
                                if locked.upstream.as_deref() != Some(latest.as_str()) {
                                    if locked.upstream.is_some() {
                                        report(Event::UpdateAvailable {
                                            id: entry.id.clone(),
                                            have: locked.upstream.clone().unwrap_or_default(),
                                            latest: latest.clone(),
                                            page: entry.id.web_url(),
                                        });
                                    }
                                    locked.upstream = Some(latest);
                                }
                            }
                        }
                    }

                    report(Event::Cached {
                        id: entry.id.clone(),
                        version: locked.version.clone(),
                    });
                    carried.push(locked);
                }
                _ => failures.push(SyncIssue {
                    id: entry.id.clone(),
                    message: "was added from a file, and that file is no longer in the \
                              store — supply it again with `modifile add-file`"
                        .to_string(),
                    waiting: false,
                }),
            }
        }

        let enabled: Vec<_> = profile
            .mods
            .iter()
            .filter(|m| m.enabled && !m.manual)
            .collect();

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

        // Sources that publish one build per game version and loader need to
        // know which the profile is for; GitHub ignores it.
        let filter = crate::source::modrinth::VersionFilter {
            game_version: profile.game_version.clone(),
            loader: profile.loader.clone(),
        };

        // Refuse rather than guess. Picking "the newest" per mod without this
        // happily mixes a NeoForge build of one mod with a Fabric build of
        // another, producing a game that will not start.
        let rules = &pack.pack.versions;
        if !rules.loaders.is_empty() && profile.loader.is_none() {
            return Err(Error::other(format!(
                "`{}` needs a mod loader before anything can be resolved. {} mods are \
                 published as a separate build per loader, and mixing them gives you a \
                 game that will not start. Set one with `modifile set {} --loader <name>`; \
                 options: {}.",
                profile.name,
                pack.pack.game.name,
                profile.name,
                rules.loaders.join(", ")
            )));
        }
        if rules.needs_game_version && profile.game_version.is_none() {
            return Err(Error::other(format!(
                "`{}` needs a game version (for example 1.20.1) before mods can be resolved. \
                 Set one with `modifile set {} --game-version <version>`.",
                profile.name, profile.name
            )));
        }

        let resolutions: Vec<std::result::Result<Resolution, SyncIssue>> =
            futures::stream::iter(enabled.iter().map(|entry| {
                let report = report.clone();
                let targets = targets.clone();
                let filter = filter.clone();
                async move {
                    report(Event::Resolving(entry.id.clone()));
                    match self.resolve_one(pack, &entry.id, entry.pin.as_deref(), entry.prerelease, &targets, &filter).await {
                        Ok(res) => {
                            report(Event::Resolved {
                                id: res.id.clone(),
                                version: res.release.tag.clone(),
                            });
                            // Say so out loud. A pin that quietly refuses every
                            // update looks identical to being up to date, and
                            // the user is the only one who can tell them apart.
                            if let Some(newer) = &res.newer {
                                report(Event::HeldBack {
                                    id: res.id.clone(),
                                    have: res.release.tag.clone(),
                                    latest: newer.clone(),
                                });
                            }
                            Ok(res)
                        }
                        Err(e) => {
                            let waiting = matches!(e, Error::NoBuildFor { .. });
                            if !waiting {
                                report(Event::Failed {
                                    id: entry.id.clone(),
                                    error: e.to_string(),
                                });
                            }
                            Err(SyncIssue {
                                id: entry.id.clone(),
                                message: e.to_string(),
                                waiting,
                            })
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
        let fetched: Vec<std::result::Result<LockEntry, SyncIssue>> =
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
                            Err(SyncIssue {
                                id,
                                message: e.to_string(),
                                waiting: false,
                            })
                        }
                    }
                }
            }))
            .buffer_unordered(DOWNLOAD_CONCURRENCY)
            .collect()
            .await;

        let mut mods = carried;
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

    /// Fetch a mod's releases and project info from whichever source owns it.
    async fn fetch(
        &self,
        id: &ModId,
        filter: &crate::source::modrinth::VersionFilter,
        cf_game: Option<u32>,
    ) -> Result<(Vec<Release>, Option<RepoInfo>)> {
        match id.kind {
            crate::source::SourceKind::GitHub => Ok((
                self.github.releases(id).await?,
                self.github.repo(id).await.unwrap_or(None),
            )),
            crate::source::SourceKind::GitLab | crate::source::SourceKind::Gitea => Ok((
                self.forge.releases(id).await?,
                self.forge.repo(id).await.unwrap_or(None),
            )),
            // Nothing to fetch: the user gave us the bytes. Its lock entry is
            // carried over untouched by `sync`, so this is unreachable in
            // practice and exists only to keep the match honest.
            crate::source::SourceKind::Local => Err(Error::other(format!(
                "{id} was added from a file, so there is nothing to check for updates. \
                 Supply a newer file with `modifile add-file` to update it."
            ))),
            crate::source::SourceKind::Modrinth => Ok((
                self.modrinth.releases(id, filter).await?,
                self.modrinth.project(id).await.unwrap_or(None),
            )),
            crate::source::SourceKind::CurseForge => {
                let Some(cf) = &self.curseforge else {
                    return Err(Error::other(format!(
                        "{id} is on CurseForge, which needs an API key you obtain yourself. \
                         Add one in Settings, or with `modifile auth --curseforge <key>`."
                    )));
                };
                Ok((
                    cf.releases(id, filter.game_version.as_deref(), cf_game).await?,
                    cf.project(id, cf_game).await.unwrap_or(None),
                ))
            }
        }
    }

    async fn resolve_one(
        &self,
        pack: &CompiledPack,
        id: &ModId,
        pin: Option<&str>,
        allow_prerelease: bool,
        targets: &[Target],
        filter: &crate::source::modrinth::VersionFilter,
    ) -> Result<Resolution> {
        let (releases, repo) = self
            .fetch(id, filter, pack.pack.search.curseforge_game_id)
            .await?;
        if releases.is_empty() {
            // "Nothing built for your version yet" is a waiting state, not a
            // broken mod, and the two must not look the same.
            if !filter.is_empty() {
                return Err(Error::NoBuildFor {
                    id: id.to_string(),
                    wanted: filter.describe(),
                });
            }
            return Err(Error::NotFound(match id.kind {
                crate::source::SourceKind::GitHub => format!(
                    "{id} has no GitHub releases — Modifile installs release assets, not \
                     source checkouts"
                ),
                _ => format!("{id} has no downloadable releases"),
            }));
        }

        // The newest release that would have been chosen with no pin in the
        // way. Computed first, and always, so a pinned mod can report what it
        // is holding back from — the alternative is an update check that
        // cheerfully reports "already newest" about a year-old version.
        let newest_usable = Self::newest_usable(pack, &releases, allow_prerelease, targets);

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
                        repo: repo.clone(),
                        newer: newest_usable.filter(|tag| *tag != release.tag.as_str())
                            .map(str::to_string),
                    });
                }
            }
        }

        if !filter.is_empty() && pin.is_none() {
            return Err(Error::NoBuildFor {
                id: id.to_string(),
                wanted: filter.describe(),
            });
        }
        Err(Error::NotFound(match pin {
            Some(p) => format!("{id} has no usable asset on pinned release {p}"),
            None => format!(
                "{id} has releases but none carry an asset this game pack accepts"
            ),
        }))
    }

    /// The tag an unpinned entry would resolve to, or `None` if nothing in the
    /// list carries an asset this pack can use.
    fn newest_usable<'a>(
        pack: &CompiledPack,
        releases: &'a [Release],
        allow_prerelease: bool,
        targets: &[Target],
    ) -> Option<&'a str> {
        releases
            .iter()
            .filter(|r| allow_prerelease || !r.prerelease)
            .find(|r| {
                let names: Vec<String> = r.assets.iter().map(|a| a.name.clone()).collect();
                targets
                    .iter()
                    .any(|target| pack.select_asset(&names, target).is_some())
            })
            .map(|r| r.tag.as_str())
    }

    /// Every release of one mod, annotated with what this profile would
    /// actually install from it.
    ///
    /// This is what lets someone choose a version rather than take whatever
    /// the newest happens to be — the case the pin flag always supported and
    /// nothing ever surfaced, leaving people to guess tag names.
    pub async fn versions(
        &self,
        profile: &Profile,
        id: &ModId,
    ) -> Result<Vec<VersionOption>> {
        let pack = self.pack_for(profile)?;
        let targets: Vec<Target> = self
            .targets(pack, profile)
            .into_iter()
            .map(|(target, _)| target)
            .collect();
        let filter = crate::source::modrinth::VersionFilter {
            game_version: profile.game_version.clone(),
            loader: profile.loader.clone(),
        };

        let (releases, _) = self
            .fetch(id, &filter, pack.pack.search.curseforge_game_id)
            .await?;

        Ok(releases
            .iter()
            .map(|release| {
                let names: Vec<String> = release.assets.iter().map(|a| a.name.clone()).collect();
                let picked = targets
                    .iter()
                    .find_map(|target| pack.select_asset(&names, target));
                VersionOption {
                    tag: release.tag.clone(),
                    name: release.name.clone(),
                    published_at: release.published_at.clone(),
                    prerelease: release.prerelease,
                    asset: picked.map(|i| release.assets[i].name.clone()),
                    size: picked.map(|i| release.assets[i].size).unwrap_or(0),
                }
            })
            .collect())
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
                // Same bytes as last time, but "what is newest upstream" is
                // exactly the thing that moves while nothing else does.
                let mut carried = prev.clone();
                carried.upstream = res.newer.clone();
                return Ok(carried);
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

        // Whatever digest the source published, the bytes must match it.
        // GitHub gives SHA-256; Modrinth gives SHA-512.
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
        if let Some(expected) = res.asset.sha512.as_deref() {
            let actual = crate::hash::sha512_file(&tmp)?;
            if !expected.eq_ignore_ascii_case(&actual) {
                let _ = std::fs::remove_file(&tmp);
                return Err(Error::Integrity {
                    name: res.asset.name.clone(),
                    expected: expected.to_string(),
                    actual,
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
            // Non-null only for a pinned entry something newer has passed, so
            // the mod list can show "held at X, Y available" without going
            // back to the network every time the window is drawn.
            upstream: res.newer.clone(),
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

    /// Look at what is actually in a game's mod folders right now: ours,
    /// changed since we placed it, or put there by something else.
    pub fn scan(
        &self,
        pack: &CompiledPack,
        profile: &Profile,
        lock: &Lock,
        target: &Target,
        root: &std::path::Path,
    ) -> Result<deploy::FolderScan> {
        // What this profile would occupy, so a harmless leftover can be told
        // apart from one that will block an install.
        let planned = self
            .plan(pack, profile, lock, target, root)
            .map(|plan| plan.files.into_iter().map(|f| f.rel).collect())
            .unwrap_or_default();
        let manifest = self.manifest(pack.id(), &target.id)?;
        Ok(deploy::scan(
            pack,
            target,
            root,
            manifest.as_ref(),
            &planned,
        ))
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
        pack: &CompiledPack,
        target: &Target,
        root: &std::path::Path,
        options: DeployOptions,
    ) -> Result<()> {
        // Some games can be modded mid-session. WoW addons are Lua read at load
        // time, so blocking there would be nuisance rather than safety.
        if pack.pack.running.allow_changes {
            return Ok(());
        }
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
        Self::require_closed(pack, target, &plan.root, options)?;
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
        //
        // First activation is a special case: the game folder may already be
        // full of settings from before Modifile existed, and the profile has
        // nothing saved. Those files are adopted rather than ignored —
        // otherwise a profile looks like it has no configs while the game is
        // plainly full of them.
        let mut restored = 0;
        let mut adopted = 0;
        for (name, live) in &state_dirs {
            let saved =
                state::profile_state_dir(&self.paths.profiles, profile_name, &plan.target)
                    .join(name);

            if !switching && state::list_files(&saved).is_empty() {
                adopted += state::capture(live, &saved)?.files;
            }
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
        report.adopted = adopted;

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
                Self::require_closed(pack, target_def, &manifest.root, options)?;
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

        report.removed = deploy::revert(&manifest, &mut report, options.force)?;

        // Keep the record when something was left behind, so the file is still
        // known to have been ours and a later deactivate can finish the job.
        // Dropping it would strand the file as "foreign" forever.
        if report.skipped.is_empty() {
            std::fs::remove_file(&path).ok();
        } else {
            let remaining: Vec<PathBuf> =
                report.skipped.iter().map(|(rel, _)| rel.clone()).collect();
            let mut pruned = manifest.clone();
            pruned.files.retain(|f| remaining.contains(&f.rel));
            // Not active any more — it just remembers what is still lying around.
            pruned.active = false;
            pruned.save(&path)?;
        }
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
                && Self::require_closed(pack, target, root, DeployOptions::default()).is_ok()
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
                && Self::require_closed(pack, target, root, DeployOptions::default()).is_ok()
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

    // -----------------------------------------------------------------------
    // Sharing
    // -----------------------------------------------------------------------

    /// Package a profile into one shareable file.
    pub fn export_profile(
        &self,
        pack: &CompiledPack,
        profile: &Profile,
        include_configs: bool,
        description: String,
    ) -> Result<crate::share::Bundle> {
        let lock = Lock::load(&self.paths.lock_file(&profile.name))?;

        let mut configs = std::collections::BTreeMap::new();
        if include_configs {
            for target in &pack.pack.targets {
                let mut files = std::collections::BTreeMap::new();
                for (name, dir) in self.config_dirs(pack, &profile.name, target) {
                    for rel in state::list_files(&dir) {
                        let display = PathBuf::from(&name)
                            .join(&rel)
                            .to_string_lossy()
                            .replace('\\', "/");
                        files.insert(
                            display,
                            crate::share::ConfigFile::read(&dir.join(&rel))?,
                        );
                    }
                }
                if !files.is_empty() {
                    configs.insert(target.id.clone(), files);
                }
            }
        }

        Ok(crate::share::Bundle::build(profile, &lock, configs, description))
    }

    /// Create a profile from a shared bundle. Returns the name actually used.
    pub fn import_profile(
        &self,
        bundle: &crate::share::Bundle,
        name: Option<&str>,
        pin_versions: bool,
    ) -> Result<String> {
        if self.pack(&bundle.game).is_none() {
            return Err(Error::NotFound(format!(
                "game pack `{}` — this profile is for a game you do not have a pack for",
                bundle.game
            )));
        }

        // Never clobber an existing profile just because a friend's was named
        // the same thing.
        let wanted = name.unwrap_or(&bundle.name);
        let mut chosen = sanitize_name(wanted);
        let mut n = 2;
        while self.paths.profile_file(&chosen).exists() {
            chosen = format!("{}-{n}", sanitize_name(wanted));
            n += 1;
        }

        let profile = bundle.to_profile(&chosen, pin_versions);
        profile.save(&self.paths.profile_file(&chosen))?;
        bundle.write_configs(&self.paths.profiles, &chosen)?;
        Ok(chosen)
    }

    /// Rename a profile and everything that hangs off its name.
    pub fn rename_profile(&self, from: &str, to: &str) -> Result<String> {
        let to = sanitize_name(to);
        if to.is_empty() {
            return Err(Error::other("a profile needs a name"));
        }
        if to == from {
            return Ok(to);
        }
        if self.paths.profile_file(&to).exists() {
            return Err(Error::other(format!("`{to}` already exists")));
        }

        let mut profile = Profile::load(&self.paths.profile_file(from))?;
        profile.name = to.clone();
        profile.save(&self.paths.profile_file(&to))?;
        std::fs::remove_file(self.paths.profile_file(from)).ok();

        // The lock and the saved configs are keyed by name too.
        let old_lock = self.paths.lock_file(from);
        if old_lock.exists() {
            std::fs::rename(&old_lock, self.paths.lock_file(&to)).ok();
        }
        let old_state = self.paths.profiles.join(format!("{from}.state"));
        if old_state.exists() {
            std::fs::rename(&old_state, self.paths.profiles.join(format!("{to}.state"))).ok();
        }

        // And any deployment that says it belongs to the old name, so the
        // "active" marker survives the rename.
        if let Ok(games) = std::fs::read_dir(&self.paths.state) {
            for game in games.flatten() {
                let Ok(entries) = std::fs::read_dir(game.path()) else {
                    continue;
                };
                for entry in entries.flatten() {
                    let path = entry.path();
                    if let Ok(Some(mut manifest)) = Manifest::load(&path) {
                        if manifest.profile == from {
                            manifest.profile = to.clone();
                            let _ = manifest.save(&path);
                        }
                    }
                }
            }
        }
        Ok(to)
    }

    /// Delete a profile: its mod list, its lock, and the settings it was
    /// keeping for you.
    ///
    /// Refused while the profile is active, because deleting it then would
    /// leave its files in the game folder with nothing left that knows they
    /// are there. Deactivate first — callers with a user in front of them
    /// should offer to do both, rather than making them find the other button.
    ///
    /// The downloads themselves are left alone. They are shared by hash with
    /// every other profile, so deciding they are garbage is `gc`'s job, not
    /// this one's.
    pub fn delete_profile(&self, name: &str) -> Result<()> {
        let file = self.paths.profile_file(name);
        if !file.exists() {
            return Err(Error::NotFound(format!("no profile called `{name}`")));
        }

        let profile = Profile::load(&file)?;
        if self.active_profiles(&profile.game).iter().any(|p| p == name) {
            return Err(Error::other(format!(
                "`{name}` is active — its mods are in the game folder right now. \
                 Deactivate it first, so the game goes back to vanilla."
            )));
        }

        std::fs::remove_file(&file).ctx(format!("deleting {}", file.display()))?;
        std::fs::remove_file(self.paths.lock_file(name)).ok();
        std::fs::remove_dir_all(self.paths.profiles.join(format!("{name}.state"))).ok();

        // A deactivated manifest hangs around to remember leftovers it could
        // not remove. One naming a profile that no longer exists is noise, so
        // drop it — unless it still has leftovers to account for.
        if let Ok(entries) = std::fs::read_dir(self.paths.state.join(&profile.game)) {
            for entry in entries.flatten() {
                let path = entry.path();
                if let Ok(Some(manifest)) = Manifest::load(&path) {
                    if manifest.profile == name && !manifest.active && manifest.files.is_empty() {
                        std::fs::remove_file(&path).ok();
                    }
                }
            }
        }
        Ok(())
    }

    /// Which profile currently occupies each target of a game.
    pub fn active_profiles(&self, game: &str) -> Vec<String> {
        let mut out = Vec::new();
        if let Ok(entries) = std::fs::read_dir(self.paths.state.join(game)) {
            for entry in entries.flatten() {
                if let Ok(Some(manifest)) = Manifest::load(&entry.path()) {
                    // A deactivated manifest only remembers leftovers.
                    if manifest.active && !out.contains(&manifest.profile) {
                        out.push(manifest.profile);
                    }
                }
            }
        }
        out
    }

    /// Take a file the user downloaded themselves and manage it like any other
    /// mod: stored by content hash, deployed by hard link, swapped with the
    /// profile, its configs kept.
    ///
    /// This is the way in for mods no API will hand over — a CurseForge project
    /// whose author disabled third-party downloads, a private beta, your own
    /// local build. It costs one drag instead of a manual copy every time.
    pub fn import_file(
        &self,
        pack: &CompiledPack,
        profile: &mut Profile,
        file: &std::path::Path,
        id: Option<ModId>,
    ) -> Result<LockEntry> {
        if !file.is_file() {
            return Err(Error::NotFound(file.display().to_string()));
        }
        let name = file
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "mod".to_string());

        // Default identity is the filename without its extension, which is
        // stable enough to re-import a newer build over the top later.
        let id = id.unwrap_or_else(|| {
            let stem = file
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| name.clone());
            ModId::project(crate::source::SourceKind::Local, sanitize_name(&stem))
        });

        let sha256 = crate::hash::sha256_file(file)?;
        let size = std::fs::metadata(file)?.len();
        let unpack = pack.should_unpack(&name);
        self.store.insert(&sha256, file, &name, unpack)?;

        let files = self.store.files(&sha256)?;
        // No repository to consult, so the assessment rests entirely on what is
        // inside the archive.
        let trust = trust::assess(pack, &files, None, false);

        let entry = LockEntry {
            id: id.clone(),
            version: format!("file: {name}"),
            asset: name,
            url: String::new(),
            sha256,
            size,
            published_at: String::new(),
            // Filled in by the first update check, which is what later
            // notifications compare against.
            upstream: None,
            trust,
        };

        // Mark it manual so update checks leave it alone rather than failing.
        let mut mod_entry = crate::profile::ModEntry::new(id);
        mod_entry.manual = true;
        profile.add(mod_entry);
        Ok(entry)
    }

    /// The loader situation for every target this profile covers.
    ///
    /// Per target, because a dedicated server needs its own copy installed into
    /// its own directory — a client with BepInEx tells you nothing about the
    /// server sitting next to it.
    pub fn loader_states(
        &self,
        pack: &CompiledPack,
        profile: &Profile,
    ) -> Vec<(Target, PathBuf, crate::pack::LoaderDef, crate::loader::LoaderState)> {
        let mut out = Vec::new();
        for (target, root) in self.targets(pack, profile) {
            let Some(root) = root else { continue };
            let Some((def, state)) = self.loader_state(pack, profile, &root) else {
                continue;
            };
            if !def.applies_to(&target.id) {
                continue;
            }
            out.push((target, root, def, state));
        }
        out
    }

    /// What loader is installed in this game directory right now.
    pub fn loader_state(
        &self,
        pack: &CompiledPack,
        profile: &Profile,
        root: &std::path::Path,
    ) -> Option<(crate::pack::LoaderDef, crate::loader::LoaderState)> {
        let def = match profile.loader.as_deref() {
            Some(id) => pack.loader(id)?.clone(),
            // One loader means there is nothing to choose — Valheim has
            // BepInEx and only BepInEx.
            None if pack.pack.loaders.len() == 1 => pack.pack.loaders[0].clone(),
            None => return None,
        };
        let state = crate::loader::detect(
            def.kind,
            &def.prefix,
            &def.page,
            &def.markers,
            &def.install_dir(root),
            root,
            profile.game_version.as_deref(),
        );
        Some((def, state))
    }

    /// Install the profile's mod loader into the game.
    ///
    /// The step people otherwise do by hand before Modifile is any use: go to
    /// the loader's site, download an installer, run it, pick a version.
    pub async fn install_loader(
        &self,
        pack: &CompiledPack,
        profile: &Profile,
        root: &std::path::Path,
    ) -> Result<String> {
        let def = match profile.loader.as_deref() {
            Some(id) => pack.loader(id).ok_or_else(|| {
                Error::NotFound(format!("loader `{id}` in the {} pack", pack.pack.game.name))
            })?,
            None if pack.pack.loaders.len() == 1 => &pack.pack.loaders[0],
            None => {
                return Err(Error::other(format!(
                    "`{}` has no mod loader set yet",
                    profile.name
                )))
            }
        };

        match def.kind {
            crate::pack::LoaderKind::FabricMeta => {
                let game_version = profile.game_version.as_deref().ok_or_else(|| {
                    Error::other(
                        "set the game version first — this loader is built for one"
                            .to_string(),
                    )
                })?;
                crate::loader::install_meta_loader(
                    self.modrinth.http(),
                    &def.meta,
                    &def.prefix,
                    &def.name,
                    root,
                    game_version,
                )
                .await
            }
            // BepInEx and friends: an archive laid over the game root. This is
            // what makes a Unity game read its plugins folder at all — without
            // it, mods install perfectly and the game ignores them.
            crate::pack::LoaderKind::Archive => {
                let source: ModId = def.source.parse()?;
                // The game's own files decide, not ours: a Windows build under
                // Proton needs the Windows loader, and a Linux server needs the
                // Linux one even when driven from a Windows desktop.
                let platform = crate::loader::detect_game_platform(root)
                    .unwrap_or_else(crate::loader::GamePlatform::host);
                let patterns = def.assets_for(platform);
                if patterns.is_empty() {
                    return Err(Error::other(format!(
                        "{} publishes no {} build — install it from {}",
                        def.name,
                        platform.label(),
                        def.page
                    )));
                }

                let (releases, _) = self
                    .fetch(
                        &source,
                        &crate::source::modrinth::VersionFilter::default(),
                        pack.pack.search.curseforge_game_id,
                    )
                    .await?;

                let matchers: Vec<globset::GlobMatcher> = patterns
                    .iter()
                    .filter_map(|p| globset::Glob::new(&p.to_ascii_lowercase()).ok())
                    .map(|g| g.compile_matcher())
                    .collect();

                let (release, asset) = releases
                    .iter()
                    .filter(|r| !r.prerelease)
                    .find_map(|r| {
                        r.assets
                            .iter()
                            .find(|a| {
                                let name = a.name.to_ascii_lowercase();
                                matchers.iter().any(|m| m.is_match(&name))
                            })
                            .map(|a| (r, a))
                    })
                    .ok_or_else(|| {
                        Error::NotFound(format!(
                            "a {} build of {} in {}",
                            platform.label(),
                            def.name,
                            def.source
                        ))
                    })?;

                let tmp = self
                    .paths
                    .downloads()
                    .join(format!("{}-{}.zip", def.id, crate::paths::now_millis()));
                self.github
                    .http()
                    .download_to(&asset.download_url, &tmp)
                    .await?;

                // Its archive ships default configs; a reinstall must not throw
                // away the ones the profile has been editing.
                let preserve: Vec<PathBuf> = pack
                    .pack
                    .state
                    .paths
                    .iter()
                    .filter_map(|name| pack.pack.paths.get(name))
                    .map(PathBuf::from)
                    .collect();

                let dest = def.install_dir(root);
                std::fs::create_dir_all(&dest).ok();
                let written = crate::store::extract_over(&tmp, &dest, &preserve);
                let _ = std::fs::remove_file(&tmp);
                let written = written?;

                Ok(format!(
                    "{} for {} ({written} files)",
                    release.tag,
                    platform.label()
                ))
            }
            crate::pack::LoaderKind::Installer => Err(Error::other(format!(
                "{} has to be installed by its own installer, which patches the game and \
                 must actually run. Get it from {} — Modifile handles the mods either way.",
                def.name, def.page
            ))),
        }
    }

    /// Find mods for a game by name, using whichever index that game's pack
    /// nominates. `check_installable` costs one extra request per hit and says
    /// whether a GitHub repository actually publishes releases.
    pub async fn search(
        &self,
        pack: &CompiledPack,
        query: &str,
        filter: &crate::source::modrinth::VersionFilter,
        check_installable: bool,
    ) -> Result<Vec<crate::source::SearchHit>> {
        let rules = &pack.pack.search;
        if rules.is_empty() {
            return Err(Error::other(format!(
                "the {} pack does not say where to search for mods. Add a [search] section \
                 to it, or paste a mod's URL directly.",
                pack.pack.game.name
            )));
        }

        let mut hits = Vec::new();
        if rules.modrinth {
            hits.extend(self.modrinth.search(query, filter).await.unwrap_or_default());
        }
        // CurseForge is where WoW addons actually are, so search it when the
        // user has a key — even though some results will not be installable.
        if let (Some(cf), Some(game_id)) = (&self.curseforge, rules.curseforge_game_id) {
            hits.extend(cf.search(query, game_id).await.unwrap_or_default());
        }
        if !rules.github_topics.is_empty() || !rules.github_terms.is_empty() {
            let mut found = self
                .github
                .search_repos(query, &rules.github_topics, &rules.github_terms)
                .await
                .unwrap_or_default();

            // "Found it" and "can install it" are different answers, and the
            // second is the one that matters.
            if check_installable {
                for hit in found.iter_mut() {
                    hit.installable = Some(self.github.has_releases(&hit.id).await);
                }
            }
            hits.extend(found);
        }
        Ok(hits)
    }

    /// What is stored and who still wants it.
    pub fn storage(&self) -> Result<crate::storage::StorageReport> {
        let mut active_by_game = std::collections::BTreeMap::new();
        for pack in &self.packs {
            active_by_game.insert(pack.id().to_string(), self.active_profiles(pack.id()));
        }
        crate::storage::report(&self.paths, &self.store, &active_by_game)
    }

    /// Delete one stored download by hash.
    pub fn forget(&self, sha256: &str) -> Result<u64> {
        self.store.remove(sha256)
    }

    /// Store entries no profile references any more.
    ///
    /// A lockfile we cannot parse aborts the whole thing. Treating it as "no
    /// references" would delete downloads that are very much still needed, and
    /// an unreadable file is not evidence of anything.
    pub fn gc(&self) -> Result<(usize, u64)> {
        let mut keep: HashSet<String> = HashSet::new();
        for entry in std::fs::read_dir(&self.paths.profiles)
            .ctx(format!("reading {}", self.paths.profiles.display()))?
            .flatten()
        {
            let path = entry.path();
            if path
                .file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.ends_with(".lock.json"))
                .unwrap_or(false)
            {
                keep.extend(Lock::load(&path)?.store_keys());
            }
        }
        self.store.gc(&keep)
    }
}

/// A profile name becomes a filename, so it cannot contain path separators or
/// anything Windows refuses.
pub fn sanitize_name(name: &str) -> String {
    name.trim()
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '-',
            c if c.is_control() => '-',
            c => c,
        })
        .collect::<String>()
        .trim_matches('.')
        .trim()
        .to_string()
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
    use super::*;
    use crate::pack::Pack;
    use crate::source::Asset;

    #[test]
    fn scales_units() {
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(2048), "2.0 KB");
        assert_eq!(format_bytes(5 * 1024 * 1024), "5.0 MB");
    }

    fn wow() -> CompiledPack {
        let (_, body) = crate::BUNDLED_PACKS
            .iter()
            .find(|(n, _)| *n == "wow.toml")
            .expect("bundled wow pack");
        CompiledPack::new(toml::from_str::<Pack>(body).expect("parses")).expect("compiles")
    }

    fn release(tag: &str, assets: &[&str], prerelease: bool) -> Release {
        Release {
            tag: tag.to_string(),
            name: tag.to_string(),
            published_at: String::new(),
            prerelease,
            assets: assets
                .iter()
                .map(|name| Asset {
                    name: name.to_string(),
                    download_url: String::new(),
                    size: 1,
                    digest: None,
                    sha512: None,
                })
                .collect(),
            web_url: String::new(),
        }
    }

    /// The bug this whole thing exists to fix: a pinned entry resolved fine and
    /// reported nothing, so an update check on an imported profile said
    /// "already newest" about a version three releases behind.
    #[test]
    fn a_pin_knows_what_it_is_holding_back() {
        let pack = wow();
        let targets = vec![pack.target("client").expect("client target").clone()];
        let releases = vec![
            release("5.20.1", &["WeakAuras-5.20.1.zip"], false),
            release("5.19.0", &["WeakAuras-5.19.0.zip"], false),
            release("5.18.0", &["WeakAuras-5.18.0.zip"], false),
        ];

        let newest = Engine::newest_usable(&pack, &releases, false, &targets);
        assert_eq!(newest, Some("5.20.1"));
    }

    /// A release with nothing installable in it must not be reported as the
    /// newest, or the UI would nag about an update that cannot be taken.
    #[test]
    fn newest_usable_skips_releases_with_no_asset_for_this_game() {
        let pack = wow();
        let targets = vec![pack.target("client").expect("client target").clone()];
        let releases = vec![
            release("6.0.0", &["source-code.tar.gz"], false),
            release("5.20.1", &["WeakAuras-5.20.1.zip"], false),
        ];

        assert_eq!(
            Engine::newest_usable(&pack, &releases, false, &targets),
            Some("5.20.1")
        );
    }

    #[test]
    fn newest_usable_ignores_prereleases_unless_asked() {
        let pack = wow();
        let targets = vec![pack.target("client").expect("client target").clone()];
        let releases = vec![
            release("6.0.0-beta", &["WeakAuras-6.0.0-beta.zip"], true),
            release("5.20.1", &["WeakAuras-5.20.1.zip"], false),
        ];

        assert_eq!(
            Engine::newest_usable(&pack, &releases, false, &targets),
            Some("5.20.1")
        );
        assert_eq!(
            Engine::newest_usable(&pack, &releases, true, &targets),
            Some("6.0.0-beta")
        );
    }

    fn scratch(tag: &str) -> Paths {
        let dir = std::env::temp_dir().join(format!(
            "modifile-engine-{tag}-{}",
            crate::paths::now_millis()
        ));
        let paths = Paths::rooted(&dir);
        paths.ensure().expect("scratch dirs");
        paths
    }

    #[test]
    fn deleting_a_profile_takes_its_lock_and_settings_with_it() {
        let paths = scratch("delete");
        let engine = Engine::open(paths.clone(), None).expect("engine");

        let profile = Profile::new("raiding", "wow");
        profile
            .save(&paths.profile_file("raiding"))
            .expect("save profile");
        Lock::default()
            .save(&paths.lock_file("raiding"))
            .expect("save lock");
        let state = paths.profiles.join("raiding.state");
        std::fs::create_dir_all(state.join("client")).expect("state dir");

        engine.delete_profile("raiding").expect("delete");

        assert!(!paths.profile_file("raiding").exists());
        assert!(!paths.lock_file("raiding").exists());
        assert!(!state.exists());
        std::fs::remove_dir_all(&paths.home).ok();
    }

    /// Deleting an active profile would leave its files in the game folder
    /// with nothing left that knows they are there.
    #[test]
    fn an_active_profile_is_not_deleted_out_from_under_the_game() {
        let paths = scratch("delete-active");
        let engine = Engine::open(paths.clone(), None).expect("engine");

        let profile = Profile::new("raiding", "wow");
        profile
            .save(&paths.profile_file("raiding"))
            .expect("save profile");

        let manifest_path = paths.manifest_file("wow", "client");
        std::fs::create_dir_all(manifest_path.parent().expect("parent")).expect("state dir");
        Manifest {
            game: "wow".into(),
            target: "client".into(),
            profile: "raiding".into(),
            root: paths.home.join("game"),
            mode: None,
            deployed_ms: 0,
            active: true,
            files: Vec::new(),
            created_dirs: Vec::new(),
        }
        .save(&manifest_path)
        .expect("save manifest");

        let err = engine.delete_profile("raiding").unwrap_err().to_string();
        assert!(err.contains("Deactivate"), "{err}");
        assert!(paths.profile_file("raiding").exists(), "profile survived");
        std::fs::remove_dir_all(&paths.home).ok();
    }
}
