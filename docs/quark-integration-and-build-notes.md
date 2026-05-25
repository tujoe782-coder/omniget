# 夸克网盘整合 + omniget 編譯/打包筆記（as-built）

> 狀態：**已實作並驗證**（2026-05-25）· 作者：Claude
> 對照設計文件：[quark-download-integration-design.md](quark-download-integration-design.md)

本檔記錄兩件事：(A) 夸克网盘下載功能的「實際做了什麼」+ 驗證過程踩到的 bug；(B) omniget 這個 Tauri 2 + SvelteKit 專案的 **dev / 編譯 / 打包 macOS 安裝檔** 流程與陷阱。

---

## A. 夸克网盘整合（as-built）

### A.1 使用流程
貼 `https://pan.quark.cn/s/<pwd_id>` 到首頁 omnibox → 偵測為「Quark (夸克网盘)」→ 點「下載全部」→ 自動讀瀏覽器登入 cookie → 遞迴列出整個分享資料夾樹 → **每檔各自入列成一個下載項**（保留原目錄結構）→ 用既有直鏈下載器抓檔。

### A.2 為什麼能下載（夸克 API 三要素）
1. **桌面客戶端 UA**（API 只認此 UA 才回直鏈）：
   `...quark-cloud-drive/3.20.0...Channel/pckk_other_ch`
2. **登入 Cookie**（取直鏈的 POST + 抓位元組都要帶）
3. **Referer** `https://pan.quark.cn/`

API 流程（share 頁）：
| 步驟 | 端點 | 取出 |
|------|------|------|
| token | `POST drive-pc.quark.cn/1/clouddrive/share/sharepage/token` `{pwd_id,passcode:""}` | `data.stoken` |
| detail（分頁+遞迴）| `GET .../share/sharepage/detail?pwd_id&stoken&pdir_fid&_page&_size=50` | `data.list[]`, `metadata._total` |
| download | `POST .../file/download` `{fids,fids_token,pwd_id,stoken}` | `data[].download_url` |
| 抓檔 | `GET <download_url>`（UA+Referer+Cookie）| bytes |

錯誤碼：`31001`=未登入 / `23018`=遊客大小限制 / HTTP `412`=抓檔未帶有效 cookie / `403`=直鏈過期。

### A.3 架構接點（順著既有 trait 插件制）
| 變更 | 檔案 |
|------|------|
| 新增 cookie 自動萃取（macOS Chromium）| `src-tauri/omniget-core/src/core/browser_cookies.rs` |
| 新增 Quark 平台（API + `PlatformDownloader`）| `src-tauri/src/platforms/quark/mod.rs` |
| 新增 Tauri 指令 | `src-tauri/src/commands/quark.rs`（`quark_list_share` / `quark_enqueue_files`）|
| URL 路由 | `omniget-core/.../platforms/mod.rs`（`from_url` 加 `quark.cn` + `quark://`）|
| content_type | `src-tauri/src/core/url_parser.rs`（加 `Folder` + `parse_quark`）|
| 接線 | `lib.rs`（AppState `quark_pending` 共享 map + registry 註冊 + invoke_handler）、`commands/mod.rs`、`platforms/mod.rs` |
| 前端 | `src/routes/+page.svelte`（偵測分支 + list→enqueue + 「下載全部」UI）、`platform-display-names.ts`、9 語系 i18n |
| 依賴 | `omniget-core/Cargo.toml` 加 `pbkdf2`(feature `hmac`) + `sha1`（已有 `aes`/`cbc`）|

**關鍵設計決策**：
- omniget 佇列是「**1 queue item = 1 檔**」單檔模型（`GenericDownloadResult` 定義了但未用於下載）。→ 夸克遞迴後「每檔入列」即可沿用既有進度/暫停/重試/佇列 UI，**下載引擎零改動**。
- 每檔以內部 URL `quark://download/<uuid>` 入列；檔案 metadata（fid/token/pwd_id/stoken/rel_path/size）存在 `AppState.quark_pending`（`Arc<Mutex<HashMap>>`，與 `QuarkDownloader` 共享同一份），`download()` 用 `opts.page_url` 的 uuid 回查。避免把狀態塞進巨大 URL。
- cookie/UA 在 `quark_enqueue_files` 透過佇列原生 `extra_headers`(Cookie) + `user_agent` 注入；`download()` 從 `opts.extra_headers` 讀。
- 直鏈抓取複用 `omniget_core::core::direct_downloader::download_direct_with_headers`（含分塊/續傳/重試）；遇 HTTP 403/expired 自動重取直鏈一次。

### A.4 cookie 自動萃取（Phase 1：macOS Chromium）
- 來源：`~/Library/Application Support/{Microsoft Edge,Google/Chrome,BraveSoftware/Brave-Browser,Chromium}/Default/Cookies`（SQLite；瀏覽器執行中被鎖 → 先 `cp` 副本）
- 解密：`v10` 前綴 + AES-128-CBC；金鑰 = Keychain `<Browser> Safe Storage`（`/usr/bin/security`）→ PBKDF2(HMAC-SHA1, salt`saltysalt`, iter`1003`, 16B)；IV = 16×`0x20`；去 PKCS7；新版 Chromium 前面有 32B SHA256(domain) → 開頭非可見字元就剝掉
- 讀 DB 用 `/usr/bin/sqlite3`（零依賴；`hex(encrypted_value)`）
- 取第一個含登入 cookie（`__pus`+`__puus`）的瀏覽器
- **安全**：cookie 只在記憶體組 header 即用即丟，不落地/不 log/不顯示。Keychain 首次存取可能跳系統密碼視窗

### A.5 ⚠️ 實機驗證抓到的 bug（已修）— 重要教訓
**症狀**：`QuarkDownloader.download()` 回 **HTTP 412**（= 未登入）。
**根因**：夸克的追蹤 cookie（`isQuark`/`grey-id`/`web-grey-id`/`__wpkreporterwid_`）解密值含**非 ASCII bytes**（如 `0xcf 0x86 0xef bf bd…`，疑似 32B domain-hash 前綴未被剝乾淨）。組 header 時 `HeaderValue::from_str(cookie)` 對非 ASCII **失敗**，原本的 `if let Ok(v) = ...` 會**靜默丟棄整個 Cookie header** → 請求變遊客 → 412。
**為何 PoC 沒抓到**：Python `urllib` 對 header 寬鬆，照送 → 矇混成功；Rust `reqwest` 嚴格驗證 → 整串被拒。
**修法**：`browser_cookies.rs` 組 cookie header 時用 `is_header_safe()`（只留 visible ASCII `0x20..=0x7e`）**過濾掉非法 cookie**。追蹤 cookie 對認證無用，丟掉無妨；登入 cookie 都乾淨保留。
**教訓**：**跨 HTTP client 行為差異**（urllib 寬鬆 vs reqwest 嚴格）會讓 PoC 過、production 掛；`if let Ok(_)` 靜默吞錯是元兇。實機端到端驗證（非只 PoC）才抓得到。

### A.6 驗證狀態
- `cargo check` ✅ / 新模組單元測試 5 個 ✅ / `pnpm check` 0 error ✅
- **Rust e2e 實測**（真實分享 `pan.quark.cn/s/3444308e99b7`）：cookie 解出 27 個、遞迴列出 7 檔跨 3 層目錄、實際 `download()` 抓檔 180239 bytes 完全吻合、目錄結構保留、有效 JPEG ✅
- App 乾淨 boot（無 panic）✅
- ⚠️ 未做：真人 GUI 滑鼠點擊全流程（背景啟動非互動 GUI session）；建議使用者自行 `pnpm tauri dev` 點過

### A.7 限制 / Phase 2
- cookie 萃取只支援 **macOS + Chromium**（Edge/Chrome/Brave/Chromium）。Safari/Firefox/Windows/Linux 未支援（Windows DPAPI、Linux gnome-keyring 各異；可評估 `rookie` crate 一次解三平台）
- 只做**分享頁** `/s/`；「自己網盤」`/list`（body 只需 `{fids}`）未做
- UC 网盘（drive.uc.cn 同源 API、UA 不同）未做

### A.8 ⚠️ 實機安裝後測出的 3 個 bug（已修）— 進度卡 95% + 慢 + 路徑重複

裝成 .dmg 實跑大檔分享後，使用者回報「下載卡 95%」。連環診斷出 **3 個 bug，前兩個同根因**：

**現象**：所有下載進度條凍結在精準 95%，速度僅 ~99 KB/s，檔案存到三層重複路徑。

**根因 1+2（卡 95% + 慢，同源）**：
- omniget 下載先發 HEAD probe 決定「多連線分段 vs 單連線」+ 取檔案大小。
- `reqwest` client 開了 `gzip/brotli/deflate`，會送 `Accept-Encoding`；Quark CDN 對 HEAD 回 content-encoding 協商 → reqwest 認為 **Content-Length 未知（`resp.content_length()` 回 None）**（`curl -I` 沒送 Accept-Encoding 所以拿得到 → 兩者差異來源）。
- size 未知 → `direct_downloader` 退回**單連線 + 「未知大小」分支**，該分支進度寫死 `min(95.0)`（→ 卡 95%）且不分段（→ 慢）。
- ✅ 實測：Quark CDN 其實支援 Range（`GET Range: bytes=0-0` 回 `206 + Content-Range: .../<total>`），且 **per-connection 限速**（單線 99 KB/s、aria2 `-x16` 達 551 KB/s ≈ 5x）。

**修法（一次解兩個）**：`HttpFetcherConfig` 加 `known_total_bytes: Option<u64>`；`HttpFetcher::download()` 有值就跳過 HEAD probe（假設 Range 支援）。`QuarkDownloader` 改**直接用 `HttpFetcher`**（不走 `direct_downloader`），傳入 listing 已知的 `file.size` + `concurrent_segments=16` + `min_size_for_chunked=5MB`。→ 多連線分段（real progress 0→100% + ~4.5x 速度）。403/412/expired 自動重取直鏈一次。

**根因 3（路徑三層重複）**：`list_share_recursive` 原本把「分享標題」當 rel_path 頂層 → 與「使用者選的同名資料夾」+「分享內同名子夾」疊成三層。修法：rel_path **只保留分享內的資料夾結構**（不前綴分享標題）。

**修後實測**（22.4MB 檔）：進度 → **100.0%**、速度 **446 KB/s**（修前 99）、路徑 `<out>/最强AI漫剧软件/软件/檔名`（乾淨）。

**改動檔**：`omniget-core/src/core/http_fetcher.rs`（+`known_total_bytes`）、`src/platforms/quark/mod.rs`（改用 HttpFetcher + rel_path 不前綴標題）。

**加速結論**：GitHub 上夸克工具（LinkSwift / netdisk-fast-download / QuarkPanTool）**都只解析直鏈、不破限速**；唯一有效的免費加速是**多連線分段**（因 Quark 為 per-connection 限速），本修法已內建（16 連線）。真正「不限速」只能靠夸克會員帳號。

### A.9 列檔進度 + API timeout（robustness 改善）

大檔分享（100+ 檔）遞迴列檔耗時長（每次 detail 呼叫間 300ms 節流 + 偶發 Quark 慢回應），原本「准备下载」期間無任何回饋，看起來像卡住。兩項改善：

1. **列檔進度**：`list_share_recursive` 每處理完一層 `app.emit("quark-listing-progress", {files, folders})`；前端 `handleAction` 的 quark 分支在 listing 期間 `listen()` 此事件，preparing 卡片顯示「正在列出 N 個檔案…」（i18n `omnibox.quark.listing`），結束於 `finally` 解除監聽。
2. **API timeout**：`QuarkDownloader` 的 reqwest client 只設 `connect_timeout(15s)`（**不可設 global request timeout** — 同一 client 也給 http_fetcher 用、會誤殺長時間的大檔下載）；token/detail/file-download 三個 JSON API 各自加**每請求** `.timeout(30s)`，避免某次 API hang 住讓整個列檔無限等。

改動檔：`platforms/quark/mod.rs`（emit + client/connect_timeout + per-request timeout + `list_share_recursive` 加 `app` 參數）、`commands/quark.rs`（傳 `Some(&app)`）、`src/routes/+page.svelte`（listen + preparing 顯示）、9 語系 i18n。

### A.10 子資料夾下載（honor URL fragment）

貼深層連結 `…/s/<pwd_id>#/list/share/<fid>`（使用者點進某子資料夾後的網址）時，原本忽略 fragment、永遠從根目錄列整個分享（大分享如 517GB/1796 檔會爆量）。修法：`start_fid_from_url()` 從 fragment 取最後一段 32-hex fid 當遞迴起點 `pdir_fid`（無有效 fid → `"0"` 全share）。`list_share_recursive` 從該 fid 起遞迴 → 只下載該子資料夾內容（rel_path 從空開始，內容直接落在 output_dir）。

改動檔：`platforms/quark/mod.rs`（`start_fid_from_url` + 遞迴起點）。實測：`…#/list/share/e4bba9a2…` → 只列該夾的 1 個 .rar（82.4MB），非整個 share。

---

## B. omniget 編譯 / 打包指南

### B.1 技術棧 / 前置
- **Tauri 2 (Rust) + SvelteKit (Svelte 5) + TypeScript**；package manager = **pnpm**
- 機器：Apple Silicon (**arm64**)，macOS 10.15+
- Tauri CLI 走 **npm 版**（`@tauri-apps/cli`），**不是** cargo subcommand
  - ❌ `cargo tauri dev`（會報 `no such command: tauri`）
  - ✅ `pnpm tauri dev` / `pnpm tauri build`

### B.2 常用指令
```bash
pnpm install          # 裝前端依賴
pnpm dev              # 只開 Vite dev server
pnpm tauri dev        # 完整 app（Rust + 前端，開視窗）
pnpm check            # svelte-check + tsc（前端型別檢查）
cd src-tauri && cargo check   # 只檢查 Rust（快）
cd src-tauri && cargo test --lib   # Rust 單元測試
```
- `pnpm tauri dev` 要在**互動式前景終端**跑視窗才會持續開著；背景啟動會初始化後 exit 0（非互動 GUI session）。

### B.3 打包 macOS 安裝檔（.dmg / .app）
```bash
pnpm tauri build
```
產出（arm64）：
- `src-tauri/target/release/bundle/dmg/omniget_<version>_aarch64.dmg` ← **安裝檔**
- `src-tauri/target/release/bundle/macos/omniget.app`

#### ⚠️ 陷阱 1：updater 私鑰（一定會卡）
`tauri.conf.json` 設了 `bundle.createUpdaterArtifacts: true` + `plugins.updater.pubkey`。build 會要求 `TAURI_SIGNING_PRIVATE_KEY` 才肯產更新檔，否則中途 fail（`A public key has been found, but no private key`）。

**個人重裝、不需自動更新**的解法 — 產一把臨時拋棄式金鑰：
```bash
pnpm tauri signer generate -w /tmp/omniget-ephemeral.key -p "" --ci -f
TAURI_SIGNING_PRIVATE_KEY_PATH=/tmp/omniget-ephemeral.key \
TAURI_SIGNING_PRIVATE_KEY_PASSWORD="" \
  pnpm tauri build
```
（`signer generate` 必帶 `-p` + `--ci`，否則會 panic 讀 tty `Device not configured`。）

#### ⚠️ 陷阱 2：未簽章 → Gatekeeper 擋
本機 build 未做 Apple Developer 簽章/公證。安裝後首次開啟會被擋（「無法打開，因為來自未識別的開發者」）。解法擇一：
- 對著 app **右鍵 → 打開**（第一次）
- 或 `xattr -dr com.apple.quarantine /Applications/omniget.app`

#### 其他
- `bundle.targets: "all"`；只要 dmg 可 `pnpm tauri build --bundles dmg`
- `src/lib/i18n/keys.ts` 是 **Vite 外掛自動產生**（dev/build 時 regenerate），含 i18n key 型別 — 新增 i18n key 後會自動更新，照常 commit

### B.4 改 code 後的驗證順序（建議）
1. `cd src-tauri && cargo check`（Rust 編得過）
2. `cargo test --lib <module>`（新模組單元測試）
3. `pnpm check`（前端 0 error）
4. （重大功能）`pnpm tauri dev` 實機點過，或寫 `examples/` 一次性 e2e probe 驗真實行為再刪
