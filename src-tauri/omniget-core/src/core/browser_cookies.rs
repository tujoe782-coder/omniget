//! Automatic browser cookie extraction (Phase 1: macOS, Chromium-family).
//!
//! Reads the on-disk Cookies SQLite DB of installed Chromium-based browsers
//! (Edge / Chrome / Brave / Chromium), decrypts the `v10` AES-128-CBC values
//! using the per-browser key stored in the macOS Keychain, and assembles a
//! `Cookie:` header string for a given root domain.
//!
//! Used by the Quark (夸克网盘) downloader so the user only has to be logged in
//! in their browser — no manual cookie import. Cookies are assembled in memory
//! and never persisted or logged.
//!
//! Decryption recipe (Chromium on macOS):
//! * Keychain generic password, service = "<Browser> Safe Storage" → PBKDF2
//!   (HMAC-SHA1, salt = "saltysalt", iterations = 1003, key len = 16)
//! * AES-128-CBC, IV = 16 × 0x20, value prefixed with literal "v10"
//! * Newer Chromium prepends a 32-byte SHA-256 domain hash to the plaintext;
//!   stripped when the leading bytes are non-printable.
//!
//! Windows (DPAPI / app-bound) and Linux (gnome-keyring / kwallet) are Phase 2.

use anyhow::{anyhow, Context, Result};
use std::path::PathBuf;
use std::process::Command;

/// A Chromium-family browser we know how to read on this platform.
struct ChromiumBrowser {
    /// Human-readable name for diagnostics.
    name: &'static str,
    /// Path to the Default profile's `Cookies` SQLite DB.
    cookies_db: PathBuf,
    /// Keychain generic-password service name holding the Safe Storage key.
    keychain_service: &'static str,
}

#[cfg(target_os = "macos")]
fn known_browsers() -> Vec<ChromiumBrowser> {
    let home = match dirs::home_dir() {
        Some(h) => h,
        None => return Vec::new(),
    };
    let app_support = home.join("Library/Application Support");
    let mk = |sub: &str, service: &'static str, name: &'static str| ChromiumBrowser {
        name,
        cookies_db: app_support.join(sub).join("Default/Cookies"),
        keychain_service: service,
    };
    vec![
        mk("Microsoft Edge", "Microsoft Edge Safe Storage", "Edge"),
        mk("Google/Chrome", "Chrome Safe Storage", "Chrome"),
        mk(
            "BraveSoftware/Brave-Browser",
            "Brave Safe Storage",
            "Brave",
        ),
        mk("Chromium", "Chromium Safe Storage", "Chromium"),
    ]
}

#[cfg(not(target_os = "macos"))]
fn known_browsers() -> Vec<ChromiumBrowser> {
    Vec::new()
}

/// Extract a `Cookie:` header for `root_domain` (e.g. "quark.cn") from the first
/// installed browser whose cookie jar contains **all** `required_keys`.
///
/// `required_keys` are the cookie names that indicate a logged-in session
/// (e.g. `["__pus", "__puus"]` for Quark). If no browser yields them, returns
/// an error describing the situation so the UI can prompt the user to log in.
///
/// This function performs blocking IO (SQLite CLI + Keychain); call it from a
/// blocking context (`tokio::task::spawn_blocking`).
pub fn extract_cookie_header(root_domain: &str, required_keys: &[&str]) -> Result<String> {
    let browsers = known_browsers();
    if browsers.is_empty() {
        return Err(anyhow!(
            "Automatic browser cookie extraction is only supported on macOS for now"
        ));
    }

    let mut last_err: Option<anyhow::Error> = None;
    let mut found_any_db = false;

    for browser in &browsers {
        if !browser.cookies_db.exists() {
            continue;
        }
        found_any_db = true;
        match read_browser_cookies(browser, root_domain) {
            Ok(jar) => {
                let has_login = required_keys.iter().all(|k| jar.contains_key(*k));
                if !jar.is_empty() && has_login {
                    // Only emit cookies that are valid as an HTTP header value
                    // (visible ASCII). Some non-essential tracking cookies (e.g.
                    // Quark's `isQuark`, `grey-id`) decrypt to binary blobs and
                    // would otherwise make `reqwest` reject the whole header —
                    // they are not needed for auth, so they are dropped.
                    let header = jar
                        .iter()
                        .filter(|(k, v)| is_header_safe(k) && is_header_safe(v))
                        .map(|(k, v)| format!("{k}={v}"))
                        .collect::<Vec<_>>()
                        .join("; ");
                    tracing::info!(
                        "[browser_cookies] using {} for {} ({} cookies)",
                        browser.name,
                        root_domain,
                        jar.len()
                    );
                    return Ok(header);
                }
                tracing::debug!(
                    "[browser_cookies] {} has {} cookies for {} but not logged in",
                    browser.name,
                    jar.len(),
                    root_domain
                );
            }
            Err(e) => {
                tracing::warn!("[browser_cookies] {} read failed: {}", browser.name, e);
                last_err = Some(e);
            }
        }
    }

    if !found_any_db {
        return Err(anyhow!(
            "No supported browser found. Install/login in Edge or Chrome."
        ));
    }
    Err(last_err.unwrap_or_else(|| {
        anyhow!(
            "Not logged in to {} in any browser — please log in first",
            root_domain
        )
    }))
}

/// Read + decrypt all cookies for `root_domain` from one browser.
fn read_browser_cookies(
    browser: &ChromiumBrowser,
    root_domain: &str,
) -> Result<std::collections::HashMap<String, String>> {
    let key = keychain_aes_key(browser.keychain_service)
        .with_context(|| format!("keychain key for {}", browser.name))?;

    // Copy the DB out — the browser holds a lock while running.
    let tmp = std::env::temp_dir().join(format!(
        "omniget-cookies-{}-{}.db",
        browser.name,
        std::process::id()
    ));
    std::fs::copy(&browser.cookies_db, &tmp).context("copy cookies db")?;
    let result = (|| {
        let rows = sqlite_query_hex(&tmp, root_domain)?;
        let mut jar = std::collections::HashMap::new();
        for (name, enc_hex, plain) in rows {
            if let Some(value) = decrypt_cookie(&enc_hex, &key) {
                jar.insert(name, value);
            } else if !plain.is_empty() {
                // Unencrypted cookie (older format) — use the plaintext column.
                jar.insert(name, plain);
            }
        }
        Ok::<_, anyhow::Error>(jar)
    })();
    let _ = std::fs::remove_file(&tmp);
    result
}

/// `(name, encrypted_value_hex, value_plaintext)` rows for the domain.
fn sqlite_query_hex(
    db: &std::path::Path,
    root_domain: &str,
) -> Result<Vec<(String, String, String)>> {
    // Unit separator (0x1f) avoids collisions with cookie content.
    let sep = "\u{1f}";
    let sql = format!(
        "SELECT name, hex(encrypted_value), value FROM cookies WHERE host_key LIKE '%{}%';",
        root_domain.replace('\'', "")
    );
    let out = Command::new("/usr/bin/sqlite3")
        .args(["-separator", sep, "-newline", "\u{1e}"])
        .arg(db.as_os_str())
        .arg(&sql)
        .output()
        .context("run sqlite3")?;
    if !out.status.success() {
        return Err(anyhow!(
            "sqlite3 failed: {}",
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut rows = Vec::new();
    for line in text.split('\u{1e}') {
        let line = line.trim_matches(['\n', '\r']);
        if line.is_empty() {
            continue;
        }
        let mut parts = line.split(sep);
        let name = parts.next().unwrap_or("").to_string();
        let enc_hex = parts.next().unwrap_or("").to_string();
        let plain = parts.next().unwrap_or("").to_string();
        if !name.is_empty() {
            rows.push((name, enc_hex, plain));
        }
    }
    Ok(rows)
}

/// Fetch the Safe Storage password from the Keychain and derive the AES key.
fn keychain_aes_key(service: &str) -> Result<[u8; 16]> {
    let out = Command::new("/usr/bin/security")
        .args(["find-generic-password", "-w", "-s", service])
        .output()
        .context("run security")?;
    if !out.status.success() {
        return Err(anyhow!("keychain entry '{}' not found", service));
    }
    let mut password = out.stdout;
    // Strip trailing newline from CLI output.
    while password.last() == Some(&b'\n') || password.last() == Some(&b'\r') {
        password.pop();
    }
    if password.is_empty() {
        return Err(anyhow!("empty keychain password for '{}'", service));
    }

    let mut key = [0u8; 16];
    pbkdf2::pbkdf2_hmac::<sha1::Sha1>(&password, b"saltysalt", 1003, &mut key);
    Ok(key)
}

/// Decrypt a Chromium `v10` cookie value (hex-encoded) → UTF-8 string.
/// Returns `None` for non-`v10` / undecryptable values (caller falls back).
fn decrypt_cookie(enc_hex: &str, key: &[u8; 16]) -> Option<String> {
    if enc_hex.is_empty() {
        return None;
    }
    let bytes = hex_decode(enc_hex)?;
    if bytes.len() < 3 || &bytes[..3] != b"v10" {
        return None;
    }
    let ct = &bytes[3..];
    if ct.is_empty() || ct.len() % 16 != 0 {
        return None;
    }

    use aes::cipher::{block_padding::Pkcs7, BlockDecryptMut, KeyIvInit};
    type Aes128CbcDec = cbc::Decryptor<aes::Aes128>;
    let iv = [0x20u8; 16];
    let mut buf = ct.to_vec();
    let pt = Aes128CbcDec::new(key.into(), &iv.into())
        .decrypt_padded_mut::<Pkcs7>(&mut buf)
        .ok()?;

    // Newer Chromium prepends a 32-byte SHA-256(domain) hash; strip if the
    // plaintext starts with non-printable bytes.
    let pt = if pt.len() >= 32 && pt[..3.min(pt.len())].iter().any(|b| !(0x20..=0x7e).contains(b)) {
        &pt[32..]
    } else {
        pt
    };

    Some(String::from_utf8_lossy(pt).into_owned())
}

/// Whether a string is safe to use inside an HTTP header value (visible ASCII).
/// Filters out cookies whose decrypted value contains control/non-ASCII bytes.
fn is_header_safe(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| (0x20..=0x7e).contains(&b))
}

/// Minimal hex decoder (avoids adding the `hex` crate).
fn hex_decode(s: &str) -> Option<Vec<u8>> {
    let s = s.trim();
    if s.len() % 2 != 0 {
        return None;
    }
    let mut out = Vec::with_capacity(s.len() / 2);
    let b = s.as_bytes();
    let nib = |c: u8| -> Option<u8> {
        match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            b'A'..=b'F' => Some(c - b'A' + 10),
            _ => None,
        }
    };
    let mut i = 0;
    while i < b.len() {
        out.push((nib(b[i])? << 4) | nib(b[i + 1])?);
        i += 2;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_roundtrip() {
        assert_eq!(hex_decode("763130").unwrap(), b"v10");
        assert_eq!(hex_decode("").unwrap(), Vec::<u8>::new());
        assert!(hex_decode("xyz").is_none());
        assert!(hex_decode("abc").is_none()); // odd length
    }

    #[test]
    fn non_v10_returns_none() {
        let key = [0u8; 16];
        // "hello" hex, no v10 prefix
        assert!(decrypt_cookie("68656c6c6f", &key).is_none());
    }
}
