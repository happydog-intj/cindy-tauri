//! 桌面外壳的纯规则：主进程文案、路径黑名单、外链放行、浏览器打开文件的目标解析。
//! 对齐 desktop 的 i18n.ts / filePathPolicy.ts / billing.ts / openFileInBrowser.ts；
//! 这些上游规则在这里重写，之后需要手动对齐。

use percent_encoding::percent_decode_str;
use serde_json::Value;
use std::path::{Component, Path, PathBuf};
use std::sync::OnceLock;

const BRAND_NAME: &str = "Cindy";
const BILLING_SUPPORT_EMAIL: &str = "xd-billing@xd.com";

// ── 文案 ──────────────────────────────────────────────────────

/// scripts/gen-main-strings.mjs 从 cindy renderer 的 locale 里抽出来的 { locale: { key: text } }。
static STRINGS_JSON: &str = include_str!("main_strings.json");

fn strings() -> &'static Value {
    static CELL: OnceLock<Value> = OnceLock::new();
    CELL.get_or_init(|| serde_json::from_str(STRINGS_JSON).expect("main_strings.json"))
}

/// desktop main/i18n.ts 的回落链；最后回落到 key 本身。
fn fallback_chain(locale: &str) -> &'static [&'static str] {
    match locale {
        "es" => &["es", "en"],
        "fr" => &["fr", "en"],
        "de" => &["de", "en"],
        "pt-PT" => &["pt-PT", "en"],
        "pt-BR" => &["pt-BR", "en"],
        "zh-Hant-x-lzh" => &["zh-Hant-x-lzh", "zh-TW", "en"],
        "zh-Hans-x-lzh" => &["zh-Hans-x-lzh", "zh-CN", "en"],
        "zh-CN" => &["zh-CN", "en"],
        "zh-TW" => &["zh-TW", "zh-CN", "en"],
        "ja" => &["ja", "en"],
        "ko" => &["ko", "en"],
        _ => &["en"],
    }
}

pub fn is_supported_locale(locale: &str) -> bool {
    matches!(
        locale,
        "es" | "fr" | "de" | "pt-PT" | "pt-BR" | "zh-Hant-x-lzh" | "zh-Hans-x-lzh" | "zh-CN" | "zh-TW" | "en" | "ja" | "ko"
    )
}

/// 系统语言 → 支持的语言（ipc.mjs preferredLocale 的同一规则）。
pub fn preferred_locale(raw: &str) -> &'static str {
    let lower = raw.to_ascii_lowercase().replace('_', "-");
    if lower.starts_with("zh") {
        let rest = &lower[2..];
        if ["-tw", "-hk", "-mo", "-hant"].iter().any(|p| rest.starts_with(p)) {
            return "zh-TW";
        }
        return "zh-CN";
    }
    if lower == "pt-br" {
        return "pt-BR";
    }
    match lower.split('-').next().unwrap_or("") {
        "pt" => "pt-PT",
        "ja" => "ja",
        "ko" => "ko",
        "es" => "es",
        "fr" => "fr",
        "de" => "de",
        _ => "en",
    }
}

pub fn system_locale() -> &'static str {
    preferred_locale(&sys_locale::get_locale().unwrap_or_else(|| "en".into()))
}

pub fn translate(key: &str, locale: &str) -> String {
    for loc in fallback_chain(locale) {
        if let Some(v) = strings().get(*loc).and_then(|m| m.get(key)).and_then(Value::as_str) {
            return v.replace("{{appName}}", BRAND_NAME);
        }
    }
    key.to_string()
}

// ── 路径策略（desktop main/filePathPolicy.ts 的系统目录黑名单） ──

fn blocklist(windows: bool) -> Vec<String> {
    if !windows {
        return ["/etc", "/proc", "/sys", "/dev", "/root", "/var/log"].iter().map(|s| s.to_string()).collect();
    }
    let env = |k: &str, d: &str| std::env::var(k).unwrap_or_else(|_| d.to_string());
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for c in [
        env("SystemRoot", "C:\\Windows"),
        env("ProgramFiles", "C:\\Program Files"),
        env("ProgramFiles(x86)", "C:\\Program Files (x86)"),
        env("ProgramData", "C:\\ProgramData"),
    ] {
        if !c.is_empty() && seen.insert(c.to_lowercase()) {
            out.push(c);
        }
    }
    out
}

fn strip_win_namespace(input: &str) -> String {
    if let Some(rest) = input.strip_prefix("\\\\?\\").or_else(|| input.strip_prefix("\\\\.\\")) {
        if rest.len() >= 4 && rest[..4].eq_ignore_ascii_case("UNC\\") {
            return format!("\\\\{}", &rest[4..]);
        }
        return rest.to_string();
    }
    input.to_string()
}

/// 词法归一：折叠 `.` 和 `..`，不碰文件系统（与 path.resolve 同口径）。
fn lexical_normalize(p: &str, windows: bool) -> String {
    let sep = if windows { '\\' } else { '/' };
    let unified: String = if windows { p.replace('/', "\\") } else { p.to_string() };
    let (prefix, rest) = if windows {
        if unified.starts_with("\\\\") {
            // UNC：\\server\share 当作前缀
            let parts: Vec<&str> = unified[2..].splitn(3, '\\').collect();
            let pre = format!("\\\\{}\\{}", parts.first().unwrap_or(&""), parts.get(1).unwrap_or(&""));
            (pre, parts.get(2).copied().unwrap_or("").to_string())
        } else if unified.len() >= 2 && unified.as_bytes()[1] == b':' {
            (unified[..2].to_string(), unified[2..].to_string())
        } else {
            (String::new(), unified.clone())
        }
    } else {
        (String::new(), unified.clone())
    };
    let mut stack: Vec<&str> = Vec::new();
    for part in rest.split(sep) {
        match part {
            "" | "." => {}
            ".." => {
                stack.pop();
            }
            other => stack.push(other),
        }
    }
    format!("{prefix}{sep}{}", stack.join(&sep.to_string()))
}

fn is_absolute(p: &str, windows: bool) -> bool {
    if windows {
        let b = p.as_bytes();
        (b.len() >= 3 && b[1] == b':' && (b[2] == b'\\' || b[2] == b'/') && b[0].is_ascii_alphabetic()) || p.starts_with("\\\\") || p.starts_with("//")
    } else {
        p.starts_with('/')
    }
}

/// 路径不在系统目录黑名单里（与 isPathAllowed 同判定；`windows` 参数让两个平台的规则都能测）。
pub fn is_path_allowed(file_path: &str, windows: bool) -> bool {
    if file_path.is_empty() {
        return false;
    }
    let input = if windows { strip_win_namespace(file_path) } else { file_path.to_string() };
    if !is_absolute(&input, windows) {
        return false;
    }
    let sep = if windows { '\\' } else { '/' };
    let fold = |s: String| if windows { s.to_lowercase() } else { s };
    let trim = |s: String| if s.len() > 1 && s.ends_with(sep) { s[..s.len() - 1].to_string() } else { s };
    let target = trim(fold(lexical_normalize(&input, windows)));
    for blocked in blocklist(windows) {
        let nb = if windows { strip_win_namespace(&blocked) } else { blocked };
        let key = trim(fold(lexical_normalize(&nb, windows)));
        if target == key || target.starts_with(&format!("{key}{sep}")) {
            return false;
        }
    }
    true
}

// ── 浏览器打开文件（desktop shared/browserOpenableExts.ts + main/openFileInBrowser.ts） ──

pub fn is_browser_openable(file_path: &str) -> bool {
    let name = file_path.rsplit(['/', '\\']).next().unwrap_or(file_path);
    match name.rfind('.') {
        Some(dot) if dot > 0 => matches!(name[dot..].to_ascii_lowercase().as_str(), ".html" | ".htm"),
        _ => false,
    }
}

#[derive(Debug, PartialEq)]
pub struct BrowserTarget {
    pub file_path: String,
    pub file_url: String,
    pub has_url_state: bool,
}

/// 绝对路径 → file:// URL（逐段百分号编码，保留 `/`）。
pub fn path_to_file_url(p: &str) -> String {
    const SET: &percent_encoding::AsciiSet =
        &percent_encoding::CONTROLS.add(b' ').add(b'"').add(b'#').add(b'%').add(b'<').add(b'>').add(b'?').add(b'`').add(b'{').add(b'}').add(b'[').add(b']').add(b'^').add(b'|').add(b'\\');
    let unified = p.replace('\\', "/");
    let body = if unified.starts_with('/') { unified } else { format!("/{unified}") };
    format!("file://{}", percent_encoding::utf8_percent_encode(&body, SET))
}

pub fn resolve_browser_target(value: &str, windows: bool) -> Option<BrowserTarget> {
    if value.is_empty() {
        return None;
    }
    if is_absolute(value, windows) {
        return Some(BrowserTarget { file_path: value.to_string(), file_url: path_to_file_url(value), has_url_state: false });
    }
    let url = url::Url::parse(value).ok()?;
    if url.scheme() != "file" || !url.username().is_empty() || url.password().is_some() || url.port().is_some() {
        return None;
    }
    match url.host_str() {
        None | Some("") | Some("localhost") => {}
        _ => return None,
    }
    let decoded = percent_decode_str(url.path()).decode_utf8().ok()?.to_string();
    let file_path = if windows {
        let t = decoded.trim_start_matches('/');
        t.replace('/', "\\")
    } else {
        decoded
    };
    if !is_absolute(&file_path, windows) {
        return None;
    }
    Some(BrowserTarget { file_path, file_url: url.to_string(), has_url_state: url.query().is_some() || url.fragment().is_some() })
}

// ── 外链放行（desktop shared/billing.ts isAllowedBillingMailto + shell:open-external 白名单） ──

const SYSTEM_SETTINGS_URLS: [&str; 3] = [
    "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility",
    "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture",
    "x-apple.systempreferences:com.apple.preference.security?Privacy_AllFiles",
];

fn is_billing_mailto(value: &str) -> bool {
    let Ok(u) = url::Url::parse(value) else { return false };
    if u.scheme() != "mailto" || !u.username().is_empty() || u.password().is_some() || u.host().is_some() || u.fragment().is_some() {
        return false;
    }
    let Ok(to) = percent_decode_str(u.path()).decode_utf8() else { return false };
    if to.to_lowercase() != BILLING_SUPPORT_EMAIL {
        return false;
    }
    let pairs: Vec<(String, String)> = u.query_pairs().map(|(k, v)| (k.into_owned(), v.into_owned())).collect();
    let mut keys = std::collections::HashSet::new();
    for (k, v) in &pairs {
        if !keys.insert(k.clone()) || !(k == "subject" || k == "body") {
            return false;
        }
        // subject 里不许有控制字符（头注入）。
        if k == "subject" && v.chars().any(|c| matches!(c as u32, 0..=0x1f | 0x7f..=0x9f)) {
            return false;
        }
    }
    true
}

pub fn is_allowed_external_url(url: &Value, macos: bool) -> bool {
    let Some(s) = url.as_str() else { return false };
    if s.len() > 32_768 {
        return false;
    }
    if let Ok(u) = url::Url::parse(s) {
        if matches!(u.scheme(), "http" | "https") {
            return true;
        }
    } else {
        return false;
    }
    if is_billing_mailto(s) {
        return true;
    }
    macos && SYSTEM_SETTINGS_URLS.contains(&s)
}

/// 两个路径是否同一个（只给测试和调用方比较用；不碰文件系统）。
pub fn same_path(a: &Path, b: &Path) -> bool {
    let norm = |p: &Path| -> PathBuf { p.components().filter(|c| !matches!(c, Component::CurDir)).collect() };
    norm(a) == norm(b)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn locale_follows_the_fallback_chain_and_the_key_is_the_last_resort() {
        assert_eq!(translate("settings.windowBehavior.trayMenu.quit", "zh-CN"), "退出 Cindy");
        assert_ne!(translate("settings.windowBehavior.trayMenu.quit", "en"), "settings.windowBehavior.trayMenu.quit");
        assert_eq!(translate("no.such.key", "en"), "no.such.key");
        // 不认识的语言按 en。
        assert_eq!(translate("settings.windowBehavior.trayMenu.quit", "xx"), translate("settings.windowBehavior.trayMenu.quit", "en"));
        assert!(is_supported_locale("zh-Hans-x-lzh") && !is_supported_locale("xx-bogus"));
    }

    #[test]
    fn system_locale_maps_like_the_sidecar_did() {
        for (raw, want) in [
            ("zh-CN", "zh-CN"), ("zh_Hans_CN", "zh-CN"), ("zh-TW", "zh-TW"), ("zh-HK", "zh-TW"), ("zh-Hant", "zh-TW"),
            ("pt-BR", "pt-BR"), ("pt-PT", "pt-PT"), ("pt", "pt-PT"), ("ja-JP", "ja"), ("en-US", "en"), ("sv-SE", "en"), ("de_DE", "de"),
        ] {
            assert_eq!(preferred_locale(raw), want, "{raw}");
        }
    }

    #[test]
    fn system_directories_are_blocked_including_dot_dot_tricks() {
        for p in ["/etc/passwd", "/etc", "/var/log/x", "/root/.ssh/id", "/tmp/../etc/hosts"] {
            assert!(!is_path_allowed(p, false), "{p}");
        }
        for p in ["/tmp/a.html", "/Users/x/etc/a", "/etcetera/a", "relative/a.html", ""] {
            assert_eq!(is_path_allowed(p, false), p.starts_with('/') && !p.is_empty(), "{p}");
        }
        assert!(!is_path_allowed("C:\\Windows\\System32\\x.html", true));
        assert!(!is_path_allowed("c:/windows/x", true));
        assert!(!is_path_allowed("\\\\?\\C:\\Windows\\x", true));
        assert!(is_path_allowed("C:\\Users\\a\\x.html", true));
    }

    #[test]
    fn browser_files_are_html_only_and_targets_resolve_like_electron() {
        assert!(is_browser_openable("/a/b/index.HTML") && is_browser_openable("C:\\a\\b.htm"));
        assert!(!is_browser_openable("/a/.html") && !is_browser_openable("/a/b.txt") && !is_browser_openable("/a/html"));
        let t = resolve_browser_target("/tmp/a b.html", false).unwrap();
        assert_eq!(t.file_url, "file:///tmp/a%20b.html");
        assert!(!t.has_url_state);
        let u = resolve_browser_target("file:///tmp/a%20b.html#top", false).unwrap();
        assert_eq!(u.file_path, "/tmp/a b.html");
        assert!(u.has_url_state);
        assert!(resolve_browser_target("file://evil.example/x.html", false).is_none());
        assert!(resolve_browser_target("file://user@localhost/x.html", false).is_none());
        assert!(resolve_browser_target("https://example.com/x.html", false).is_none());
        assert!(resolve_browser_target("relative.html", false).is_none());
        assert!(resolve_browser_target("file:///C:/Users/a/x.html", true).is_some_and(|t| t.file_path == "C:\\Users\\a\\x.html"));
    }

    #[test]
    fn external_urls_allow_web_billing_mail_and_mac_privacy_panes_only() {
        let ok = |s: &str, mac: bool| is_allowed_external_url(&json!(s), mac);
        assert!(ok("https://example.com/x", false) && ok("http://localhost:3000", false));
        for bad in ["javascript:alert(1)", "file:///etc/passwd", "mailto:a@b.com", "not a url", "data:text/html,x"] {
            assert!(!ok(bad, false), "{bad}");
        }
        assert!(ok("mailto:xd-billing@xd.com?subject=hi&body=x", false));
        assert!(!ok("mailto:xd-billing@xd.com?subject=a&subject=b", false), "重复参数");
        assert!(!ok("mailto:xd-billing@xd.com?cc=a@b.com", false), "不许加收件人");
        assert!(!ok("mailto:xd-billing@xd.com?subject=a%0Ab", false), "subject 里的换行");
        assert!(!ok("mailto:xd-billing@xd.com#x", false));
        let pane = "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility";
        assert!(ok(pane, true) && !ok(pane, false));
        assert!(!is_allowed_external_url(&json!(5), true));
        assert!(!is_allowed_external_url(&json!(format!("https://a.com/{}", "x".repeat(33_000))), false));
    }
}
