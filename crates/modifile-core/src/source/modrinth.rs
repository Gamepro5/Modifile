//! Modrinth.
//!
//! No API key, no application, no approval — the reason it is the first source
//! added after GitHub. It also publishes each project's `source_url`, so a mod
//! whose repository is not obvious from its page can still be traced back to
//! readable code.

use serde::Deserialize;

use crate::error::Result;
use crate::http::Http;
use crate::source::{Asset, ModId, Release, RepoInfo, SearchHit};

const API: &str = "https://api.modrinth.com/v2";

/// Percent-encode a search term. Small enough not to warrant a dependency.
pub(crate) fn urlencode(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for byte in input.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            b' ' => out.push('+'),
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

#[derive(Clone)]
pub struct Modrinth {
    http: Http,
}

#[derive(Debug, Deserialize)]
struct WireProject {
    #[serde(default)]
    title: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    source_url: Option<String>,
    #[serde(default)]
    license: Option<WireLicense>,
    #[serde(default)]
    downloads: u64,
}

#[derive(Debug, Deserialize)]
struct WireLicense {
    #[serde(default)]
    id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WireVersion {
    #[serde(default)]
    name: String,
    version_number: String,
    #[serde(default)]
    date_published: String,
    /// `release`, `beta` or `alpha`.
    #[serde(default)]
    version_type: String,
    #[serde(default)]
    game_versions: Vec<String>,
    #[serde(default)]
    loaders: Vec<String>,
    #[serde(default)]
    files: Vec<WireFile>,
}

#[derive(Debug, Deserialize)]
struct WireFile {
    url: String,
    filename: String,
    #[serde(default)]
    size: u64,
    #[serde(default)]
    primary: bool,
    #[serde(default)]
    hashes: WireHashes,
}

#[derive(Debug, Default, Deserialize)]
struct WireHashes {
    #[serde(default)]
    sha512: Option<String>,
}

/// Narrowing for games whose mods are version- and loader-specific.
#[derive(Debug, Clone, Default)]
pub struct VersionFilter {
    /// e.g. `1.20.1`
    pub game_version: Option<String>,
    /// e.g. `fabric`
    pub loader: Option<String>,
}

impl VersionFilter {
    fn matches(&self, version: &WireVersion) -> bool {
        let game_ok = self
            .game_version
            .as_ref()
            .map(|want| version.game_versions.iter().any(|v| v == want))
            .unwrap_or(true);
        let loader_ok = self
            .loader
            .as_ref()
            .map(|want| {
                version
                    .loaders
                    .iter()
                    .any(|l| l.eq_ignore_ascii_case(want))
            })
            .unwrap_or(true);
        game_ok && loader_ok
    }

    pub fn is_empty(&self) -> bool {
        self.game_version.is_none() && self.loader.is_none()
    }

    pub fn describe(&self) -> String {
        match (&self.game_version, &self.loader) {
            (Some(g), Some(l)) => format!("{g} / {l}"),
            (Some(g), None) => g.clone(),
            (None, Some(l)) => l.clone(),
            (None, None) => "any version".to_string(),
        }
    }
}

#[derive(Debug, Deserialize)]
struct WireSearch {
    #[serde(default)]
    hits: Vec<WireHit>,
}

#[derive(Debug, Deserialize)]
struct WireHit {
    slug: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    downloads: u64,
    #[serde(default)]
    license: Option<String>,
}

impl Modrinth {
    pub fn new(http: Http) -> Self {
        Self { http }
    }

    /// Find mods by name.
    ///
    /// Modrinth is used as the index even for mods you found on CurseForge:
    /// most are published to both, and only Modrinth will tell you, without a
    /// key, where the source code is.
    pub async fn search(&self, query: &str, filter: &VersionFilter) -> Result<Vec<SearchHit>> {
        let mut facets: Vec<String> = Vec::new();
        if let Some(v) = &filter.game_version {
            facets.push(format!("[\"versions:{v}\"]"));
        }
        if let Some(l) = &filter.loader {
            facets.push(format!("[\"categories:{}\"]", l.to_ascii_lowercase()));
        }
        let facet_param = if facets.is_empty() {
            String::new()
        } else {
            format!("&facets=[{}]", facets.join(","))
        };

        let url = format!(
            "{API}/search?query={}&limit=20{facet_param}",
            urlencode(query)
        );
        let wire: WireSearch = self.http.get_json(&url).await?.unwrap_or(WireSearch {
            hits: Vec::new(),
        });

        // The search index does not carry source_url, so ask each project for
        // it. Twenty small requests against a keyless API is acceptable and it
        // is the whole point of the feature.
        let mut out = Vec::new();
        for hit in wire.hits {
            let id = ModId::project(crate::source::SourceKind::Modrinth, &hit.slug);
            let source_url = self
                .project(&id)
                .await
                .ok()
                .flatten()
                .and_then(|p| p.source_url);
            out.push(crate::source::SearchHit {
                id,
                title: hit.title,
                description: hit.description,
                stars: hit.downloads,
                license: hit.license,
                source_url,
                installable: Some(true),
            });
        }
        Ok(out)
    }

    pub fn http(&self) -> &Http {
        &self.http
    }

    /// Versions newest first, filtered to what this profile can actually run.
    pub async fn releases(&self, id: &ModId, filter: &VersionFilter) -> Result<Vec<Release>> {
        let url = format!("{API}/project/{}/version", id.repo);
        let wire: Vec<WireVersion> = self.http.get_json(&url).await?.unwrap_or_default();

        Ok(wire
            .into_iter()
            .filter(|v| filter.matches(v))
            .map(|v| {
                // The primary file is the mod itself; the rest are sources
                // jars and extras the pack's asset rules would reject anyway.
                let mut files = v.files;
                files.sort_by_key(|f| !f.primary);
                Release {
                    tag: v.version_number,
                    name: v.name,
                    published_at: v.date_published,
                    prerelease: v.version_type != "release",
                    web_url: format!("https://modrinth.com/project/{}", id.repo),
                    assets: files
                        .into_iter()
                        .map(|f| Asset {
                            name: f.filename,
                            download_url: f.url,
                            size: f.size,
                            digest: None,
                            sha512: f.hashes.sha512,
                        })
                        .collect(),
                }
            })
            .collect())
    }

    pub async fn project(&self, id: &ModId) -> Result<Option<RepoInfo>> {
        let url = format!("{API}/project/{}", id.repo);
        let Some(wire) = self.http.get_json::<WireProject>(&url).await? else {
            return Ok(None);
        };
        Ok(Some(RepoInfo {
            description: if wire.description.is_empty() {
                wire.title
            } else {
                wire.description
            },
            license: wire
                .license
                .and_then(|l| l.id)
                .filter(|id| !id.is_empty() && id != "unknown" && id != "LicenseRef-Unknown"),
            archived: false,
            stars: wire.downloads,
            source_url: wire.source_url.filter(|u| !u.is_empty()),
        }))
    }
}
