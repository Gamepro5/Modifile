//! Orchestration: resolve what a profile means, fetch what is missing, and put
//! it in the game directory.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
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

/// Thunderstore's own category name for modpacks. It is the same across every
/// community, and it is the only thing distinguishing a pack from a mod there.
const MODPACK_CATEGORY: &str = "Modpacks";

/// How many repositories to query at once. GitHub tolerates this comfortably
/// and it turns a 40-mod update check from a minute into a couple of seconds.
const RESOLVE_CONCURRENCY: usize = 8;
/// Downloads are heavier, so fewer at a time.
const DOWNLOAD_CONCURRENCY: usize = 4;

/// The largest settings file Modifile will open in its own editor.
///
/// Configs are human-scale — the biggest a mod writes is a few hundred
/// kilobytes. Anything past this is a cache or a database that something
/// dropped in the config folder, and pouring it into a text box would hang the
/// window rather than help.
const MAX_CONFIG_BYTES: u64 = 2 * 1024 * 1024;

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

/// Where a modpack archive comes from.
#[derive(Debug, Clone)]
pub enum ModpackSource {
    /// A file on disk. Both stores hand you a zip, so this is the common case.
    Path(PathBuf),
    /// A direct link to the archive.
    Url(String),
    /// A CurseForge project and one of its files.
    CurseForgeFile { project: u64, file: u64 },
    /// A Modrinth version id, whose primary file is the `.mrpack`.
    ModrinthVersion(String),
    /// A Modrinth project, taking whatever its newest version is.
    ModrinthProject(String),
    /// A CurseForge project, taking whatever its newest file is.
    CurseForgeProject(u64),
    /// A Thunderstore package, at an exact version or the newest one.
    ThunderstorePackage {
        namespace: String,
        name: String,
        version: Option<String>,
    },
}

impl ModpackSource {
    /// Work out what the user typed.
    ///
    /// Guessing is safe here in a way it usually is not: a modpack arrives
    /// either as a file you downloaded or as a link you copied, and the two
    /// are never confusable. Anything that is not a URL is a path, and a bad
    /// path fails immediately with the name you gave it.
    pub fn parse(input: &str) -> Self {
        let trimmed = input.trim();

        // The ids search prints, so that what it tells you to run actually
        // runs. `modrinth:sodium-pack`, `curseforge:123456`.
        if let Some(rest) = trimmed.strip_prefix("modrinth:").map(str::trim) {
            if !rest.is_empty() {
                return ModpackSource::ModrinthProject(rest.to_string());
            }
        }
        if let Some(rest) = trimmed.strip_prefix("curseforge:").map(str::trim) {
            if let Ok(id) = rest.parse::<u64>() {
                return ModpackSource::CurseForgeProject(id);
            }
        }
        // `thunderstore:Ns/Name` from search output, and Thunderstore's own
        // `Namespace-Name-Version` spelling, which is what a pack's dependency
        // list and its download page both use.
        for prefix in ["thunderstore:", "ts:"] {
            if let Some(rest) = trimmed.strip_prefix(prefix).map(str::trim) {
                let mut parts = rest.split('/').filter(|p| !p.is_empty());
                if let (Some(ns), Some(name)) = (parts.next(), parts.next()) {
                    return ModpackSource::ThunderstorePackage {
                        namespace: ns.to_string(),
                        name: name.to_string(),
                        version: parts.next().map(str::to_string),
                    };
                }
                if let Some((ns, name, version)) =
                    crate::source::thunderstore::split_dependency(rest)
                {
                    return ModpackSource::ThunderstorePackage {
                        namespace: ns,
                        name,
                        version: Some(version),
                    };
                }
            }
        }

        let bare = trimmed
            .trim_start_matches("https://")
            .trim_start_matches("http://")
            .trim_start_matches("www.");
        // modrinth.com/modpack/<slug>/version/<id> — the page someone copies
        // out of the address bar when they mean "this exact pack version" —
        // and modrinth.com/modpack/<slug>, which means "the newest one".
        if let Some(rest) = bare.strip_prefix("modrinth.com/") {
            if let Some((_, id)) = rest.split_once("/version/") {
                let id = id.split(['/', '?', '#']).next().unwrap_or(id);
                if !id.is_empty() {
                    return ModpackSource::ModrinthVersion(id.to_string());
                }
            }
            let mut parts = rest.split('/').filter(|p| !p.is_empty());
            if let (Some("modpack"), Some(slug)) = (parts.next(), parts.next()) {
                let slug = slug.split(['?', '#']).next().unwrap_or(slug);
                if !slug.is_empty() {
                    return ModpackSource::ModrinthProject(slug.to_string());
                }
            }
        }

        // thunderstore.io/package/<ns>/<name>[/<version>]/ and the
        // /c/<community>/p/<ns>/<name>/ form.
        if let Some(rest) = bare.strip_prefix("thunderstore.io/") {
            let parts: Vec<&str> = rest.split('/').filter(|p| !p.is_empty()).collect();
            let found = match parts.as_slice() {
                // `/package/download/<ns>/<name>/<ver>/` is a direct archive
                // link and is better handled as a plain URL.
                ["package", "download", ..] => None,
                ["package", ns, name, rest @ ..] => Some((*ns, *name, rest.first().copied())),
                ["c", _community, "p", ns, name, rest @ ..] => {
                    Some((*ns, *name, rest.first().copied()))
                }
                _ => None,
            };
            if let Some((ns, name, version)) = found {
                return ModpackSource::ThunderstorePackage {
                    namespace: ns.to_string(),
                    name: name.to_string(),
                    version: version
                        .map(|v| v.split(['?', '#']).next().unwrap_or(v).to_string())
                        .filter(|v| !v.is_empty()),
                };
            }
        }

        if trimmed.starts_with("https://") || trimmed.starts_with("http://") {
            return ModpackSource::Url(trimmed.to_string());
        }
        ModpackSource::Path(PathBuf::from(trimmed))
    }
}

/// A pack archive that is now in the store, and what we learned fetching it.
#[derive(Debug, Clone)]
struct StagedPack {
    sha256: String,
    size: u64,
    /// The file name it arrived under, which is what the game pack's asset
    /// rules match against.
    name: String,
    /// Thunderstore's community slug, when the pack came from there. The only
    /// thing that says which game a Thunderstore pack is for.
    community: Option<String>,
}

/// What importing a modpack produced.
///
/// Deliberately detailed. A pack is a few hundred decisions somebody else made
/// on your behalf, and the difference between "installed 312 mods" and knowing
/// which four could not be fetched is the difference between a game that starts
/// and an evening of guessing.
#[derive(Debug, Clone, Default)]
pub struct ModpackReport {
    /// The profile that was created, which may be a de-duplicated name.
    pub profile: String,
    /// The game pack it was created for. Together with `profile` this is the
    /// profile's full identity — the name alone does not say which game.
    pub game: String,
    pub pack: String,
    pub format: String,
    pub game_version: Option<String>,
    pub loader: Option<String>,
    /// The loader build the pack was tested against. Modifile installs the
    /// newest stable of that loader rather than this exact one, so it is worth
    /// saying which was asked for.
    pub loader_version: Option<String>,
    /// Mods added to the profile, pinned to the pack's versions.
    pub mods: usize,
    /// Files the pack named that no index could attribute to a project. Taken
    /// as direct downloads and held by hash, so they still install — but
    /// nothing can check them for updates.
    pub untraced: usize,
    /// Files the pack named that could not be taken at all, and why.
    pub skipped: Vec<(String, String)>,
    /// Files in the pack's own overrides tree.
    pub overrides: usize,
    /// Things worth saying that are not failures — a loader version that will
    /// not be pinned, a loader this game pack does not list.
    pub notes: Vec<String>,
    /// What the trust ladder made of the pack's overrides.
    pub trust: Option<TrustReport>,
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
    /// Progress that belongs to a whole modpack rather than to one mod.
    ///
    /// Every other variant is keyed by a `ModId`, which is right for a sync
    /// and useless for "reading the manifest" or "matching 312 files to their
    /// projects" — work that is neither instant nor attributable to any one
    /// entry, and which is silent without this.
    Pack { stage: String, detail: String },
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
    /// Keyless, and the index for most BepInEx games.
    pub thunderstore: crate::source::thunderstore::Thunderstore,
    /// GitLab, Gitea and Forgejo, including self-hosted instances.
    pub forge: crate::source::forge::Forge,
    /// Present only when the user has supplied their own CurseForge key.
    pub curseforge: Option<crate::source::curseforge::CurseForge>,
    pub packs: Vec<CompiledPack>,
    pub pack_errors: Vec<(PathBuf, Error)>,
    pub policy: TrustPolicy,
    pub roots: crate::roots::GlobalRoots,
}

/// What one profile entry asks the resolver for.
///
/// Grouped rather than passed loose because the four travel together and three
/// of them are easy to transpose at a call site.
#[derive(Debug, Clone, Copy)]
struct Want<'a> {
    id: &'a ModId,
    /// Hold at this release tag.
    pin: Option<&'a str>,
    /// The source's own handle for one exact artifact, where a tag is not
    /// enough — a CurseForge `fileID`.
    file: Option<&'a str>,
    allow_prerelease: bool,
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
        // Modrinth, Thunderstore and CurseForge take no bearer token, so they
        // get their own client without GitHub's Authorization header attached.
        let plain = Http::new(paths.http_cache(), None)?;
        let curseforge_key = std::fs::read_to_string(paths.curseforge_key_file())
            .ok()
            .map(|k| k.trim().to_string())
            .filter(|k| !k.is_empty())
            .or_else(|| std::env::var("CURSEFORGE_API_KEY").ok())
            .filter(|k| !k.trim().is_empty());

        // Opt-in, stored as a plain marker file so it is obvious and revocable.
        let cf_direct = paths.curseforge_direct_file().exists();

        // Likewise for "install mods that publish no source at all". This is
        // read here rather than set by each front end, because it was only the
        // GUI that honoured it — so the same profile the window would install
        // was refused from the command line, with no way to say otherwise.
        let policy = TrustPolicy {
            minimum: if paths.allow_no_source_file().exists() {
                crate::trust::TrustLevel::Blocked
            } else {
                TrustPolicy::default().minimum
            },
            ..TrustPolicy::default()
        };

        // Profiles moved from one flat directory to one per game, so that a
        // name only has to be unique within its own game. A no-op once there
        // is nothing left at the top level.
        crate::profile::migrate_flat_layout(&paths.profiles);

        let (packs, pack_errors) = load_dir(&paths.packs);
        let roots = crate::roots::GlobalRoots::load(&paths.roots_file()).unwrap_or_default();
        Ok(Self {
            store: Store::new(paths.store.clone()),
            modrinth: crate::source::modrinth::Modrinth::new(plain.clone()),
            thunderstore: crate::source::thunderstore::Thunderstore::new(plain.clone()),
            forge: crate::source::forge::Forge::new(plain.clone()),
            curseforge: curseforge_key
                .map(|key| crate::source::curseforge::CurseForge::new(plain, key, cf_direct)),
            github: GitHub::new(http),
            packs,
            pack_errors,
            policy,
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
    /// Whether mods that publish no source at all may be installed.
    pub fn allows_no_source(&self) -> bool {
        self.paths.allow_no_source_file().exists()
    }

    /// Allow, or stop allowing, mods that publish no source code anywhere.
    ///
    /// Stored as a marker file rather than a settings key, for the same reason
    /// the CurseForge direct-download switch is: it is trivially inspectable
    /// and trivially undone, and a setting that weakens a safety default
    /// should not be buried where nobody can find it again.
    pub fn set_allow_no_source(&mut self, on: bool) -> Result<()> {
        let path = self.paths.allow_no_source_file();
        if on {
            crate::paths::write_atomic(
                &path,
                b"Mods that publish no source code at all may be installed.\n\
                  Delete this file to go back to refusing them.\n",
            )?;
            self.policy.minimum = crate::trust::TrustLevel::Blocked;
        } else {
            let _ = std::fs::remove_file(&path);
            self.policy.minimum = TrustPolicy::default().minimum;
        }
        Ok(())
    }

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
                    let want = Want {
                        id: &entry.id,
                        pin: entry.pin.as_deref(),
                        file: entry.file.as_deref(),
                        allow_prerelease: entry.prerelease,
                    };
                    match self.resolve_one(pack, want, &targets, &filter).await {
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
    /// Every release of one mod, plus whatever the source says about it.
    ///
    /// `file` narrows to a single exact artifact when the caller already knows
    /// which one it wants — a modpack naming a CurseForge `fileID`. Without it
    /// the source's ordinary listing is returned, which for CurseForge is only
    /// the newest fifty files.
    async fn fetch(
        &self,
        id: &ModId,
        filter: &crate::source::modrinth::VersionFilter,
        pack: &CompiledPack,
        pin: Option<&str>,
        file: Option<&str>,
    ) -> Result<(Vec<Release>, Option<RepoInfo>)> {
        let cf_game = pack.pack.search.curseforge_game_id;
        let community = pack.pack.search.thunderstore_community.as_deref();
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
            // Thunderstore's API publishes the newest version and any exact
            // one, but no history — so a pin is resolved directly rather than
            // by searching a list. That also keeps a modpack's several hundred
            // pinned dependencies to one small request each, instead of the
            // tens of megabytes its community listing would cost.
            // One request, not two: the version record already carries the
            // description and source link that a second `project` call would
            // fetch, and a modpack asking twice per mod is what tips
            // Thunderstore into rate-limiting.
            crate::source::SourceKind::Thunderstore => {
                self.thunderstore.resolve(id, pin, community).await
            }
            crate::source::SourceKind::CurseForge => {
                let Some(cf) = &self.curseforge else {
                    return Err(Error::other(format!(
                        "{id} is on CurseForge, which needs an API key you obtain yourself. \
                         Add one in Settings, or with `modifile auth --curseforge <key>`."
                    )));
                };
                // A pack named one exact file. Ask for that file rather than
                // hoping it is still among the project's newest fifty.
                if let Some(file_id) = file.and_then(|f| f.parse::<u64>().ok()) {
                    let project = id.repo.parse::<u64>().map_err(|_| {
                        Error::other(format!(
                            "{id} is pinned to CurseForge file {file_id}, but its project \
                             id is not numeric — the pin cannot be resolved."
                        ))
                    })?;
                    return Ok((
                        vec![cf.release_for_file(project, file_id).await?],
                        cf.project(id, cf_game).await.unwrap_or(None),
                    ));
                }
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
        want: Want<'_>,
        targets: &[Target],
        filter: &crate::source::modrinth::VersionFilter,
    ) -> Result<Resolution> {
        let Want {
            id,
            pin,
            file,
            allow_prerelease,
        } = want;
        let (mut releases, repo) = self
            .fetch(id, filter, pack, pin, file)
            .await?;

        // The newest release we could have used had the entry not been pinned.
        // Computed here, from the *filtered* list, and kept — so that widening
        // the search below to honour a pin cannot turn "3.1.4 is out" into a
        // suggestion to install a build for a different game version.
        let newest_usable =
            Self::newest_usable(pack, &releases, allow_prerelease, targets).map(str::to_string);

        // A pin is a statement that this exact version is wanted. The version
        // filter is there to choose among *unpinned* candidates, and must not
        // be able to hide a release someone explicitly asked for.
        //
        // This is not a corner case: a modpack pins every one of its mods, and
        // pack authors routinely ship a library whose metadata lists only the
        // previous game version. Without this, importing a pack reports a
        // dozen of its mods as "no build for your version yet" while the pack
        // itself runs fine.
        if let Some(wanted) = pin {
            if !filter.is_empty() && !releases.iter().any(|r| r.tag == wanted) {
                if let Ok((wide, _)) = self
                    .fetch(
                        id,
                        &crate::source::modrinth::VersionFilter::default(),
                        pack,
                        pin,
                        file,
                    )
                    .await
                {
                    if wide.iter().any(|r| r.tag == wanted) {
                        releases = wide;
                    }
                }
            }
        }

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

        // An exact file id already named one artifact, so there is nothing
        // left to filter — and a pack is entitled to pin a beta if that is
        // what its author tested against.
        let exact = file.is_some();

        // Walk back through releases until one carries an asset we can use.
        // A tag with no build attached is common and should not be fatal.
        for release in releases.iter().filter(|r| {
            exact || pin.map(|p| r.tag == p).unwrap_or(allow_prerelease || !r.prerelease)
        }) {
            let names: Vec<String> = release.assets.iter().map(|a| a.name.clone()).collect();
            for target in targets {
                if let Some(idx) = pack.select_asset(&names, target) {
                    return Ok(Resolution {
                        id: id.clone(),
                        release: release.clone(),
                        asset: release.assets[idx].clone(),
                        repo: repo.clone(),
                        newer: newest_usable
                            .clone()
                            .filter(|tag| tag.as_str() != release.tag.as_str()),
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

        // Deliberately not narrowed to a pinned file: this is the list someone
        // opens to move *off* whatever a modpack pinned them to.
        let (releases, _) = self
            .fetch(id, &filter, pack, None, None)
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

    /// What Activate would install: files in the game folder, where the game
    /// finds them however it is started.
    ///
    /// Deliberately not instanced. Instancing is Play's mechanism, not the
    /// program's default — a game that declares `[instance]` is saying it
    /// *can* be pointed elsewhere, not that Activate should point it there.
    /// Conflating the two emptied `BepInEx/plugins` on every activate and left
    /// a Steam launch running vanilla.
    pub fn plan(
        &self,
        pack: &CompiledPack,
        profile: &Profile,
        lock: &Lock,
        target: &Target,
        root: &std::path::Path,
    ) -> Result<Plan> {
        deploy::plan(pack, target, root, None, profile, lock, &self.store)
    }

    /// What Play would install: the profile's own tree, leaving the game
    /// folder as close to vanilla as the loader allows.
    pub fn plan_instanced(
        &self,
        pack: &CompiledPack,
        profile: &Profile,
        lock: &Lock,
        target: &Target,
        root: &std::path::Path,
    ) -> Result<Plan> {
        let instance = self.instance_for(pack, profile);
        deploy::plan(
            pack,
            target,
            root,
            instance.as_deref(),
            profile,
            lock,
            &self.store,
        )
    }

    /// Where this profile's own tree goes, if it has one.
    ///
    /// A profile is instanced when its game can be pointed at a directory of
    /// its own. That is a property of the game, declared in its pack — not a
    /// per-profile choice, because a game either supports the redirection or
    /// it does not.
    pub fn instance_for(
        &self,
        pack: &CompiledPack,
        profile: &Profile,
    ) -> Option<std::path::PathBuf> {
        pack.instancing()?;
        Some(self.paths.instance_dir(&profile.id()))
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
        profile: &crate::profile::ProfileId,
        options: DeployOptions,
    ) -> Result<DeployReport> {
        let profile_name = profile.name.as_str();
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
            // The outgoing profile belongs to this same game, so its id is the
            // manifest's name scoped to the game being deployed.
            let outgoing = crate::profile::ProfileId::new(
                &plan.game,
                previous.as_ref().expect("checked above").profile.clone(),
            );
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
                state::profile_state_dir(&self.paths.profiles, profile, &plan.target)
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
                        &crate::profile::ProfileId::new(game, &manifest.profile),
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
        profile: &crate::profile::ProfileId,
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
        profile: &crate::profile::ProfileId,
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

    /// Resolve one of a profile's saved settings files to a real path.
    ///
    /// `rel` is a display path in the form `saved_configs` returns: the state
    /// path's own name, then the file beneath it. Both halves are checked. The
    /// first has to be a state path this pack actually declares, and the rest
    /// has to be plain names — no `..`, no root, no drive letter. This path is
    /// about to be read and written, and it arrives from the UI, so it is not
    /// taken on trust.
    pub fn config_path(
        &self,
        pack: &CompiledPack,
        profile: &crate::profile::ProfileId,
        target: &Target,
        rel: &std::path::Path,
    ) -> Result<PathBuf> {
        let mut parts = rel.components();
        let head = match parts.next() {
            Some(std::path::Component::Normal(head)) => head.to_string_lossy().into_owned(),
            _ => {
                return Err(Error::other(format!(
                    "`{}` does not name a settings file",
                    rel.display()
                )));
            }
        };

        let Some((_, dir)) = self
            .config_dirs(pack, profile, target)
            .into_iter()
            .find(|(name, _)| *name == head)
        else {
            return Err(Error::other(format!(
                "`{head}` is not a settings folder for {}",
                pack.pack.game.name
            )));
        };

        let mut path = dir;
        let mut named_a_file = false;
        for part in parts {
            match part {
                std::path::Component::Normal(name) => {
                    path.push(name);
                    named_a_file = true;
                }
                // `..`, `/`, `C:` — the ways a path climbs out of where it is
                // supposed to be. A settings file has no use for any of them.
                _ => {
                    return Err(Error::other(format!(
                        "`{}` leaves the settings folder",
                        rel.display()
                    )));
                }
            }
        }
        if !named_a_file {
            return Err(Error::other(format!(
                "`{}` names a folder, not a settings file",
                rel.display()
            )));
        }
        Ok(path)
    }

    /// Read one of a profile's settings files as text, for editing.
    ///
    /// Refuses anything that is not text. Mods keep settings in `.toml`,
    /// `.json`, `.cfg` and `.properties`, but they also drop caches and
    /// databases in the same folders, and showing one of those in a text box
    /// would offer to save mojibake back over a working file.
    pub fn read_config(
        &self,
        pack: &CompiledPack,
        profile: &crate::profile::ProfileId,
        target: &Target,
        rel: &std::path::Path,
    ) -> Result<String> {
        let path = self.config_path(pack, profile, target, rel)?;
        let size = std::fs::metadata(&path)
            .ctx(format!("reading {}", path.display()))?
            .len();
        if size > MAX_CONFIG_BYTES {
            return Err(Error::other(format!(
                "{} is {}, too large to edit here — open it in a text editor",
                rel.display(),
                format_bytes(size)
            )));
        }
        let bytes = std::fs::read(&path).ctx(format!("reading {}", path.display()))?;
        String::from_utf8(bytes).map_err(|_| {
            Error::other(format!("{} is not a text file", rel.display()))
        })
    }

    /// Save an edited settings file back to the profile.
    ///
    /// The profile's own copy is the source of truth and is always written.
    /// When this profile is the one currently installed, the live file is
    /// written too, so the change is in effect without a redeploy — but only
    /// when the game is closed, on the same rule every other write to a game
    /// folder follows. When it is not, the profile's copy still holds the edit
    /// and the next deploy applies it.
    ///
    /// Returns whether the live copy was updated, because that is the
    /// difference between "this is in effect" and "this applies next time".
    pub fn write_config(
        &self,
        pack: &CompiledPack,
        profile: &crate::profile::ProfileId,
        target: &Target,
        rel: &std::path::Path,
        text: &str,
        root: Option<&std::path::Path>,
    ) -> Result<bool> {
        let path = self.config_path(pack, profile, target, rel)?;
        crate::paths::write_atomic(&path, text.as_bytes())?;

        let Some(root) = root else { return Ok(false) };
        let deployed_here = self
            .manifest(pack.id(), &target.id)?
            .map(|m| m.profile == profile.name)
            .unwrap_or(false);
        if !deployed_here
            || Self::require_closed(pack, target, root, DeployOptions::default()).is_err()
        {
            return Ok(false);
        }

        // Same split as `config_path`, against the live folder this time.
        let mut parts = rel.components();
        let Some(std::path::Component::Normal(head)) = parts.next() else {
            return Ok(false);
        };
        let Some((_, live)) = pack
            .state_dirs(target, root)
            .into_iter()
            .find(|(name, _)| std::path::Path::new(name) == std::path::Path::new(head))
        else {
            return Ok(false);
        };
        let mut live = live;
        live.extend(parts);
        crate::paths::write_atomic(&live, text.as_bytes())?;
        Ok(true)
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
        profile: &crate::profile::ProfileId,
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
                .map(|m| m.profile == profile.name)
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
        profile: &crate::profile::ProfileId,
        target: &Target,
        source: &ConfigSource,
        root: Option<&std::path::Path>,
    ) -> Result<usize> {
        let mut copied = 0;
        for (name, dest) in self.config_dirs(pack, profile, target) {
            let from = match source {
                // Copying settings between profiles only makes sense within one
                // game, so the source is named rather than fully qualified.
                ConfigSource::Profile(other) => state::profile_state_dir(
                    &self.paths.profiles,
                    &crate::profile::ProfileId::new(&profile.game, other),
                    &target.id,
                )
                .join(&name),
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
                .map(|m| m.profile == profile.name)
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
        let lock = Lock::load(&self.paths.lock_file(&profile.id()))?;

        let mut configs = std::collections::BTreeMap::new();
        if include_configs {
            for target in &pack.pack.targets {
                let mut files = std::collections::BTreeMap::new();
                for (name, dir) in self.config_dirs(pack, &profile.id(), target) {
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

    /// Write a profile out as a `.mfpack`.
    pub fn export_mfpack(
        &self,
        pack: &CompiledPack,
        profile: &Profile,
        include_configs: bool,
        description: String,
        path: &std::path::Path,
    ) -> Result<crate::share::Bundle> {
        let bundle = self.export_profile(pack, profile, include_configs, description)?;
        crate::mfpack::write(path, &bundle)?;
        Ok(bundle)
    }

    /// Open a `.mfpack`.
    ///
    /// Decided by what the file *is* rather than what it is called, so a pack
    /// that lost its extension on the way through a chat client still opens.
    ///
    /// The single-file JSON bundle that came before is not read. It is still
    /// *recognised*, because refusing it by name is the difference between an
    /// answer and a zip parse error — but recognising a format is not
    /// supporting it, and nothing here will open one.
    pub fn read_shared(&self, path: &std::path::Path) -> Result<crate::share::Bundle> {
        if crate::mfpack::looks_like_pack(path) {
            return crate::mfpack::read(path);
        }
        if looks_like_old_bundle(path) {
            return Err(Error::other(format!(
                "`{}` is the old single-file profile, which Modifile no longer reads. \
                 Ask whoever sent it to export again — the current format is a .mfpack, \
                 and it carries the loader and game version that one could not.",
                path.display()
            )));
        }
        Err(Error::other(format!(
            "`{}` is not a Modifile pack. A .mfpack is a zip holding {}.",
            path.display(),
            crate::mfpack::INDEX_NAME
        )))
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
        let chosen = self.free_name(&bundle.game, wanted);
        let id = crate::profile::ProfileId::new(&bundle.game, &chosen);

        let profile = bundle.to_profile(&chosen, pin_versions);
        profile.save(&self.paths.profile_file(&id))?;
        bundle.write_configs(&self.paths.profiles, &id)?;
        Ok(chosen)
    }

    /// Every profile on this machine, as `game/name`.
    pub fn all_profiles(&self) -> Vec<crate::profile::ProfileId> {
        let mut out = Vec::new();
        let Ok(games) = std::fs::read_dir(&self.paths.profiles) else {
            return out;
        };
        for game in games.flatten() {
            if !game.path().is_dir() {
                continue;
            }
            let Ok(entries) = std::fs::read_dir(game.path()) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("toml") {
                    continue;
                }
                if let Ok(profile) = Profile::load(&path) {
                    out.push(profile.id());
                }
            }
        }
        out.sort();
        out
    }

    /// Turn what someone typed into one profile.
    ///
    /// Accepts `game/name`, and a bare `name` when only one game has it —
    /// which is almost always, so the short form keeps working. When two games
    /// both have that name the answer is to say so and list them, rather than
    /// pick one and be wrong half the time.
    pub fn resolve_profile(&self, spec: &str) -> Result<crate::profile::ProfileId> {
        let spec = spec.trim();
        if let Some((game, name)) = spec.split_once('/') {
            let id = crate::profile::ProfileId::new(game.trim(), name.trim());
            if self.paths.profile_file(&id).exists() {
                return Ok(id);
            }
            return Err(Error::NotFound(format!("profile `{id}`")));
        }

        let matches: Vec<_> = self
            .all_profiles()
            .into_iter()
            .filter(|id| id.name == spec)
            .collect();

        match matches.as_slice() {
            [only] => Ok(only.clone()),
            [] => Err(Error::NotFound(format!("a profile called `{spec}`"))),
            many => Err(Error::other(format!(
                "`{spec}` is a profile in {} games — say which: {}.",
                many.len(),
                many.iter()
                    .map(|id| id.qualified())
                    .collect::<Vec<_>>()
                    .join(", ")
            ))),
        }
    }

    /// A name for this game that nothing is using yet.
    ///
    /// Only within the game: two games may each have a `main`, which is the
    /// whole point of scoping names to their game.
    pub fn free_name(&self, game: &str, wanted: &str) -> String {
        let base = sanitize_name(wanted);
        let base = if base.is_empty() {
            "profile".to_string()
        } else {
            base
        };
        let mut chosen = base.clone();
        let mut n = 2;
        while self
            .paths
            .profile_file(&crate::profile::ProfileId::new(game, &chosen))
            .exists()
        {
            chosen = format!("{base}-{n}");
            n += 1;
        }
        chosen
    }

    /// Rename a profile and everything that hangs off its name.
    ///
    /// Within its own game: renaming cannot move a profile to another game,
    /// because its mods were resolved against this one.
    pub fn rename_profile(
        &self,
        from: &crate::profile::ProfileId,
        to: &str,
    ) -> Result<crate::profile::ProfileId> {
        let to = crate::profile::ProfileId::new(&from.game, sanitize_name(to));
        if to.name.is_empty() {
            return Err(Error::other("a profile needs a name"));
        }
        if to == *from {
            return Ok(to);
        }
        if self.paths.profile_file(&to).exists() {
            return Err(Error::other(format!(
                "`{}` already has a profile called `{}`",
                from.game, to.name
            )));
        }

        let mut profile = Profile::load(&self.paths.profile_file(from))?;
        profile.name = to.name.clone();
        profile.save(&self.paths.profile_file(&to))?;
        std::fs::remove_file(self.paths.profile_file(from)).ok();

        // The lock, the saved configs and any instance are keyed by name too.
        let old_lock = self.paths.lock_file(from);
        if old_lock.exists() {
            std::fs::rename(&old_lock, self.paths.lock_file(&to)).ok();
        }
        let dir = self.paths.profile_dir(&from.game);
        let old_state = dir.join(format!("{}.state", from.name));
        if old_state.exists() {
            std::fs::rename(&old_state, dir.join(format!("{}.state", to.name))).ok();
        }
        let old_instance = self.paths.instance_dir(from);
        if old_instance.exists() {
            std::fs::rename(&old_instance, self.paths.instance_dir(&to)).ok();
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
                        // A manifest names its game, so only this game's
                        // deployments are touched — another game's `main` is
                        // a different profile and must be left alone.
                        if manifest.game == from.game && manifest.profile == from.name {
                            manifest.profile = to.name.clone();
                            let _ = manifest.save(&path);
                        }
                    }
                }
            }
        }
        Ok(to)
    }

    /// Re-checksum everything this profile has in the store.
    ///
    /// Reads every stored byte, so it is an explicit action rather than
    /// something that happens on the way to somewhere else.
    pub fn store_health(&self, lock: &Lock) -> Vec<(ModId, crate::store::EntryHealth)> {
        lock.mods
            .iter()
            .map(|entry| (entry.id.clone(), self.store.check(&entry.sha256)))
            .collect()
    }

    /// Throw away stored copies whose bytes no longer match what was
    /// downloaded, so the next sync fetches them again.
    ///
    /// This is the only cure for a store entry that a deployed hard link was
    /// written through: the bytes are gone, and no amount of re-linking them
    /// into the game folder brings the mod back.
    pub fn discard_damaged(&self, lock: &Lock) -> Result<Vec<ModId>> {
        let mut discarded = Vec::new();
        for (id, health) in self.store_health(lock) {
            if let crate::store::EntryHealth::Damaged { .. } = health {
                if let Some(entry) = lock.get(&id) {
                    self.store.discard(&entry.sha256)?;
                    discarded.push(id);
                }
            }
        }
        Ok(discarded)
    }

    /// Remove this target's deployed files that have been changed since we
    /// placed them, so the next activate restores them.
    pub fn drop_tampered(&self, game: &str, target: &str) -> Result<(usize, Vec<PathBuf>)> {
        let path = self.paths.manifest_file(game, target);
        let Some(manifest) = Manifest::load(&path)? else {
            return Ok((0, Vec::new()));
        };
        Ok(crate::deploy::drop_modified(&manifest))
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
    pub fn delete_profile(&self, id: &crate::profile::ProfileId) -> Result<()> {
        let file = self.paths.profile_file(id);
        if !file.exists() {
            return Err(Error::NotFound(format!("no profile called `{id}`")));
        }

        let profile = Profile::load(&file)?;
        if self.active_profiles(&profile.game).contains(&id.name) {
            return Err(Error::other(format!(
                "`{}` is active — its mods are in the game folder right now. \
                 Deactivate it first, so the game goes back to vanilla.",
                id.name
            )));
        }

        std::fs::remove_file(&file).ctx(format!("deleting {}", file.display()))?;
        std::fs::remove_file(self.paths.lock_file(id)).ok();
        std::fs::remove_dir_all(
            self.paths
                .profile_dir(&id.game)
                .join(format!("{}.state", id.name)),
        )
        .ok();
        // An instance is this profile's alone and goes with it.
        std::fs::remove_dir_all(self.paths.instance_dir(id)).ok();

        // A deactivated manifest hangs around to remember leftovers it could
        // not remove. One naming a profile that no longer exists is noise, so
        // drop it — unless it still has leftovers to account for.
        if let Ok(entries) = std::fs::read_dir(self.paths.state.join(&profile.game)) {
            for entry in entries.flatten() {
                let path = entry.path();
                if let Ok(Some(manifest)) = Manifest::load(&path) {
                    if manifest.profile == id.name
                        && !manifest.active
                        && manifest.files.is_empty()
                    {
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

    // -----------------------------------------------------------------------
    // Updating Modifile itself
    // -----------------------------------------------------------------------

    /// Whether to look for new versions of Modifile on startup.
    pub fn checks_for_updates(&self) -> bool {
        !self.paths.no_update_check_file().exists()
    }

    /// Whether a found update is installed without asking.
    pub fn auto_updates(&self) -> bool {
        self.paths.auto_update_file().exists()
    }

    pub fn set_update_checks(&self, on: bool) -> Result<()> {
        toggle_marker(
            &self.paths.no_update_check_file(),
            // Inverted: the file means "do not check".
            !on,
            b"Modifile will not look for new versions of itself.\n\
              Delete this file to turn the check back on.\n",
        )
    }

    pub fn set_auto_update(&self, on: bool) -> Result<()> {
        toggle_marker(
            &self.paths.auto_update_file(),
            on,
            b"Modifile installs its own updates without asking.\n\
              Delete this file to be asked first.\n",
        )
    }

    /// Is there a newer Modifile?
    pub async fn check_for_update(&self) -> Result<Option<crate::selfupdate::Available>> {
        crate::selfupdate::check(&self.github).await
    }

    /// Download, verify and install an update.
    ///
    /// Returns which binaries were replaced. The running program is not
    /// restarted — that is the caller's business, and on a desktop app it is
    /// the user's.
    pub async fn install_update(
        &self,
        available: &crate::selfupdate::Available,
    ) -> Result<crate::selfupdate::Applied> {
        let dir = crate::selfupdate::install_dir()?;

        // Fail before downloading 12 MB rather than after.
        let probe = dir.join(format!(".modifile-write-test-{}", crate::paths::now_millis()));
        std::fs::write(&probe, b"x").map_err(|e| {
            Error::other(format!(
                "cannot write to {} ({e}), so the update cannot be installed there. \
                 If Modifile lives somewhere privileged, download {} yourself and \
                 unpack it over the top.",
                dir.display(),
                available.asset.name
            ))
        })?;
        let _ = std::fs::remove_file(&probe);

        let archive = crate::selfupdate::stage(
            self.github.http(),
            available,
            &self.paths.updates(),
        )
        .await?;

        let report = crate::selfupdate::apply(&archive, &dir);
        // The archive is large and has done its job either way.
        let _ = std::fs::remove_file(&archive);
        report
    }

    /// Remove binaries parked aside by a previous update. Called at startup.
    pub fn tidy_after_update(&self) -> usize {
        crate::selfupdate::install_dir()
            .map(|dir| crate::selfupdate::cleanup(&dir))
            .unwrap_or(0)
    }

    // -----------------------------------------------------------------------
    // Playing
    // -----------------------------------------------------------------------

    /// The user's own launch commands.
    pub fn launch_settings(&self) -> crate::launch::LaunchSettings {
        crate::launch::LaunchSettings::load(&self.paths.launch_file())
    }

    pub fn set_launch_command(&self, game: &str, command: Option<&str>) -> Result<()> {
        let mut settings = self.launch_settings();
        settings.set(game, command);
        settings.save(&self.paths.launch_file())
    }

    /// How this target would be started, without starting it.
    ///
    /// Used to say what Play will do before it does it, and to grey out a
    /// button that could not work.
    pub fn launch_method(
        &self,
        pack: &CompiledPack,
        target: &Target,
        root: &Path,
    ) -> Option<crate::launch::LaunchMethod> {
        let settings = self.launch_settings();
        crate::launch::resolve(target, root, settings.get(pack.id()))
    }

    /// Start the game with this profile's instance.
    ///
    /// Play only exists for games that can be pointed at a directory of their
    /// own. That is what makes it safe: the mods are in the instance, the game
    /// folder is untouched, and quitting — or crashing, or losing power —
    /// leaves nothing to undo. A game that cannot do that is refused here
    /// rather than given a Play button that quietly modifies the install.
    pub fn play(
        &self,
        pack: &CompiledPack,
        profile: &Profile,
        target: &Target,
        root: &Path,
    ) -> Result<crate::launch::LaunchMethod> {
        let Some(rules) = pack.instancing() else {
            return Err(Error::other(format!(
                "{} cannot be launched with a profile. It reads its mods from one fixed \
                 place inside its own folder, so there is no way to point it somewhere \
                 else for a single run — Modifile would have to modify the install and \
                 put it back afterwards, and an interrupted session would leave it \
                 modified. Activate `{}` and start the game yourself instead.",
                pack.pack.game.name, profile.name
            )));
        };

        let manifest = self.manifest(pack.id(), &target.id)?;
        let deployed = manifest
            .as_ref()
            .filter(|m| m.active && m.profile == profile.name)
            .is_some();
        if !deployed {
            return Err(Error::other(format!(
                "`{}` has not been set up for {} yet, so starting the game would start \
                 it unmodded. Activate it first.",
                profile.name, target.name
            )));
        }

        let instance = self
            .instance_for(pack, profile)
            .ok_or_else(|| Error::other("this profile has no instance directory"))?;

        let args = match rules.kind {
            crate::pack::InstanceKind::Doorstop => {
                let target_assembly = instance.join(
                    rules
                        .target
                        .as_deref()
                        .unwrap_or("BepInEx/core/BepInEx.Preloader.dll"),
                );
                if !target_assembly.is_file() {
                    return Err(Error::other(format!(
                        "this profile's mod loader is not installed in its own folder yet \
                         — {} is missing. Install the loader for `{}` and try again.",
                        target_assembly.display(),
                        profile.name
                    )));
                }
                crate::launch::LaunchArgs::doorstop(&target_assembly)
            }
            // Minecraft's launcher owns the process; Modifile writes a profile
            // pointing at the instance and opens the launcher.
            crate::pack::InstanceKind::MinecraftLauncher => {
                crate::loader::write_game_dir_profile(root, &profile.name, &instance)?;
                crate::launch::LaunchArgs::default()
            }
        };

        let method = self
            .launch_method(pack, target, root)
            .ok_or_else(|| crate::launch::no_method(pack.id(), &target.name))?;
        crate::launch::spawn(&method, root, &args)?;
        Ok(method)
    }

    // -----------------------------------------------------------------------
    // Modpacks
    // -----------------------------------------------------------------------

    /// Fetch a URL straight into the content-addressed store.
    ///
    /// The missing primitive: `acquire` can only be reached through a
    /// `Resolution`, which assumes a mod resolved from a source. A modpack
    /// needs "these exact bytes, from this exact URL" — for the pack archive
    /// itself, and for the files inside a pack that belong to no index.
    ///
    /// Deliberately on the keyless client. The one that carries the GitHub
    /// token has no business talking to a mod CDN.
    pub async fn import_url(
        &self,
        url: &str,
        name: &str,
        unpack: bool,
        expect_sha512: Option<&str>,
    ) -> Result<(String, u64)> {
        let tmp = self.paths.downloads().join(format!(
            "url-{}-{}",
            sanitize_name(name),
            crate::paths::now_millis()
        ));
        let (size, sha256) = self.modrinth.http().download_to(url, &tmp).await?;

        // A pack that publishes a hash gets checked against it. This is the
        // only integrity claim these files carry.
        if let Some(expected) = expect_sha512 {
            let actual = crate::hash::sha512_file(&tmp)?;
            if !expected.eq_ignore_ascii_case(&actual) {
                let _ = std::fs::remove_file(&tmp);
                return Err(Error::Integrity {
                    name: name.to_string(),
                    expected: expected.to_string(),
                    actual,
                });
            }
        }

        let stored = self.store.insert(&sha256, &tmp, name, unpack);
        let _ = std::fs::remove_file(&tmp);
        stored?;
        Ok((sha256, size))
    }

    /// Get a modpack archive into the store, whichever way it was named.
    ///
    /// Returns its hash, its size, the file name it arrived under, and — for a
    /// Thunderstore package — the community that lists it, which is the only
    /// thing saying which game the pack is for.
    async fn stage_modpack(
        &self,
        source: &ModpackSource,
        community: Option<&str>,
    ) -> Result<StagedPack> {
        let plain = |sha: String, size: u64, name: String| StagedPack {
            sha256: sha,
            size,
            name,
            community: None,
        };
        match source {
            ModpackSource::Path(path) => {
                if !path.is_file() {
                    return Err(Error::NotFound(path.display().to_string()));
                }
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "modpack.zip".to_string());
                let sha256 = crate::hash::sha256_file(path)?;
                let size = std::fs::metadata(path)?.len();
                self.store.insert(&sha256, path, &name, true)?;
                Ok(plain(sha256, size, name))
            }
            ModpackSource::Url(url) => {
                let name = url
                    .rsplit('/')
                    .next()
                    .filter(|n| !n.is_empty())
                    .unwrap_or("modpack.zip")
                    .split('?')
                    .next()
                    .unwrap_or("modpack.zip")
                    .to_string();
                let (sha, size) = self.import_url(url, &name, true, None).await?;
                Ok(plain(sha, size, name))
            }
            ModpackSource::CurseForgeFile { project, file } => {
                let cf = self.curseforge.as_ref().ok_or_else(curseforge_key_needed)?;
                let release = cf.release_for_file(*project, *file).await?;
                let asset = release.assets.into_iter().next().ok_or_else(|| {
                    Error::NotFound(format!("a downloadable file for CurseForge {file}"))
                })?;
                let (sha, size) = self
                    .import_url(&asset.download_url, &asset.name, true, None)
                    .await?;
                Ok(plain(sha, size, asset.name))
            }
            ModpackSource::ModrinthVersion(id) => {
                let asset = self.modrinth.version_primary_file(id).await?;
                let (sha, size) = self
                    .import_url(
                        &asset.download_url,
                        &asset.name,
                        true,
                        asset.sha512.as_deref(),
                    )
                    .await?;
                Ok(plain(sha, size, asset.name))
            }
            ModpackSource::ModrinthProject(slug) => {
                let asset = self.modrinth.newest_version_file(slug).await?;
                let (sha, size) = self
                    .import_url(
                        &asset.download_url,
                        &asset.name,
                        true,
                        asset.sha512.as_deref(),
                    )
                    .await?;
                Ok(plain(sha, size, asset.name))
            }
            ModpackSource::CurseForgeProject(project) => {
                let cf = self.curseforge.as_ref().ok_or_else(curseforge_key_needed)?;
                let id = ModId::project(
                    crate::source::SourceKind::CurseForge,
                    project.to_string(),
                );
                let asset = cf
                    .releases(&id, None, None)
                    .await?
                    .into_iter()
                    .find_map(|r| r.assets.into_iter().next())
                    .ok_or_else(|| {
                        Error::NotFound(format!(
                            "a downloadable file for CurseForge project {project}"
                        ))
                    })?;
                let (sha, size) = self
                    .import_url(&asset.download_url, &asset.name, true, None)
                    .await?;
                Ok(plain(sha, size, asset.name))
            }
            ModpackSource::ThunderstorePackage {
                namespace,
                name,
                version,
            } => {
                let id = ModId {
                    kind: crate::source::SourceKind::Thunderstore,
                    owner: namespace.clone(),
                    repo: name.clone(),
                    host: None,
                };
                // One request either way, and it carries the community that
                // says which game this pack is for.
                let package = self
                    .thunderstore
                    .package(&id, community)
                    .await?
                    .ok_or_else(|| {
                        Error::NotFound(format!("Thunderstore package {namespace}/{name}"))
                    })?;
                let listed_in = package.community().map(str::to_string);

                let chosen = match version {
                    Some(v) if v != &package.latest.version_number => self
                        .thunderstore
                        .version(&id, v)
                        .await?
                        .ok_or_else(|| {
                            Error::NotFound(format!(
                                "Thunderstore version {v} of {namespace}/{name}"
                            ))
                        })?,
                    _ => package.latest.clone(),
                };

                let asset_name = chosen.asset_name();
                let (sha, size) = self
                    .import_url(&chosen.download_url, &asset_name, true, None)
                    .await?;
                Ok(StagedPack {
                    sha256: sha,
                    size,
                    name: asset_name,
                    community: listed_in.or_else(|| community.map(str::to_string)),
                })
            }
        }
    }

    /// Get a pack into the store and read its index. Shared by import and
    /// inspect, so that looking before you leap costs nothing the second time:
    /// the archive is content-addressed, so importing it afterwards downloads
    /// nothing again.
    ///
    /// `game` is the caller's answer to "which game is this for", which only
    /// matters for Thunderstore: a CurseForge or Modrinth pack states its game
    /// in the format itself, and a Thunderstore pack does not state it at all.
    async fn read_modpack(
        &self,
        source: &ModpackSource,
        game: Option<&str>,
        report: &Reporter,
    ) -> Result<(crate::modpack::PackPlan, StagedPack, Vec<crate::store::StoredFile>)> {
        report(Event::Pack {
            stage: "Reading".to_string(),
            detail: "fetching the pack archive".to_string(),
        });
        // When the caller named a game, its Thunderstore community is the
        // fallback route for packages whose per-package endpoint misbehaves.
        let community = game
            .and_then(|id| self.pack(id))
            .and_then(|p| p.pack.search.thunderstore_community.clone());
        let staged = self.stage_modpack(source, community.as_deref()).await?;

        let stored = self.store.files(&staged.sha256)?;
        let rels: Vec<String> = stored.iter().map(|f| f.rel.clone()).collect();
        let Some(index_rel) = crate::modpack::find_index(&rels) else {
            // One of ours, opened by the wrong door. Saying "not a modpack"
            // about a Modifile pack is both wrong and a dead end.
            if rels.iter().any(|rel| rel == crate::mfpack::INDEX_NAME) {
                return Err(Error::other(format!(
                    "`{}` is a Modifile pack, which this route does not read — it is for \
                     CurseForge, Modrinth and Thunderstore packs. Open it with \
                     `modifile import` instead.",
                    staged.name
                )));
            }
            return Err(Error::other(format!(
                "`{}` is not a modpack — it holds no manifest.json (CurseForge or \
                 Thunderstore) and no modrinth.index.json (.mrpack). If it is a single \
                 mod, add it with `modifile add-file` instead.",
                staged.name
            )));
        };
        let index_path = stored
            .iter()
            .find(|f| f.rel == index_rel)
            .map(|f| f.abs.clone())
            .ok_or_else(|| Error::NotFound(index_rel.clone()))?;
        let raw = std::fs::read(&index_path).ctx(format!("reading {index_rel}"))?;

        // The file name does not settle it: CurseForge and Thunderstore both
        // call their index manifest.json.
        let format = crate::modpack::sniff(&index_rel, &raw).ok_or_else(|| {
            Error::other(format!(
                "`{index_rel}` in `{}` is not a modpack index in any format Modifile \
                 reads — CurseForge, Modrinth `.mrpack` or Thunderstore.",
                staged.name
            ))
        })?;

        let plan = match format {
            crate::modpack::ModpackFormat::CurseForge => {
                crate::modpack::CurseForgeManifest::parse(&raw)?.to_plan()
            }
            crate::modpack::ModpackFormat::Modrinth => {
                crate::modpack::ModrinthIndex::parse(&raw)?.to_plan()?
            }
            crate::modpack::ModpackFormat::Thunderstore => {
                let game = self.thunderstore_game(game, staged.community.as_deref())?;
                crate::modpack::ThunderstoreManifest::parse(&raw)?.to_plan(&game)?
            }
        };

        Ok((plan, staged, stored))
    }

    /// How many files in the pack archive this game would actually install.
    ///
    /// Asked of the game pack's own install rules rather than by looking for
    /// an `overrides/` directory, because only two of the three formats have
    /// one. A Thunderstore package keeps its files at the archive root, and
    /// they are installed by exactly the same rules as any other package's —
    /// which is the whole reason a pack needs no special handling downstream.
    fn pack_own_files(
        &self,
        plan: &crate::modpack::PackPlan,
        stored: &[crate::store::StoredFile],
    ) -> usize {
        match self.pack(&plan.game) {
            Some(pack) => stored
                .iter()
                .filter(|f| {
                    pack.pack
                        .targets
                        .iter()
                        .any(|t| pack.rule_for(&f.rel, &t.id).is_some())
                })
                .count(),
            // No game pack installed to ask, so fall back to what the format
            // itself declares.
            None => {
                let prefixes = override_prefixes(plan);
                stored.iter().filter(|f| under_any(&f.rel, &prefixes)).count()
            }
        }
    }

    /// Which game a Thunderstore pack belongs to.
    ///
    /// Thunderstore packages carry no game anywhere — not in the manifest, not
    /// in the archive. The community that lists the package is the only signal,
    /// and a pack handed over as a plain file does not even have that. So this
    /// takes what it can get and otherwise asks, rather than guessing and
    /// installing a Valheim pack into Lethal Company.
    fn thunderstore_game(&self, explicit: Option<&str>, community: Option<&str>) -> Result<String> {
        if let Some(id) = explicit {
            return match self.pack(id) {
                Some(pack) => Ok(pack.id().to_string()),
                None => Err(Error::NotFound(format!("game pack `{id}`"))),
            };
        }

        if let Some(community) = community {
            if let Some(pack) = self.packs.iter().find(|p| {
                p.pack
                    .search
                    .thunderstore_community
                    .as_deref()
                    .is_some_and(|c| c.eq_ignore_ascii_case(community))
            }) {
                return Ok(pack.id().to_string());
            }
            return Err(Error::other(format!(
                "this pack is listed under Thunderstore's `{community}` community, and no \
                 game pack of yours claims it. Add `thunderstore_community = \"{community}\"` \
                 to that game's [search] section, or say which game with --game."
            )));
        }

        // A Thunderstore package in a file has nothing at all to go on.
        let known: Vec<&str> = self
            .packs
            .iter()
            .filter(|p| p.pack.search.thunderstore_community.is_some())
            .map(|p| p.id())
            .collect();
        Err(Error::other(format!(
            "this is a Thunderstore pack, and a Thunderstore package does not record which \
             game it is for. Say which with --game. {}",
            if known.is_empty() {
                "No game pack of yours declares a Thunderstore community yet.".to_string()
            } else {
                format!("Games set up for Thunderstore: {}.", known.join(", "))
            }
        )))
    }

    /// Read a modpack and describe it, without creating anything.
    ///
    /// Needs no CurseForge key: counting what a pack asks for is reading its
    /// index, and only fetching the files it names needs one.
    pub async fn inspect_modpack(
        &self,
        source: ModpackSource,
        game: Option<&str>,
    ) -> Result<ModpackReport> {
        let report = silent();
        let (plan, _staged, stored) = self.read_modpack(&source, game, &report).await?;

        Ok(ModpackReport {
            profile: String::new(),
            game: plan.game.clone(),
            pack: plan.label(),
            format: plan.format_name.clone(),
            game_version: plan.game_version.clone(),
            loader: plan.loader.clone(),
            loader_version: plan.loader_version.clone(),
            mods: plan.entries.len(),
            untraced: 0,
            skipped: Vec::new(),
            overrides: self.pack_own_files(&plan, &stored),
            notes: match self.pack(&plan.game) {
                Some(_) => Vec::new(),
                None => vec![format!(
                    "you have no game pack for `{}`, so this cannot be imported yet. \
                     Run `modifile init` to write the bundled ones.",
                    plan.game
                )],
            },
            trust: None,
        })
    }

    /// Find modpacks, rather than mods.
    ///
    /// A separate entry point rather than a flag on `search`, because the two
    /// answer different questions and returning them mixed is how someone ends
    /// up installing a 300-mod pack believing it was one addon.
    pub async fn search_modpacks(
        &self,
        pack: &CompiledPack,
        query: &str,
    ) -> Result<Vec<crate::source::SearchHit>> {
        let rules = &pack.pack.search;
        let mut hits = Vec::new();

        if rules.modrinth {
            hits.extend(
                self.modrinth
                    .search_type(
                        query,
                        &crate::source::modrinth::VersionFilter::default(),
                        Some("modpack"),
                    )
                    .await
                    .unwrap_or_default(),
            );
        }
        if let (Some(cf), Some(game_id), Some(class)) = (
            &self.curseforge,
            rules.curseforge_game_id,
            rules.curseforge_modpack_class_id,
        ) {
            hits.extend(
                cf.search(query, game_id, Some(class))
                    .await
                    .unwrap_or_default(),
            );
        }
        // Thunderstore marks packs with its own category, and this is the only
        // one of the three that carries packs for games other than Minecraft.
        if let Some(community) = &rules.thunderstore_community {
            hits.extend(
                self.thunderstore
                    .search(community, query, Some(MODPACK_CATEGORY))
                    .await
                    .unwrap_or_default(),
            );
        }

        if hits.is_empty()
            && !rules.modrinth
            && rules.curseforge_modpack_class_id.is_none()
            && rules.thunderstore_community.is_none()
        {
            return Err(Error::other(format!(
                "the {} pack does not say where to look for modpacks. Add `modrinth = true`, \
                 `curseforge_modpack_class_id` or `thunderstore_community` to its [search] \
                 section, or point `modifile pack add` straight at a file or link.",
                pack.pack.game.name
            )));
        }
        Ok(hits)
    }

    /// Read a modpack and write it out as a profile.
    ///
    /// A pack is a profile that somebody else assembled: a game version, a
    /// loader, a pinned mod list and a tree of files for the game directory.
    /// So this creates exactly that and stops. Nothing is downloaded beyond
    /// the pack itself and whatever no index could account for — the mods are
    /// fetched by the next `sync`, through the same resolve, verify and trust
    /// path every other mod takes. A pack gets no shortcut past the trust
    /// ladder just for arriving in bulk.
    pub async fn import_modpack(
        &self,
        source: ModpackSource,
        name: Option<&str>,
        game: Option<&str>,
        report: Option<Reporter>,
    ) -> Result<ModpackReport> {
        let report = report.unwrap_or_else(silent);
        let (plan, staged, stored) = self.read_modpack(&source, game, &report).await?;
        let (archive_sha, archive_size, archive_name) =
            (staged.sha256.clone(), staged.size, staged.name.clone());

        let pack = self.pack(&plan.game).ok_or_else(|| {
            Error::NotFound(format!(
                "a game pack for `{}` — this modpack is for a game you have no pack for. \
                 Run `modifile init` to write the bundled ones.",
                plan.game
            ))
        })?;

        let mut out = ModpackReport {
            pack: plan.label(),
            format: plan.format_name.clone(),
            game_version: plan.game_version.clone(),
            loader: plan.loader.clone(),
            loader_version: plan.loader_version.clone(),
            ..Default::default()
        };

        report(Event::Pack {
            stage: "Reading".to_string(),
            detail: format!(
                "{} — {} files, {} {}",
                plan.label(),
                plan.entries.len(),
                plan.loader.clone().unwrap_or_else(|| "no loader".into()),
                plan.game_version.clone().unwrap_or_default()
            ),
        });

        // --- the profile it becomes -------------------------------------
        let wanted = name.filter(|n| !n.trim().is_empty()).unwrap_or(&plan.name);
        let chosen = self.free_name(pack.id(), wanted);
        let profile_id = crate::profile::ProfileId::new(pack.id(), &chosen);

        let mut profile = Profile::new(&chosen, pack.id());
        profile.game_version = plan.game_version.clone();
        profile.loader = plan.loader.clone();

        // A loader this game pack has never heard of would fail much later,
        // during a sync, with an error about the profile rather than the pack.
        if let Some(loader) = &plan.loader {
            if !pack.pack.versions.loaders.is_empty()
                && !pack
                    .pack
                    .versions
                    .loaders
                    .iter()
                    .any(|l| l.eq_ignore_ascii_case(loader))
            {
                out.notes.push(format!(
                    "the pack asks for the `{loader}` loader, which the {} pack does not \
                     list. Known loaders: {}.",
                    pack.pack.game.name,
                    pack.pack.versions.loaders.join(", ")
                ));
            }
        }
        if let Some(v) = &plan.loader_version {
            out.notes.push(format!(
                "the pack was built against {} {v}. Modifile installs the newest stable \
                 build of that loader rather than pinning this one.",
                plan.loader.clone().unwrap_or_else(|| "the loader".into())
            ));
        }

        let side_ids = |kind: crate::pack::TargetKind| -> Vec<String> {
            pack.pack
                .targets
                .iter()
                .filter(|t| t.kind == kind)
                .map(|t| t.id.clone())
                .collect()
        };
        let client_ids = side_ids(crate::pack::TargetKind::Client);
        let server_ids = side_ids(crate::pack::TargetKind::Server);

        let mut lock = Lock {
            profile: chosen.clone(),
            generated_ms: crate::paths::now_millis(),
            mods: Vec::new(),
        };

        // --- CurseForge entries -----------------------------------------
        let cf_wanted: Vec<(u64, u64)> = plan
            .entries
            .iter()
            .filter_map(|e| match e {
                crate::modpack::PackEntry::CurseForge { project, file, .. } => {
                    Some((*project, *file))
                }
                _ => None,
            })
            .collect();

        if !cf_wanted.is_empty() {
            let cf = self.curseforge.as_ref().ok_or_else(curseforge_key_needed)?;
            report(Event::Pack {
                stage: "Resolving".to_string(),
                detail: format!("{} CurseForge files", cf_wanted.len()),
            });

            // One batch instead of one request per mod. A 300-mod pack is
            // otherwise 300 round trips before anything is downloaded.
            let ids: Vec<u64> = cf_wanted.iter().map(|(_, f)| *f).collect();
            let mut named: std::collections::BTreeMap<u64, String> = Default::default();
            let mut sides: std::collections::BTreeMap<u64, (bool, bool)> = Default::default();
            for wire in cf.files_by_id(&ids).await.unwrap_or_default() {
                if !wire.display_name.is_empty() {
                    named.insert(wire.id, wire.display_name.clone());
                }
                sides.insert(
                    wire.id,
                    crate::source::curseforge::declared_sides(&wire.game_versions),
                );
            }
            let mut client_only = 0usize;
            let mut server_only = 0usize;

            for (project, file) in cf_wanted {
                let mut entry = crate::profile::ModEntry::new(ModId::project(
                    crate::source::SourceKind::CurseForge,
                    project.to_string(),
                ));
                // The file id is what resolves it; the display name is what a
                // person reads in the mod list.
                entry.file = Some(file.to_string());
                entry.pin = named.get(&file).cloned();

                // Keep a client-only mod off a dedicated server. CurseForge's
                // manifest has no side field, so this is the only signal there
                // is — and it is only sometimes there, which is why silence
                // means "both" rather than "client".
                match sides.get(&file).copied().unwrap_or((true, true)) {
                    (true, false) if !client_ids.is_empty() => {
                        entry.targets = Some(client_ids.clone());
                        client_only += 1;
                    }
                    (false, true) if !server_ids.is_empty() => {
                        entry.targets = Some(server_ids.clone());
                        server_only += 1;
                    }
                    _ => {}
                }
                profile.add(entry);
                out.mods += 1;
            }

            if client_only > 0 {
                out.notes.push(format!(
                    "{client_only} mod(s) are marked client-only and will not be installed \
                     on a dedicated server."
                ));
            }
            if server_only > 0 {
                out.notes.push(format!(
                    "{server_only} mod(s) are marked server-only and will not be installed \
                     on the client."
                ));
            }
        }

        // --- Thunderstore entries ----------------------------------------
        // Each dependency already names its exact version, so there is nothing
        // to look up: the pin resolves against Thunderstore's own
        // single-version endpoint when the profile is synced.
        let ts_wanted: Vec<(&String, &String, &String)> = plan
            .entries
            .iter()
            .filter_map(|e| match e {
                crate::modpack::PackEntry::Thunderstore {
                    namespace,
                    name,
                    version,
                } => Some((namespace, name, version)),
                _ => None,
            })
            .collect();

        if !ts_wanted.is_empty() {
            report(Event::Pack {
                stage: "Resolving".to_string(),
                detail: format!("{} Thunderstore packages", ts_wanted.len()),
            });
            for (namespace, name, version) in ts_wanted {
                let mut entry = crate::profile::ModEntry::new(ModId {
                    kind: crate::source::SourceKind::Thunderstore,
                    owner: namespace.clone(),
                    repo: name.clone(),
                    host: None,
                });
                entry.pin = Some(version.clone());
                profile.add(entry);
                out.mods += 1;
            }
        }

        // --- direct-download entries (.mrpack) ---------------------------
        let downloads: Vec<&crate::modpack::PackEntry> = plan
            .entries
            .iter()
            .filter(|e| matches!(e, crate::modpack::PackEntry::Download { .. }))
            .collect();

        if !downloads.is_empty() {
            let hashes: Vec<String> = downloads
                .iter()
                .filter_map(|e| match e {
                    crate::modpack::PackEntry::Download { sha512, .. } => sha512.clone(),
                    _ => None,
                })
                .collect();

            report(Event::Pack {
                stage: "Resolving".to_string(),
                detail: format!("matching {} files to their projects", hashes.len()),
            });

            // The index names URLs, not projects. This is what turns them back
            // into real Modrinth mods that can be updated and audited, rather
            // than a heap of anonymous jars.
            let matched = self
                .modrinth
                .versions_by_hash(&hashes, "sha512")
                .await
                .unwrap_or_default();

            for entry in downloads {
                let crate::modpack::PackEntry::Download {
                    path,
                    urls,
                    sha512,
                    size,
                    client,
                    server,
                    ..
                } = entry
                else {
                    continue;
                };

                let file_name = path.rsplit('/').next().unwrap_or(path).to_string();

                // Which side of the game the pack says this belongs on.
                let targets = match (client.wanted(), server.wanted()) {
                    (false, false) => {
                        out.skipped.push((
                            file_name.clone(),
                            "the pack marks it unsupported on both client and server"
                                .to_string(),
                        ));
                        continue;
                    }
                    (true, true) => None,
                    (true, false) => Some(client_ids.clone()),
                    (false, true) => Some(server_ids.clone()),
                };
                if targets.as_ref().is_some_and(|t| t.is_empty()) {
                    out.skipped.push((
                        file_name.clone(),
                        format!(
                            "the pack restricts it to a side the {} pack has no target for",
                            pack.pack.game.name
                        ),
                    ));
                    continue;
                }

                match sha512.as_ref().and_then(|h| matched.get(h)) {
                    Some(version) if !version.project_id.is_empty() => {
                        let mut mod_entry = crate::profile::ModEntry::new(ModId::project(
                            crate::source::SourceKind::Modrinth,
                            version.project_id.clone(),
                        ));
                        mod_entry.pin = Some(version.version_number.clone())
                            .filter(|v| !v.is_empty());
                        mod_entry.targets = targets;
                        profile.add(mod_entry);
                        out.mods += 1;
                    }
                    // No index knows this file. It still has a URL and a hash,
                    // so it can be fetched and held — it just cannot be checked
                    // for updates, and the report says so.
                    _ => {
                        let Some(url) = urls.first() else {
                            out.skipped.push((
                                file_name.clone(),
                                "the pack gives no download URL for it".to_string(),
                            ));
                            continue;
                        };
                        match self
                            .import_url(url, &file_name, false, sha512.as_deref())
                            .await
                        {
                            Ok((sha, actual_size)) => {
                                let id = ModId::project(
                                    crate::source::SourceKind::Local,
                                    sanitize_name(
                                        file_name.rsplit_once('.').map(|(s, _)| s).unwrap_or(&file_name),
                                    ),
                                );
                                let files = self.store.files(&sha)?;
                                lock.mods.push(LockEntry {
                                    id: id.clone(),
                                    version: format!("from {}", plan.label()),
                                    asset: file_name.clone(),
                                    url: url.clone(),
                                    sha256: sha,
                                    size: if actual_size > 0 { actual_size } else { *size },
                                    published_at: String::new(),
                                    upstream: None,
                                    trust: trust::assess(pack, &files, None, false),
                                });
                                let mut mod_entry = crate::profile::ModEntry::new(id);
                                mod_entry.manual = true;
                                mod_entry.targets = targets;
                                profile.add(mod_entry);
                                out.untraced += 1;
                            }
                            Err(e) => out.skipped.push((file_name.clone(), e.to_string())),
                        }
                    }
                }
            }
        }

        // --- the pack's own files ----------------------------------------
        // Added last on purpose: `deploy::plan` lets the later claim on a
        // destination win, and a pack's overrides are meant to beat the
        // defaults shipped inside its mods.
        out.overrides = self.pack_own_files(&plan, &stored);

        if out.overrides > 0 {
            let trust = trust::assess(pack, &stored, None, false);
            out.trust = Some(trust.clone());

            let id = ModId::project(
                crate::source::SourceKind::Local,
                sanitize_name(&format!("{chosen}-pack-files")),
            );
            lock.mods.push(LockEntry {
                id: id.clone(),
                version: plan.label(),
                asset: archive_name.clone(),
                url: String::new(),
                sha256: archive_sha.clone(),
                size: archive_size,
                published_at: String::new(),
                upstream: None,
                trust,
            });
            let mut mod_entry = crate::profile::ModEntry::new(id);
            mod_entry.manual = true;
            profile.add(mod_entry);
        }

        profile.save(&self.paths.profile_file(&profile_id))?;
        lock.save(&self.paths.lock_file(&profile_id))?;

        out.profile = chosen;
        out.game = pack.id().to_string();
        Ok(out)
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
                        pack,
                        None,
                        None,
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

                // An instanced profile gets its own loader, in its own folder.
                // That is the whole mechanism: BepInEx works out where it
                // lives from where its preloader was loaded from, so a loader
                // inside the instance takes plugins and configs with it.
                let instance = self.instance_for(pack, profile);
                let dest = match &instance {
                    Some(dir) => def.install_dir(dir),
                    None => def.install_dir(root),
                };
                std::fs::create_dir_all(&dest).ok();
                let written = crate::store::extract_over(&tmp, &dest, &preserve);
                let _ = std::fs::remove_file(&tmp);
                let written = written?;

                // The injector is the exception and has to sit beside the
                // executable — the game loads it on startup and will not look
                // anywhere else. It does nothing unless Modifile launches the
                // game with redirection switched on, so the install stays
                // vanilla when started any other way.
                let mut planted = 0;
                if let (Some(dir), Some(rules)) = (&instance, pack.instancing()) {
                    planted = plant_injector(pack, dir, root, rules)?;
                }

                Ok(format!(
                    "{} for {} ({written} files{})",
                    release.tag,
                    platform.label(),
                    match planted {
                        0 => String::new(),
                        n => format!(", {n} into the game folder"),
                    }
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
            hits.extend(
                cf.search(query, game_id, rules.curseforge_class_id)
                    .await
                    .unwrap_or_default(),
            );
        }
        // Thunderstore is where the mods actually are for BepInEx games, and
        // it needs no key.
        if let Some(community) = &rules.thunderstore_community {
            hits.extend(
                self.thunderstore
                    .search(community, query, None)
                    .await
                    .unwrap_or_default(),
            );
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

    /// Everything a page about one mod or pack needs.
    ///
    /// Costs more than a search hit — a long description, a screenshot list —
    /// so it is asked for only when someone opens the thing, never for a list.
    pub async fn details(
        &self,
        pack: &CompiledPack,
        id: &ModId,
    ) -> Result<crate::source::Details> {
        let rules = &pack.pack.search;
        match id.kind {
            crate::source::SourceKind::Modrinth => self.modrinth.details(id).await,
            crate::source::SourceKind::Thunderstore => {
                self.thunderstore
                    .details(id, rules.thunderstore_community.as_deref())
                    .await
            }
            crate::source::SourceKind::CurseForge => {
                let cf = self.curseforge.as_ref().ok_or_else(curseforge_key_needed)?;
                cf.details(
                    id,
                    rules.curseforge_game_id,
                    rules.curseforge_modpack_class_id,
                )
                .await
            }
            // A git forge has no storefront page, so the repository *is* the
            // description. That is less than the others give, and it is also
            // the only one of them whose claims can be checked.
            crate::source::SourceKind::GitHub
            | crate::source::SourceKind::GitLab
            | crate::source::SourceKind::Gitea => {
                let info = match id.kind {
                    crate::source::SourceKind::GitHub => self.github.repo(id).await?,
                    _ => self.forge.repo(id).await?,
                };
                let info = info.unwrap_or_default();
                Ok(crate::source::Details {
                    id: Some(id.clone()),
                    title: id.display(),
                    summary: info.description,
                    body: None,
                    icon_url: None,
                    gallery: Vec::new(),
                    authors: vec![id.owner.clone()],
                    downloads: info.stars,
                    source_url: Some(id.web_url()),
                    web_url: id.web_url(),
                    license: info.license,
                    is_pack: false,
                })
            }
            crate::source::SourceKind::Local => Ok(crate::source::Details {
                id: Some(id.clone()),
                title: id.short().to_string(),
                summary: "Supplied from a file on this machine.".to_string(),
                ..Default::default()
            }),
        }
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
        // Every lockfile, and they live one directory per game. Reading the
        // top level alone finds none of them and concludes that the entire
        // store is garbage — which it then deletes.
        let mut found_any = false;
        for game in std::fs::read_dir(&self.paths.profiles)
            .ctx(format!("reading {}", self.paths.profiles.display()))?
            .flatten()
        {
            let dir = game.path();
            if !dir.is_dir() {
                continue;
            }
            for entry in std::fs::read_dir(&dir)
                .ctx(format!("reading {}", dir.display()))?
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
                    found_any = true;
                }
            }
        }

        // A profile whose lock cannot be read is not evidence that nothing
        // needs its downloads. Refuse rather than delete on an empty answer
        // when there are plainly profiles present.
        if !found_any && !self.all_profiles().is_empty() {
            return Err(Error::other(
                "no lockfiles could be read, but profiles exist — refusing to delete \
                 downloads on that basis. Run an update first.",
            ));
        }
        self.store.gc(&keep)
    }
}

/// A profile name becomes a filename, so it cannot contain path separators or
/// anything Windows refuses.
#[cfg(test)]
mod modpack_source_tests {
    use super::ModpackSource;

    #[test]
    fn recognises_a_modrinth_version_page() {
        for input in [
            "https://modrinth.com/modpack/fabulously-optimized/version/abcd1234",
            "https://www.modrinth.com/modpack/fabulously-optimized/version/abcd1234",
            "modrinth.com/modpack/fabulously-optimized/version/abcd1234/",
            "https://modrinth.com/modpack/x/version/abcd1234?foo=1",
        ] {
            assert!(
                matches!(ModpackSource::parse(input), ModpackSource::ModrinthVersion(id) if id == "abcd1234"),
                "input: {input}"
            );
        }
    }

    #[test]
    fn a_project_page_is_not_mistaken_for_a_version_id() {
        // The slug is not a version id, and fetching it as one would 404.
        match ModpackSource::parse("https://modrinth.com/modpack/fabulously-optimized") {
            ModpackSource::ModrinthProject(slug) => {
                assert_eq!(slug, "fabulously-optimized")
            }
            other => panic!("should be a project, got {other:?}"),
        }
    }

    #[test]
    fn recognises_the_ids_that_search_prints() {
        // `pack search` tells you to run `pack add <id>`, so the ids it prints
        // have to be ones this accepts.
        assert!(
            matches!(ModpackSource::parse("modrinth:fabulously-optimized"),
                ModpackSource::ModrinthProject(s) if s == "fabulously-optimized")
        );
        assert!(matches!(
            ModpackSource::parse("curseforge:123456"),
            ModpackSource::CurseForgeProject(123456)
        ));
        // A project page with no version means "the newest one".
        assert!(matches!(
            ModpackSource::parse("https://modrinth.com/modpack/fabulously-optimized"),
            ModpackSource::ModrinthProject(_)
        ));
    }

    #[test]
    fn anything_else_is_a_path_or_a_url() {
        assert!(matches!(
            ModpackSource::parse("https://example.com/pack.mrpack"),
            ModpackSource::Url(_)
        ));
        assert!(matches!(
            ModpackSource::parse("  C:/Downloads/pack.mrpack  "),
            ModpackSource::Path(_)
        ));
        assert!(matches!(
            ModpackSource::parse("./pack.zip"),
            ModpackSource::Path(_)
        ));
    }
}

/// The archive-relative directory prefixes holding a pack's own game files.
fn override_prefixes(plan: &crate::modpack::PackPlan) -> Vec<String> {
    plan.overrides
        .iter()
        .map(|o| format!("{}/", o.trim_end_matches('/').to_ascii_lowercase()))
        .collect()
}

fn under_any(rel: &str, prefixes: &[String]) -> bool {
    let lowered = rel.to_ascii_lowercase();
    prefixes.iter().any(|p| lowered.starts_with(p.as_str()))
}

/// Move the injector out of an instance and into the real game folder.
///
/// The one thing an instanced profile cannot keep to itself. A doorstop shim
/// is a DLL the game loads by name at startup, so it has to be beside the
/// executable; everything it then loads comes from the instance.
///
/// It is inert on its own. Started from Steam or a shortcut, the game finds a
/// doorstop that has not been told to do anything and runs vanilla.
fn plant_injector(
    pack: &CompiledPack,
    instance: &Path,
    game_root: &Path,
    rules: &crate::pack::InstanceRules,
) -> Result<usize> {
    if rules.game_files.is_empty() {
        return Ok(0);
    }

    let mut moved = 0;
    let Ok(entries) = std::fs::read_dir(instance) else {
        return Ok(0);
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(name) = path.file_name() else { continue };
        if !pack.belongs_in_game_dir(Path::new(name)) {
            continue;
        }
        let dest = game_root.join(name);
        // Already there and identical: leave it, so a second install does not
        // churn a file the game may have open.
        if std::fs::copy(&path, &dest).is_ok() {
            let _ = std::fs::remove_file(&path);
            moved += 1;
        }
    }
    Ok(moved)
}

/// Create or delete a marker file, the way every opt-in here is stored.
fn toggle_marker(path: &Path, on: bool, body: &[u8]) -> Result<()> {
    if on {
        crate::paths::write_atomic(path, body)
    } else {
        let _ = std::fs::remove_file(path);
        Ok(())
    }
}

/// The one error message for "this needs a CurseForge key you supply yourself".
/// Whether a file is one of the single-file JSON profiles Modifile used to
/// write, so it can be turned away by name instead of by parse error.
///
/// Sniffed rather than trusted from the extension, and only far enough to tell
/// what it is: a JSON object carrying the marker key that format led with.
fn looks_like_old_bundle(path: &std::path::Path) -> bool {
    let Ok(bytes) = std::fs::read(path) else {
        return false;
    };
    // Only the head: these were small, but a hostile file need not be.
    let head = &bytes[..bytes.len().min(4096)];
    let Ok(text) = std::str::from_utf8(head) else {
        return false;
    };
    text.trim_start().starts_with('{') && text.contains("\"modifile_profile\"")
}

fn curseforge_key_needed() -> Error {
    Error::other(
        "this modpack is built from CurseForge files, and fetching those needs an API key \
         you obtain yourself — Overwolf issues them after a human review and forbids \
         sharing one, so an open-source binary cannot ship a working key. Add yours in \
         Settings, or with `modifile auth --curseforge <key>`. Modrinth `.mrpack` packs \
         need no key at all."
            .to_string(),
    )
}

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

    fn id(game: &str, name: &str) -> crate::profile::ProfileId {
        crate::profile::ProfileId::new(game, name)
    }

    #[test]
    fn deleting_a_profile_takes_its_lock_and_settings_with_it() {
        let paths = scratch("delete");
        let engine = Engine::open(paths.clone(), None).expect("engine");

        let raiding = id("wow", "raiding");
        let profile = Profile::new("raiding", "wow");
        profile
            .save(&paths.profile_file(&raiding))
            .expect("save profile");
        Lock::default()
            .save(&paths.lock_file(&raiding))
            .expect("save lock");
        let state = paths.profile_dir("wow").join("raiding.state");
        std::fs::create_dir_all(state.join("client")).expect("state dir");

        engine.delete_profile(&raiding).expect("delete");

        assert!(!paths.profile_file(&raiding).exists());
        assert!(!paths.lock_file(&raiding).exists());
        assert!(!state.exists());
        std::fs::remove_dir_all(&paths.home).ok();
    }

    /// Export to a `.mfpack` and import it back, through the engine.
    ///
    /// The part that matters is what survives: the loader and game version
    /// decide which build of every mod is correct, and a pack that loses them
    /// hands the importer a set of mods that will not load together.
    #[test]
    fn a_profile_round_trips_through_an_mfpack() {
        let paths = scratch("mfpack");
        // Importing resolves the pack by id, so the game has to be installed
        // here as it would be on a real machine.
        crate::install_bundled_packs(&paths).expect("packs");
        let engine = Engine::open(paths.clone(), None).expect("engine");
        let pack = bundled("minecraft.toml");
        let target = pack.target("client").expect("client");

        let mine = id("minecraft", "hardcore");
        let mut profile = Profile::new("hardcore", "minecraft");
        profile.game_version = Some("1.20.1".to_string());
        profile.loader = Some("fabric".to_string());
        profile.save(&paths.profile_file(&mine)).expect("save");

        // A config the profile owns, as activating would have produced.
        let config = engine
            .config_path(&pack, &mine, target, std::path::Path::new("config/sodium.json"))
            .expect("config path");
        std::fs::create_dir_all(config.parent().unwrap()).expect("dir");
        std::fs::write(&config, "{\"quality\":\"fast\"}").expect("write config");

        let out = paths.home.join("hardcore.mfpack");
        let exported = engine
            .export_mfpack(&pack, &profile, true, "my pack".into(), &out)
            .expect("export");
        assert_eq!(exported.config_count(), 1);
        assert!(crate::mfpack::looks_like_pack(&out), "must be a zip");

        // Back in, as a different profile, exactly as a friend would.
        let reopened = engine.read_shared(&out).expect("read back");
        let name = engine
            .import_profile(&reopened, Some("theirs"), true)
            .expect("import");
        assert_eq!(name, "theirs");

        let theirs = Profile::load(&paths.profile_file(&id("minecraft", "theirs")))
            .expect("load imported");
        assert_eq!(theirs.game_version.as_deref(), Some("1.20.1"));
        assert_eq!(theirs.loader.as_deref(), Some("fabric"));

        // And their configs arrived, so the pack plays as its author tuned it.
        let landed = engine
            .read_config(
                &pack,
                &id("minecraft", "theirs"),
                target,
                std::path::Path::new("config/sodium.json"),
            )
            .expect("their config");
        assert_eq!(landed, "{\"quality\":\"fast\"}");
        std::fs::remove_dir_all(&paths.home).ok();
    }

    /// The single-file JSON profile is not read any more. It is still
    /// recognised, so someone holding one is told what it is and what to do —
    /// which is not the same as supporting it, and is much better than the
    /// "not a zip" error they would otherwise get.
    #[test]
    fn the_old_json_profile_is_refused_by_name() {
        let paths = scratch("legacy-json");
        let engine = Engine::open(paths.clone(), None).expect("engine");

        let old = paths.home.join("raiding.modifile.json");
        std::fs::write(
            &old,
            br#"{"modifile_profile":1,"name":"raiding","game":"valheim","mods":[]}"#,
        )
        .expect("write");

        let error = engine.read_shared(&old).expect_err("must refuse").to_string();
        assert!(
            error.contains("no longer reads") && error.contains(".mfpack"),
            "should say what it is and what to do instead: {error}"
        );

        // And something that is neither gets its own answer rather than being
        // blamed on the old format.
        let junk = paths.home.join("holiday.png");
        std::fs::write(&junk, [0x89, b'P', b'N', b'G']).expect("write");
        let error = engine.read_shared(&junk).expect_err("must refuse").to_string();
        assert!(error.contains("not a Modifile pack"), "{error}");
        std::fs::remove_dir_all(&paths.home).ok();
    }

    fn bundled(file: &str) -> CompiledPack {
        let (_, body) = crate::BUNDLED_PACKS
            .iter()
            .find(|(n, _)| *n == file)
            .unwrap_or_else(|| panic!("bundled pack {file}"));
        CompiledPack::new(toml::from_str(body).expect("parses")).expect("compiles")
    }

    /// Activate installs into the game folder. Always.
    ///
    /// The regression this pins: a game that declares `[instance]` is saying
    /// it *can* be redirected, which is Play's business. Deriving the deploy
    /// base from that emptied `BepInEx/plugins` on every activate — the
    /// manifest said the mods were installed, the game folder had none of
    /// them, and a launch from Steam ran vanilla.
    #[test]
    fn activate_installs_into_the_game_folder_even_when_the_game_can_be_instanced() {
        let paths = scratch("activate-base");
        let engine = Engine::open(paths.clone(), None).expect("engine");
        let pack = bundled("valheim.toml");
        assert!(
            pack.instancing().is_some(),
            "valheim must declare [instance] or this test proves nothing"
        );
        let target = pack.target("client").expect("client target");

        // One mod, one plugin file, in the store where a deploy would find it.
        let sha = "a".repeat(64);
        let staging = paths.home.join("staging");
        std::fs::create_dir_all(&staging).expect("staging");
        let jar = staging.join("ValheimPlus.dll");
        std::fs::write(&jar, b"not really a dll").expect("write");
        engine
            .store
            .insert(&sha, &jar, "ValheimPlus.dll", false)
            .expect("store insert");

        let id = ModId::project(crate::source::SourceKind::Local, "valheimplus");
        let mut profile = Profile::new("main", "valheim");
        profile.add(crate::profile::ModEntry::new(id.clone()));
        let lock = Lock {
            profile: "main".to_string(),
            generated_ms: 0,
            mods: vec![crate::profile::LockEntry {
                id,
                version: "1.0".to_string(),
                asset: "ValheimPlus.dll".to_string(),
                url: String::new(),
                sha256: sha,
                size: 16,
                published_at: String::new(),
                upstream: None,
                trust: crate::trust::TrustReport {
                    level: crate::TrustLevel::Unchecked,
                    license: None,
                    attested: false,
                    readable_files: 0,
                    data_files: 0,
                    executable_files: 1,
                    notes: Vec::new(),
                },
            }],
        };

        let root = paths.home.join("Valheim");
        std::fs::create_dir_all(&root).expect("game dir");

        let plan = engine
            .plan(&pack, &profile, &lock, target, &root)
            .expect("plan");
        assert!(!plan.files.is_empty(), "the mod should have been placed");
        for file in &plan.files {
            assert_eq!(
                file.base,
                crate::deploy::Base::Game,
                "{} went to the instance on a plain activate",
                file.rel.display()
            );
        }

        // Play is the one that redirects, and still does.
        let instanced = engine
            .plan_instanced(&pack, &profile, &lock, target, &root)
            .expect("instanced plan");
        assert!(
            instanced
                .files
                .iter()
                .any(|f| f.base == crate::deploy::Base::Instance),
            "Play should still install into the profile's own tree"
        );
        std::fs::remove_dir_all(&paths.home).ok();
    }

    fn minecraft_pack() -> CompiledPack {
        let (_, body) = crate::BUNDLED_PACKS
            .iter()
            .find(|(n, _)| *n == "minecraft.toml")
            .expect("bundled minecraft pack");
        CompiledPack::new(toml::from_str(body).expect("parses")).expect("compiles")
    }

    /// The editor hands `config_path` a string that came from the UI, and the
    /// result is written to. Everything that would climb out of the settings
    /// folder has to be refused, not normalised.
    #[test]
    fn a_settings_path_cannot_leave_its_folder() {
        let paths = scratch("config-escape");
        let engine = Engine::open(paths.clone(), None).expect("engine");
        let pack = minecraft_pack();
        let target = pack.target("client").expect("client target");
        let profile = id("minecraft", "main");

        let refused = [
            // Climbing out with `..`.
            "config/../../../../evil.toml",
            "config/..",
            // Absolute, and a drive-qualified absolute.
            "/etc/passwd",
            r"C:\Windows\System32\drivers\etc\hosts",
            // A folder that is not one this pack declares as state.
            "mods/sodium.jar",
            "../config/x.toml",
            // Names a folder rather than a file.
            "config",
        ];
        for rel in refused {
            assert!(
                engine
                    .config_path(&pack, &profile, target, std::path::Path::new(rel))
                    .is_err(),
                "`{rel}` should have been refused"
            );
        }

        // And the ordinary case still resolves, inside the profile's own dir.
        let ok = engine
            .config_path(
                &pack,
                &profile,
                target,
                std::path::Path::new("config/sodium-options.json"),
            )
            .expect("a plain settings path resolves");
        assert!(ok.starts_with(&paths.profiles), "{}", ok.display());
        assert!(ok.ends_with("config/sodium-options.json"), "{}", ok.display());
        std::fs::remove_dir_all(&paths.home).ok();
    }

    #[test]
    fn editing_a_settings_file_round_trips() {
        let paths = scratch("config-edit");
        let engine = Engine::open(paths.clone(), None).expect("engine");
        let pack = minecraft_pack();
        let target = pack.target("client").expect("client target");
        let profile = id("minecraft", "main");
        let rel = std::path::Path::new("config/sodium-options.json");

        engine
            .write_config(&pack, &profile, target, rel, "{\"quality\":\"fast\"}", None)
            .expect("write");
        assert_eq!(
            engine.read_config(&pack, &profile, target, rel).expect("read"),
            "{\"quality\":\"fast\"}"
        );

        // It shows up as one of the profile's settings files, by the same
        // display path the editor was given.
        let saved = engine.saved_configs(&pack, &profile, target);
        assert!(saved.contains(&rel.to_path_buf()), "{saved:?}");
        std::fs::remove_dir_all(&paths.home).ok();
    }

    /// A cache or a database in the config folder is not something to offer to
    /// edit — saving a text box back over it would corrupt it.
    #[test]
    fn a_settings_file_that_is_not_text_is_refused() {
        let paths = scratch("config-binary");
        let engine = Engine::open(paths.clone(), None).expect("engine");
        let pack = minecraft_pack();
        let target = pack.target("client").expect("client target");
        let profile = id("minecraft", "main");
        let rel = std::path::Path::new("config/cache.bin");

        let path = engine
            .config_path(&pack, &profile, target, rel)
            .expect("path");
        std::fs::create_dir_all(path.parent().unwrap()).expect("dir");
        std::fs::write(&path, [0xff, 0xfe, 0x00, 0x80]).expect("write");

        assert!(engine.read_config(&pack, &profile, target, rel).is_err());
        std::fs::remove_dir_all(&paths.home).ok();
    }

    /// The reason names are scoped to a game at all.
    #[test]
    fn two_games_can_both_have_a_profile_called_main() {
        let paths = scratch("same-name");
        let engine = Engine::open(paths.clone(), None).expect("engine");

        let wow = id("wow", "main");
        let valheim = id("valheim", "main");
        Profile::new("main", "wow")
            .save(&paths.profile_file(&wow))
            .expect("save wow");
        Profile::new("main", "valheim")
            .save(&paths.profile_file(&valheim))
            .expect("save valheim");

        assert_ne!(paths.profile_file(&wow), paths.profile_file(&valheim));
        assert!(paths.profile_file(&wow).exists());
        assert!(paths.profile_file(&valheim).exists());

        // And deleting one leaves the other entirely alone.
        engine.delete_profile(&wow).expect("delete wow");
        assert!(!paths.profile_file(&wow).exists());
        assert!(
            paths.profile_file(&valheim).exists(),
            "the other game's `main` must survive"
        );

        std::fs::remove_dir_all(&paths.home).ok();
    }

    #[test]
    fn profiles_are_moved_out_of_the_old_flat_layout() {
        // Existing installs keep their profiles, their locks and their saved
        // settings when the layout changes under them.
        let paths = scratch("migrate");

        let flat = paths.profiles.join("raiding.toml");
        Profile::new("raiding", "wow").save(&flat).expect("save");
        Lock::default()
            .save(&paths.profiles.join("raiding.lock.json"))
            .expect("save lock");
        std::fs::create_dir_all(paths.profiles.join("raiding.state").join("client"))
            .expect("state");

        let moved = crate::profile::migrate_flat_layout(&paths.profiles);
        assert_eq!(moved, vec![id("wow", "raiding")]);

        let raiding = id("wow", "raiding");
        assert!(paths.profile_file(&raiding).exists(), "profile moved");
        assert!(paths.lock_file(&raiding).exists(), "lock moved");
        assert!(
            paths
                .profile_dir("wow")
                .join("raiding.state")
                .join("client")
                .is_dir(),
            "saved settings moved"
        );
        assert!(!flat.exists(), "the old copy is gone");

        // Running it again finds nothing to do.
        assert!(crate::profile::migrate_flat_layout(&paths.profiles).is_empty());

        std::fs::remove_dir_all(&paths.home).ok();
    }

    /// Deleting an active profile would leave its files in the game folder
    /// with nothing left that knows they are there.
    #[test]
    fn an_active_profile_is_not_deleted_out_from_under_the_game() {
        let paths = scratch("delete-active");
        let engine = Engine::open(paths.clone(), None).expect("engine");

        let raiding = id("wow", "raiding");
        let profile = Profile::new("raiding", "wow");
        profile
            .save(&paths.profile_file(&raiding))
            .expect("save profile");

        let manifest_path = paths.manifest_file("wow", "client");
        std::fs::create_dir_all(manifest_path.parent().expect("parent")).expect("state dir");
        Manifest {
            game: "wow".into(),
            target: "client".into(),
            profile: "raiding".into(),
            root: paths.home.join("game"),
            instance: None,
            mode: None,
            deployed_ms: 0,
            active: true,
            files: Vec::new(),
            created_dirs: Vec::new(),
        }
        .save(&manifest_path)
        .expect("save manifest");

        let err = engine.delete_profile(&raiding).unwrap_err().to_string();
        assert!(err.contains("Deactivate"), "{err}");
        assert!(paths.profile_file(&raiding).exists(), "profile survived");
        std::fs::remove_dir_all(&paths.home).ok();
    }
}
