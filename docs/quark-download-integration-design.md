# 設計文件：夸克网盘（Quark）分享下載整合

> 狀態：**草案 v1，待 Sunny review** · 作者：Claude · 日期：2026-05-25
> 目標讀者：review 後依此實作（先 review 再寫 code，per 慣例）

---

## 0. TL;DR

在 omniget 偵測到使用者貼上 `pan.quark.cn` 連結時，**自動讀取本機瀏覽器的夸克登入 cookie**，呼叫夸克私有 API **遞迴列出整個分享資料夾樹**，把**每一個檔案各自入列成一個下載項**（保留原目錄結構），用現有的直鏈下載器抓檔。

三項已確認決策（Sunny 2026-05-25）：
1. **Cookie 來源** = 自動抓瀏覽器（讀 cookie DB + Keychain 解密，如 PoC 驗證）
2. **下載 UX** = 遞迴全抓（偵測到 → 自動抓整個分享，不做勾選 UI）
3. **進行方式** = 先出本設計文件 review，再實作

---

## 1. 背景與 PoC 驗證結果

已用 `~/Documents/nProjecs/kanban-studio/ref-doc/assets.user.js`（LinkSwift）逆向出夸克 API，並在本機 **端到端驗證成功**（下載 `pan.quark.cn/s/3444308e99b7` 的檔案，大小完全吻合、圖片可解析）。

### 1.1 已驗證的 API 流程（share 分享頁）

所有請求都需帶兩個 header：
- `User-Agent`: 夸克 PC 桌面客戶端 UA（**關鍵**，API 只認桌面 UA）
  ```
  Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) quark-cloud-drive/3.20.0 Chrome/112.0.5615.165 Electron/24.1.3.8 Safari/537.36 Channel/pckk_other_ch
  ```
- `Referer`: `https://pan.quark.cn/`

| 步驟 | Method / URL | Body / Query | 取出 |
|------|--------------|--------------|------|
| ① 取 stoken | `POST https://drive-pc.quark.cn/1/clouddrive/share/sharepage/token?pr=ucpro&fr=pc` | `{"pwd_id":"<id>","passcode":""}` | `data.stoken` |
| ② 列檔案（分頁、遞迴） | `GET .../share/sharepage/detail?pr=ucpro&fr=pc&pwd_id=<id>&stoken=<enc>&pdir_fid=<dir>&_page=<n>&_size=50&_sort=file_type:asc,updated_at:desc` | — | `data.list[]`、`metadata._total` |
| ③ 取直鏈 | `POST https://drive-pc.quark.cn/1/clouddrive/file/download?entry=ft&fr=pc&pr=ucpro` | `{"fids":[..],"fids_token":[..],"pwd_id":"<id>","stoken":"<stoken>"}` | `data[].download_url` |
| ④ 抓位元組 | `GET <download_url>` | 需帶 UA + Referer + **Cookie** | 檔案內容 |

- `pwd_id` 來自網址 `pan.quark.cn/s/{pwd_id}`（本例 `3444308e99b7`）
- detail 回傳每筆：`fid`、`file_name`、`dir`(bool)、`file`(bool)、`size`、`share_fid_token`、`include_items`（資料夾才有，子項數）
- **遞迴**：`dir==true` 的項，用其 `fid` 當下一層 `pdir_fid` 再呼叫 detail
- **節流**：detail/download 每批間隔 ~1s；download 一批最多 15 個 fid
- **stoken 要 URL-encode**（含 `=`、`+`）

### 1.2 錯誤碼對照（步驟 ③④ 的 `code` / HTTP status）

| 碼 | 意義 | 處理 |
|----|------|------|
| `code 0` | 成功 | — |
| `code 31001` | 未登入 | 提示「需先登入夸克」，cookie 失效 |
| `code 23018` | 超出遊客可下載大小 | 提示需登入（理論上有 cookie 不會遇到）|
| HTTP `412` | 抓位元組時未帶有效 Cookie | **PoC 關鍵教訓**：直鏈要帶 Cookie 才能抓 |
| HTTP `403` | 直鏈過期 | 重新取直鏈（直鏈有時效，約數小時）|

### 1.3 PoC 同時驗證的 cookie 萃取（macOS / Edge）

- cookie DB：`~/Library/Application Support/Microsoft Edge/Default/Cookies`（SQLite，瀏覽器執行中被鎖 → 先 `cp` 副本再讀）
- 加密：`encrypted_value` 以 `v10` 前綴 + AES-128-CBC
  - 金鑰：Keychain `security find-generic-password -w -s "Microsoft Edge Safe Storage"` → PBKDF2(HMAC-SHA1, salt=`saltysalt`, iter=`1003`, dklen=`16`)
  - IV：16 個空白字元（`0x20`）
  - 解密後去 PKCS7 padding；新版 Chromium 會在明文前面塞 32 bytes SHA256(domain)，**開頭非可見字元就剝掉 32 bytes**
- 夸克關鍵 cookie：`__pus`、`__puus`、`__kp`、`__kps`、`__uid`、`__ktd`（登入態），全部組成 `Cookie:` header

---

## 2. omniget 架構現況（接點分析）

| 元件 | 路徑 | 角色 |
|------|------|------|
| URL → 平台路由 | `omniget-core/src/platforms/mod.rs::Platform::from_url()` | 加 quark 判斷處 |
| 平台 trait | `omniget-core/src/platforms/traits.rs::PlatformDownloader` | `can_handle / get_media_info / download` |
| 平台註冊 | `omniget-core/src/core/registry.rs::PlatformRegistry` | `find_platform(url)` |
| URL 細部解析 | `src-tauri/src/core/url_parser.rs::parse_url()` | content_id / content_type |
| 下載指令入口 | `src-tauri/src/commands/downloads.rs` | `detect_platform / download_from_url` |
| 佇列 | `src-tauri/src/core/queue.rs` | **1 queue item = 1 檔**（單檔模型）|
| 直鏈下載器 | `omniget-core/src/core/direct_downloader.rs::download_direct_with_headers()` | **可複用**抓夸克直鏈 |
| Cookie store（匯入式）| `src-tauri/src/cookies/` | 現為匯入式，本案**不走這條**（改自動萃取）|
| 前端貼連結 | `src/components/omnibox/OmniboxInput.svelte` + `media-preview-store` | 偵測 + 預覽入口 |
| 前端佇列 store | `src/lib/stores/download-store.svelte.ts` | 下載清單狀態 |

**關鍵設計事實**：
- `GenericDownloadResult { files: Vec<DownloadedFile> }` 雖定義了，但**下載流程未使用**；佇列是單檔模型。
- → 夸克遞迴的正解：**列完所有檔 → 每檔 enqueue 一個 queue item**，沿用既有單檔進度/暫停/重試/佇列 UI，零改動下載引擎。

---

## 3. 整合方案總覽

```
使用者貼 pan.quark.cn/s/xxx
        │
        ▼ (前端 OmniboxInput 偵測)
detect_platform() 回傳 platform="quark", content_type="share"
        │
        ▼ (前端改走夸克分支，不走單媒體 preview)
invoke("quark_list_share", {url})  ──► Rust
        │                               ├─ 自動萃取瀏覽器 cookie（quark.cn）
        │                               ├─ token → stoken
        │                               └─ 遞迴 detail 列出整棵樹（保留相對路徑）
        ▼
回傳 QuarkFile[] { fid, fids_token, pwd_id, stoken, rel_path, name, size }
        │
        ▼ (前端把每個檔 enqueue)
invoke("quark_enqueue_files", {files, output_dir})  ──► Rust
        │                                               └─ 每檔 → queue item（downloader=quark）
        ▼
queue::spawn_download → QuarkDownloader.download()
        ├─ POST file/download 取 download_url（帶 cookie+UA）
        ├─ direct_downloader 抓位元組（帶 cookie+UA+referer）
        ├─ 403 過期 → 重取直鏈一次
        └─ 寫入 output_dir/<分享名>/<rel_path>/<name>
```

---

## 4. 詳細設計

### 4.1 新增：瀏覽器 cookie 自動萃取模組

**位置**：`omniget-core/src/core/browser_cookies.rs`（新檔）

**公開 API**：
```rust
/// 從本機瀏覽器讀取指定 root domain 的 cookie，組成 "k=v; k=v" header 字串。
/// 依序嘗試 browsers，回傳第一個成功且含登入態的結果。
pub async fn extract_cookie_header(
    root_domain: &str,            // "quark.cn"
    required_keys: &[&str],       // ["__pus","__puus"] 用來判斷是否登入
) -> anyhow::Result<String>;

pub enum Browser { Edge, Chrome, Brave, Chromium /* , Arc */ }
```

**Phase 1（macOS）實作**：
- 列舉各瀏覽器 Cookies DB 路徑（見 §1.3），存在才嘗試
- 複製 DB 到 temp（避開鎖）→ `rusqlite`/`sqlx` 讀 `host_key LIKE '%<domain>%'`
- Keychain 取 Safe Storage 金鑰（每瀏覽器服務名不同）→ PBKDF2 → AES-128-CBC 解密（Cargo 已有 `aes` + `cbc`）
- 多瀏覽器：擇一含 `required_keys` 者；都沒有 → `Err`（前端提示「請先在瀏覽器登入夸克」）

**新增依賴**：
- SQLite reader：優先用既有 `sqlx`（已在 stack）；若 desktop-only 不便，退而用 `rusqlite`（待 review 決定）
- Keychain：macOS 用 `security` CLI（`std::process::Command`，零依賴）或 `security-framework` crate

**跨平台（Phase 2，本案先不做）**：
- Windows：DPAPI + AES-GCM（v10/v20 app-bound），與 mac 不同
- Linux：gnome-keyring / kwallet / 固定 `peanuts`
- **替代方案（review 討論）**：引入成熟 crate [`rookie`](https://crates.io/crates/rookie) 一次解決三平台，省去自己維護解密細節；代價是多一個依賴 + 信任第三方讀 cookie。**建議 Phase 2 評估 `rookie`，Phase 1 先手刻 macOS。**

> ⚠️ 安全：cookie 只在記憶體組成 header 即用即丟，**不落地、不寫 log、不顯示**。Keychain 首次存取可能跳系統密碼視窗（使用者授權一次）。

### 4.2 新增：Quark 平台模組

**位置**：`src-tauri/src/platforms/quark/mod.rs`（新檔，參考 twitter/ 自訂 reqwest 平台）

**內容**：
```rust
pub struct QuarkDownloader { http: reqwest::Client }

const QUARK_UA: &str = "Mozilla/5.0 ... quark-cloud-drive/3.20.0 ... Channel/pckk_other_ch";
const API_BASE: &str = "https://drive-pc.quark.cn/1/clouddrive";

// ── 分享 API ──
async fn get_stoken(&self, pwd_id: &str, cookie: &str) -> Result<String>;
async fn list_dir(&self, pwd_id, stoken, pdir_fid, cookie) -> Result<Vec<QuarkEntry>>; // 含分頁迴圈
async fn list_share_recursive(&self, pwd_id, cookie) -> Result<Vec<QuarkFile>>;        // DFS，組 rel_path
async fn get_download_url(&self, file: &QuarkFile, cookie) -> Result<String>;

// ── trait 實作 ──
impl PlatformDownloader for QuarkDownloader {
    fn name(&self) -> &str { "quark" }
    fn can_handle(&self, url) -> bool { /* pan.quark.cn / drive.uc.cn */ }
    async fn get_media_info(&self, url) -> ...   // 回傳分享標題/檔數（給 UI 摘要）
    async fn download(&self, info, opts, progress) -> DownloadResult {
        // opts 內帶 fid/fids_token/pwd_id/stoken/rel_path（見 §4.4 傳遞方式）
        // 1. 自動萃取 cookie（或用 opts.extra_headers 帶入）
        // 2. get_download_url
        // 3. direct_downloader::download_direct_with_headers(url, headers{UA,Referer,Cookie}, progress)
        // 4. HTTP 403 → 重取直鏈重試一次
    }
}
```

**`QuarkFile`**（跨 Rust↔前端，serde）：
```rust
struct QuarkFile {
    fid: String, share_fid_token: String,
    pwd_id: String, stoken: String,
    name: String, rel_path: String,  // "软件/子夾"，根層為 ""
    size: u64,
}
```

### 4.3 URL 路由變更

`omniget-core/src/platforms/mod.rs`：
- `Platform` enum 加 `Quark`（或用既有 `Other("quark")`，避免動 enum → **建議 `Other("quark")`** 以縮小改動面，待 review）
- `from_url()` 加：`matches("quark.cn")` 或 host `pan.quark.cn` → quark；（選配）`drive.uc.cn` → UC 同源 API（UA/endpoint 不同，Phase 2）

`src-tauri/src/core/url_parser.rs`：
- 加 `parse_quark()`：`/s/{pwd_id}` 或 `/list/share` → content_id=pwd_id, content_type=`Course`（暫借「多檔集合」語意；或新增 `Folder` 變體，待 review）

### 4.4 新增 Tauri 指令（`src-tauri/src/commands/downloads.rs` 或新 `commands/quark.rs`）

```rust
#[tauri::command]
async fn quark_list_share(url: String) -> Result<QuarkShareListing, String>;
//   → { title, total_files, total_bytes, files: Vec<QuarkFile> }

#[tauri::command]
async fn quark_enqueue_files(
    app, state,
    files: Vec<QuarkFile>,
    output_dir: String,
) -> Result<Vec<DownloadStarted>, String>;
//   → 每檔 enqueue 一個 queue item，output = output_dir/<title>/<rel_path>/<name>
```

**fid 等參數如何傳到 `download()`**：queue item 以 URL 為主鍵。做法（待 review 擇一）：
- (A) 把參數 JSON 塞進 `DownloadOptions.extra_headers`/新欄位 → 改動 model
- (B) 自訂 URL scheme：`quark://download?pwd_id=..&fid=..&token=..&path=..`，`can_handle` 認得，`download()` 解析。**建議 (B)**，零 model 改動，與 `p2p:`/`magnet:` 既有慣例一致。

### 4.5 前端變更

`src/components/omnibox/OmniboxInput.svelte`（或其偵測邏輯）：
- `detect_platform()` 回傳 `platform=="quark"` 時，**不走** `prefetch_media_info` 單媒體預覽，改：
  1. `invoke("quark_list_share", {url})` → 顯示摘要卡：「分享『{title}』· {n} 個檔案 · {size}」+ 一顆「下載全部」
  2. 按下 → `invoke("quark_enqueue_files", {files, output_dir})`
  3. 導到下載頁，沿用既有佇列 UI
- 失敗（未登入/無 cookie）→ 既有 toast：明確錯誤 +「請先在瀏覽器登入夸克」

新增 i18n keys（`i18n/{en,zh-CN,...}/...json`）：
`quark.share_summary`、`quark.download_all`、`quark.not_logged_in`、`quark.listing`、`quark.no_cookie` 等。

---

## 5. 邊界情況 / 風險

| 項目 | 處理 |
|------|------|
| 直鏈過期（403）| download() 內重取一次；仍失敗 → 標準 retry 機制 |
| 大量檔案（數百）| 遞迴 + 分頁（_size=50）；enqueue 沿用 `max_concurrent` 節流；列檔加 ~300ms/批節流避免風控 |
| 同名檔/路徑非法字元 | 沿用 `sanitize-filename`（已在 deps）；rel_path 逐段 sanitize |
| 巢狀深資料夾 | DFS 設深度上限（如 30）防環/異常 |
| 未登入 / 無 cookie | 明確錯誤，引導登入；**不** silently 失敗 |
| 瀏覽器執行中 DB 鎖 | 先 `cp` 副本再讀（PoC 已驗證）|
| Keychain 拒絕授權 | 捕捉錯誤 → toast 提示需授權 |
| 多瀏覽器都登入夸克 | 擇一含登入 cookie 者；（選配）設定讓使用者指定偏好瀏覽器 |
| 風控 / 限流 | 節流 + UA 一致；遇限流明確提示，不暴力重試 |
| `自己網盤`（/list 非 share）| Phase 2（API body 只需 `{fids}`，無 stoken）；本案先做 share |

---

## 6. 安全與合規

- Cookie 即用即丟，不落地/不 log/不顯示（與 PoC 一致）
- 僅下載「使用者自己有合法存取權」的分享（用其本人登入態）
- UA 偽裝為桌面客戶端是繞夸克「強制裝客戶端」策略，與 LinkSwift 同原理；屬個人自用工具範疇
- 文件/程式碼**不得**出現任何實際 cookie / token 明文

---

## 7. 測試計畫

- **Rust 單元**：URL 路由（pan.quark.cn 各形式）、stoken 解析、detail 分頁聚合、rel_path 組裝、cookie 解密（v10 去 padding/去 domain 前綴）
- **整合（手動，用真分享）**：`pan.quark.cn/s/3444308e99b7`（PoC 已知內容：1 資料夾 → 2 圖片 + 2 子夾）→ 驗證遞迴列出全部、目錄結構正確、大小吻合
- **錯誤路徑**：未登入瀏覽器 → 明確提示；過期直鏈 → 自動重取
- `cargo check` + `pnpm check` 全綠

---

## 8. 分階段交付

| Phase | 範圍 |
|-------|------|
| **P1（本案 MVP）** | macOS + Edge/Chrome cookie 自動萃取；share 頁遞迴全抓；每檔入列；403 重取；前端偵測+摘要卡+下載全部；i18n |
| P2 | Windows/Linux cookie（評估 `rookie`）；`自己網盤`(/list) 支援；UC 网盘（drive.uc.cn 同源）；偏好瀏覽器設定；檔案樹勾選 UX（若日後要）|

---

## 9. 待 review 的開放問題

1. **Platform enum**：新增 `Quark` 變體，還是用 `Other("quark")`？（建議 Other，縮小改動）
2. **fid 參數傳遞**：自訂 `quark://` URL scheme（建議）還是擴 `DownloadOptions`？
3. **SQLite reader**：複用 `sqlx` 還是加 `rusqlite`？
4. **Keychain 存取**：`security` CLI（零依賴）還是 `security-framework` crate？
5. **content_type**：借用 `Course` 還是新增 `Folder` 變體？
6. 是否要做「偵測到夸克但瀏覽器沒登入」時，引導開啟登入頁的流程？

---

## 10. 預估改動檔案清單

**新增**
- `omniget-core/src/core/browser_cookies.rs`
- `src-tauri/src/platforms/quark/mod.rs`
- （選配）`src-tauri/src/commands/quark.rs`
- i18n `quark.*` keys（各語系）

**修改**
- `omniget-core/src/platforms/mod.rs`（from_url + 模組宣告）
- `omniget-core/src/core/mod.rs`（pub mod browser_cookies）
- `src-tauri/src/platforms/mod.rs`（pub mod quark）
- `src-tauri/src/core/url_parser.rs`（parse_quark）
- `src-tauri/src/commands/downloads.rs` 或 `mod.rs`（註冊新指令、registry 註冊 QuarkDownloader）
- `src-tauri/src/lib.rs`（invoke_handler 加新指令）
- `src/components/omnibox/OmniboxInput.svelte`（夸克分支）
- `src/lib/stores/`（必要時加夸克列檔暫存）
- `Cargo.toml`（若加 rusqlite/security-framework）
