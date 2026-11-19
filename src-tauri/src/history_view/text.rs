//! 文本判定：交付正文、富答案围栏、合成续跑指令、messageContentToPreview。
use serde_json::Value;

const DELIVERY_PROSE_MIN_LENGTH: usize = 600;
const DELIVERY_PROSE_MIN_LIST_ITEMS: usize = 3;
const UI_ACTION_TRIGGER_PREFIX: &str = "[UI_ACTION_TRIGGER]";
const CONTINUE_COMMON: &str = " Review the conversation above to determine how far the task progressed, then continue from where it left off. Do not repeat steps that already completed and had external effects (messages or comments already sent, commits already pushed, files already written).";
const CHECKPOINT: &str = "\n\n[CINDY_RECOVERY_CHECKPOINT v1]";

use crate::local_db::mapper::{is_js_ws, js_trim};

fn utf16_len(s: &str) -> usize {
    s.encode_utf16().count()
}

/// JS 的 `\s`（与 `is_js_ws` 同集合）。
fn is_s(c: char) -> bool {
    is_js_ws(c)
}

/// 去掉行尾 `\r?\n`（JS：`line.replace(/\r?\n$/, "")`）。
fn strip_eol(line: &str) -> &str {
    if let Some(l) = line.strip_suffix('\n') { l.strip_suffix('\r').unwrap_or(l) } else { line }
}

/// 把文本按 `(?<=\n)` 切行，保留换行。
fn lines_keep(source: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    for (i, c) in source.char_indices() {
        if c == '\n' {
            out.push(&source[start..i + 1]);
            start = i + 1;
        }
    }
    if start < source.len() {
        out.push(&source[start..]);
    }
    out
}

/// `^ {0,3}(`{3,}|~{3,})([^\r\n]*)$`：返回 (围栏字符, 数量, 其后文本)。
fn fence_open(body: &str) -> Option<(char, usize, &str)> {
    let spaces = body.chars().take_while(|c| *c == ' ').count();
    if spaces > 3 {
        return None;
    }
    let rest = &body[spaces..];
    let ch = rest.chars().next().filter(|c| *c == '`' || *c == '~')?;
    let count = rest.chars().take_while(|c| *c == ch).count();
    if count < 3 {
        return None;
    }
    let tail = &rest[count..];
    // `[^\r\n]*$`：单行里不含 \r（body 已去掉行尾 \r\n，行内的孤立 \r 不匹配）
    if tail.contains('\r') {
        return None;
    }
    Some((ch, count, tail))
}

/// `^ {0,3}<ch>{count,}\s*$`
fn fence_close(body: &str, ch: char, count: usize) -> bool {
    let spaces = body.chars().take_while(|c| *c == ' ').count();
    if spaces > 3 {
        return false;
    }
    let rest = &body[spaces..];
    let n = rest.chars().take_while(|c| *c == ch).count();
    n >= count && rest[n * ch.len_utf8()..].chars().all(is_s)
}

/// `splitRichAnswer(source).some(p => p.kind === "rich")`：只需要知道有没有完整或未闭合的 cindy-ui 围栏。
pub fn has_rich_answer(source: &str) -> bool {
    // fence: (char, count, rich)
    let mut fence: Option<(char, usize, bool)> = None;
    for line in lines_keep(source) {
        let body = strip_eol(line);
        match fence {
            None => {
                if let Some((ch, count, tail)) = fence_open(body) {
                    let rich = js_trim(tail) == "cindy-ui";
                    if rich {
                        return true; // 一旦打开 rich 围栏，无论是否闭合都产生 rich 片段
                    }
                    fence = Some((ch, count, rich));
                }
            }
            Some((ch, count, _)) => {
                if fence_close(body, ch, count) {
                    fence = None;
                }
            }
        }
    }
    false
}

/// `^[ \t]{0,3}#{1,6}[ \t]+\S`（m 标志：任一行）
fn heading_line(line: &str) -> bool {
    let b: Vec<char> = line.chars().collect();
    let mut i = 0;
    while i < b.len() && i < 3 && (b[i] == ' ' || b[i] == '\t') {
        i += 1;
    }
    let hs = b[i..].iter().take_while(|c| **c == '#').count();
    if hs == 0 || hs > 6 {
        // `#{1,6}` 后必须是 [ \t]：7 个 # 时回溯到 6 个后下一个仍是 #，不匹配
        return false;
    }
    let mut j = i + hs;
    let ws_start = j;
    while j < b.len() && (b[j] == ' ' || b[j] == '\t') {
        j += 1;
    }
    j > ws_start && j < b.len() && !is_s(b[j])
}

/// `^[ \t]{0,3}\|?[ \t]*:?-{3,}:?[ \t]*\|[-:| \t]*$`
fn table_divider_line(line: &str) -> bool {
    let b: Vec<char> = line.chars().collect();
    let mut i = 0;
    while i < b.len() && i < 3 && (b[i] == ' ' || b[i] == '\t') {
        i += 1;
    }
    if i < b.len() && b[i] == '|' {
        i += 1;
    }
    while i < b.len() && (b[i] == ' ' || b[i] == '\t') {
        i += 1;
    }
    if i < b.len() && b[i] == ':' {
        i += 1;
    }
    let d = b[i..].iter().take_while(|c| **c == '-').count();
    if d < 3 {
        return false;
    }
    i += d;
    if i < b.len() && b[i] == ':' {
        i += 1;
    }
    while i < b.len() && (b[i] == ' ' || b[i] == '\t') {
        i += 1;
    }
    if i >= b.len() || b[i] != '|' {
        return false;
    }
    i += 1;
    b[i..].iter().all(|c| matches!(c, '-' | ':' | '|' | ' ' | '\t'))
}

/// 以 `\n`、`\r`、U+2028、U+2029 为行界（JS `m` 标志下 `^`/`$` 的行定义）。
fn js_lines(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let bytes: Vec<(usize, char)> = s.char_indices().collect();
    let mut k = 0;
    while k < bytes.len() {
        let (i, c) = bytes[k];
        if matches!(c, '\n' | '\r' | '\u{2028}' | '\u{2029}') {
            out.push(&s[start..i]);
            let mut next = i + c.len_utf8();
            if c == '\r' && k + 1 < bytes.len() && bytes[k + 1].1 == '\n' {
                k += 1;
                next += 1;
            }
            start = next;
        }
        k += 1;
    }
    out.push(&s[start..]);
    out
}

/// `[ \t]{0,3}(?:[-*+][ \t]+|\d{1,3}[.)][ \t]+)\S` 的行匹配（g+m：每行至多一次）。
fn list_item_line(line: &str) -> bool {
    let b: Vec<char> = line.chars().collect();
    let mut i = 0;
    while i < b.len() && i < 3 && (b[i] == ' ' || b[i] == '\t') {
        i += 1;
    }
    let mut j;
    if i < b.len() && matches!(b[i], '-' | '*' | '+') {
        j = i + 1;
    } else {
        let d = b[i..].iter().take_while(|c| c.is_ascii_digit()).count();
        if d == 0 || d > 3 || i + d >= b.len() || !matches!(b[i + d], '.' | ')') {
            return false;
        }
        j = i + d + 1;
    }
    let ws = j;
    while j < b.len() && (b[j] == ' ' || b[j] == '\t') {
        j += 1;
    }
    j > ws && j < b.len() && !is_s(b[j])
}

/// `!\[[^\]\n]*\]\([^)\s]+(?:\s[^)]*)?\)`
fn markdown_image(s: &str) -> bool {
    let b: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i + 1 < b.len() {
        if b[i] == '!' && b[i + 1] == '[' {
            let mut j = i + 2;
            while j < b.len() && b[j] != ']' && b[j] != '\n' {
                j += 1;
            }
            if j + 1 < b.len() && b[j] == ']' && b[j + 1] == '(' {
                let mut k = j + 2;
                let url_start = k;
                while k < b.len() && b[k] != ')' && !is_s(b[k]) {
                    k += 1;
                }
                if k > url_start && k < b.len() {
                    if b[k] == ')' {
                        return true;
                    }
                    // `\s[^)]*\)`
                    let mut m = k + 1;
                    while m < b.len() && b[m] != ')' {
                        m += 1;
                    }
                    if m < b.len() {
                        return true;
                    }
                }
            }
        }
        i += 1;
    }
    false
}

/// `<img(?=[\s/>])[^<>]*\ssrc\s*=\s*["']?[^\s"'<>]+[^<>]*>`（i）
fn html_image(s: &str) -> bool {
    let lower: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i + 3 < lower.len() {
        let is_img = lower[i] == '<' && lower[i + 1].eq_ignore_ascii_case(&'i') && lower[i + 2].eq_ignore_ascii_case(&'m') && lower[i + 3].eq_ignore_ascii_case(&'g');
        if is_img && i + 4 < lower.len() && (is_s(lower[i + 4]) || lower[i + 4] == '/' || lower[i + 4] == '>') {
            // 取到下一个 `<`/`>`：`[^<>]*` 之后必须以 `>` 结束，且其间含 `\ssrc\s*=\s*["']?[^\s"'<>]+`
            let mut end = i + 4;
            while end < lower.len() && lower[end] != '<' && lower[end] != '>' {
                end += 1;
            }
            if end < lower.len() && lower[end] == '>' {
                let inner: String = lower[i + 4..end].iter().collect();
                if src_attr(&inner) {
                    return true;
                }
            }
        }
        i += 1;
    }
    false
}

fn src_attr(inner: &str) -> bool {
    let c: Vec<char> = inner.chars().collect();
    for p in 0..c.len() {
        if !is_s(c[p]) {
            continue;
        }
        let rest = &c[p + 1..];
        if rest.len() < 3 || !(rest[0].eq_ignore_ascii_case(&'s') && rest[1].eq_ignore_ascii_case(&'r') && rest[2].eq_ignore_ascii_case(&'c')) {
            continue;
        }
        let mut k = 3;
        while k < rest.len() && is_s(rest[k]) {
            k += 1;
        }
        if k >= rest.len() || rest[k] != '=' {
            continue;
        }
        k += 1;
        while k < rest.len() && is_s(rest[k]) {
            k += 1;
        }
        if k < rest.len() && (rest[k] == '"' || rest[k] == '\'') {
            // 可选引号：吃掉它后需要有一个值字符；否则回溯到不吃引号（引号本身不是值字符）
            if k + 1 < rest.len() && !is_s(rest[k + 1]) && !matches!(rest[k + 1], '"' | '\'' | '<' | '>') {
                return true;
            }
            continue;
        }
        if k < rest.len() && !is_s(rest[k]) && !matches!(rest[k], '"' | '\'' | '<' | '>') {
            return true;
        }
    }
    false
}

pub fn is_delivery_prose_text(text: &str) -> bool {
    let t = js_trim(text);
    if t.is_empty() {
        return false;
    }
    if has_rich_answer(t) {
        return true;
    }
    if utf16_len(t) >= DELIVERY_PROSE_MIN_LENGTH {
        return true;
    }
    let lines = js_lines(t);
    if lines.iter().any(|l| heading_line(l)) || lines.iter().any(|l| table_divider_line(l)) {
        return true;
    }
    if markdown_image(t) || html_image(t) {
        return true;
    }
    lines.iter().filter(|l| list_item_line(l)).count() >= DELIVERY_PROSE_MIN_LIST_ITEMS
}

/// messageContentToPreview：数组逐项拼接，对象取文本字段，其余 JSON.stringify(…, null, 2)。
pub fn content_to_preview(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(a) => a.iter().map(content_to_preview).filter(|s| !s.is_empty()).collect::<Vec<_>>().join("\n"),
        Value::Object(o) => {
            let ty = o.get("type").and_then(Value::as_str);
            if ty == Some("text") {
                if let Some(t) = o.get("text").and_then(Value::as_str) {
                    return t.to_owned();
                }
            }
            if ty == Some("thinking") {
                if let Some(t) = o.get("thinking").and_then(Value::as_str) {
                    return t.to_owned();
                }
            }
            if let Some(t) = o.get("text").and_then(Value::as_str) {
                return t.to_owned();
            }
            if let Some(t) = o.get("content").and_then(Value::as_str) {
                return t.to_owned();
            }
            serde_json::to_string_pretty(content).unwrap_or_default()
        }
        Value::Null => String::new(),
        other => js_to_string(other),
    }
}

fn js_to_string(v: &Value) -> String {
    match v {
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => {
            if let Some(f) = n.as_f64() {
                if f.fract() == 0.0 && f.abs() < 1e21 {
                    return format!("{}", f as i64);
                }
            }
            n.to_string()
        }
        _ => String::new(),
    }
}

/// `syntheticTriggerKind(text) === 'continue'`
fn is_continue_trigger(text: &str) -> bool {
    if !text.starts_with(UI_ACTION_TRIGGER_PREFIX) {
        return false;
    }
    let exit = format!("{UI_ACTION_TRIGGER_PREFIX} The previous turn was interrupted because the app exited mid-run.{CONTINUE_COMMON}");
    let err = format!("{UI_ACTION_TRIGGER_PREFIX} The previous turn errored partway through.{CONTINUE_COMMON}");
    text == exit || text == err || text.starts_with(&format!("{exit}{CHECKPOINT}")) || text.starts_with(&format!("{err}{CHECKPOINT}"))
}

/// isContinuationMessage：隐藏的续跑指令（含手机端 JSON 信封里的文本）。
pub fn is_continuation_message(m: &Value) -> bool {
    if m.get("role").and_then(Value::as_str) != Some("user") {
        return false;
    }
    if m.get("isContinuationTrigger") == Some(&Value::Bool(true)) || m.pointer("/agentMeta/autoResume") == Some(&Value::Bool(true)) || m.get("systemCardType").and_then(Value::as_str) == Some("auto-resume") {
        return true;
    }
    let mut content = m.get("content").cloned().unwrap_or(Value::Null);
    if let Value::String(s) = &content {
        // `trimStart().startsWith('{')`
        if s.trim_start_matches(is_js_ws).starts_with('{') {
            match serde_json::from_str::<Value>(s) {
                Ok(v) => content = v,
                Err(_) => return false,
            }
        }
    }
    let text = match &content {
        Value::String(s) => Some(s.as_str()),
        other => other.get("text").and_then(Value::as_str),
    };
    text.is_some_and(is_continue_trigger)
}
