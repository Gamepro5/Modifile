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

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::http::Http;
use crate::source::{Asset, ModId, Release, RepoInfo};

const API: &str = "https://api.curseforge.com/v1";

/// How many ids to put in one batch request. A 300-mod pack in three round
/// trips instead of three hundred, without betting on an undocumented cap.
const BATCH: usize = 100;

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
pub struct WireMod {
    pub id: u64,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub slug: String,
    #[serde(default)]
    pub download_count: u64,
    #[serde(default)]
    pub links: Option<WireLinks>,
    /// False when the author has disabled third-party distribution.
    #[serde(default = "yes")]
    pub allow_mod_distribution: bool,
    #[serde(default)]
    pub logo: Option<WireImage>,
    #[serde(default)]
    pub screenshots: Vec<WireImage>,
    #[serde(default)]
    pub authors: Vec<WireAuthor>,
    #[serde(default)]
    pub class_id: Option<u32>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WireImage {
    #[serde(default)]
    pub thumbnail_url: String,
    #[serde(default)]
    pub url: String,
}

impl WireImage {
    /// The thumbnail where there is one: these are shown at 48–96 px and the
    /// full-size asset can be several megabytes.
    pub fn small(&self) -> Option<String> {
        [&self.thumbnail_url, &self.url]
            .into_iter()
            .find(|u| !u.is_empty())
            .cloned()
    }

    pub fn large(&self) -> Option<String> {
        [&self.url, &self.thumbnail_url]
            .into_iter()
            .find(|u| !u.is_empty())
            .cloned()
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct WireAuthor {
    #[serde(default)]
    pub name: String,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WireLinks {
    #[serde(default)]
    pub source_url: Option<String>,
    #[serde(default)]
    pub website_url: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WireFile {
    #[serde(default)]
    pub id: u64,
    /// The project this file belongs to. Present on the batch endpoint, which
    /// is what lets a pack's flat list of file ids be matched back to its
    /// projects without a second lookup.
    #[serde(default)]
    pub mod_id: u64,
    #[serde(default)]
    pub display_name: String,
    pub file_name: String,
    #[serde(default)]
    pub file_date: String,
    /// 1 = release, 2 = beta, 3 = alpha.
    #[serde(default)]
    pub release_type: u8,
    #[serde(default)]
    pub file_length: u64,
    /// Null exactly when the author disallowed third-party distribution.
    #[serde(default)]
    pub download_url: Option<String>,
    #[serde(default)]
    pub game_versions: Vec<String>,
}

/// Which sides of the game a CurseForge file says it runs on.
///
/// CurseForge's manifest format has no per-file side field, and neither does
/// the file record proper — for Minecraft the tags arrive inside the same
/// `gameVersions` list that carries game versions and loaders, so "Client" and
/// "Server" turn up as entries beside "1.20.1" and "Fabric".
///
/// A file naming neither is not making a claim, and is left alone. Treating
/// silence as "client only" would empty a dedicated server built from any pack
/// exported before CurseForge started tagging, which is most of them.
pub fn declared_sides(game_versions: &[String]) -> (bool, bool) {
    let mut client = false;
    let mut server = false;
    for entry in game_versions {
        if entry.eq_ignore_ascii_case("client") {
            client = true;
        } else if entry.eq_ignore_ascii_case("server") {
            server = true;
        }
    }
    match (client, server) {
        (false, false) => (true, true),
        both => both,
    }
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

    /// One file's metadata, addressed by its own id.
    ///
    /// A modpack pins `projectID` + `fileID`, and `releases()` only lists the
    /// newest fifty files of a project — which a pack published a year ago has
    /// long fallen off the end of. This asks for exactly the file the pack
    /// named.
    pub async fn release_for_file(&self, project: u64, file_id: u64) -> Result<Release> {
        let url = self.url(&format!("/mods/{project}/files/{file_id}"));
        let wire: Envelope<WireFile> = self
            .http
            .get_json_with(&url, &[("x-api-key", self.key.as_str())])
            .await?
            .ok_or_else(|| {
                Error::NotFound(format!("CurseForge file {file_id} of project {project}"))
            })?;

        let web = format!("https://www.curseforge.com/projects/{project}");
        self.to_release(wire.data, &web).ok_or_else(|| {
            Error::other(format!(
                "this pack needs file {file_id} of CurseForge project {project}, whose \
                 author has disabled third-party downloads. Only their own app can fetch \
                 it. Get it from {web} in a browser and add it with `modifile add-file`, \
                 or turn on direct CurseForge downloads in Settings."
            ))
        })
    }

    /// Metadata for many files at once.
    ///
    /// The reason a modpack import is one round trip rather than three hundred.
    pub async fn files_by_id(&self, file_ids: &[u64]) -> Result<Vec<WireFile>> {
        #[derive(Serialize)]
        struct Body<'a> {
            #[serde(rename = "fileIds")]
            file_ids: &'a [u64],
        }

        let mut out = Vec::with_capacity(file_ids.len());
        for chunk in file_ids.chunks(BATCH) {
            let url = self.url("/mods/files");
            let wire: Envelope<Vec<WireFile>> = self
                .http
                .post_json_with(
                    &url,
                    &Body { file_ids: chunk },
                    &[("x-api-key", self.key.as_str())],
                )
                .await?
                .unwrap_or(Envelope { data: Vec::new() });
            out.extend(wire.data);
        }
        Ok(out)
    }

    /// Project metadata for many projects at once.
    pub async fn mods_by_id(&self, mod_ids: &[u64]) -> Result<Vec<WireMod>> {
        #[derive(Serialize)]
        struct Body<'a> {
            #[serde(rename = "modIds")]
            mod_ids: &'a [u64],
        }

        let mut out = Vec::with_capacity(mod_ids.len());
        for chunk in mod_ids.chunks(BATCH) {
            let url = self.url("/mods");
            let wire: Envelope<Vec<WireMod>> = self
                .http
                .post_json_with(
                    &url,
                    &Body { mod_ids: chunk },
                    &[("x-api-key", self.key.as_str())],
                )
                .await?
                .unwrap_or(Envelope { data: Vec::new() });
            out.extend(wire.data);
        }
        Ok(out)
    }

    /// Turn one file record into a release, or `None` when its author has
    /// switched off third-party distribution and we have not been told to go
    /// to the CDN directly.
    fn to_release(&self, f: WireFile, web_url: &str) -> Option<Release> {
        let download_url = match f.download_url.clone() {
            Some(url) => url,
            None if self.direct => cdn_url(f.id, &f.file_name),
            None => return None,
        };
        Some(Release {
            tag: f.display_name.clone(),
            name: f.display_name,
            published_at: f.file_date,
            prerelease: f.release_type != 1,
            web_url: web_url.to_string(),
            assets: vec![Asset {
                name: f.file_name,
                download_url,
                size: f.file_length,
                digest: None,
                sha512: None,
            }],
        })
    }

    /// Find mods by name. Useful for WoW, where CurseForge is where the addons
    /// actually are.
    ///
    /// `class_id` narrows to one kind of content — `CLASS_MODS` or
    /// `CLASS_MODPACKS`. Without it the results mix the two, which reads as a
    /// bug to anyone who asked for one of them.
    pub async fn search(
        &self,
        query: &str,
        game_id: u32,
        class_id: Option<u32>,
    ) -> Result<Vec<crate::source::SearchHit>> {
        let class = class_id
            .map(|c| format!("&classId={c}"))
            .unwrap_or_default();
        let url = self.url(&format!(
            "/mods/search?gameId={game_id}{class}&searchFilter={}&pageSize=20&sortField=2&sortOrder=desc",
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
                    icon_url: m.logo.as_ref().and_then(WireImage::small),
                    author: m.authors.first().map(|a| a.name.clone()),
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
                let release = self.to_release(f, &id.web_url());
                if release.is_none() {
                    blocked += 1;
                }
                release
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

    /// Everything a page about one project needs.
    ///
    /// Two requests, because CurseForge keeps the long description behind its
    /// own endpoint. Worth it here and nowhere else: a list of twenty results
    /// must not pay for twenty descriptions nobody has asked to read.
    pub async fn details(
        &self,
        id: &ModId,
        game_id: Option<u32>,
        modpack_class: Option<u32>,
    ) -> Result<crate::source::Details> {
        let project = self.numeric_id(id, game_id).await?;
        let url = self.url(&format!("/mods/{project}"));
        let wire: Envelope<WireMod> = self
            .http
            .get_json_with(&url, &[("x-api-key", self.key.as_str())])
            .await?
            .ok_or_else(|| Error::NotFound(format!("CurseForge project {project}")))?;
        let data = wire.data;

        // The description is HTML, and a failure to fetch it should not lose
        // the rest of the page.
        let body = self
            .http
            .get_json_with::<Envelope<String>>(
                &self.url(&format!("/mods/{project}/description")),
                &[("x-api-key", self.key.as_str())],
            )
            .await
            .ok()
            .flatten()
            .map(|e| crate::text::html_to_text(&e.data))
            .filter(|b| !b.trim().is_empty());

        let links = data.links.clone();
        Ok(crate::source::Details {
            id: Some(ModId::project(
                crate::source::SourceKind::CurseForge,
                project.to_string(),
            )),
            title: data.name,
            summary: data.summary,
            body,
            icon_url: data.logo.as_ref().and_then(WireImage::large),
            gallery: data.screenshots.iter().filter_map(WireImage::large).collect(),
            authors: data.authors.iter().map(|a| a.name.clone()).collect(),
            downloads: data.download_count,
            source_url: links
                .as_ref()
                .and_then(|l| l.source_url.clone())
                .or_else(|| links.and_then(|l| l.website_url))
                .filter(|u: &String| !u.is_empty()),
            web_url: if data.slug.is_empty() {
                format!("https://www.curseforge.com/projects/{project}")
            } else {
                format!("https://www.curseforge.com/projects/{}", data.slug)
            },
            // CurseForge publishes no SPDX licence through the API.
            license: None,
            is_pack: modpack_class.is_some() && data.class_id == modpack_class,
        })
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

#[cfg(test)]
mod side_tests {
    use super::declared_sides;

    fn v(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_file_that_names_no_side_runs_on_both() {
        // Most exported packs look like this, and reading it as client-only
        // would install nothing at all on a dedicated server.
        assert_eq!(declared_sides(&v(&["1.20.1", "Fabric"])), (true, true));
        assert_eq!(declared_sides(&[]), (true, true));
    }

    #[test]
    fn side_tags_are_read_out_of_the_version_list() {
        assert_eq!(
            declared_sides(&v(&["1.20.1", "Fabric", "Client"])),
            (true, false)
        );
        assert_eq!(
            declared_sides(&v(&["1.20.1", "Forge", "Server"])),
            (false, true)
        );
        assert_eq!(
            declared_sides(&v(&["1.20.1", "Client", "Server"])),
            (true, true)
        );
        // CurseForge is not consistent about case.
        assert_eq!(declared_sides(&v(&["1.20.1", "client"])), (true, false));
    }

    /// A game version or a loader must never be mistaken for a side tag.
    #[test]
    fn versions_and_loaders_are_not_sides() {
        assert_eq!(
            declared_sides(&v(&["1.21.4", "NeoForge", "Quilt"])),
            (true, true)
        );
    }
}
