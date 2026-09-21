//! Modpack index formats.
//!
//! A modpack is a profile. Both formats in use carry exactly what a `Profile`
//! already holds — a game version, a loader, a pinned mod list and a tree of
//! config overrides — so importing one generates a profile and everything
//! downstream (sync, the trust ladder, the store, deploy, sharing) works
//! unchanged.
//!
//! Two formats, and the difference between them is worth knowing:
//!
//! - **CurseForge** ships a `manifest.json` of `projectID`/`fileID` pairs. It
//!   names no URLs and no hashes, so resolving one needs the CurseForge API,
//!   which needs the key the user supplies themselves.
//! - **Modrinth `.mrpack`** ships a `modrinth.index.json` of direct download
//!   URLs *with* SHA-1 and SHA-512 for every file. It needs no key at all, and
//!   the hashes let each file be traced back to the project that published it.
//!
//! This module parses and nothing else: no network, no disk, no store. That
//! keeps the formats testable against fixtures and keeps the decisions about
//! what to *do* with a pack in the engine where they belong.

use serde::Deserialize;

use crate::error::{Error, Result};

/// Which index a pack archive turned out to contain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModpackFormat {
    CurseForge,
    Modrinth,
    /// A Thunderstore package whose manifest lists other packages. The only
    /// one of the three that is not Minecraft-specific.
    Thunderstore,
}

impl ModpackFormat {
    pub fn label(self) -> &'static str {
        match self {
            ModpackFormat::CurseForge => "CurseForge",
            ModpackFormat::Modrinth => "Modrinth",
            ModpackFormat::Thunderstore => "Thunderstore",
        }
    }
}

/// Whether a pack file belongs on one side of the game.
///
/// `.mrpack` states this per file, which is how a pack ships a minimap for the
/// client and an anti-cheat for the server out of one archive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Requirement {
    #[default]
    Required,
    Optional,
    Unsupported,
}

impl Requirement {
    fn parse(value: &str) -> Self {
        match value {
            "unsupported" => Requirement::Unsupported,
            "optional" => Requirement::Optional,
            _ => Requirement::Required,
        }
    }

    /// Whether the file should be installed on this side at all. Optional
    /// counts as yes: a pack marking something optional still shipped it, and
    /// silently dropping it changes the pack the author published.
    pub fn wanted(self) -> bool {
        !matches!(self, Requirement::Unsupported)
    }
}

/// One thing a modpack asks for, in whichever way its format expresses it.
#[derive(Debug, Clone)]
pub enum PackEntry {
    /// A CurseForge project held at an exact file. The file id, not the
    /// project's newest release — a pack that resolved to "whatever is newest"
    /// would not be the pack its author tested.
    CurseForge {
        project: u64,
        file: u64,
        required: bool,
    },
    /// A Thunderstore package at an exact version.
    Thunderstore {
        namespace: String,
        name: String,
        version: String,
    },
    /// A file with its own download URLs and hashes, as `.mrpack` publishes.
    Download {
        /// Destination inside the game directory, e.g. `mods/sodium.jar`.
        path: String,
        urls: Vec<String>,
        sha512: Option<String>,
        sha1: Option<String>,
        size: u64,
        client: Requirement,
        server: Requirement,
    },
}

/// A modpack reduced to Modifile's own terms.
#[derive(Debug, Clone, Default)]
pub struct PackPlan {
    pub format_name: String,
    /// The game the pack is for, as the format states it. Both formats in use
    /// define Minecraft and nothing else, so this is how an import finds the
    /// right game pack without being told.
    pub game: String,
    pub name: String,
    pub version: String,
    pub author: String,
    pub summary: String,
    pub game_version: Option<String>,
    /// Loader id as this project spells it: `fabric`, `quilt`, `forge`,
    /// `neoforge`. Already translated from whatever the pack called it.
    pub loader: Option<String>,
    /// The loader build the pack was tested against. Recorded and reported;
    /// Modifile installs the newest stable of that loader rather than pinning
    /// it, and says so rather than pretending otherwise.
    pub loader_version: Option<String>,
    pub entries: Vec<PackEntry>,
    /// Directory inside the archive holding the pack's own game files.
    pub overrides: Vec<String>,
}

impl PackPlan {
    pub fn label(&self) -> String {
        if self.version.is_empty() {
            self.name.clone()
        } else {
            format!("{} {}", self.name, self.version)
        }
    }
}

// ---------------------------------------------------------------------------
// Locating the index
// ---------------------------------------------------------------------------

/// Which file in a pack archive is its index.
///
/// Matched at the archive root only. A random `manifest.json` three
/// directories down belongs to something else, and treating it as a pack index
/// would turn an ordinary mod into a confusing failure.
///
/// The format is *not* decided here, because CurseForge and Thunderstore both
/// call their index `manifest.json` and only the contents tell them apart.
/// That is `sniff`'s job.
pub fn find_index(rel_paths: &[String]) -> Option<String> {
    for name in ["modrinth.index.json", "manifest.json"] {
        if let Some(path) = rel_paths.iter().find(|p| p.eq_ignore_ascii_case(name)) {
            return Some(path.clone());
        }
    }
    None
}

/// Which format an index actually is, from its contents.
///
/// Needed because the file name is ambiguous: a CurseForge pack and a
/// Thunderstore pack both ship `manifest.json`, and they share no fields. Each
/// format is identified by something only it has, so a file that is neither is
/// reported as neither rather than being misread as the first guess.
pub fn sniff(file_name: &str, bytes: &[u8]) -> Option<ModpackFormat> {
    if file_name.eq_ignore_ascii_case("modrinth.index.json") {
        return Some(ModpackFormat::Modrinth);
    }

    let value: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    let object = value.as_object()?;

    // CurseForge: the game section, or the type tag.
    if object.contains_key("minecraft") || object.contains_key("manifestType") {
        return Some(ModpackFormat::CurseForge);
    }
    // Thunderstore: a version and a list of `Namespace-Name-Version` strings.
    if object.contains_key("version_number") && object.contains_key("dependencies") {
        return Some(ModpackFormat::Thunderstore);
    }
    // A .mrpack index that somehow arrived under another name.
    if object.contains_key("formatVersion") && object.contains_key("files") {
        return Some(ModpackFormat::Modrinth);
    }
    None
}

// ---------------------------------------------------------------------------
// CurseForge
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CurseForgeManifest {
    #[serde(default)]
    pub manifest_type: String,
    #[serde(default)]
    pub manifest_version: u32,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub author: String,
    #[serde(default)]
    pub minecraft: CfMinecraft,
    #[serde(default)]
    pub files: Vec<CfFile>,
    /// Defaults to `overrides`; packs occasionally rename it.
    #[serde(default = "default_overrides")]
    pub overrides: String,
}

fn default_overrides() -> String {
    "overrides".to_string()
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CfMinecraft {
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub mod_loaders: Vec<CfLoader>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CfLoader {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub primary: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CfFile {
    #[serde(rename = "projectID")]
    pub project_id: u64,
    #[serde(rename = "fileID")]
    pub file_id: u64,
    #[serde(default = "yes")]
    pub required: bool,
}

fn yes() -> bool {
    true
}

impl CurseForgeManifest {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let parsed: Self = serde_json::from_slice(bytes)
            .map_err(|e| Error::other(format!("this manifest.json is not readable: {e}")))?;
        if !parsed.manifest_type.is_empty() && parsed.manifest_type != "minecraftModpack" {
            return Err(Error::other(format!(
                "this is a `{}` manifest. Modifile understands CurseForge's Minecraft \
                 modpack format; other games' packs use a different one.",
                parsed.manifest_type
            )));
        }
        Ok(parsed)
    }

    pub fn to_plan(&self) -> PackPlan {
        // The primary loader is the one the pack expects to run on; the rest
        // are alternatives its author listed.
        let loader = self
            .minecraft
            .mod_loaders
            .iter()
            .find(|l| l.primary)
            .or_else(|| self.minecraft.mod_loaders.first());
        let (loader_id, loader_version) = loader
            .map(|l| split_loader_id(&l.id))
            .unwrap_or((None, None));

        PackPlan {
            format_name: ModpackFormat::CurseForge.label().to_string(),
            // `manifestType: minecraftModpack` is checked on parse, so by the
            // time there is a plan the game is not in doubt.
            game: "minecraft".to_string(),
            name: self.name.clone(),
            version: self.version.clone(),
            author: self.author.clone(),
            summary: String::new(),
            game_version: non_empty(&self.minecraft.version),
            loader: loader_id,
            loader_version,
            entries: self
                .files
                .iter()
                .map(|f| PackEntry::CurseForge {
                    project: f.project_id,
                    file: f.file_id,
                    required: f.required,
                })
                .collect(),
            overrides: vec![self.overrides.clone()],
        }
    }
}

/// `forge-47.2.0` -> (`forge`, `47.2.0`). Also copes with `fabric-loader-0.15.7`,
/// which some exporters write instead of `fabric-0.15.7`.
fn split_loader_id(id: &str) -> (Option<String>, Option<String>) {
    let id = id.trim();
    if id.is_empty() {
        return (None, None);
    }
    // The version is the tail beginning at the first digit-led segment, so a
    // multi-word loader name survives intact.
    let mut name_parts: Vec<&str> = Vec::new();
    let mut version: Option<String> = None;
    let mut rest = id.split('-').peekable();
    while let Some(part) = rest.next() {
        if part.chars().next().is_some_and(|c| c.is_ascii_digit()) {
            let mut v = vec![part];
            v.extend(rest);
            version = Some(v.join("-"));
            break;
        }
        name_parts.push(part);
    }
    // `fabric-loader` and `quilt-loader` are the same loaders this project
    // calls `fabric` and `quilt`.
    if name_parts.len() > 1 && name_parts.last() == Some(&"loader") {
        name_parts.pop();
    }
    let name = name_parts.join("-").to_ascii_lowercase();
    (non_empty(&name), version)
}

// ---------------------------------------------------------------------------
// Modrinth
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModrinthIndex {
    #[serde(default)]
    pub format_version: u32,
    #[serde(default)]
    pub game: String,
    #[serde(default)]
    pub version_id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub files: Vec<MrFile>,
    #[serde(default)]
    pub dependencies: std::collections::BTreeMap<String, String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MrFile {
    pub path: String,
    #[serde(default)]
    pub hashes: MrHashes,
    #[serde(default)]
    pub env: Option<MrEnv>,
    #[serde(default)]
    pub downloads: Vec<String>,
    #[serde(default)]
    pub file_size: u64,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct MrHashes {
    #[serde(default)]
    pub sha1: Option<String>,
    #[serde(default)]
    pub sha512: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct MrEnv {
    #[serde(default)]
    pub client: Option<String>,
    #[serde(default)]
    pub server: Option<String>,
}

impl ModrinthIndex {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let parsed: Self = serde_json::from_slice(bytes).map_err(|e| {
            Error::other(format!("this modrinth.index.json is not readable: {e}"))
        })?;
        if !parsed.game.is_empty() && parsed.game != "minecraft" {
            return Err(Error::other(format!(
                "this pack is for `{}`. The .mrpack format only defines Minecraft today.",
                parsed.game
            )));
        }
        if parsed.format_version > 1 {
            return Err(Error::other(format!(
                "this pack uses .mrpack format version {}, which is newer than the one \
                 Modifile knows how to read. Update Modifile, or ask the pack's author \
                 for a version 1 export.",
                parsed.format_version
            )));
        }
        Ok(parsed)
    }

    pub fn to_plan(&self) -> Result<PackPlan> {
        let mut entries = Vec::with_capacity(self.files.len());
        for file in &self.files {
            // The format forbids these, and a pack that carries one is trying
            // to write outside the game directory.
            check_pack_path(&file.path)?;
            let env = file.env.clone().unwrap_or_default();
            entries.push(PackEntry::Download {
                path: file.path.clone(),
                urls: file.downloads.clone(),
                sha512: file.hashes.sha512.clone(),
                sha1: file.hashes.sha1.clone(),
                size: file.file_size,
                client: env
                    .client
                    .as_deref()
                    .map(Requirement::parse)
                    .unwrap_or_default(),
                server: env
                    .server
                    .as_deref()
                    .map(Requirement::parse)
                    .unwrap_or_default(),
            });
        }

        let (loader, loader_version) = ["fabric-loader", "quilt-loader", "neoforge", "forge"]
            .iter()
            .find_map(|key| {
                self.dependencies
                    .get(*key)
                    .map(|v| (split_loader_id(key).0, Some(v.clone())))
            })
            .unwrap_or((None, None));

        Ok(PackPlan {
            format_name: ModpackFormat::Modrinth.label().to_string(),
            // Empty only on a pack that omitted the field; the format defines
            // no other value, and parse rejects any it does not know.
            game: if self.game.is_empty() { "minecraft".into() } else { self.game.clone() },
            name: self.name.clone(),
            version: self.version_id.clone(),
            author: String::new(),
            summary: self.summary.clone(),
            game_version: self.dependencies.get("minecraft").cloned(),
            loader,
            loader_version,
            entries,
            // Applied in this order, so a side-specific tree wins over the
            // shared one — which is how the format defines it.
            overrides: vec![
                "overrides".to_string(),
                "client-overrides".to_string(),
                "server-overrides".to_string(),
            ],
        })
    }
}

// ---------------------------------------------------------------------------
// Thunderstore
// ---------------------------------------------------------------------------

/// A Thunderstore package manifest.
///
/// Every package has one; what makes a package a *modpack* is that its
/// `dependencies` are the point and it ships little or nothing of its own.
/// Thunderstore does not mark packs in the manifest, so this is not a flag we
/// can read — which is fine, because the two cases collapse: a pack's
/// dependencies become the profile's mods, and whatever files the package
/// itself carries are installed alongside them exactly as any other mod's
/// would be.
///
/// It is also the only pack format here that is not tied to one game. The
/// game comes from the profile, not the manifest.
#[derive(Debug, Clone, Deserialize)]
pub struct ThunderstoreManifest {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub version_number: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub website_url: String,
    /// `Namespace-Name-Version` strings.
    #[serde(default)]
    pub dependencies: Vec<String>,
}

impl ThunderstoreManifest {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        serde_json::from_slice(bytes)
            .map_err(|e| Error::other(format!("this Thunderstore manifest.json is not readable: {e}")))
    }

    /// `game` cannot come from the manifest, because a Thunderstore pack does
    /// not name its game — the community it was published under does. The
    /// caller supplies it.
    pub fn to_plan(&self, game: &str) -> Result<PackPlan> {
        let mut entries = Vec::with_capacity(self.dependencies.len());
        let mut unreadable = Vec::new();

        for dep in &self.dependencies {
            match crate::source::thunderstore::split_dependency(dep) {
                Some((namespace, name, version)) => entries.push(PackEntry::Thunderstore {
                    namespace,
                    name,
                    version,
                }),
                None => unreadable.push(dep.clone()),
            }
        }

        if entries.is_empty() && !unreadable.is_empty() {
            return Err(Error::other(format!(
                "none of this pack's {} dependencies are in Thunderstore's \
                 `Namespace-Name-Version` form, so there is nothing to install. \
                 First one: `{}`.",
                unreadable.len(),
                unreadable[0]
            )));
        }

        Ok(PackPlan {
            format_name: ModpackFormat::Thunderstore.label().to_string(),
            game: game.to_string(),
            name: self.name.clone(),
            version: self.version_number.clone(),
            author: String::new(),
            summary: self.description.clone(),
            // A Thunderstore pack states neither, because its packages are
            // built against the game itself rather than a loader/version pair.
            game_version: None,
            loader: None,
            loader_version: None,
            entries,
            // A Thunderstore package has no overrides directory: its own files
            // sit at the archive root and are installed by the game pack's
            // ordinary rules, like any other package's.
            overrides: Vec::new(),
        })
    }
}

/// Reject anything that would escape the game directory.
///
/// The store's extractor already refuses zip-slip on the way in; this is the
/// same rule applied to the paths a pack *declares*, which arrive as text in a
/// JSON document rather than as archive entries.
fn check_pack_path(path: &str) -> Result<()> {
    let bad = path.is_empty()
        || path.starts_with('/')
        || path.starts_with('\\')
        || path.contains("..")
        || path.chars().nth(1) == Some(':');
    if bad {
        return Err(Error::other(format!(
            "this pack asks to write to `{path}`, which is outside the game directory. \
             Refusing the whole pack."
        )));
    }
    Ok(())
}

fn non_empty(s: &str) -> Option<String> {
    let s = s.trim();
    (!s.is_empty()).then(|| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const CF: &str = r#"{
      "minecraft": {
        "version": "1.20.1",
        "modLoaders": [{ "id": "forge-47.2.0", "primary": true }]
      },
      "manifestType": "minecraftModpack",
      "manifestVersion": 1,
      "name": "Example Pack",
      "version": "1.0.0",
      "author": "someone",
      "files": [
        { "projectID": 238222, "fileID": 5246076, "required": true },
        { "projectID": 306612, "fileID": 4634018, "required": false }
      ],
      "overrides": "overrides"
    }"#;

    const MR: &str = r#"{
      "formatVersion": 1,
      "game": "minecraft",
      "versionId": "2.1.0",
      "name": "Tidy Pack",
      "summary": "a small one",
      "files": [
        {
          "path": "mods/sodium.jar",
          "hashes": { "sha1": "aaa", "sha512": "bbb" },
          "env": { "client": "required", "server": "unsupported" },
          "downloads": ["https://cdn.modrinth.com/data/AANobbMI/versions/x/sodium.jar"],
          "fileSize": 1234
        },
        {
          "path": "mods/spark.jar",
          "hashes": { "sha512": "ccc" },
          "downloads": ["https://cdn.modrinth.com/data/l6YH9Als/versions/y/spark.jar"],
          "fileSize": 99
        }
      ],
      "dependencies": { "minecraft": "1.20.1", "fabric-loader": "0.15.7" }
    }"#;

    const TS: &str = r#"{
      "name": "MyPack",
      "version_number": "1.4.0",
      "website_url": "https://github.com/someone/mypack",
      "description": "A pack for R.E.P.O.",
      "dependencies": [
        "BepInEx-BepInExPack-5.4.2100",
        "Zehs-REPOLib-2.1.0"
      ]
    }"#;

    #[test]
    fn reads_a_curseforge_manifest() {
        let plan = CurseForgeManifest::parse(CF.as_bytes()).unwrap().to_plan();
        assert_eq!(plan.name, "Example Pack");
        assert_eq!(plan.game_version.as_deref(), Some("1.20.1"));
        assert_eq!(plan.loader.as_deref(), Some("forge"));
        assert_eq!(plan.loader_version.as_deref(), Some("47.2.0"));
        assert_eq!(plan.entries.len(), 2);
        match &plan.entries[0] {
            PackEntry::CurseForge { project, file, required } => {
                assert_eq!((*project, *file, *required), (238222, 5246076, true));
            }
            other => panic!("wrong entry kind: {other:?}"),
        }
    }

    #[test]
    fn reads_an_mrpack_index() {
        let plan = ModrinthIndex::parse(MR.as_bytes()).unwrap().to_plan().unwrap();
        assert_eq!(plan.name, "Tidy Pack");
        assert_eq!(plan.game_version.as_deref(), Some("1.20.1"));
        assert_eq!(plan.loader.as_deref(), Some("fabric"));
        assert_eq!(plan.loader_version.as_deref(), Some("0.15.7"));

        match &plan.entries[0] {
            PackEntry::Download { path, client, server, sha512, .. } => {
                assert_eq!(path, "mods/sodium.jar");
                // A client-only mod must not be planted on a dedicated server.
                assert!(client.wanted());
                assert!(!server.wanted());
                assert_eq!(sha512.as_deref(), Some("bbb"));
            }
            other => panic!("wrong entry kind: {other:?}"),
        }
        // A file with no `env` at all is wanted on both sides.
        match &plan.entries[1] {
            PackEntry::Download { client, server, .. } => {
                assert!(client.wanted() && server.wanted());
            }
            other => panic!("wrong entry kind: {other:?}"),
        }
    }

    #[test]
    fn splits_every_loader_spelling() {
        assert_eq!(
            split_loader_id("forge-47.2.0"),
            (Some("forge".into()), Some("47.2.0".into()))
        );
        assert_eq!(
            split_loader_id("neoforge-21.0.0-beta"),
            (Some("neoforge".into()), Some("21.0.0-beta".into()))
        );
        // Both spellings of Fabric reduce to the id this project uses.
        assert_eq!(split_loader_id("fabric-0.15.7").0, Some("fabric".into()));
        assert_eq!(
            split_loader_id("fabric-loader-0.15.7").0,
            Some("fabric".into())
        );
        assert_eq!(split_loader_id("quilt-loader").0, Some("quilt".into()));
    }

    #[test]
    fn finds_the_index_at_the_archive_root_only() {
        assert_eq!(
            find_index(&["manifest.json".into(), "overrides/config/a.toml".into()]),
            Some("manifest.json".to_string())
        );
        // A mod that happens to carry a manifest.json is not a modpack.
        assert_eq!(find_index(&["some/nested/manifest.json".into()]), None);
        // An mrpack index wins: an exporter may write both.
        assert_eq!(
            find_index(&["manifest.json".into(), "modrinth.index.json".into()]),
            Some("modrinth.index.json".to_string())
        );
    }

    #[test]
    fn tells_the_two_manifest_json_formats_apart() {
        // CurseForge and Thunderstore both call their index manifest.json, so
        // the name settles nothing and only the contents do. Getting this
        // wrong means reading a Thunderstore pack as a Minecraft one.
        assert_eq!(
            sniff("manifest.json", CF.as_bytes()),
            Some(ModpackFormat::CurseForge)
        );
        assert_eq!(
            sniff("manifest.json", TS.as_bytes()),
            Some(ModpackFormat::Thunderstore)
        );
        assert_eq!(
            sniff("modrinth.index.json", MR.as_bytes()),
            Some(ModpackFormat::Modrinth)
        );
        // An ordinary mod's manifest is none of them.
        assert_eq!(sniff("manifest.json", br#"{"schemaVersion":1}"#), None);
        assert_eq!(sniff("manifest.json", b"not json at all"), None);
    }

    #[test]
    fn reads_a_thunderstore_manifest() {
        let plan = ThunderstoreManifest::parse(TS.as_bytes())
            .unwrap()
            .to_plan("repo")
            .unwrap();
        assert_eq!(plan.name, "MyPack");
        assert_eq!(plan.game, "repo");
        // A Thunderstore pack names no game version and no loader; its
        // packages are built against the game itself.
        assert!(plan.game_version.is_none() && plan.loader.is_none());
        assert_eq!(plan.entries.len(), 2);
        match &plan.entries[0] {
            PackEntry::Thunderstore { namespace, name, version } => {
                assert_eq!((namespace.as_str(), name.as_str(), version.as_str()),
                           ("BepInEx", "BepInExPack", "5.4.2100"));
            }
            other => panic!("wrong entry kind: {other:?}"),
        }
    }

    #[test]
    fn a_thunderstore_pack_with_no_readable_dependencies_is_refused() {
        // Better to refuse than to create an empty profile and call it a pack.
        let bad = r#"{"name":"X","version_number":"1.0.0","dependencies":["garbage"]}"#;
        let err = ThunderstoreManifest::parse(bad.as_bytes())
            .unwrap()
            .to_plan("repo")
            .unwrap_err()
            .to_string();
        assert!(err.contains("garbage"), "{err}");
    }

    #[test]
    fn refuses_a_pack_that_writes_outside_the_game_folder() {
        for path in [
            "../../evil.jar",
            "/etc/passwd",
            "C:/Windows/System32/x.dll",
            "mods/../../escape.jar",
        ] {
            assert!(check_pack_path(path).is_err(), "should refuse {path}");
        }
        assert!(check_pack_path("mods/fine.jar").is_ok());
    }

    #[test]
    fn refuses_another_games_manifest() {
        let other = r#"{"manifestType":"someOtherGameModpack","name":"x"}"#;
        let err = CurseForgeManifest::parse(other.as_bytes()).unwrap_err().to_string();
        assert!(err.contains("someOtherGameModpack"), "{err}");
    }
}
