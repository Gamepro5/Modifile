//! CurseForge.
//!
//! Two things make this source different from the others, and both are the
//! platform's choices rather than ours:
//!
//! 1. **You supply the key.** Overwolf issues API keys after a human review,
//!    and the terms forbid sharing one. An open-source binary therefore cannot
//!    ship a working key, so Modifile asks for yours.
//! 2. **Some mods cannot be downloaded at all.** Authors can switch off
//!    third-party distribution per project. The API then returns the file with
//!    a null download URL. No key and no client-side cleverness changes that —
//!    the only honest thing is to say so clearly and point at the web page.

use serde::Deserialize;

use crate::error::{Error, Result};
use crate::http::Http;
use crate::source::{Asset, ModId, Release, RepoInfo};

const API: &str = "https://api.curseforge.com/v1";

#[derive(Clone)]
pub struct CurseForge {
    http: Http,
    key: String,
    /// Fetch files whose author disabled third-party downloads, by addressing
    /// the CDN directly.
    ///
    /// Off unless the user turns it on, and deliberately so. The bytes are the
    /// same ones a browser would get and the user is entitled to them, but the
    /// API withholds the URL on purpose, and the terms attached to the user's
    /// own key cover this. The realistic cost of being caught is that key being
    /// revoked — which also loses them search and version checks. That is their
    /// call to make knowingly, not a default to inherit.
    direct: bool,
}

/// CurseForge's CDN lays files out as `<id first 4>/<id last 3>/<filename>`.
fn cdn_url(file_id: u64, file_name: &str) -> String {
    format!(
        "https://edge.forgecdn.net/files/{}/{}/{}",
        file_id / 1000,
        file_id % 1000,
        crate::source::modrinth::urlencode(file_name).replace('+', "%20")
    )
}

#[derive(Debug, Deserialize)]
struct Envelope<T> {
    data: T,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireMod {
    id: u64,
    #[serde(default)]
    name: String,
    #[serde(default)]
    summary: String,
    #[serde(default)]
    slug: String,
    #[serde(default)]
    download_count: u64,
    #[serde(default)]
    links: Option<WireLinks>,
    /// False when the author has disabled third-party distribution.
    #[serde(default = "yes")]
    allow_mod_distribution: bool,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireLinks {
    #[serde(default)]
    source_url: Option<String>,
    #[serde(default)]
    website_url: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireFile {
    #[serde(default)]
    id: u64,
    #[serde(default)]
    display_name: String,
    file_name: String,
    #[serde(default)]
    file_date: String,
    /// 1 = release, 2 = beta, 3 = alpha.
    #[serde(default)]
    release_type: u8,
    #[serde(default)]
    file_length: u64,
    /// Null exactly when the author disallowed third-party distribution.
    #[serde(default)]
    download_url: Option<String>,
    #[serde(default)]
    game_versions: Vec<String>,
}

impl CurseForge {
    pub fn new(http: Http, key: String, direct: bool) -> Self {
        Self { http, key, direct }
    }

    fn url(&self, path: &str) -> String {
        format!("{API}{path}")
    }

    /// CurseForge addresses projects by numeric id, but people copy slugs out
    /// of the address bar. Look the slug up rather than making the user go
    /// hunting for a number in a web sidebar.
    async fn numeric_id(&self, id: &ModId, game_id: Option<u32>) -> Result<u64> {
        if let Ok(n) = id.repo.parse::<u64>() {
            return Ok(n);
        }
        let Some(game_id) = game_id else {
            return Err(Error::other(format!(
                "`{}` is a CurseForge slug, and resolving one needs the game's CurseForge id. \
                 Add `curseforge_game_id` to this game's pack under [search], or use the \
                 numeric Project ID from the mod's page.",
                id.repo
            )));
        };

        let url = self.url(&format!(
            "/mods/search?gameId={game_id}&slug={}&pageSize=5",
            crate::source::modrinth::urlencode(&id.repo)
        ));
        let found: Envelope<Vec<WireMod>> = self
            .http
            .get_json_with(&url, &[("x-api-key", self.key.as_str())])
            .await?
            .ok_or_else(|| Error::NotFound(format!("CurseForge project `{}`", id.repo)))?;

        // An exact slug match wins; otherwise the best the search could do.
        found
            .data
            .iter()
            .find(|m| m.slug.eq_ignore_ascii_case(&id.repo))
            .or_else(|| found.data.first())
            .map(|m| m.id)
            .ok_or_else(|| {
                Error::NotFound(format!(
                    "no CurseForge project with the slug `{}` for this game",
                    id.repo
                ))
            })
    }

    /// Find mods by name. Useful for WoW, where CurseForge is where the addons
    /// actually are.
    pub async fn search(
        &self,
        query: &str,
        game_id: u32,
    ) -> Result<Vec<crate::source::SearchHit>> {
        let url = self.url(&format!(
            "/mods/search?gameId={game_id}&searchFilter={}&pageSize=20&sortField=2&sortOrder=desc",
            crate::source::modrinth::urlencode(query)
        ));
        let found: Envelope<Vec<WireMod>> = self
            .http
            .get_json_with(&url, &[("x-api-key", self.key.as_str())])
            .await?
            .unwrap_or(Envelope { data: Vec::new() });

        Ok(found
            .data
            .into_iter()
            .map(|m| {
                let links = m.links;
                crate::source::SearchHit {
                    // Numeric, so it resolves without another lookup later.
                    id: ModId::project(crate::source::SourceKind::CurseForge, m.id.to_string()),
                    title: m.name,
                    description: m.summary,
                    stars: m.download_count,
                    license: None,
                    source_url: links
                        .as_ref()
                        .and_then(|l| l.source_url.clone())
                        .or_else(|| links.and_then(|l| l.website_url)),
                    // The author's distribution switch decides this, not us.
                    installable: Some(m.allow_mod_distribution),
                }
            })
            .collect())
    }

    /// The newest published version, whether or not it can be downloaded.
    ///
    /// The distribution switch withholds the *file*, not the metadata. So even
    /// for a mod nothing can fetch, we can still say "9.0.3 is out, you have
    /// 9.0.2" — which is the only reason most people keep the CurseForge app
    /// installed at all.
    pub async fn latest_version(
        &self,
        id: &ModId,
        game_id: Option<u32>,
        game_version: Option<&str>,
    ) -> Result<Option<String>> {
        let project = self.numeric_id(id, game_id).await?;
        let url = self.url(&format!("/mods/{project}/files?pageSize=20"));
        let wire: Envelope<Vec<WireFile>> = self
            .http
            .get_json_with(&url, &[("x-api-key", self.key.as_str())])
            .await?
            .unwrap_or(Envelope { data: Vec::new() });

        Ok(wire
            .data
            .into_iter()
            .filter(|f| f.release_type == 1)
            .filter(|f| {
                game_version
                    .map(|want| f.game_versions.iter().any(|v| v == want))
                    .unwrap_or(true)
            })
            .map(|f| f.display_name)
            .next())
    }

    pub async fn releases(
        &self,
        id: &ModId,
        game_version: Option<&str>,
        game_id: Option<u32>,
    ) -> Result<Vec<Release>> {
        let project = self.numeric_id(id, game_id).await?;
        let url = self.url(&format!("/mods/{project}/files?pageSize=50"));
        let wire: Envelope<Vec<WireFile>> = self
            .http
            .get_json_with(&url, &[("x-api-key", self.key.as_str())])
            .await?
            .ok_or_else(|| Error::NotFound(format!("CurseForge project {project}")))?;

        let mut blocked = 0;
        let releases: Vec<Release> = wire
            .data
            .into_iter()
            .filter(|f| {
                game_version
                    .map(|want| f.game_versions.iter().any(|v| v == want))
                    .unwrap_or(true)
            })
            .filter_map(|f| {
                let download_url = match f.download_url.clone() {
                    Some(url) => url,
                    None if self.direct => cdn_url(f.id, &f.file_name),
                    None => {
                        blocked += 1;
                        return None;
                    }
                };
                Some(Release {
                    tag: f.display_name.clone(),
                    name: f.display_name,
                    published_at: f.file_date,
                    prerelease: f.release_type != 1,
                    web_url: id.web_url(),
                    assets: vec![Asset {
                        name: f.file_name,
                        download_url,
                        size: f.file_length,
                        digest: None,
                        sha512: None,
                    }],
                })
            })
            .collect();

        if releases.is_empty() && blocked > 0 {
            return Err(Error::other(format!(
                "this mod's author has disabled third-party downloads on CurseForge, so its \
                 API hands out no file. Download it from {} in a browser, then run \
                 `modifile add-file <profile> <the-downloaded-file> --id {id}` — Modifile \
                 manages it normally from then on, only updates stay manual.",
                id.web_url()
            )));
        }
        Ok(releases)
    }

    pub async fn project(&self, id: &ModId, game_id: Option<u32>) -> Result<Option<RepoInfo>> {
        let project = self.numeric_id(id, game_id).await?;
        let url = self.url(&format!("/mods/{project}"));
        let Some(wire) = self
            .http
            .get_json_with::<Envelope<WireMod>>(&url, &[("x-api-key", self.key.as_str())])
            .await?
        else {
            return Ok(None);
        };
        let data = wire.data;
        let _ = (data.id, data.slug);

        Ok(Some(RepoInfo {
            description: if data.summary.is_empty() {
                data.name
            } else {
                data.summary
            },
            // CurseForge does not publish an SPDX licence through the API, so
            // there is nothing honest to report here.
            license: None,
            archived: !data.allow_mod_distribution,
            stars: data.download_count,
            source_url: data
                .links
                .and_then(|l| l.source_url.or(l.website_url))
                .filter(|u| !u.is_empty()),
        }))
    }
}
