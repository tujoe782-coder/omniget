//! 夸克网盘 (Quark) share downloader.
//!
//! Quark exposes a private "PC desktop client" API that returns real download
//! URLs — but only when the request carries the desktop client User-Agent *and*
//! the user's login cookie. Browsers cannot set the UA header, which is why a
//! plain link never downloads; this module replicates the desktop client.
//!
//! Flow (share pages, `pan.quark.cn/s/<pwd_id>`):
//!   1. `share/sharepage/token`  → `stoken`
//!   2. `share/sharepage/detail` → recursively list the whole folder tree
//!   3. `file/download`          → per-file `download_url`
//!   4. fetch bytes (UA + Referer + Cookie) via the shared direct downloader
//!
//! Cookies are pulled automatically from the user's browser
//! (`omniget_core::core::browser_cookies`) — no manual import.
//!
//! Integration with the single-file download queue: the recursive listing is
//! flattened and each file is enqueued as its own queue item under the internal
//! `quark://download/<uuid>` URL. The file metadata lives in a shared `pending`
//! map so `get_media_info` / `download` can resolve the uuid back to its file.

use std::collections::HashMap;
use std::sync::Arc;

use std::path::Path;

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use omniget_core::core::http_fetcher::{HttpFetcher, HttpFetcherConfig};
use omniget_core::models::media::{
    DownloadOptions, DownloadResult, MediaInfo, MediaType, VideoQuality,
};
use omniget_core::platforms::traits::PlatformDownloader;
use reqwest::header::HeaderMap;
use serde::{Deserialize, Serialize};
use tauri::Emitter;
use tokio::sync::{mpsc, Mutex};
use tokio_util::sync::CancellationToken;

/// Desktop client UA — the API only returns download URLs for this UA.
const QUARK_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) quark-cloud-drive/3.20.0 Chrome/112.0.5615.165 Electron/24.1.3.8 Safari/537.36 Channel/pckk_other_ch";
const REFERER: &str = "https://pan.quark.cn/";
const API: &str = "https://drive-pc.quark.cn/1/clouddrive";
/// Cookie names that indicate a logged-in Quark session.
const REQUIRED_COOKIES: &[&str] = &["__pus", "__puus"];
const MAX_DEPTH: usize = 30;
const LIST_THROTTLE_MS: u64 = 300;
/// Per-request timeout for the JSON API calls (token/detail/download link).
/// Not applied to the file download itself (that can legitimately run long).
const API_TIMEOUT_SECS: u64 = 30;

/// One file resolved from a share, ready to enqueue/download.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuarkFile {
    pub fid: String,
    pub share_fid_token: String,
    pub pwd_id: String,
    pub stoken: String,
    pub name: String,
    /// Relative directory (share title + sub-folders), e.g. "4.30/软件".
    pub rel_path: String,
    pub size: u64,
}

/// Result of listing an entire share for the UI summary card.
#[derive(Debug, Clone, Serialize)]
pub struct QuarkShareListing {
    pub title: String,
    pub total_files: usize,
    pub total_bytes: u64,
    pub files: Vec<QuarkFile>,
}

/// Progress event emitted while recursively listing a share (large shares can
/// take a while; this drives the "listing N files…" UI so it doesn't look hung).
#[derive(Debug, Clone, Serialize)]
pub struct QuarkListProgress {
    pub files: usize,
    pub folders: usize,
}

/// uuid → QuarkFile, shared between commands and the downloader instance.
pub type QuarkPending = Arc<Mutex<HashMap<String, QuarkFile>>>;

pub struct QuarkDownloader {
    client: reqwest::Client,
    pending: QuarkPending,
}

impl QuarkDownloader {
    pub fn new(pending: QuarkPending) -> Self {
        // connect_timeout only — a global request timeout would also kill the
        // (long) file download since the same client backs http_fetcher.
        // Per-request timeouts are applied to the API calls instead (see below).
        let client = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(15))
            .build()
            .unwrap_or_default();
        Self { client, pending }
    }

    pub fn pending(&self) -> QuarkPending {
        self.pending.clone()
    }

    /// Extract `pwd_id` from a share URL like `https://pan.quark.cn/s/<id>`.
    pub fn pwd_id_from_url(share_url: &str) -> Option<String> {
        let parsed = url::Url::parse(share_url).ok()?;
        let mut segs = parsed.path_segments()?;
        match segs.next()? {
            "s" | "share" => segs.next().map(|s| s.to_string()),
            _ => None,
        }
    }

    /// Starting folder fid from the URL fragment. When the user is inside a
    /// sub-folder the link looks like `…/s/<pwd_id>#/list/share/<fid>` — we honor
    /// that so only the chosen sub-folder is downloaded (not the whole share).
    /// Returns `"0"` (share root) when no valid 32-hex fid is in the fragment.
    pub fn start_fid_from_url(share_url: &str) -> String {
        url::Url::parse(share_url)
            .ok()
            .and_then(|u| u.fragment().map(|f| f.to_string()))
            .and_then(|frag| {
                frag.rsplit('/')
                    .find(|s| !s.is_empty())
                    .filter(|s| s.len() == 32 && s.chars().all(|c| c.is_ascii_hexdigit()))
                    .map(|s| s.to_string())
            })
            .unwrap_or_else(|| "0".to_string())
    }

    /// Acquire the Quark login cookie from the user's browser.
    pub async fn acquire_cookie() -> Result<String> {
        tokio::task::spawn_blocking(|| {
            omniget_core::core::browser_cookies::extract_cookie_header("quark.cn", REQUIRED_COOKIES)
        })
        .await
        .context("cookie extraction task")?
    }

    fn headers(cookie: &str) -> reqwest::header::HeaderMap {
        use reqwest::header::{HeaderMap, HeaderValue, COOKIE, REFERER as REFERER_H, USER_AGENT};
        let mut h = HeaderMap::new();
        h.insert(USER_AGENT, HeaderValue::from_static(QUARK_UA));
        h.insert(REFERER_H, HeaderValue::from_static(REFERER));
        if let Ok(v) = HeaderValue::from_str(cookie) {
            h.insert(COOKIE, v);
        }
        h
    }

    /// Step 1: resolve the share's `stoken` (and title).
    async fn get_stoken(&self, pwd_id: &str, cookie: &str) -> Result<(String, String)> {
        let url = format!("{API}/share/sharepage/token?pr=ucpro&fr=pc");
        let body = serde_json::json!({ "pwd_id": pwd_id, "passcode": "" });
        let resp: serde_json::Value = self
            .client
            .post(&url)
            .headers(Self::headers(cookie))
            .json(&body)
            .timeout(std::time::Duration::from_secs(API_TIMEOUT_SECS))
            .send()
            .await?
            .json()
            .await
            .context("parse token response")?;

        let code = resp.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
        if code != 0 {
            return Err(anyhow!("Quark token failed (code {code})"));
        }
        let stoken = resp
            .pointer("/data/stoken")
            .and_then(|s| s.as_str())
            .ok_or_else(|| anyhow!("no stoken in response"))?
            .to_string();
        let title = resp
            .pointer("/data/title")
            .and_then(|s| s.as_str())
            .unwrap_or("Quark Share")
            .to_string();
        Ok((stoken, title))
    }

    /// Step 2 (one directory, all pages): list direct children of `pdir_fid`.
    async fn list_dir(
        &self,
        pwd_id: &str,
        stoken: &str,
        pdir_fid: &str,
        cookie: &str,
    ) -> Result<Vec<QuarkEntry>> {
        let enc_stoken = urlencoding::encode(stoken);
        let mut entries = Vec::new();
        let mut page = 1usize;
        loop {
            let url = format!(
                "{API}/share/sharepage/detail?pr=ucpro&fr=pc&pwd_id={pwd_id}&stoken={enc_stoken}\
                 &pdir_fid={pdir_fid}&force=0&_page={page}&_size=50&_fetch_banner=0\
                 &_fetch_share=0&_fetch_total=1&_sort=file_type:asc,updated_at:desc"
            );
            let resp: serde_json::Value = self
                .client
                .get(&url)
                .headers(Self::headers(cookie))
                .timeout(std::time::Duration::from_secs(API_TIMEOUT_SECS))
                .send()
                .await?
                .json()
                .await
                .context("parse detail response")?;

            let code = resp.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
            if code != 0 {
                return Err(anyhow!("Quark list failed (code {code})"));
            }
            let list = resp
                .pointer("/data/list")
                .and_then(|l| l.as_array())
                .cloned()
                .unwrap_or_default();
            if list.is_empty() {
                break;
            }
            for item in &list {
                entries.push(QuarkEntry::from_json(item));
            }
            let total = resp
                .pointer("/metadata/_total")
                .and_then(|t| t.as_u64())
                .unwrap_or(entries.len() as u64);
            if entries.len() as u64 >= total {
                break;
            }
            page += 1;
            tokio::time::sleep(std::time::Duration::from_millis(LIST_THROTTLE_MS)).await;
        }
        Ok(entries)
    }

    /// Recursively list the entire share into a flat file list.
    pub async fn list_share_recursive(
        &self,
        share_url: &str,
        cookie: &str,
        app: Option<&tauri::AppHandle>,
    ) -> Result<QuarkShareListing> {
        let pwd_id = Self::pwd_id_from_url(share_url)
            .ok_or_else(|| anyhow!("Could not extract share id from URL"))?;
        let (stoken, title) = self.get_stoken(&pwd_id, cookie).await?;

        let mut files: Vec<QuarkFile> = Vec::new();
        let mut folders = 0usize;
        // (pdir_fid, rel_path, depth) — DFS via stack to avoid async recursion.
        // rel_path is the in-share folder path only (NOT prefixed with the share
        // title) so files land under <output_dir>/<sub-folders>/<name> without a
        // redundant title layer.
        // Start at the sub-folder fid from the URL fragment when present, so
        // pasting a deep link downloads only that folder instead of the whole share.
        let start_fid = Self::start_fid_from_url(share_url);
        let mut stack: Vec<(String, String, usize)> = vec![(start_fid, String::new(), 0)];

        while let Some((pdir, rel, depth)) = stack.pop() {
            if depth > MAX_DEPTH {
                tracing::warn!("[quark] max depth reached at {rel}, skipping");
                continue;
            }
            let entries = self.list_dir(&pwd_id, &stoken, &pdir, cookie).await?;
            for e in entries {
                if e.is_dir {
                    folders += 1;
                    let child = if rel.is_empty() {
                        e.name.clone()
                    } else {
                        format!("{rel}/{}", e.name)
                    };
                    stack.push((e.fid, child, depth + 1));
                } else {
                    files.push(QuarkFile {
                        fid: e.fid,
                        share_fid_token: e.share_fid_token,
                        pwd_id: pwd_id.clone(),
                        stoken: stoken.clone(),
                        name: e.name,
                        rel_path: rel.clone(),
                        size: e.size,
                    });
                }
            }
            if let Some(app) = app {
                let _ = app.emit(
                    "quark-listing-progress",
                    QuarkListProgress {
                        files: files.len(),
                        folders,
                    },
                );
            }
            tokio::time::sleep(std::time::Duration::from_millis(LIST_THROTTLE_MS)).await;
        }

        let total_bytes = files.iter().map(|f| f.size).sum();
        Ok(QuarkShareListing {
            title,
            total_files: files.len(),
            total_bytes,
            files,
        })
    }

    /// Step 3: resolve the time-limited direct download URL for one file.
    async fn get_download_url(&self, f: &QuarkFile, cookie: &str) -> Result<String> {
        let url = format!("{API}/file/download?entry=ft&fr=pc&pr=ucpro");
        let body = serde_json::json!({
            "fids": [f.fid],
            "fids_token": [f.share_fid_token],
            "pwd_id": f.pwd_id,
            "stoken": f.stoken,
        });
        let resp: serde_json::Value = self
            .client
            .post(&url)
            .headers(Self::headers(cookie))
            .json(&body)
            .timeout(std::time::Duration::from_secs(API_TIMEOUT_SECS))
            .send()
            .await?
            .json()
            .await
            .context("parse download response")?;

        let code = resp.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
        match code {
            0 => {}
            31001 => return Err(anyhow!("Not logged in to Quark (cookie expired)")),
            23018 => return Err(anyhow!("Quark guest size limit — login required")),
            _ => return Err(anyhow!("Quark download link failed (code {code})")),
        }
        resp.pointer("/data/0/download_url")
            .and_then(|u| u.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| anyhow!("no download_url in response"))
    }

    /// Download `url` to `output` via the multi-connection segmented fetcher,
    /// feeding it the known size so it skips the (unreliable for Quark) HEAD
    /// probe — this both fixes the progress display and enables ~5x speed by
    /// using parallel range requests instead of a single throttled stream.
    async fn fetch(
        &self,
        url: &str,
        output: &Path,
        headers: &HeaderMap,
        size: u64,
        cancel: &CancellationToken,
        progress: mpsc::Sender<f64>,
    ) -> Result<u64> {
        let cfg = HttpFetcherConfig {
            known_total_bytes: if size > 0 { Some(size) } else { None },
            concurrent_segments: 16,
            min_size_for_chunked: 5 * 1024 * 1024,
            ..Default::default()
        };
        let fetcher = HttpFetcher::new(self.client.clone(), url.to_string(), output.to_path_buf())
            .with_headers(headers.clone())
            .with_cancel(cancel.clone())
            .with_config(cfg);
        Ok(fetcher.download(progress).await?.bytes_written)
    }

    fn resolve_pending_id(url: &str) -> Option<&str> {
        url.strip_prefix("quark://download/")
    }
}

/// One raw entry from a `detail` listing.
struct QuarkEntry {
    fid: String,
    name: String,
    size: u64,
    share_fid_token: String,
    is_dir: bool,
}

impl QuarkEntry {
    fn from_json(v: &serde_json::Value) -> Self {
        let is_dir = v
            .get("dir")
            .and_then(|d| d.as_bool())
            .or_else(|| v.get("file").and_then(|f| f.as_bool()).map(|f| !f))
            .unwrap_or(false);
        QuarkEntry {
            fid: v.get("fid").and_then(|s| s.as_str()).unwrap_or("").to_string(),
            name: v
                .get("file_name")
                .and_then(|s| s.as_str())
                .unwrap_or("unnamed")
                .to_string(),
            size: v.get("size").and_then(|s| s.as_u64()).unwrap_or(0),
            share_fid_token: v
                .get("share_fid_token")
                .and_then(|s| s.as_str())
                .unwrap_or("")
                .to_string(),
            is_dir,
        }
    }
}

fn media_type_for(name: &str) -> MediaType {
    let ext = name.rsplit('.').next().unwrap_or("").to_lowercase();
    match ext.as_str() {
        "mp4" | "mkv" | "mov" | "avi" | "webm" | "flv" | "ts" | "m4v" => MediaType::Video,
        "mp3" | "flac" | "wav" | "aac" | "m4a" | "ogg" => MediaType::Audio,
        "jpg" | "jpeg" | "png" | "gif" | "webp" | "bmp" | "heic" => MediaType::Photo,
        _ => MediaType::Video,
    }
}

#[async_trait]
impl PlatformDownloader for QuarkDownloader {
    fn name(&self) -> &str {
        "quark"
    }

    fn can_handle(&self, url: &str) -> bool {
        url.starts_with("quark://")
    }

    async fn get_media_info(&self, url: &str) -> Result<MediaInfo> {
        let id = Self::resolve_pending_id(url)
            .ok_or_else(|| anyhow!("invalid quark url: {url}"))?;
        let file = {
            let map = self.pending.lock().await;
            map.get(id).cloned()
        }
        .ok_or_else(|| anyhow!("quark file not found (stale download?)"))?;

        Ok(MediaInfo {
            title: file.name.clone(),
            author: "Quark".to_string(),
            platform: "quark".to_string(),
            duration_seconds: None,
            thumbnail_url: None,
            available_qualities: Vec::<VideoQuality>::new(),
            media_type: media_type_for(&file.name),
            file_size_bytes: if file.size > 0 { Some(file.size) } else { None },
        })
    }

    async fn download(
        &self,
        _info: &MediaInfo,
        opts: &DownloadOptions,
        progress: mpsc::Sender<f64>,
    ) -> Result<DownloadResult> {
        // The queue passes the item URL via page_url; fall back to filename map.
        let url = opts
            .page_url
            .clone()
            .ok_or_else(|| anyhow!("quark download missing page_url"))?;
        let id = Self::resolve_pending_id(&url)
            .ok_or_else(|| anyhow!("invalid quark url: {url}"))?;
        let file = {
            let map = self.pending.lock().await;
            map.get(id).cloned()
        }
        .ok_or_else(|| anyhow!("quark file not found (stale download?)"))?;

        // Cookie: prefer the one captured at enqueue, else re-acquire.
        let cookie = match opts
            .extra_headers
            .as_ref()
            .and_then(|h| h.get("Cookie").cloned())
        {
            Some(c) => c,
            None => Self::acquire_cookie().await?,
        };

        // Build output path: output_dir / <rel_path segments> / <name>
        let mut output = opts.output_dir.clone();
        for seg in file.rel_path.split('/').filter(|s| !s.is_empty()) {
            output.push(sanitize_filename::sanitize(seg));
        }
        output.push(sanitize_filename::sanitize(&file.name));

        let headers = Self::headers(&cookie);

        // Resolve link + download; on expiry (403) re-resolve once.
        let mut dl_url = self.get_download_url(&file, &cookie).await?;
        let bytes = match self
            .fetch(
                &dl_url,
                &output,
                &headers,
                file.size,
                &opts.cancel_token,
                progress.clone(),
            )
            .await
        {
            Ok(b) => b,
            Err(e)
                if e.to_string().contains("403")
                    || e.to_string().contains("expired")
                    || e.to_string().contains("412") =>
            {
                tracing::warn!("[quark] link expired, re-resolving: {}", file.name);
                dl_url = self.get_download_url(&file, &cookie).await?;
                self.fetch(
                    &dl_url,
                    &output,
                    &headers,
                    file.size,
                    &opts.cancel_token,
                    progress,
                )
                .await?
            }
            Err(e) => return Err(e),
        };

        Ok(DownloadResult {
            file_path: output,
            file_size_bytes: bytes,
            duration_seconds: 0.0,
            torrent_id: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pwd_id_parses_share_url() {
        assert_eq!(
            QuarkDownloader::pwd_id_from_url("https://pan.quark.cn/s/3444308e99b7#/list/share")
                .as_deref(),
            Some("3444308e99b7")
        );
        assert_eq!(
            QuarkDownloader::pwd_id_from_url("https://pan.quark.cn/share/abc123").as_deref(),
            Some("abc123")
        );
        assert_eq!(
            QuarkDownloader::pwd_id_from_url("https://pan.quark.cn/list").as_deref(),
            None
        );
    }

    #[test]
    fn start_fid_from_fragment() {
        // deep link → sub-folder fid
        assert_eq!(
            QuarkDownloader::start_fid_from_url(
                "https://pan.quark.cn/s/6959f1e185ac#/list/share/e4bba9a2039c4372aae534b2f053d965"
            ),
            "e4bba9a2039c4372aae534b2f053d965"
        );
        // no fragment → share root
        assert_eq!(
            QuarkDownloader::start_fid_from_url("https://pan.quark.cn/s/6959f1e185ac"),
            "0"
        );
        // non-fid fragment → share root
        assert_eq!(
            QuarkDownloader::start_fid_from_url("https://pan.quark.cn/s/6959f1e185ac#/list/all"),
            "0"
        );
    }

    #[test]
    fn resolves_pending_id() {
        assert_eq!(
            QuarkDownloader::resolve_pending_id("quark://download/abc-123"),
            Some("abc-123")
        );
        assert_eq!(QuarkDownloader::resolve_pending_id("https://x.com"), None);
    }

    #[test]
    fn media_type_by_ext() {
        assert_eq!(media_type_for("a.mp4"), MediaType::Video);
        assert_eq!(media_type_for("a.png"), MediaType::Photo);
        assert_eq!(media_type_for("a.mp3"), MediaType::Audio);
    }
}
