use serde::Deserialize;

use crate::error::Result;
use crate::hash::normalize_digest;
use crate::http::Http;
use crate::source::modrinth::urlencode;
use crate::source::{Asset, ModId, Release, RepoInfo, SearchHit};

const API: &str = "https://api.github.com";

/// How many releases to look back through when hunting for one that actually
/// carries a matching asset. Authors sometimes tag a release with no build.
const RELEASE_WINDOW: usize = 10;

#[derive(Clone)]
pub struct GitHub {
    http: Http,
}

// --- wire types -----------------------------------------------------------
// Deliberately narrow: GitHub's release payload is enormous and we want the
// cache entries small.

#[derive(Debug, Deserialize)]
struct WireRelease {
    #[serde(default)]
    id: u64,
    tag_name: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    published_at: Option<String>,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    html_url: String,
    #[serde(default)]
    assets: Vec<WireAsset>,
}

#[derive(Debug, Deserialize)]
struct WireAsset {
    name: String,
    browser_download_url: String,
    #[serde(default)]
    size: u64,
    #[serde(default)]
    digest: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WireRepo {
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    license: Option<WireLicense>,
    #[serde(default)]
    archived: bool,
    #[serde(default)]
    stargazers_count: u64,
}

#[derive(Debug, Deserialize)]
struct WireLicense {
    #[serde(default)]
    spdx_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WireSearch {
    #[serde(default)]
    items: Vec<WireSearchItem>,
}

#[derive(Debug, Deserialize)]
struct WireSearchItem {
    name: String,
    #[serde(default)]
    full_name: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    html_url: String,
    #[serde(default)]
    stargazers_count: u64,
    #[serde(default)]
    owner: Option<WireOwner>,
    #[serde(default)]
    license: Option<WireLicense>,
}

#[derive(Debug, Deserialize)]
struct WireOwner {
    login: String,
    /// GitHub publishes no project icon, so the owner's avatar is the closest
    /// thing to one — and it is at least recognisable, which a generated tile
    /// is not.
    #[serde(default)]
    avatar_url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WireAttestations {
    #[serde(default)]
    attestations: Vec<serde_json::Value>,
}

impl GitHub {
    pub fn new(http: Http) -> Self {
        Self { http }
    }

    pub fn http(&self) -> &Http {
        &self.http
    }

    /// How many asset-less releases to double-check per listing.
    ///
    /// Only the newest few matter — the question this answers is "is there an
    /// update?", and nobody is offered a jump to a five-year-old tag. A repo
    /// that tags without uploading binaries would otherwise cost one request
    /// per release, every time.
    const CONFIRM_LIMIT: usize = 3;

    /// Re-read the assets of releases the listing claims are empty.
    ///
    /// GitHub's `/releases` response embeds an `assets` array that can lag
    /// behind reality: a file uploaded to an existing release shows on the web
    /// page and from `/releases/{id}/assets` while the listing still says the
    /// release has nothing. Worse, the stale listing keeps its ETag, so
    /// revalidating returns 304 and the wrong answer sticks rather than
    /// ageing out.
    ///
    /// The visible cost of believing it is severe and silent: Modifile decides
    /// the newest release carries nothing installable, greys it out, and tells
    /// the author their week-old version is "already newest" — about a release
    /// they just published a binary to.
    ///
    /// So an empty list is not taken as an answer, it is confirmed. A release
    /// that really has no assets costs one extra request and still ends up
    /// empty; one that has them gets them. Failures leave the release as the
    /// listing described it, because a missing confirmation is not evidence.
    async fn confirm_empty_assets(&self, id: &ModId, releases: &mut [WireRelease]) {
        let stale: Vec<usize> = releases
            .iter()
            .enumerate()
            .filter(|(_, r)| !r.draft && r.assets.is_empty() && r.id != 0)
            .map(|(i, _)| i)
            .take(Self::CONFIRM_LIMIT)
            .collect();

        for i in stale {
            let url = format!(
                "{API}/repos/{}/{}/releases/{}/assets",
                id.owner, id.repo, releases[i].id
            );
            if let Ok(Some(assets)) = self.http.get_json::<Vec<WireAsset>>(&url).await {
                releases[i].assets = assets;
            }
        }
    }

    /// Recent releases, newest first, drafts removed.
    pub async fn releases(&self, id: &ModId) -> Result<Vec<Release>> {
        let url = format!(
            "{API}/repos/{}/{}/releases?per_page={RELEASE_WINDOW}",
            id.owner, id.repo
        );
        let mut wire: Vec<WireRelease> = self.http.get_json(&url).await?.unwrap_or_default();
        self.confirm_empty_assets(id, &mut wire).await;
        Ok(wire
            .into_iter()
            .filter(|r| !r.draft)
            .map(|r| Release {
                tag: r.tag_name,
                name: r.name.unwrap_or_default(),
                published_at: r.published_at.unwrap_or_default(),
                prerelease: r.prerelease,
                web_url: r.html_url,
                assets: r
                    .assets
                    .into_iter()
                    .map(|a| Asset {
                        name: a.name,
                        // browser_download_url is served off the CDN and does
                        // not spend API budget, unlike the api.github.com asset
                        // endpoint.
                        download_url: a.browser_download_url,
                        size: a.size,
                        digest: a.digest.map(|d| normalize_digest(&d)),
                        sha512: None,
                    })
                    .collect(),
            })
            .collect())
    }

    /// Find repositories by name.
    ///
    /// For games with no usable third-party index — World of Warcraft being the
    /// case in point — GitHub is the index, and it has the advantage of only
    /// showing things Modifile can actually install.
    pub async fn search_repos(
        &self,
        query: &str,
        topics: &[String],
        terms: &[String],
    ) -> Result<Vec<SearchHit>> {
        // GitHub ANDs repeated qualifiers, so `topic:a topic:b` means "has both"
        // and finds almost nothing. One query per topic, merged, is the only way
        // to express "any of these".
        let base = query.trim();
        let mut queries: Vec<String> = topics
            .iter()
            .map(|topic| format!("{base} topic:{topic} archived:false"))
            .collect();
        if queries.is_empty() {
            let extra = terms.join(" ");
            queries.push(format!("{base} {extra} archived:false"));
        }

        let mut seen: Vec<SearchHit> = Vec::new();
        for q in queries {
            let url = format!(
                "{API}/search/repositories?q={}&sort=stars&order=desc&per_page=15",
                urlencode(&q)
            );
            let wire: WireSearch = self
                .http
                .get_json(&url)
                .await
                .unwrap_or(None)
                .unwrap_or(WireSearch { items: Vec::new() });

            for item in wire.items {
                let owner = item.owner;
                let id = ModId::github(
                    owner.as_ref().map(|o| o.login.clone()).unwrap_or_default(),
                    item.name,
                );
                if seen.iter().any(|h| h.id == id) {
                    continue;
                }
                seen.push(SearchHit {
                    id,
                    title: item.full_name,
                    description: item.description.unwrap_or_default(),
                    stars: item.stargazers_count,
                    license: item
                        .license
                        .and_then(|l| l.spdx_id)
                        .filter(|s| s != "NOASSERTION"),
                    source_url: Some(item.html_url),
                    installable: None,
                    icon_url: owner.as_ref().and_then(|o| o.avatar_url.clone()),
                    author: owner.map(|o| o.login),
                });
            }
        }

        // Topic searches miss repositories whose authors never set topics, so
        // fall back to plain words when the tagged results are thin.
        if seen.len() < 5 && !terms.is_empty() {
            let q = format!("{base} {} archived:false", terms.join(" "));
            let url = format!(
                "{API}/search/repositories?q={}&sort=stars&order=desc&per_page=15",
                urlencode(&q)
            );
            if let Ok(Some(wire)) = self.http.get_json::<WireSearch>(&url).await {
                for item in wire.items {
                    let owner = item.owner;
                    let id = ModId::github(
                        owner.as_ref().map(|o| o.login.clone()).unwrap_or_default(),
                        item.name,
                    );
                    if seen.iter().any(|h| h.id == id) {
                        continue;
                    }
                    seen.push(SearchHit {
                        id,
                        title: item.full_name,
                        description: item.description.unwrap_or_default(),
                        stars: item.stargazers_count,
                        license: item
                            .license
                            .and_then(|l| l.spdx_id)
                            .filter(|s| s != "NOASSERTION"),
                        source_url: Some(item.html_url),
                        installable: None,
                        icon_url: owner.as_ref().and_then(|o| o.avatar_url.clone()),
                        author: owner.map(|o| o.login),
                    });
                }
            }
        }

        seen.sort_by(|a, b| b.stars.cmp(&a.stars));
        seen.truncate(20);
        Ok(seen)
    }

    /// Whether a repository publishes release assets at all — the difference
    /// between "found it" and "can install it".
    pub async fn has_releases(&self, id: &ModId) -> bool {
        self.releases(id)
            .await
            .map(|r| r.iter().any(|rel| !rel.assets.is_empty()))
            .unwrap_or(false)
    }

    pub async fn repo(&self, id: &ModId) -> Result<Option<RepoInfo>> {
        let url = format!("{API}/repos/{}/{}", id.owner, id.repo);
        let Some(wire) = self.http.get_json::<WireRepo>(&url).await? else {
            return Ok(None);
        };
        Ok(Some(RepoInfo {
            description: wire.description.unwrap_or_default(),
            license: wire
                .license
                .and_then(|l| l.spdx_id)
                // GitHub says "NOASSERTION" when it sees a LICENSE file it
                // cannot identify, which is not a license for our purposes.
                .filter(|s| !s.is_empty() && s != "NOASSERTION"),
            archived: wire.archived,
            stars: wire.stargazers_count,
            // The repository is the source, so it is its own source_url.
            source_url: Some(id.web_url()),
        }))
    }

    /// Whether GitHub holds a build-provenance attestation whose subject is
    /// this artifact digest, under this repository.
    ///
    /// This proves an attestation exists and is bound to the repo. It is not
    /// yet full offline Sigstore bundle verification (certificate chain,
    /// transparency log inclusion) — that is the next step for this function,
    /// and until it lands the `Verified` rung is reported as attested-by-API
    /// rather than cryptographically checked locally.
    pub async fn has_attestation(&self, id: &ModId, sha256: &str) -> Result<bool> {
        let url = format!(
            "{API}/repos/{}/{}/attestations/sha256:{}",
            id.owner, id.repo, sha256
        );
        match self.http.get_json::<WireAttestations>(&url).await {
            Ok(Some(v)) => Ok(!v.attestations.is_empty()),
            Ok(None) => Ok(false),
            // A missing attestation must never fail an install; it just means
            // the mod does not reach the top rung.
            Err(_) => Ok(false),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The listing's `assets` array is the field that goes stale, and `id` is
    /// what makes re-reading it possible. Both have to survive parsing, or the
    /// confirmation step silently cannot run.
    #[test]
    fn a_release_listing_keeps_the_id_needed_to_re_read_its_assets() {
        let listing = r#"[
            {"id": 394466396, "tag_name": "1.4.0", "draft": false, "assets": []},
            {"id": 392911824, "tag_name": "1.3.0", "draft": false,
             "assets": [{"name": "Mod.dll", "browser_download_url": "https://x/Mod.dll",
                         "size": 50688}]}
        ]"#;
        let wire: Vec<WireRelease> = serde_json::from_str(listing).expect("parses");

        assert_eq!(wire[0].id, 394466396);
        assert!(
            wire[0].assets.is_empty(),
            "this is the shape that must trigger a re-read"
        );
        assert_eq!(wire[1].assets.len(), 1);
    }

    /// The assets endpoint returns a bare array, not the release object, so it
    /// has to deserialize into the same asset type the listing embeds.
    #[test]
    fn the_assets_endpoint_parses_into_the_same_shape() {
        let assets = r#"[
            {"name": "BetterCharacterController.dll",
             "browser_download_url": "https://x/BetterCharacterController.dll",
             "size": 52224, "state": "uploaded",
             "digest": "sha256:3b5b57d59c686f82b0000000000000000000000000000000000000000000000"}
        ]"#;
        let parsed: Vec<WireAsset> = serde_json::from_str(assets).expect("parses");
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].name, "BetterCharacterController.dll");
        assert_eq!(parsed[0].size, 52224);
    }

    /// Only the newest few are worth a second request. A repo that tags
    /// without uploading binaries must not cost one request per release on
    /// every check.
    #[test]
    fn only_the_newest_empty_releases_are_confirmed() {
        assert_eq!(GitHub::CONFIRM_LIMIT, 3);
    }
}
