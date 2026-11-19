//! 工具结果载荷里的「可见产物」判定（上游 payloadSummary 的 extractPayloadToolResult*）。
//! 投影只需要知道是否存在媒体 / 文件 / 卡片，所以这里返回布尔值，不构造条目。
//! 文件提取依赖一个源码词法器（引号、三引号、反引号、注释、xdt-file URL），按上游逐行移植。
use crate::local_db::mapper::{is_js_ws, js_trim};
use serde_json::{Map, Value};

use super::text::content_to_preview;

fn rec(v: &Value) -> Option<&Map<String, Value>> {
    v.as_object()
}

fn non_empty_str(v: Option<&Value>) -> Option<&str> {
    v.and_then(Value::as_str).filter(|s| !s.is_empty())
}

/// 只解开文档化的 ghost_call 信封：`{ok:true,result:{...}}`。
pub fn parse_tool_result_payload(text: &str) -> Option<Map<String, Value>> {
    let outer = match serde_json::from_str::<Value>(text) {
        Ok(Value::Object(o)) => o,
        _ => return None,
    };
    let inner = if outer.get("ok") == Some(&Value::Bool(true)) { outer.get("result").and_then(rec).cloned() } else { None };
    let Some(inner) = inner else { return Some(outer) };
    let inner_off = inner.get("_xdt_render_image") == Some(&Value::Bool(false));
    let mut merged = inner;
    for (k, v) in outer {
        merged.insert(k, v);
    }
    if inner_off {
        merged.insert("_xdt_render_image".into(), Value::Bool(false));
    }
    Some(merged)
}

fn managed_media_kind(url: &str) -> bool {
    if url.starts_with("xdt-image://") || url.starts_with("xdt-video://") || url.starts_with("xdt-audio://") {
        return true;
    }
    let Some(rest) = url.strip_prefix("cindy-media://blobs/") else { return false };
    let Some((hash, ext)) = rest.split_once('.') else { return false };
    if hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) {
        return false;
    }
    matches!(ext, "png" | "jpg" | "jpeg" | "gif" | "webp" | "avif" | "bmp" | "svg" | "mp4" | "webm" | "mov" | "m4v" | "mp3" | "wav" | "m4a" | "ogg" | "flac" | "aac" | "opus")
}

pub fn has_media(text: &str) -> bool {
    if !text.contains("xdt_") {
        return false;
    }
    let Some(p) = parse_tool_result_payload(text) else { return false };
    if p.get("_xdt_render_image") == Some(&Value::Bool(false)) {
        return false;
    }
    let s = |v: Option<&Value>, pre: &str| v.and_then(Value::as_str).is_some_and(|u| u.starts_with(pre) || u.starts_with("cindy-media://"));
    let list = |v: Option<&Value>, pre: &str| v.and_then(Value::as_array).is_some_and(|a| a.iter().any(|u| s(Some(u), pre)));
    if s(p.get("xdt_image_url"), "xdt-image://") || list(p.get("xdt_image_urls"), "xdt-image://") || s(p.get("xdt_video_url"), "xdt-video://") || list(p.get("xdt_video_urls"), "xdt-video://") {
        return true;
    }
    let raw = match p.get("xdt_audio_tracks") {
        Some(Value::Null) | None => p.get("_xdt_audio_tracks"),
        other => other,
    };
    let mut audio = false;
    if let Some(Value::Array(tracks)) = raw {
        for t in tracks {
            let track = rec(t);
            let url = non_empty_str(track.and_then(|t| t.get("xdt_audio_url"))).or_else(|| non_empty_str(track.and_then(|t| t.get("audioUrl"))));
            if url.is_some_and(|u| u.starts_with("xdt-audio://") || u.starts_with("cindy-media://")) {
                audio = true;
            }
        }
    }
    if audio || list(p.get("xdt_audio_urls"), "xdt-audio://") {
        return true;
    }
    p.get("xdt_media_produced").and_then(Value::as_array).is_some_and(|a| a.iter().any(|u| u.as_str().is_some_and(managed_media_kind)))
}

pub fn has_card_ids(text: &str) -> bool {
    let Some(p) = parse_tool_result_payload(text) else { return false };
    ["xdt_card_id", "xdt_anchor_card_id"].iter().any(|k| {
        p.get(*k).and_then(Value::as_str).is_some_and(|id| {
            let n = id.encode_utf16().count();
            n > 0 && n <= 128
        })
    })
}

// ───────────── 文件提取 ─────────────

const FILE_SCHEME: &str = "xdt-file://";

fn is_url_char(c: char) -> bool {
    !is_js_ws(c) && !matches!(c, '"' | '<' | '>' | '\\' | ')')
}

fn starts_at(v: &[char], i: usize, pat: &str) -> bool {
    let p: Vec<char> = pat.chars().collect();
    v.len() >= i + p.len() && v[i..i + p.len()] == p[..]
}

/// `xdt-file://[^\s"<>\\)]+` 在 `i` 处的粘性匹配，返回结束下标。
fn file_url_at(v: &[char], i: usize) -> Option<usize> {
    if !starts_at(v, i, FILE_SCHEME) {
        return None;
    }
    let mut j = i + FILE_SCHEME.len();
    let s = j;
    while j < v.len() && is_url_char(v[j]) {
        j += 1;
    }
    (j > s).then_some(j)
}

struct Tok {
    start: usize,
    end: usize,
    comment: bool,
    closed: bool,
}

fn tokens(v: &[char]) -> Vec<Tok> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < v.len() {
        if let Some(e) = file_url_at(v, i) {
            i = e;
            continue;
        }
        let start = i;
        let q = v[i];
        if q == '"' || q == '\'' || q == '`' {
            let triple = q != '`' && starts_at(v, i, &q.to_string().repeat(3));
            let dl = if triple { 3 } else { 1 };
            i += dl;
            let mut closed = false;
            while i < v.len() {
                if v[i] == '\\' {
                    i = (i + 2).min(v.len());
                } else if v[i] == q && (dl == 1 || starts_at(v, i, &q.to_string().repeat(3))) {
                    i += dl;
                    closed = true;
                    break;
                } else {
                    i += 1;
                }
            }
            out.push(Tok { start, end: i, comment: false, closed });
        } else if starts_at(v, i, "/*") {
            let close = (i + 2..v.len().saturating_sub(1)).find(|&k| v[k] == '*' && v[k + 1] == '/');
            i = close.map_or(v.len(), |k| k + 2);
            out.push(Tok { start, end: i, comment: true, closed: close.is_some() });
        } else if starts_at(v, i, "//") && (i == 0 || (v[i - 1] != ':' && v[i - 1] != '/')) {
            i += 2;
            while i < v.len() && v[i] != '\r' && v[i] != '\n' {
                i += 1;
            }
            out.push(Tok { start, end: i, comment: true, closed: true });
        } else {
            i += 1;
        }
    }
    out
}

fn without_comments(v: &[char]) -> Vec<char> {
    let mut out = v.to_vec();
    for t in tokens(v) {
        if t.comment {
            for c in &mut out[t.start..t.end] {
                if *c != '\r' && *c != '\n' {
                    *c = ' ';
                }
            }
        }
    }
    out
}

fn is_word(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

fn trim_end_ws(v: &[char]) -> &[char] {
    let mut e = v.len();
    while e > 0 && is_js_ws(v[e - 1]) {
        e -= 1;
    }
    &v[..e]
}

/// 去掉紧贴引号的字面量前缀标记（r/u/b/f/br/rb/fr/rf/$/@），再 trimEnd。
fn literal_prefix(v: &[char], quote_index: usize) -> Vec<char> {
    let raw = &v[..quote_index.min(v.len())];
    let n = raw.len();
    let mut cut = n;
    if v.get(quote_index) != Some(&'`') {
        let lc = |c: char| c.to_ascii_lowercase();
        let boundary = |i: usize| i == 0 || !is_word(raw[i - 1]);
        if n >= 2 {
            let (a, b) = (lc(raw[n - 2]), lc(raw[n - 1]));
            let two = matches!((a, b), ('b', 'r') | ('r', 'b') | ('f', 'r') | ('r', 'f')) && boundary(n - 2);
            if two || (raw[n - 2] == '$' && raw[n - 1] == '@') || (raw[n - 2] == '@' && raw[n - 1] == '$') {
                cut = n - 2;
            }
        }
        if cut == n && n >= 1 {
            let c = raw[n - 1];
            if (matches!(lc(c), 'r' | 'u' | 'b' | 'f') && boundary(n - 1)) || c == '$' || c == '@' {
                cut = n - 1;
            }
        }
    }
    trim_end_ws(&raw[..cut]).to_vec()
}

fn ends_with(v: &[char], s: &str) -> bool {
    let p: Vec<char> = s.chars().collect();
    v.len() >= p.len() && v[v.len() - p.len()..] == p[..]
}

fn cond_colon(prefix: &[char]) -> bool {
    let n = prefix.len();
    if n == 0 || prefix[n - 1] != ':' {
        return false;
    }
    for q in 0..n - 1 {
        if prefix[q] != '?' {
            continue;
        }
        let mut p = q + 1;
        let mut ok = true;
        while p < n - 1 {
            let c = prefix[p];
            if !matches!(c, ';' | '\r' | '\n') {
                p += 1;
            } else if c == '\r' && p + 2 < n && prefix[p + 1] == '\n' && matches!(prefix[p + 2], ' ' | '\t') {
                p += 3;
            } else if c == '\n' && p + 1 < n && matches!(prefix[p + 1], ' ' | '\t') {
                p += 2;
            } else {
                ok = false;
                break;
            }
        }
        if ok && p == n - 1 {
            return true;
        }
    }
    false
}

fn object_key_colon(prefix: &[char]) -> bool {
    let n = prefix.len();
    if n == 0 || prefix[n - 1] != ':' {
        return false;
    }
    let s = trim_end_ws(&prefix[..n - 1]);
    let opener = |rest: &[char]| matches!(trim_end_ws(rest).last(), Some('{') | Some(','));
    let mut k = s.len();
    while k > 0 && (is_word(s[k - 1]) || s[k - 1] == '$') {
        k -= 1;
    }
    if k < s.len() && opener(&s[..k]) {
        return true;
    }
    if let Some(&last) = s.last() {
        if (last == '"' || last == '\'') && s.len() >= 3 {
            let mut j = s.len() - 1;
            while j > 0 && s[j - 1] != '"' && s[j - 1] != '\'' {
                j -= 1;
            }
            if j > 0 && j < s.len() && j - 1 + 1 < s.len() && s[j - 1] != '\0' && j != s.len() - 0 {
                let open = j - 1;
                if s.len() - 1 - open > 1 && opener(&s[..open]) {
                    return true;
                }
            }
        }
    }
    false
}

fn looks_like_source_literal(v: &[char], index: usize, end: usize) -> bool {
    if index == 0 {
        return false;
    }
    let before = v[index - 1];
    if before != '`' && before != '\'' && before != '"' {
        return false;
    }
    let after = v.get(end).copied();
    let prefix = literal_prefix(v, index - 1);
    let last = prefix.last().copied();
    let word_return = ends_with(&prefix, "return") && (prefix.len() == 6 || !is_word(prefix[prefix.len() - 7]));
    if matches!(last, Some('=') | Some('[') | Some('(') | Some(',') | Some('?')) || ends_with(&prefix, "=>") || ends_with(&prefix, "&&") || ends_with(&prefix, "||") || word_return {
        return true;
    }
    if cond_colon(&prefix) || object_key_colon(&prefix) {
        return true;
    }
    if after == Some(before) {
        let mut k = end + 1;
        while k < v.len() && is_js_ws(v[k]) {
            k += 1;
        }
        return k < v.len() && v[k] == ';';
    }
    false
}

fn quoted_ranges(v: &[char]) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut prev: Option<(usize, bool)> = None;
    for t in tokens(v) {
        if t.comment {
            continue;
        }
        let start = t.start;
        let end = t.end - usize::from(t.closed);
        let sep = match prev {
            Some((pe, _)) => {
                let p = literal_prefix(v, start);
                let tail: String = p.get(pe + 1..).map(|s| s.iter().collect()).unwrap_or_default();
                js_trim(&tail).to_owned()
            }
            None => String::new(),
        };
        let cont = sep == "," || sep == "+";
        let source = match prev {
            Some((_, ps)) if cont => ps,
            _ => looks_like_source_literal(v, start + 1, end),
        };
        if source {
            out.push((start, end));
        }
        prev = Some((end, source));
    }
    out
}

fn percent_decode_strict(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let h = std::str::from_utf8(b.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(h, 16).ok().filter(|_| h.bytes().all(|c| c.is_ascii_hexdigit()))?);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

fn absolute_file_path(url: &str) -> Option<String> {
    let path = if url.starts_with("xdt-file:///") {
        let raw = &url[FILE_SCHEME.len()..];
        percent_decode_strict(raw).unwrap_or_else(|| raw.to_owned())
    } else {
        let u = url::Url::parse(url).ok()?;
        let p = u.query_pairs().find(|(k, _)| k == "path").map(|(_, v)| v.into_owned()).unwrap_or_default();
        p
    };
    let b = path.as_bytes();
    let drive = b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'\\' || b[2] == b'/');
    (path.starts_with('/') || drive).then_some(path)
}

/// `add()` 能否接受该 URL（只关心是否产生条目）。
fn addable(url: &str) -> bool {
    if let Some(rest) = url.strip_prefix(FILE_SCHEME) {
        return !rest.is_empty() && !rest.chars().any(is_js_ws) && absolute_file_path(url).is_some();
    }
    url.strip_prefix("cindy-media://blobs/")
        .and_then(|r| r.strip_suffix(".glb"))
        .is_some_and(|h| h.len() == 64 && h.bytes().all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c)))
}

fn scan_string(value: &str) -> bool {
    let v: Vec<char> = value.chars().collect();
    let ctx = without_comments(&v);
    let ranges = quoted_ranges(&ctx);
    let mut ri = 0;
    let mut prev: Option<(usize, bool)> = None;
    let mut pos = 0;
    while pos < v.len() {
        // 在 pos 之后找下一个 URL 匹配
        let Some(start) = (pos..v.len()).find(|&i| file_url_at(&v, i).is_some()) else { break };
        let end = file_url_at(&v, start).unwrap();
        pos = end;
        if ctx[start] != v[start] {
            continue;
        }
        while ri < ranges.len() && ranges[ri].1 < start {
            ri += 1;
        }
        if ri < ranges.len() && ranges[ri].0 < start && start < ranges[ri].1 {
            continue;
        }
        let quote = if start > 0 { Some(v[start - 1]) } else { None };
        let mut url_end = end;
        if let Some(q) = quote.filter(|q| *q == '\'' || *q == '`') {
            for k in start..end {
                if v[k] != q {
                    continue;
                }
                let mut m = k + 1;
                while m < end && matches!(v[m], ',' | '.' | ';' | ':' | '!' | '?' | ']' | '}') {
                    m += 1;
                }
                // 前瞻只在匹配文本 match[0] 内求值：到末尾，或 ["'`]xdt-file://
                let at_end = m == end;
                let next_url = m < end && matches!(v[m], '"' | '\'' | '`') && starts_at(&v[..end], m + 1, FILE_SCHEME);
                if at_end || next_url {
                    url_end = k;
                    break;
                }
            }
        }
        pos = url_end;
        let url_chars = &v[start..url_end];
        let is_q = |c: Option<char>| matches!(c, Some('"') | Some('\'') | Some('`'));
        let continues = match prev {
            Some((pe, _)) => {
                let between: Vec<char> = if pe + 1 < start.saturating_sub(1) { v[pe + 1..start - 1].to_vec() } else { Vec::new() };
                let masked: String = without_comments(&between).iter().collect();
                is_q(v.get(pe).copied()) && is_q(quote) && js_trim(&masked) == ","
            }
            None => false,
        };
        let source = match prev {
            Some((_, ps)) if continues => ps,
            _ => looks_like_source_literal(&ctx, start, start + url_chars.len()),
        };
        if !source && addable(&url_chars.iter().collect::<String>()) {
            return true;
        }
        prev = Some((start + url_chars.len(), source));
    }
    false
}

pub fn has_files(text: &str) -> bool {
    let parsed = parse_tool_result_payload(text);
    if parsed.as_ref().and_then(|p| p.get("_xdt_render_image")) == Some(&Value::Bool(false)) {
        return false;
    }
    if let Some(p) = &parsed {
        if let Some(Value::Array(a)) = p.get("_xdt_model_files") {
            if a.iter().any(|f| rec(f).and_then(|f| f.get("url")).and_then(Value::as_str).is_some_and(addable)) {
                return true;
            }
        }
        if let Some(Value::Array(a)) = p.get("xdt_media_produced") {
            if a.iter().any(|u| u.as_str().is_some_and(addable)) {
                return true;
            }
        }
    }
    let content = match parsed {
        Some(p) => Value::Object(p),
        None => serde_json::from_str::<Value>(text).unwrap_or_else(|_| Value::String(text.to_owned())),
    };
    let mut pending = vec![&content];
    while let Some(v) = pending.pop() {
        match v {
            Value::String(s) => {
                if s.contains(FILE_SCHEME) && scan_string(s) {
                    return true;
                }
            }
            Value::Array(a) => pending.extend(a.iter().rev()),
            Value::Object(o) => {
                for (k, c) in o.iter().rev() {
                    if k != "_xdt_model_files" && k != "xdt_media_produced" {
                        pending.push(c);
                    }
                }
            }
            _ => {}
        }
    }
    false
}

pub fn has_visible_history_result(content: &Value) -> bool {
    let text = content_to_preview(content);
    if has_media(&text) || has_files(&text) || has_card_ids(&text) {
        return true;
    }
    let t = js_trim(&text);
    for pre in ["cindy-media://", "xdt-file://"] {
        if let Some(rest) = t.strip_prefix(pre) {
            if !rest.is_empty() && !rest.chars().any(is_js_ws) {
                return true;
            }
        }
    }
    match serde_json::from_str::<Value>(&text) {
        Ok(Value::Object(o)) => {
            o.get("xdt_card_id").is_some_and(Value::is_string)
                || o.get("xdt_anchor_card_id").is_some_and(Value::is_string)
                || (o.get("ok") == Some(&Value::Bool(true)) && o.get("target_session_id").is_some_and(Value::is_string) && o.get("wake_kind").is_some_and(Value::is_string))
        }
        _ => false,
    }
}
