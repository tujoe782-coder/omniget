//! Tauri commands for 夸克网盘 (Quark) folder-share downloads.
//!
//! `quark_list_share` recursively lists a share into a flat file list (for the
//! UI summary card). `quark_enqueue_files` registers each file in the shared
//! `pending` map and enqueues it as its own single-file queue item, reusing the
//! normal download pipeline (progress / pause / retry).

use std::collections::HashMap;

use crate::core::queue::{self, emit_queue_state_from_state};
use crate::platforms::quark::{QuarkDownloader, QuarkFile, QuarkShareListing};
use crate::storage::config;
use crate::AppState;

use super::downloads::{next_download_id, DownloadStarted};

const QUARK_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) quark-cloud-drive/3.20.0 Chrome/112.0.5615.165 Electron/24.1.3.8 Safari/537.36 Channel/pckk_other_ch";

/// Recursively list an entire Quark share. Pulls the login cookie from the
/// user's browser automatically; errors clearly if not logged in.
#[cfg(not(target_os = "android"))]
#[tauri::command]
pub async fn quark_list_share(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    url: String,
) -> Result<QuarkShareListing, String> {
    let cookie = QuarkDownloader::acquire_cookie()
        .await
        .map_err(|e| format!("QuarkCookie|{e}"))?;
    let downloader = QuarkDownloader::new(state.quark_pending.clone());
    downloader
        .list_share_recursive(&url, &cookie, Some(&app))
        .await
        .map_err(|e| format!("QuarkList|{e}"))
}

/// Enqueue every file from a previously-listed share as its own download item.
#[cfg(not(target_os = "android"))]
#[tauri::command]
pub async fn quark_enqueue_files(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    files: Vec<QuarkFile>,
    output_dir: String,
) -> Result<Vec<DownloadStarted>, String> {
    if files.is_empty() {
        return Err("No files to download".to_string());
    }
    if let Err(err) = crate::core::path_limits::validate_output_dir(&output_dir) {
        return Err(format!(
            "PathTooLong|{}|{}|{}",
            err.limit, err.current, err.reserve
        ));
    }

    // One Keychain read for the whole batch; cookie stays server-side.
    let cookie = QuarkDownloader::acquire_cookie()
        .await
        .map_err(|e| format!("QuarkCookie|{e}"))?;

    let downloader = match state.registry.find_platform("quark://download/_") {
        Some(d) => d,
        None => return Err("Quark downloader not registered".to_string()),
    };

    let mut started = Vec::with_capacity(files.len());
    let download_queue = state.download_queue.clone();

    {
        let settings = config::load_settings(&app);
        let mut q = download_queue.lock().await;
        q.max_concurrent = settings.advanced.max_concurrent_downloads.max(1);
        q.stagger_delay_ms = settings.advanced.stagger_delay_ms;
        q.default_max_retries = settings.advanced.max_retries;

        let mut extra_headers = HashMap::new();
        extra_headers.insert("Cookie".to_string(), cookie.clone());

        for file in files {
            let id = next_download_id();
            let uuid = uuid::Uuid::new_v4().to_string();
            let item_url = format!("quark://download/{uuid}");

            // Register file metadata for get_media_info / download lookup.
            {
                let mut pending = state.quark_pending.lock().await;
                pending.insert(uuid.clone(), file.clone());
            }

            q.enqueue(
                id,
                item_url.clone(),
                "quark".to_string(),
                file.name.clone(),
                output_dir.clone(),
                None,                            // download_mode
                None,                            // quality
                None,                            // format_id
                Some("https://pan.quark.cn/".to_string()), // referer
                Some(extra_headers.clone()),     // extra_headers (Cookie)
                Some(item_url.clone()),          // page_url → download() resolves uuid
                Some(QUARK_UA.to_string()),      // user_agent
                None,                            // media_info (resolved lazily)
                if file.size > 0 { Some(file.size) } else { None }, // total_bytes
                None,                            // file_count
                downloader.clone(),
                None,                            // ytdlp_path
                false,                           // from_hotkey
                None,                            // cookie_slug
                None,                            // custom_ytdlp_args
            );
            started.push(DownloadStarted {
                id,
                title: file.name,
            });
        }
    }

    // Start up to max_concurrent; the rest stay queued.
    let state_to_emit = {
        let mut q = download_queue.lock().await;
        for nid in q.next_queued_ids() {
            q.mark_active(nid);
        }
        q.get_state()
    };
    emit_queue_state_from_state(&app, state_to_emit);

    let q_clone = download_queue.clone();
    let app_clone = app.clone();
    tokio::spawn(async move {
        let (ids_to_start, stagger) = {
            let q = q_clone.lock().await;
            let ids = q
                .items
                .iter()
                .filter(|i| i.status == queue::QueueStatus::Active)
                .map(|i| i.id)
                .collect::<Vec<_>>();
            (ids, q.stagger_delay_ms)
        };
        for (i, nid) in ids_to_start.into_iter().enumerate() {
            if i > 0 && stagger > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(stagger)).await;
            }
            let a = app_clone.clone();
            let qc = q_clone.clone();
            tokio::spawn(async move {
                queue::spawn_download(a, qc, nid).await;
            });
        }
    });

    Ok(started)
}
