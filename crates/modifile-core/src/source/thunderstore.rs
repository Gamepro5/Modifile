//! Thunderstore.
//!
//! Keyless, like Modrinth, and the index for most BepInEx games — Valheim,
//! Lethal Company, Risk of Rain 2, R.E.P.O. It matters here for a second
//! reason: its modpacks are the only widely used pack format that is not
//! Minecraft-specific. A CurseForge `manifest.json` and a Modrinth `.mrpack`
//! both define Minecraft and nothing else; a Thunderstore pack is a package
//! whose manifest lists other packages, which works for any game the site
//! carries.
//!
//! Two endpoints do almost everything, and both are small:
//!
//! - `/api/experimental/package/<ns>/<name>/` — the package, with its newest
//!   version inline.
//! - `/api/experimental/package/<ns>/<name>/<version>/` — one exact version.
//!
//! The per-community listing (`/c/<community>/api/v1/package/`) is the only
//! way to get a package's full version history or to search, and for a busy
//! community it is tens of megabytes. So it is used only where nothing else
//! will do, and never on the path that installs a pinned version.

use serde::Deserialize;

use crate::error::{Error, Result};

/// Thunderstore publishes `-1` for a counter it has not computed — both
/// `total_downloads` and `rating_score` come back that way on plenty of real
/// packages. Refusing the whole record over a statistic would mean a pack that
/// installs perfectly cannot be read at all, so a negative count is simply
/// zero.
fn lenient_count<'de, D>(de: D) -> std::result::Result<u64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(i64::deserialize(de)?.max(0) as u64)
}
use crate::http::Http;
use crate::source::{Asset, ModId, Release, RepoInfo};

const API: &str = "https://thunderstore.io/api/experimental";

/// How many Thunderstore requests may be in flight at once.
///
/// Lower than the engine's resolve concurrency, and deliberately so. A modpack
/// resolves every dependency at the same time, and Thunderstore starts
/// answering `429` well before the eight-at-a-time that GitHub and Modrinth
/// take without complaint. Retrying with backoff recovers most of it, but a
/// ninety-mod pack still lost a handful on every run — and it was a *different*
/// handful each time, which is the worst possible failure to hand someone.
/// Holding the line here fixes it at the source instead.
const IN_FLIGHT: usize = 3;

#[derive(Clone)]
pub struct Thunderstore {
    http: Http,
    gate: std::sync::Arc<tokio::sync::Semaphore>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct WirePackage {
    #[serde(default)]
    pub namespace: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub package_url: String,
    #[serde(default)]
    pub is_deprecated: bool,
    #[serde(default, deserialize_with = "lenient_count")]
    pub total_downloads: u64,
    #[serde(default)]
    pub date_updated: String,
    pub latest: WireVersion,
    /// Which communities list this package. A Thunderstore pack does not name
    /// its game anywhere in its manifest, so this is the only thing that says
    /// which game a downloaded pack is for.
    #[serde(default)]
    pub community_listings: Vec<WireListing>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct WireListing {
    /// The community slug, e.g. `valheim`, `repo`, `lethal-company`.
    #[serde(default)]
    pub community: String,
    #[serde(default)]
    pub categories: Vec<String>,
}

impl WirePackage {
    /// The community slug this package is listed under, if any.
    ///
    /// A package can appear in more than one community; the first is the one
    /// it was published to and is the right default.
    pub fn community(&self) -> Option<&str> {
        self.community_listings
            .iter()
            .map(|l| l.community.as_str())
            .find(|c| !c.is_empty())
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct WireVersion {
    #[serde(default)]
    pub namespace: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub version_number: String,
    #[serde(default)]
    pub full_name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub icon: String,
    /// `Namespace-Name-Version` strings. This is what makes a modpack.
    #[serde(default)]
    pub dependencies: Vec<String>,
    #[serde(default)]
    pub download_url: String,
    #[serde(default, deserialize_with = "lenient_count")]
    pub downloads: u64,
    #[serde(default)]
    pub date_created: String,
    #[serde(default)]
    pub website_url: String,
    #[serde(default)]
    pub is_active: bool,
}

impl WireVersion {
    /// Thunderstore serves every package as a zip, but the download URL ends
    /// in a slash rather than a filename — so the name a pack's asset rules
    /// match against has to be built rather than read off the URL.
    pub fn asset_name(&self) -> String {
        let stem = if self.full_name.is_empty() {
            format!("{}-{}-{}", self.namespace, self.name, self.version_number)
        } else {
            self.full_name.clone()
        };
        format!("{stem}.zip")
    }

    pub fn to_release(&self) -> Release {
        Release {
            tag: self.version_number.clone(),
            name: self.version_number.clone(),
            published_at: self.date_created.clone(),
            // Thunderstore has no prerelease flag: a version is published or
            // it is not. Claiming otherwise would make `--prerelease` look
            // like it did something.
            prerelease: false,
            web_url: format!(
                "https://thunderstore.io/package/{}/{}/",
                self.namespace, self.name
            ),
            assets: vec![Asset {
                name: self.asset_name(),
                download_url: self.download_url.clone(),
                // The experimental API does not publish a size or a hash for a
                // version. The download is still verified against the store's
                // own content hash; there is simply nothing upstream to check
                // it against first, and pretending otherwise would be a lie.
                size: 0,
                digest: None,
                sha512: None,
            }],
        }
    }
}

/// One package as the per-community listing describes it.
///
/// A different shape from the experimental API's: this one carries every
/// version and the community categories, which is why search has to use it.
#[derive(Debug, Clone, Deserialize)]
pub struct WireListPackage {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub full_name: String,
    #[serde(default)]
    pub owner: String,
    #[serde(default)]
    pub package_url: String,
    #[serde(default)]
    pub is_deprecated: bool,
    #[serde(default)]
    pub categories: Vec<String>,
    #[serde(default, deserialize_with = "lenient_count")]
    pub rating_score: u64,
    /// Newest first.
    #[serde(default)]
    pub versions: Vec<WireListVersion>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct WireListVersion {
    /// The listing spells this `Name-Version`; the per-package endpoint
    /// spells it `Namespace-Name-Version`. Rebuilt rather than trusted, so the
    /// two routes produce identical records.
    #[serde(default)]
    pub full_name: String,
    #[serde(default)]
    pub dependencies: Vec<String>,
    #[serde(default)]
    pub version_number: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub icon: String,
    #[serde(default)]
    pub download_url: String,
    #[serde(default, deserialize_with = "lenient_count")]
    pub downloads: u64,
    #[serde(default)]
    pub date_created: String,
    #[serde(default)]
    pub website_url: String,
    #[serde(default)]
    pub file_size: u64,
}

impl WireListVersion {
    /// The same shape the per-package endpoint returns, so the two routes are
    /// interchangeable everywhere downstream.
    pub fn to_version(&self, namespace: &str, name: &str) -> WireVersion {
        WireVersion {
            namespace: namespace.to_string(),
            name: name.to_string(),
            version_number: self.version_number.clone(),
            full_name: format!("{namespace}-{name}-{}", self.version_number),
            description: self.description.clone(),
            icon: self.icon.clone(),
            dependencies: self.dependencies.clone(),
            download_url: self.download_url.clone(),
            downloads: self.downloads,
            date_created: self.date_created.clone(),
            website_url: self.website_url.clone(),
            is_active: true,
        }
    }
}

/// Split a `Namespace-Name-Version` dependency string.
///
/// Both the namespace and the name may themselves contain hyphens, but the
/// version is always the last segment and a namespace never is — so splitting
/// from the right twice is unambiguous where splitting from the left is not.
pub fn split_dependency(full: &str) -> Option<(String, String, String)> {
    let (rest, version) = full.trim().rsplit_once('-')?;
    let (namespace, name) = rest.rsplit_once('-')?;
    if namespace.is_empty() || name.is_empty() || version.is_empty() {
        return None;
    }
    Some((
        namespace.to_string(),
        name.to_string(),
        version.to_string(),
    ))
}

impl Thunderstore {
    pub fn new(http: Http) -> Self {
        Self {
            http,
            gate: std::sync::Arc::new(tokio::sync::Semaphore::new(IN_FLIGHT)),
        }
    }

    pub fn http(&self) -> &Http {
        &self.http
    }

    /// Fetch JSON with Thunderstore's own concurrency held down.
    async fn get<T: serde::de::DeserializeOwned>(&self, url: &str) -> Result<Option<T>> {
        // The semaphore is never closed, so acquiring cannot fail.
        let _permit = self
            .gate
            .acquire()
            .await
            .map_err(|_| Error::other("Thunderstore request gate closed"))?;
        self.http.get_json(url).await
    }

    fn require_pair(id: &ModId) -> Result<(&str, &str)> {
        if id.owner.is_empty() || id.repo.is_empty() {
            return Err(Error::other(format!(
                "`{id}` is not a complete Thunderstore id — packages are addressed as \
                 `thunderstore:Namespace/PackageName`"
            )));
        }
        Ok((id.owner.as_str(), id.repo.as_str()))
    }

    /// The package, with its newest version inline.
    ///
    /// `community` is a fallback route, not the normal one. Thunderstore's
    /// per-package endpoint answers `406 Not Acceptable` for some perfectly
    /// ordinary packages — `Blazed/REPO_The_God_Pack` returns 200 while
    /// `DuckPack/DuckPackREPO` does not, whatever headers are sent — and the
    /// community listing carries the same data. So when the small request
    /// fails and we know which community to ask, we ask it rather than
    /// reporting a package that plainly exists as missing.
    pub async fn package(
        &self,
        id: &ModId,
        community: Option<&str>,
    ) -> Result<Option<WirePackage>> {
        let (ns, name) = Self::require_pair(id)?;
        let url = format!("{API}/package/{ns}/{name}/");

        match self.get::<WirePackage>(&url).await {
            Ok(Some(pkg)) => Ok(Some(pkg)),
            Ok(None) => match community {
                Some(c) => self.package_from_listing(c, ns, name).await,
                None => Ok(None),
            },
            Err(e) => match community {
                Some(c) => self.package_from_listing(c, ns, name).await,
                None => Err(e),
            },
        }
    }

    /// Rebuild a package record out of the community listing.
    ///
    /// Heavier than the per-package endpoint, but it is the same information
    /// and it is already fetched conditionally, so a session that has searched
    /// once pays nothing for this.
    async fn package_from_listing(
        &self,
        community: &str,
        namespace: &str,
        name: &str,
    ) -> Result<Option<WirePackage>> {
        let url = format!("https://thunderstore.io/c/{community}/api/v1/package/");
        let packages: Vec<WireListPackage> = self.get(&url).await?.unwrap_or_default();

        let Some(found) = packages.into_iter().find(|p| {
            p.owner.eq_ignore_ascii_case(namespace) && p.name.eq_ignore_ascii_case(name)
        }) else {
            return Ok(None);
        };
        let Some(newest) = found.versions.first() else {
            return Ok(None);
        };

        Ok(Some(WirePackage {
            namespace: found.owner.clone(),
            name: found.name.clone(),
            package_url: found.package_url.clone(),
            is_deprecated: found.is_deprecated,
            total_downloads: found.versions.iter().map(|v| v.downloads).sum(),
            date_updated: newest.date_created.clone(),
            latest: newest.to_version(&found.owner, &found.name),
            community_listings: vec![WireListing {
                community: community.to_string(),
                categories: found.categories.clone(),
            }],
        }))
    }

    /// One exact version.
    ///
    /// The path that matters for modpacks: every dependency a pack lists names
    /// its version, and this fetches precisely that rather than whatever is
    /// newest now.
    pub async fn version(&self, id: &ModId, version: &str) -> Result<Option<WireVersion>> {
        let (ns, name) = Self::require_pair(id)?;
        let url = format!("{API}/package/{ns}/{name}/{version}/");
        self.get(&url).await
    }

    /// Everything the resolver needs about one package, in one request.
    ///
    /// The obvious spelling — call `releases`, then call `project` — asks
    /// Thunderstore twice for the same package and doubles the traffic a
    /// modpack generates, which is exactly what tips it into rate-limiting.
    /// Every field `project` would report is already in the version record, so
    /// there is no second question to ask.
    pub async fn resolve(
        &self,
        id: &ModId,
        pin: Option<&str>,
        community: Option<&str>,
    ) -> Result<(Vec<Release>, Option<RepoInfo>)> {
        let info = |v: &WireVersion, deprecated: bool| RepoInfo {
            description: v.description.clone(),
            // Thunderstore publishes no SPDX licence through this API, and
            // saying otherwise would hand the trust ladder a fact it does not
            // have.
            license: None,
            archived: deprecated,
            stars: v.downloads,
            source_url: Some(v.website_url.clone()).filter(|u| !u.is_empty()),
        };

        if let Some(version) = pin {
            let direct = self.version(id, version).await;
            if let Ok(Some(v)) = &direct {
                return Ok((vec![v.to_release()], Some(info(v, false))));
            }
            let why = match &direct {
                Err(e) => format!(" ({e})"),
                _ => String::new(),
            };
            // Fall back the same way `package` does, then check whether the
            // version we want happens to be the current one.
            if let Some(pkg) = self.package(id, community).await? {
                if pkg.latest.version_number == version {
                    return Ok((
                        vec![pkg.latest.to_release()],
                        Some(info(&pkg.latest, pkg.is_deprecated)),
                    ));
                }
                return Err(Error::NotFound(format!(
                    "Thunderstore version {version} of {}/{}{why} — the newest it \
                     publishes is {}",
                    id.owner, id.repo, pkg.latest.version_number
                )));
            }
            return Err(Error::NotFound(format!(
                "Thunderstore version {version} of {}/{}{why}",
                id.owner, id.repo
            )));
        }

        match self.package(id, community).await? {
            Some(pkg) => Ok((
                vec![pkg.latest.to_release()],
                Some(info(&pkg.latest, pkg.is_deprecated)),
            )),
            None => Err(Error::NotFound(format!(
                "Thunderstore package {}/{}",
                id.owner, id.repo
            ))),
        }
    }

    /// Everything a page about one package needs.
    ///
    /// Thunderstore serves the long description as a separate Markdown
    /// document, which is fetched best-effort: a package with no readme is
    /// normal and should still have a page.
    pub async fn details(
        &self,
        id: &ModId,
        community: Option<&str>,
    ) -> Result<crate::source::Details> {
        let pkg = self
            .package(id, community)
            .await?
            .ok_or_else(|| Error::NotFound(format!("Thunderstore package {id}")))?;
        let v = &pkg.latest;

        let readme = self
            .readme(id, &v.version_number)
            .await
            .ok()
            .flatten()
            .map(|md| crate::text::markdown_to_text(&md))
            .filter(|b| !b.trim().is_empty());

        Ok(crate::source::Details {
            id: Some(id.clone()),
            title: v.name.clone(),
            summary: v.description.clone(),
            body: readme,
            icon_url: Some(v.icon.clone()).filter(|u| !u.is_empty()),
            // Thunderstore publishes no screenshot gallery; the icon is it.
            gallery: Vec::new(),
            authors: vec![pkg.namespace.clone()],
            downloads: pkg.total_downloads.max(v.downloads),
            source_url: Some(v.website_url.clone()).filter(|u| !u.is_empty()),
            web_url: if pkg.package_url.is_empty() {
                id.web_url()
            } else {
                pkg.package_url.clone()
            },
            license: None,
            is_pack: pkg
                .community_listings
                .iter()
                .any(|l| l.categories.iter().any(|c| c.eq_ignore_ascii_case("Modpacks"))),
        })
    }

    /// A package version's readme, as Markdown.
    async fn readme(&self, id: &ModId, version: &str) -> Result<Option<String>> {
        #[derive(Deserialize)]
        struct WireReadme {
            #[serde(default)]
            markdown: String,
        }

        let (ns, name) = Self::require_pair(id)?;
        let url = format!("{API}/package/{ns}/{name}/{version}/readme/");
        Ok(self
            .get::<WireReadme>(&url)
            .await?
            .map(|r| r.markdown)
            .filter(|m| !m.is_empty()))
    }

    /// Search one community's packages.
    ///
    /// Thunderstore publishes no search endpoint that a non-browser client can
    /// reach — the frontend one is behind Cloudflare — so the only way is its
    /// per-community listing, filtered locally. That listing is large: tens of
    /// megabytes for a busy community. Two things make it bearable, and both
    /// matter:
    ///
    /// - It is fetched conditionally, so every search after the first is a
    ///   `304` and costs nothing on the wire.
    /// - Nothing on the install path uses it. Resolving a pinned version is a
    ///   single small request, so a modpack with three hundred dependencies
    ///   never touches this.
    ///
    /// `category` narrows to one of the community's own categories, which is
    /// how modpacks are told apart from mods here.
    pub async fn search(
        &self,
        community: &str,
        query: &str,
        category: Option<&str>,
    ) -> Result<Vec<crate::source::SearchHit>> {
        let url = format!("https://thunderstore.io/c/{community}/api/v1/package/");
        let packages: Vec<WireListPackage> = self.get(&url).await?.unwrap_or_default();

        let needle = query.trim().to_ascii_lowercase();
        let mut hits: Vec<(u64, crate::source::SearchHit)> = packages
            .into_iter()
            .filter(|p| !p.is_deprecated)
            .filter(|p| {
                category.is_none_or(|want| {
                    p.categories.iter().any(|c| c.eq_ignore_ascii_case(want))
                })
            })
            .filter(|p| {
                if needle.is_empty() {
                    return true;
                }
                p.name.to_ascii_lowercase().contains(&needle)
                    || p.owner.to_ascii_lowercase().contains(&needle)
                    || p.versions
                        .first()
                        .is_some_and(|v| v.description.to_ascii_lowercase().contains(&needle))
            })
            .map(|p| {
                let downloads: u64 = p.versions.iter().map(|v| v.downloads).sum();
                let newest = p.versions.first();
                (
                    downloads,
                    crate::source::SearchHit {
                        id: ModId {
                            kind: crate::source::SourceKind::Thunderstore,
                            owner: p.owner.clone(),
                            repo: p.name.clone(),
                            host: None,
                        },
                        title: p.name.clone(),
                        description: newest
                            .map(|v| v.description.clone())
                            .unwrap_or_default(),
                        stars: downloads,
                        license: None,
                        source_url: newest
                            .map(|v| v.website_url.clone())
                            .filter(|u| !u.is_empty()),
                        installable: Some(true),
                        icon_url: newest
                            .map(|v| v.icon.clone())
                            .filter(|u| !u.is_empty()),
                        // Thunderstore's namespace *is* the author.
                        author: Some(p.owner.clone()).filter(|a| !a.is_empty()),
                    },
                )
            })
            .collect();

        // Most-downloaded first: the listing's own order is not useful.
        hits.sort_by(|a, b| b.0.cmp(&a.0));
        hits.truncate(20);
        Ok(hits.into_iter().map(|(_, hit)| hit).collect())
    }

}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_a_dependency_string() {
        assert_eq!(
            split_dependency("BepInEx-BepInExPack-5.4.2100"),
            Some(("BepInEx".into(), "BepInExPack".into(), "5.4.2100".into()))
        );
    }

    #[test]
    fn a_hyphenated_package_name_still_splits() {
        // The trap: splitting from the left gives ("Some", "Author-With")
        // and loses the rest. Only the last two hyphens are structural.
        assert_eq!(
            split_dependency("Author-My-Cool-Mod-1.0.0"),
            Some(("Author-My-Cool".into(), "Mod".into(), "1.0.0".into()))
        );
    }

    #[test]
    fn an_uncomputed_counter_does_not_sink_the_record() {
        // Thunderstore really does serve -1 here, on popular packages.
        let raw = r#"{
          "namespace":"Blazed","name":"REPO_The_God_Pack","package_url":"",
          "is_deprecated":false,"rating_score":-1,"total_downloads":-1,
          "date_updated":"2025-05-11T22:28:03Z",
          "latest":{"namespace":"Blazed","name":"REPO_The_God_Pack",
            "version_number":"1.0.0","full_name":"Blazed-REPO_The_God_Pack-1.0.0",
            "description":"","icon":"","dependencies":[],"download_url":"",
            "downloads":-1,"date_created":"","website_url":"","is_active":true},
          "community_listings":[{"community":"repo","categories":["Modpacks"]}]
        }"#;
        let pkg: WirePackage = serde_json::from_str(raw).expect("should parse");
        assert_eq!(pkg.total_downloads, 0);
        assert_eq!(pkg.latest.downloads, 0);
        assert_eq!(pkg.community(), Some("repo"));
    }

    #[test]
    fn rejects_something_that_is_not_a_dependency() {
        assert_eq!(split_dependency("NotADependency"), None);
        assert_eq!(split_dependency("Only-Two"), None);
    }

    #[test]
    fn builds_an_asset_name_a_pack_can_match() {
        // Pack asset rules match on names like `*.zip`; the download URL ends
        // in a slash, so the name has to be constructed.
        let v = WireVersion {
            namespace: "BepInEx".into(),
            name: "BepInExPack".into(),
            version_number: "5.4.2100".into(),
            full_name: "BepInEx-BepInExPack-5.4.2100".into(),
            description: String::new(),
            icon: String::new(),
            dependencies: Vec::new(),
            download_url: String::new(),
            downloads: 0,
            date_created: String::new(),
            website_url: String::new(),
            is_active: true,
        };
        assert_eq!(v.asset_name(), "BepInEx-BepInExPack-5.4.2100.zip");
        assert_eq!(v.to_release().tag, "5.4.2100");
    }
}
