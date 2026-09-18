//! HTTP with an on-disk conditional-request cache.
//!
//! GitHub gives unauthenticated clients 60 requests per hour, and — contrary to
//! what its docs imply in several places — a `304 Not Modified` still spends
//! one of them. Only an authenticated 304 is free. So the cache here is not a
//! nicety: without it a 40-addon profile cannot complete a single update check.
//! With a token the same cache turns update checks into free 304s.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::error::{Context, Error, Result};
use crate::hash::sha256_bytes;
use crate::paths::write_atomic;

const USER_AGENT: &str = concat!("modifile/", env!("CARGO_PKG_VERSION"));

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CacheEntry {
    etag: Option<String>,
    last_modified: Option<String>,
    body: String,
}

/// What the last response told us about our remaining budget.
#[derive(Debug, Clone, Copy, Default)]
pub struct RateLimit {
    pub remaining: u32,
    pub limit: u32,
    pub reset_epoch: u64,
}

#[derive(Clone)]
pub struct Http {
    client: reqwest::Client,
    cache_dir: PathBuf,
    token: Option<String>,
    remaining: Arc<AtomicU32>,
    limit: Arc<AtomicU32>,
    reset: Arc<AtomicU32>,
}

impl Http {
    pub fn new(cache_dir: PathBuf, token: Option<String>) -> Result<Self> {
        let client = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .timeout(std::time::Duration::from_secs(60))
            .connect_timeout(std::time::Duration::from_secs(15))
            // One connection per host, reused across the whole update check.
            .pool_max_idle_per_host(8)
            .build()?;
        std::fs::create_dir_all(&cache_dir).ctx(format!("creating {}", cache_dir.display()))?;
        Ok(Self {
            client,
            cache_dir,
            token,
            remaining: Arc::new(AtomicU32::new(u32::MAX)),
            limit: Arc::new(AtomicU32::new(0)),
            reset: Arc::new(AtomicU32::new(0)),
        })
    }

    pub fn has_token(&self) -> bool {
        self.token.is_some()
    }

    pub fn rate_limit(&self) -> RateLimit {
        RateLimit {
            remaining: self.remaining.load(Ordering::Relaxed),
            limit: self.limit.load(Ordering::Relaxed),
            reset_epoch: self.reset.load(Ordering::Relaxed) as u64,
        }
    }

    fn cache_path(&self, url: &str) -> PathBuf {
        self.cache_dir.join(format!("{}.json", sha256_bytes(url.as_bytes())))
    }

    fn read_cache(&self, url: &str) -> Option<CacheEntry> {
        let raw = std::fs::read(self.cache_path(url)).ok()?;
        serde_json::from_slice(&raw).ok()
    }

    fn record_limits(&self, headers: &reqwest::header::HeaderMap) {
        let get = |name: &str| -> Option<u32> {
            headers.get(name)?.to_str().ok()?.parse().ok()
        };
        if let Some(v) = get("x-ratelimit-remaining") {
            self.remaining.store(v, Ordering::Relaxed);
        }
        if let Some(v) = get("x-ratelimit-limit") {
            self.limit.store(v, Ordering::Relaxed);
        }
        if let Some(v) = get("x-ratelimit-reset") {
            self.reset.store(v, Ordering::Relaxed);
        }
    }

    fn request(&self, url: &str) -> reqwest::RequestBuilder {
        let mut req = self
            .client
            .get(url)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28");
        if let Some(token) = &self.token {
            req = req.header("Authorization", format!("Bearer {token}"));
        }
        req
    }

    /// GET a JSON document, revalidating a cached copy when we have one.
    ///
    /// `Ok(None)` means the server said 404 — a missing release is a normal
    /// answer, not a failure.
    pub async fn get_json<T: DeserializeOwned>(&self, url: &str) -> Result<Option<T>> {
        let cached = self.read_cache(url);

        let mut req = self.request(url);
        if let Some(entry) = &cached {
            if let Some(etag) = &entry.etag {
                req = req.header("If-None-Match", etag.clone());
            } else if let Some(lm) = &entry.last_modified {
                req = req.header("If-Modified-Since", lm.clone());
            }
        }

        let resp = req.send().await.ctx(format!("GET {url}"))?;
        self.record_limits(resp.headers());
        let status = resp.status();

        if status == reqwest::StatusCode::NOT_MODIFIED {
            let entry = cached.ok_or_else(|| {
                Error::other("server returned 304 but the local cache entry is gone")
            })?;
            return Ok(Some(serde_json::from_str(&entry.body)?));
        }
        if status == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if status == reqwest::StatusCode::FORBIDDEN || status == reqwest::StatusCode::TOO_MANY_REQUESTS
        {
            let limits = self.rate_limit();
            if limits.remaining == 0 {
                return Err(Error::RateLimited {
                    remaining: limits.remaining,
                    reset: format_epoch(limits.reset_epoch),
                });
            }
        }
        if !status.is_success() {
            return Err(Error::other(format!("GET {url} returned {status}")));
        }

        let etag = header_string(resp.headers(), "etag");
        let last_modified = header_string(resp.headers(), "last-modified");
        let body = resp.text().await?;

        let value = serde_json::from_str::<T>(&body)
            .map_err(|e| Error::other(format!("GET {url}: unexpected response shape: {e}")))?;

        // Cache failures are not request failures.
        let _ = write_atomic(
            &self.cache_path(url),
            &serde_json::to_vec(&CacheEntry {
                etag,
                last_modified,
                body,
            })?,
        );

        Ok(Some(value))
    }

    /// Stream a download to disk, hashing as it goes so we never hold a 200 MB
    /// modpack in memory and never hash the file a second time.
    pub async fn download_to(&self, url: &str, dest: &std::path::Path) -> Result<(u64, String)> {
        use futures::StreamExt;
        use sha2::{Digest, Sha256};
        use tokio::io::AsyncWriteExt;

        if let Some(parent) = dest.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        let mut req = self.client.get(url).header("Accept", "application/octet-stream");
        if let Some(token) = &self.token {
            req = req.header("Authorization", format!("Bearer {token}"));
        }
        let resp = req.send().await.ctx(format!("downloading {url}"))?;
        self.record_limits(resp.headers());
        if !resp.status().is_success() {
            return Err(Error::other(format!(
                "downloading {url} returned {}",
                resp.status()
            )));
        }

        let mut file = tokio::fs::File::create(dest).await?;
        let mut hasher = Sha256::new();
        let mut written = 0u64;
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            hasher.update(&chunk);
            written += chunk.len() as u64;
            file.write_all(&chunk).await?;
        }
        file.flush().await?;
        Ok((written, hex::encode(hasher.finalize())))
    }
}

fn header_string(headers: &reqwest::header::HeaderMap, name: &str) -> Option<String> {
    headers.get(name)?.to_str().ok().map(|s| s.to_string())
}

fn format_epoch(epoch: u64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    if epoch <= now {
        return "now".to_string();
    }
    let mins = (epoch - now).div_ceil(60);
    format!("in {mins} min")
}
