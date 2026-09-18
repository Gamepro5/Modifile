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

        Ok(Self {
            reject,
            unpack,
            install,
            pack,
        })
    }

    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).ctx(format!("reading {}", path.display()))?;
        let pack: Pack = toml::from_str(&text).ctx(format!("parsing {}", path.display()))?;
        Self::new(pack)
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
