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
    id: String,
    #[serde(default)]
    slug: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    description: String,
    /// The long description, as Markdown.
    #[serde(default)]
    body: String,
    #[serde(default)]
    icon_url: Option<String>,
    #[serde(default)]
    gallery: Vec<WireGallery>,
    #[serde(default)]
    project_type: String,
    #[serde(default)]
    source_url: Option<String>,
    #[serde(default)]
    license: Option<WireLicense>,
    #[serde(default)]
    downloads: u64,
}

#[derive(Debug, Deserialize)]
struct WireGallery {
    #[serde(default)]
    url: String,
    #[serde(default)]
    featured: bool,
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

/// A version identified by one of its files' hashes.
///
/// Only the fields a modpack import needs: which project owns the file, and
/// what that version is called, so it can be pinned by name rather than by an
/// opaque id.
#[derive(Debug, Clone, Deserialize)]
pub struct HashedVersion {
    #[serde(default)]
    pub project_id: String,
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub version_number: String,
    #[serde(default)]
    pub name: String,
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
    icon_url: Option<String>,
    #[serde(default)]
    author: Option<String>,
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
        self.search_type(query, filter, None).await
    }

    /// As `search`, narrowed to one kind of project — `mod`, `modpack`,
    /// `shader`, `resourcepack`. Without it the index answers with all of
    /// them, and a modpack listed among mods is an invitation to install a
    /// zip full of other people's jars as though it were one addon.
    pub async fn search_type(
        &self,
        query: &str,
        filter: &VersionFilter,
        project_type: Option<&str>,
    ) -> Result<Vec<SearchHit>> {
        let mut facets: Vec<String> = Vec::new();
        if let Some(t) = project_type {
            facets.push(format!("[\"project_type:{t}\"]"));
        }
        if let Some(v) = &filter.game_version {
            facets.push(format!("[\"versions:{v}\"]"));
        }
        // A modpack is not published "for Fabric" the way a mod is — the pack
        // *contains* its loader choice — so this facet would return nothing.
        let searching_packs = project_type == Some("modpack");
        if let Some(l) = filter.loader.as_ref().filter(|_| !searching_packs) {
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
                icon_url: hit.icon_url.filter(|u| !u.is_empty()),
                author: hit.author.filter(|a| !a.is_empty()),
            });
        }
        Ok(out)
    }

    pub fn http(&self) -> &Http {
        &self.http
    }

    /// Everything a page about one project needs, in one request.
    ///
    /// Modrinth is the well-behaved source here: icon, gallery, long
    /// description and licence all come back together, so a detail page costs
    /// exactly one call.
    pub async fn details(&self, id: &ModId) -> Result<crate::source::Details> {
        let url = format!("{API}/project/{}", urlencode(&id.repo));
        let wire: WireProject = self
            .http
            .get_json(&url)
            .await?
            .ok_or_else(|| crate::error::Error::NotFound(format!("Modrinth project {id}")))?;

        // Featured images first: that is the author saying which one to show.
        let mut gallery: Vec<&WireGallery> = wire.gallery.iter().collect();
        gallery.sort_by_key(|g| !g.featured);

        let slug = if wire.slug.is_empty() { id.repo.clone() } else { wire.slug };
        Ok(crate::source::Details {
            id: Some(ModId::project(
                crate::source::SourceKind::Modrinth,
                if wire.id.is_empty() { slug.clone() } else { wire.id },
            )),
            title: wire.title,
            summary: wire.description,
            body: Some(crate::text::markdown_to_text(&wire.body))
                .filter(|b| !b.trim().is_empty()),
            icon_url: wire.icon_url.filter(|u| !u.is_empty()),
            gallery: gallery
                .into_iter()
                .map(|g| g.url.clone())
                .filter(|u| !u.is_empty())
                .collect(),
            // The search index carries an author; the project endpoint carries
            // a team id, which is not a name. Left to the caller.
            authors: Vec::new(),
            downloads: wire.downloads,
            source_url: wire.source_url.filter(|u| !u.is_empty()),
            web_url: format!("https://modrinth.com/project/{slug}"),
            license: wire.license.and_then(|l| l.id),
            is_pack: wire.project_type == "modpack",
        })
    }

    /// The downloadable file of one version, addressed by version id.
    ///
    /// Used to turn "install this modpack" into an archive: a `.mrpack` is the
    /// primary file of an ordinary Modrinth version.
    pub async fn version_primary_file(&self, version_id: &str) -> Result<Asset> {
        let url = format!("{API}/version/{}", urlencode(version_id));
        let wire: WireVersion = self
            .http
            .get_json(&url)
            .await?
            .ok_or_else(|| crate::error::Error::NotFound(format!("Modrinth version {version_id}")))?;

        let file = wire
            .files
            .iter()
            .find(|f| f.primary)
            .or_else(|| wire.files.first())
            .ok_or_else(|| {
                crate::error::Error::NotFound(format!(
                    "Modrinth version {version_id} publishes no files"
                ))
            })?;

        Ok(Asset {
            name: file.filename.clone(),
            download_url: file.url.clone(),
            size: file.size,
            digest: None,
            sha512: file.hashes.sha512.clone(),
        })
    }

    /// The downloadable file of a project's newest version.
    ///
    /// What "install this modpack" means when someone names the pack rather
    /// than one of its versions.
    pub async fn newest_version_file(&self, slug: &str) -> Result<Asset> {
        let url = format!("{API}/project/{}/version", urlencode(slug));
        let versions: Vec<WireVersion> = self
            .http
            .get_json(&url)
            .await?
            .ok_or_else(|| crate::error::Error::NotFound(format!("Modrinth project {slug}")))?;

        // A release beats a beta, but a project that has only ever published
        // betas should still install rather than report nothing.
        let chosen = versions
            .iter()
            .find(|v| v.version_type == "release")
            .or_else(|| versions.first())
            .ok_or_else(|| {
                crate::error::Error::NotFound(format!("any published version of {slug}"))
            })?;

        let file = chosen
            .files
            .iter()
            .find(|f| f.primary)
            .or_else(|| chosen.files.first())
            .ok_or_else(|| {
                crate::error::Error::NotFound(format!("a file in the newest version of {slug}"))
            })?;

        Ok(Asset {
            name: file.filename.clone(),
            download_url: file.url.clone(),
            size: file.size,
            digest: None,
            sha512: file.hashes.sha512.clone(),
        })
    }

    /// Which project and version each of these file hashes belongs to.
    ///
    /// This is what makes `.mrpack` import honest. The index gives a path, a
    /// URL and a hash but never says which project published the file — so
    /// without this a pack would import as a heap of anonymous downloads, with
    /// no update path and nothing for the trust ladder to look up. One POST
    /// turns the whole list back into real Modrinth projects pinned to real
    /// versions.
    ///
    /// Hashes that belong to no Modrinth project are simply absent from the
    /// result, which is the answer for a jar the pack pulled from GitHub.
    pub async fn versions_by_hash(
        &self,
        hashes: &[String],
        algorithm: &str,
    ) -> Result<std::collections::BTreeMap<String, HashedVersion>> {
        #[derive(serde::Serialize)]
        struct Body<'a> {
            hashes: &'a [String],
            algorithm: &'a str,
        }

        let mut out = std::collections::BTreeMap::new();
        // Chunked for the same reason the CurseForge batches are: a 300-file
        // pack should not turn into one enormous request body.
        for chunk in hashes.chunks(100) {
            let url = format!("{API}/version_files");
            let found: std::collections::BTreeMap<String, HashedVersion> = self
                .http
                .post_json_with(&url, &Body { hashes: chunk, algorithm }, &[])
                .await?
                .unwrap_or_default();
            out.extend(found);
        }
        Ok(out)
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
