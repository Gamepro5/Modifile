//! Game packs: the declarative description of how one game is modded.
//!
//! A pack is pure data. It cannot execute anything — no scripts, no hooks, no
//! shell-outs. That is deliberate: packs are meant to be contributed by
//! strangers, and a format that can run code is a format that can run malware.
//! Everything a pack can express is a path, a glob, or a string.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use globset::{Glob, GlobMatcher};
use serde::{Deserialize, Serialize};

use crate::error::{Context, Error, Result};
use crate::paths::expand_env;

pub const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Pack {
    pub schema: u32,
    pub game: GameMeta,
    #[serde(default)]
    pub targets: Vec<Target>,
    /// Logical path name -> path relative to a target root. Targets may
    /// override or extend these.
    #[serde(default)]
    pub paths: BTreeMap<String, String>,
    #[serde(default)]
    pub assets: AssetRules,
    #[serde(default)]
    pub install: Vec<InstallRule>,
    #[serde(default)]
    pub state: StateRules,
    #[serde(default)]
    pub versions: VersionRules,
    /// Mod loaders this game can use, installable from here.
    #[serde(default)]
    pub loaders: Vec<LoaderDef>,
    /// How this game can be pointed at a mod directory that is not its own.
    /// Absent means it cannot, and Play is not offered for it.
    #[serde(default)]
    pub instance: Option<InstanceRules>,
    #[serde(default)]
    pub search: SearchRules,
    #[serde(default)]
    pub running: RunningRules,
    /// Extensions treated as human-readable source. Used to decide whether a
    /// mod is auditable or merely claimed to be open source.
    #[serde(default = "default_readable")]
    pub readable_extensions: Vec<String>,
    /// Extensions that can execute code you cannot read. These, not "anything
    /// that isn't text", are what stop a mod being auditable — a texture or a
    /// font is opaque but inert, and treating it as a binary would wrongly
    /// demote every addon that ships an icon.
    #[serde(default = "default_executable")]
    pub executable_extensions: Vec<String>,
}

fn default_readable() -> Vec<String> {
    [
        "lua", "xml", "toc", "txt", "json", "md", "cfg", "conf", "ini", "yml", "yaml", "js", "ts",
        "py", "sh", "bat", "ps1", "cs", "java", "kt", "properties", "csv", "html", "css",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

fn default_executable() -> Vec<String> {
    [
        "dll", "exe", "so", "dylib", "jar", "class", "pyc", "pyd", "wasm", "msi", "scr", "com",
        "o", "a", "lib", "node", "bin", "elf",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct GameMeta {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// Who to blame for the pack, not the game.
    #[serde(default)]
    pub maintainers: Vec<String>,
    /// A square icon for the game, as a URL.
    ///
    /// A URL rather than a bundled file, deliberately. Game artwork is not
    /// ours to redistribute, and a pack is data that anyone may write — so the
    /// pack points at art it is entitled to point at, and a pack that names
    /// none gets a generated tile instead of a broken image.
    #[serde(default)]
    pub icon: Option<String>,
    /// A wide image for the game's own page.
    #[serde(default)]
    pub art: Option<String>,
}

/// An install target: a client, a dedicated server, or a game flavor (WoW
/// retail vs. classic). Dedicated servers are first-class here rather than an
/// afterthought — they are just another root with their own path map.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Target {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub kind: TargetKind,
    /// Substituted into asset patterns as `{flavor}`. WoW uses this to keep a
    /// Classic profile from installing a retail build.
    #[serde(default)]
    pub flavor: Option<String>,
    /// Extra asset rejections for this target only. Retail WoW uses this to
    /// refuse `-classic`/`-bcc`/`-wrath` builds, which is not expressible with
    /// a pack-wide reject list because every flavor rejects the *others*.
    #[serde(default)]
    pub asset_reject: Vec<String>,
    /// Relative paths that identify this target. Any one of them existing is
    /// enough — the same game ships `valheim.exe` on Windows and
    /// `valheim.x86_64` on Linux, so requiring all of them would never match.
    #[serde(default)]
    pub markers: Vec<String>,
    /// Target-specific logical paths, merged over `Pack::paths`.
    #[serde(default)]
    pub paths: BTreeMap<String, String>,
    /// Executable names that mean this game is running. Deploying is refused
    /// while any of them is alive. Leave empty for games whose process name is
    /// too generic to match on (Minecraft is `javaw.exe`); running-from-the-
    /// game-folder detection covers those.
    #[serde(default)]
    pub processes: Vec<String>,
    #[serde(default)]
    pub steam: Option<SteamHint>,
    /// Extra guesses, `${VAR}` expanded. Skipped when a variable is unset.
    #[serde(default)]
    pub candidates: Vec<String>,
    /// Executables that start this target, relative to its root. The first one
    /// that exists is used, which is how one entry covers `game.exe` and
    /// `game.x86_64` without knowing the platform.
    ///
    /// Paths only, and only inside the game directory — a pack cannot name a
    /// command. See `crate::launch` for why that line is where it is. Steam
    /// targets need nothing here: `[targets.steam]` is a better route.
    #[serde(default)]
    pub launch: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TargetKind {
    #[default]
    Client,
    Server,
}

impl TargetKind {
    pub fn label(self) -> &'static str {
        match self {
            TargetKind::Client => "client",
            TargetKind::Server => "server",
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SteamHint {
    pub app_id: u32,
    /// Directory under `steamapps/common`.
    pub dir: String,
}

/// How to pick one file out of a release that may carry a dozen.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AssetRules {
    /// Tried in order; the first pattern with a match wins.
    #[serde(default)]
    pub prefer: Vec<String>,
    /// Fallback if nothing preferred matched.
    #[serde(default)]
    pub accept: Vec<String>,
    /// Always excluded, even if preferred. Source jars, nolib builds, sigs.
    #[serde(default)]
    pub reject: Vec<String>,
    /// Assets matching these are extracted; everything else is installed as a
    /// single file. This has to be declared rather than sniffed, because a
    /// Minecraft `.jar` is a zip that must be installed *unopened*.
    #[serde(default = "default_unpack")]
    pub unpack: Vec<String>,
}

fn default_unpack() -> Vec<String> {
    vec!["*.zip".to_string()]
}

// Hand-written so that a pack omitting `[assets]` entirely still gets the
// `*.zip` unpack default, which a derived `Default` would blank out.
impl Default for AssetRules {
    fn default() -> Self {
        Self {
            prefer: Vec::new(),
            accept: Vec::new(),
            reject: Vec::new(),
            unpack: default_unpack(),
        }
    }
}

/// Directories whose contents belong to the *profile*, not to the mods.
///
/// A mod's `.dll` is immutable content that can be hard-linked out of the
/// shared store. A mod's config file is the opposite: the game rewrites it, the
/// user edits it, and two profiles want different values. Hard-linking one
/// would write the edit straight back into the shared store and leak it into
/// every other profile.
///
/// Paths listed here are captured into the active profile when you switch away
/// and restored when you switch back.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct StateRules {
    /// Logical path names from `[paths]`.
    #[serde(default)]
    pub paths: Vec<String>,
}

/// Whether mods can be changed while the game is running.
///
/// Refusing is the safe default and the right one for most games: a BepInEx
/// plugin is a DLL mapped into the running process, and the game rewrites its
/// configs on exit, overwriting whatever was just captured.
///
/// World of Warcraft is the counter-example. Addons are plain Lua read at load
/// time, nothing holds the files open, and `/reload` picks up changes — so
/// blocking there is pure nuisance. Which behaviour applies is a property of
/// the game, so the game's pack says.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct RunningRules {
    /// True when installing and removing mods is safe mid-session.
    #[serde(default)]
    pub allow_changes: bool,
    /// Shown after a change was made while the game was open.
    #[serde(default)]
    pub note: String,
}

/// How to find mods for this game by name.
///
/// There is no single index that covers every game. Modrinth is excellent for
/// Minecraft and has nothing at all for World of Warcraft; WoW addons live on
/// CurseForge, but the ones that can actually be installed from here are the
/// ones that publish GitHub releases — so for WoW, GitHub *is* the right index.
/// How a game can be told to read its mods from somewhere else.
///
/// This is what makes Play safe. Activating puts mods in the game folder and
/// leaves them there; an instance never touches the game folder at all, so
/// there is nothing to undo when you quit and nothing to lose if the power
/// goes out mid-session. It is what r2modman and Prism do, and the reason
/// neither of them has to "revert" anything.
///
/// It costs almost nothing here. Other launchers duplicate mod files per
/// profile; Modifile deploys by hard link from one content-addressed store, so
/// ten instances of the same 900 MB pack cost 900 MB and some directory
/// entries. The only per-instance data is saves and configs, which you wanted
/// separate anyway.
///
/// Not every game can do this. WoW reads addons from `Interface/AddOns` and
/// nowhere else, so its pack declares no `[instance]` and Modifile says so
/// rather than pretending.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct InstanceRules {
    pub kind: InstanceKind,
    /// `doorstop`: the assembly to invoke, relative to the instance. BepInEx
    /// derives its whole root from where this lives, which is exactly the
    /// hook an instance needs.
    #[serde(default)]
    pub target: Option<String>,
    /// Files that must sit in the *real* game folder for the redirection to
    /// work at all — the doorstop shim is a DLL the game loads on startup, so
    /// it cannot live anywhere else.
    ///
    /// These are the only things an instanced profile writes into the game
    /// directory, and they do nothing unless Modifile launches the game with
    /// redirection switched on. Launched any other way, the game is vanilla.
    #[serde(default)]
    pub game_files: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum InstanceKind {
    /// UnityDoorstop, which every BepInEx game uses. The game is launched with
    /// `--doorstop-target-assembly <instance>/…`, and BepInEx then reads its
    /// plugins and configs from beside that assembly.
    Doorstop,
    /// Minecraft's own launcher, which takes a `gameDir` per profile. Modifile
    /// writes the profile; the launcher runs the game against it.
    MinecraftLauncher,
}

/// Which to use is therefore a property of the game, and lives in the pack.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct SearchRules {
    /// Search Modrinth. Correct for Minecraft; useless elsewhere.
    #[serde(default)]
    pub modrinth: bool,
    /// Search GitHub repositories carrying any of these topics.
    #[serde(default)]
    pub github_topics: Vec<String>,
    /// Extra words added to every GitHub query, to cut down false positives.
    #[serde(default)]
    pub github_terms: Vec<String>,
    /// CurseForge's numeric id for this game — 1 for World of Warcraft, 432 for
    /// Minecraft. Needed because CurseForge addresses projects by number, so a
    /// slug copied out of a URL has to be looked up against a specific game.
    #[serde(default)]
    pub curseforge_game_id: Option<u32>,
    /// CurseForge's class id for this game's *mods*. Per game, not universal —
    /// Minecraft mods are 6, and a number from one game means nothing in
    /// another. Absent means "do not narrow", which returns everything the
    /// game publishes and is the safe answer for a pack that has not been
    /// told which class to ask for.
    #[serde(default)]
    pub curseforge_class_id: Option<u32>,
    /// CurseForge's class id for this game's *modpacks*, where it has them.
    /// Absent means this game has no modpacks to browse.
    #[serde(default)]
    pub curseforge_modpack_class_id: Option<u32>,
    /// Thunderstore's community slug for this game — `valheim`, `repo`,
    /// `lethal-company`. A Thunderstore package names no game, so this is what
    /// lets a downloaded pack be matched to the game pack it belongs to.
    #[serde(default)]
    pub thunderstore_community: Option<String>,
}

impl SearchRules {
    pub fn is_empty(&self) -> bool {
        !self.modrinth && self.github_topics.is_empty() && self.github_terms.is_empty()
    }
}

/// How one mod loader is installed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum LoaderKind {
    /// Fabric and Quilt: a metadata service returns a finished version profile,
    /// so installing is writing one JSON file. No installer, no Java.
    #[default]
    FabricMeta,
    /// BepInEx and friends: an archive extracted over the game root. This is
    /// most Unity modding — Valheim, Subnautica, Risk of Rain 2 — and without
    /// it the game never reads the plugins folder at all, so mods install
    /// perfectly and do nothing.
    Archive,
    /// Forge and NeoForge: their installer patches the game and has to actually
    /// run, so Modifile points at it rather than pretending to do it.
    Installer,
}

/// A mod loader this game can use.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LoaderDef {
    /// Matches the value a profile stores in `loader`.
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub kind: LoaderKind,
    /// Metadata service base URL, for `fabric-meta` loaders.
    #[serde(default)]
    pub meta: String,
    /// Prefix of the `versions/` folder it creates, e.g. `fabric`.
    #[serde(default)]
    pub prefix: String,
    /// Where to send the user when we cannot install it ourselves.
    #[serde(default)]
    pub page: String,

    // --- for `archive` loaders ------------------------------------------
    /// Where the archive comes from, as a mod id, e.g. `github:BepInEx/BepInEx`.
    #[serde(default)]
    pub source: String,
    /// Asset name patterns per platform — these ship one build per OS.
    #[serde(default)]
    pub windows_assets: Vec<String>,
    #[serde(default)]
    pub linux_assets: Vec<String>,
    #[serde(default)]
    pub macos_assets: Vec<String>,
    /// Files that exist once it is installed, relative to the game root.
    #[serde(default)]
    pub markers: Vec<String>,
    /// Where the archive unpacks, relative to the game root. Empty means the
    /// root itself, which is what BepInEx wants; a loader that lives in a
    /// subfolder names it here.
    #[serde(default)]
    pub into: String,
    /// Targets this loader applies to. Absent means all of them — a dedicated
    /// server usually needs the same loader as the client.
    #[serde(default)]
    pub targets: Option<Vec<String>>,
}

impl LoaderDef {
    /// Does this loader apply to the given target?
    pub fn applies_to(&self, target_id: &str) -> bool {
        self.targets
            .as_ref()
            .map(|ids| ids.iter().any(|id| id == target_id))
            .unwrap_or(true)
    }

    /// Where its archive unpacks, given a game root.
    pub fn install_dir(&self, root: &Path) -> PathBuf {
        if self.into.is_empty() || self.into == "." {
            root.to_path_buf()
        } else {
            root.join(&self.into)
        }
    }

    /// Asset patterns for the platform a game was *built* for.
    ///
    /// Not the host: a Windows game under Proton needs the Windows loader, and
    /// a Linux dedicated server needs the Linux one even when Modifile is
    /// driving it from Windows over a file share.
    pub fn assets_for(&self, platform: crate::loader::GamePlatform) -> &[String] {
        match platform {
            crate::loader::GamePlatform::Windows => &self.windows_assets,
            crate::loader::GamePlatform::MacOs => &self.macos_assets,
            crate::loader::GamePlatform::Linux => &self.linux_assets,
        }
    }
}

/// Games where a mod is built against a specific game version and mod loader.
///
/// Minecraft is the case that forces this: Sodium for NeoForge and Lithium for
/// Fabric are both "the newest release", and installing one of each produces a
/// game that does not start. When a pack lists loaders, a profile must pick one
/// before anything can be resolved.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct VersionRules {
    /// e.g. `["fabric", "forge", "neoforge", "quilt"]`. Empty means the game
    /// has no such concept and nothing is asked of the user.
    #[serde(default)]
    pub loaders: Vec<String>,
    /// Whether a game version (`1.20.1`) is also required.
    #[serde(default)]
    pub needs_game_version: bool,
}

impl VersionRules {
    pub fn applies(&self) -> bool {
        !self.loaders.is_empty() || self.needs_game_version
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct InstallRule {
    /// Glob against the archive-relative path, matched case-insensitively
    /// because mod authors are inconsistent and Windows does not care.
    #[serde(rename = "match")]
    pub pattern: String,
    /// Logical path name to install into.
    pub into: String,
    /// Strip this leading path prefix before joining onto the destination.
    #[serde(default)]
    pub strip: Option<String>,
    /// Discard all directory structure and drop the bare file in.
    #[serde(default)]
    pub flatten: bool,
    /// Restrict this rule to specific target ids. Absent means all targets —
    /// this is how a pack says "shaders are client-only".
    #[serde(default)]
    pub targets: Option<Vec<String>>,
    /// This file is a *default* for something the user will edit. It is copied
    /// rather than linked, only when nothing is already there, and it is never
    /// removed on undeploy — the profile owns it from then on.
    #[serde(default)]
    pub mutable: bool,
    /// Matched, and deliberately not installed.
    ///
    /// Needed because matching is first-wins and the general rules carry no
    /// target restriction. A modpack's `client-overrides/config/**` has to be
    /// claimed for the server by *something*, or it falls through to the plain
    /// config rule and gets planted on a dedicated server — which is the one
    /// thing naming the tree "client" was meant to prevent. `into` is ignored
    /// for these.
    #[serde(default)]
    pub skip: bool,
}

// ---------------------------------------------------------------------------
// Compiled form
// ---------------------------------------------------------------------------

/// A pack with its globs compiled once, rather than per file per mod.
#[derive(Debug, Clone)]
pub struct CompiledPack {
    pub pack: Pack,
    install: Vec<CompiledRule>,
    reject: Vec<GlobMatcher>,
    unpack: Vec<GlobMatcher>,
    /// `[instance] game_files`, compiled once.
    game_files: Vec<GlobMatcher>,
}

#[derive(Debug, Clone)]
struct CompiledRule {
    matcher: GlobMatcher,
    rule: InstallRule,
}

fn compile(pattern: &str) -> Result<GlobMatcher> {
    Ok(Glob::new(pattern)?.compile_matcher())
}

/// Asset globs carry `{flavor}`, which depends on the target, so they are
/// compiled per resolution rather than once per pack.
fn compile_with_flavor(pattern: &str, flavor: Option<&str>) -> Option<GlobMatcher> {
    let expanded = match flavor {
        Some(f) => pattern.replace("{flavor}", f),
        // A flavor pattern on a flavorless target is meaningless, not a match.
        None if pattern.contains("{flavor}") => return None,
        None => pattern.to_string(),
    };
    Glob::new(&expanded).ok().map(|g| g.compile_matcher())
}

impl CompiledPack {
    pub fn new(pack: Pack) -> Result<Self> {
        if pack.schema != SCHEMA_VERSION {
            return Err(Error::pack(
                &pack.game.id,
                format!(
                    "schema version {} is not supported (this build understands {SCHEMA_VERSION})",
                    pack.schema
                ),
            ));
        }
        if pack.targets.is_empty() {
            return Err(Error::pack(&pack.game.id, "pack defines no targets"));
        }
        for rule in &pack.install {
            // Fail at load time rather than halfway through an install.
            let known = pack.paths.contains_key(&rule.into)
                || pack
                    .targets
                    .iter()
                    .any(|t| t.paths.contains_key(&rule.into));
            if !known {
                return Err(Error::pack(
                    &pack.game.id,
                    format!("install rule targets unknown path name `{}`", rule.into),
                ));
            }
        }

        let install = pack
            .install
            .iter()
            .map(|rule| {
                Ok(CompiledRule {
                    matcher: compile(&rule.pattern)?,
                    rule: rule.clone(),
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let reject = pack
            .assets
            .reject
            .iter()
            .filter_map(|p| compile(p).ok())
            .collect();
        let unpack = pack
            .assets
            .unpack
            .iter()
            .filter_map(|p| compile(p).ok())
            .collect();

        let game_files = pack
            .instance
            .as_ref()
            .map(|i| {
                i.game_files
                    .iter()
                    .filter_map(|p| compile(&p.to_ascii_lowercase()).ok())
                    .collect()
            })
            .unwrap_or_default();

        Ok(Self {
            reject,
            unpack,
            install,
            game_files,
            pack,
        })
    }

    /// Can this game read its mods from a directory that is not its own?
    pub fn instancing(&self) -> Option<&InstanceRules> {
        self.pack.instance.as_ref()
    }

    /// Does this destination have to go into the real game folder even when
    /// the profile is instanced?
    ///
    /// Only the injector does. Everything else belongs to the instance, which
    /// is the entire point: the game directory stays vanilla.
    pub fn belongs_in_game_dir(&self, rel: &Path) -> bool {
        if self.game_files.is_empty() {
            return false;
        }
        let lowered = rel.to_string_lossy().to_ascii_lowercase().replace('\\', "/");
        let base = lowered.rsplit('/').next().unwrap_or(&lowered).to_string();
        self.game_files
            .iter()
            .any(|m| m.is_match(&lowered) || m.is_match(&base))
    }

    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).ctx(format!("reading {}", path.display()))?;
        let pack: Pack = toml::from_str(&text).ctx(format!("parsing {}", path.display()))?;
        Self::new(pack)
    }

    /// The Steam app id of this game's client, if it has one.
    fn steam_app(&self) -> Option<u32> {
        self.pack
            .targets
            .iter()
            .find(|t| t.kind == TargetKind::Client)
            .or_else(|| self.pack.targets.first())
            .and_then(|t| t.steam.as_ref())
            .map(|s| s.app_id)
    }

    /// Portrait box art for the game, for a tile in a grid.
    ///
    /// A pack's own `icon` wins. Failing that, a game with a Steam app id gets
    /// Steam's public library art — which is the game's own store artwork,
    /// served by the same CDN a browser would use, and costs the pack author
    /// nothing to have. A game with neither gets `None`, and the UI draws a
    /// generated tile rather than a broken image.
    pub fn icon_url(&self) -> Option<String> {
        if let Some(url) = &self.pack.game.icon {
            return Some(url.clone());
        }
        self.steam_app().map(|id| {
            format!("https://cdn.cloudflare.steamstatic.com/steam/apps/{id}/library_600x900.jpg")
        })
    }

    /// A wide image for the top of the game's own page.
    pub fn banner_url(&self) -> Option<String> {
        if let Some(url) = &self.pack.game.art {
            return Some(url.clone());
        }
        self.steam_app().map(|id| {
            format!("https://cdn.cloudflare.steamstatic.com/steam/apps/{id}/header.jpg")
        })
    }

    pub fn id(&self) -> &str {
        &self.pack.game.id
    }

    pub fn target(&self, id: &str) -> Option<&Target> {
        self.pack.targets.iter().find(|t| t.id == id)
    }

    /// Logical path names for a target: pack defaults with target overrides on top.
    pub fn paths_for(&self, target: &Target) -> BTreeMap<String, String> {
        let mut map = self.pack.paths.clone();
        map.extend(target.paths.clone());
        map
    }

    /// Resolve a logical path name against a concrete game root.
    pub fn resolve_path(&self, target: &Target, name: &str, root: &Path) -> Option<PathBuf> {
        let map = self.paths_for(target);
        let rel = map.get(name)?;
        Some(if rel == "." {
            root.to_path_buf()
        } else {
            root.join(rel)
        })
    }

    /// Pick the install rule for one archive entry, or `None` to skip the file.
    ///
    /// A rule marked `skip` answers `None` too: it exists to stop a later,
    /// broader rule from claiming the file, and "nothing installs this" is the
    /// same answer either way as far as every caller is concerned.
    pub fn rule_for(&self, archive_path: &str, target_id: &str) -> Option<&InstallRule> {
        let lowered = archive_path.to_ascii_lowercase();
        self.install
            .iter()
            .find(|c| {
                let applies = c
                    .rule
                    .targets
                    .as_ref()
                    .map(|ids| ids.iter().any(|id| id == target_id))
                    .unwrap_or(true);
                applies && c.matcher.is_match(&lowered)
            })
            .map(|c| &c.rule)
            .filter(|rule| !rule.skip)
    }

    /// Where one archive entry lands, relative to the target root.
    pub fn destination_for(
        &self,
        archive_path: &str,
        target: &Target,
        rule: &InstallRule,
    ) -> Option<PathBuf> {
        let map = self.paths_for(target);
        let base = map.get(&rule.into)?;

        let mut rel = archive_path.to_string();
        if let Some(strip) = &rule.strip {
            let strip = strip.trim_end_matches('/');
            // Prefixes can appear mid-path when authors nest their zip.
            if let Some(idx) = rel.to_ascii_lowercase().find(&strip.to_ascii_lowercase()) {
                let cut = idx + strip.len();
                rel = rel[cut..].trim_start_matches('/').to_string();
            }
        }
        if rule.flatten {
            rel = Path::new(&rel)
                .file_name()?
                .to_string_lossy()
                .into_owned();
        }
        if rel.is_empty() {
            return None;
        }

        Some(if base == "." {
            PathBuf::from(rel)
        } else {
            Path::new(base).join(rel)
        })
    }

    /// Choose one asset from a release. Returns the index of the winner.
    pub fn select_asset(&self, names: &[String], target: &Target) -> Option<usize> {
        let flavor = target.flavor.as_deref();
        let target_reject: Vec<GlobMatcher> = target
            .asset_reject
            .iter()
            .filter_map(|p| compile(p).ok())
            .collect();

        let allowed: Vec<usize> = names
            .iter()
            .enumerate()
            .filter(|(_, n)| {
                let lowered = n.to_ascii_lowercase();
                !self.reject.iter().any(|r| r.is_match(&lowered))
                    && !target_reject.iter().any(|r| r.is_match(&lowered))
            })
            .map(|(i, _)| i)
            .collect();
        if allowed.is_empty() {
            return None;
        }

        for group in [&self.pack.assets.prefer, &self.pack.assets.accept] {
            for pattern in group {
                let Some(matcher) = compile_with_flavor(pattern, flavor) else {
                    continue;
                };
                if let Some(&idx) = allowed
                    .iter()
                    .find(|&&i| matcher.is_match(&names[i].to_ascii_lowercase()))
                {
                    return Some(idx);
                }
            }
        }
        None
    }

    /// One loader definition by id.
    pub fn loader(&self, id: &str) -> Option<&LoaderDef> {
        self.pack.loaders.iter().find(|l| l.id.eq_ignore_ascii_case(id))
    }

    /// Directories this pack installs into, resolved against a game root.
    ///
    /// The bool says the directory holds profile-owned state (configs), where a
    /// file we did not place is normal rather than suspicious.
    pub fn managed_dirs(&self, target: &Target, root: &Path) -> Vec<(PathBuf, bool)> {
        let mut seen: Vec<String> = Vec::new();
        for rule in &self.pack.install {
            let applies = rule
                .targets
                .as_ref()
                .map(|ids| ids.iter().any(|id| id == &target.id))
                .unwrap_or(true);
            if applies && !seen.contains(&rule.into) {
                seen.push(rule.into.clone());
            }
        }
        seen.into_iter()
            .filter_map(|name| {
                let is_state = self.pack.state.paths.contains(&name);
                // "." would mean scanning the whole game install.
                let rel = self.paths_for(target).get(&name)?.clone();
                (rel != ".").then(|| (root.join(rel), is_state))
            })
            .collect()
    }

    /// Directories holding profile-owned state, resolved against a game root.
    pub fn state_dirs(&self, target: &Target, root: &Path) -> Vec<(String, PathBuf)> {
        self.pack
            .state
            .paths
            .iter()
            .filter_map(|name| {
                self.resolve_path(target, name, root)
                    .map(|path| (name.clone(), path))
            })
            .collect()
    }

    /// Whether a downloaded asset should be extracted or stored whole.
    pub fn should_unpack(&self, asset_name: &str) -> bool {
        let lowered = asset_name.to_ascii_lowercase();
        self.unpack.iter().any(|m| m.is_match(&lowered))
    }

    pub fn is_readable(&self, file_name: &str) -> bool {
        self.has_extension(file_name, &self.pack.readable_extensions)
    }

    /// Can this file run code the user cannot read?
    pub fn is_executable(&self, file_name: &str) -> bool {
        self.has_extension(file_name, &self.pack.executable_extensions)
    }

    fn has_extension(&self, file_name: &str, list: &[String]) -> bool {
        let ext = Path::new(file_name)
            .extension()
            .map(|e| e.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default();
        list.iter().any(|e| e.eq_ignore_ascii_case(&ext))
    }

    /// Directories that look like this target, best guesses first.
    pub fn detect(&self, target: &Target) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let mut push = |p: PathBuf| {
            if self.matches_markers(target, &p) && !out.contains(&p) {
                out.push(p);
            }
        };

        if let Some(steam) = &target.steam {
            for lib in crate::steam::library_paths() {
                push(lib.join("steamapps").join("common").join(&steam.dir));
            }
        }
        for candidate in &target.candidates {
            if let Some(expanded) = expand_env(candidate) {
                push(PathBuf::from(expanded));
            }
        }
        out
    }

    /// A directory is this target if any marker is present. An empty marker
    /// list never matches, so a pack cannot accidentally claim a whole drive.
    pub fn matches_markers(&self, target: &Target, root: &Path) -> bool {
        target.markers.iter().any(|m| root.join(m).exists())
    }
}

/// Load every `*.toml` in a directory, skipping unreadable ones rather than
/// letting one broken community pack take the whole app down.
pub fn load_dir(dir: &Path) -> (Vec<CompiledPack>, Vec<(PathBuf, Error)>) {
    let mut packs = Vec::new();
    let mut errors = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return (packs, errors);
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("toml") {
            continue;
        }
        match CompiledPack::load(&path) {
            Ok(p) => packs.push(p),
            Err(e) => errors.push((path, e)),
        }
    }
    packs.sort_by(|a, b| a.pack.game.name.cmp(&b.pack.game.name));
    (packs, errors)
}
