//! 语音输入词典的纯函数：desktop shared/voiceInputData.ts 词典部分 + voice-input-core 的
//! dictionaryLearningPolicy / dictionary-sync/text 的 Rust 版。逻辑用 `voice_input_golden.json`（上游代码实际输出）核对。
//!
//! 时间与 id 经 `Env` 注入：生产用系统时钟和 UUID，黄金测试用固定值。
//! 文本截断按 UTF-16 码元数（与 JS slice 一致）；截断落在代理对中间时 JS 会留下孤立代理，这里用替换符，属已知边角差异。

use crate::appearance_core::{js_trim, jnum, utf16_len};
use serde_json::{json, Map, Value};

pub const MAX_ENTRIES: usize = 1000;
pub const MAX_CANDIDATES: usize = 200;
pub const MAX_ENTRY_CHARS: usize = 120;
pub const MAX_ALIASES: usize = 8;
pub const CANDIDATE_PROMOTION_COUNT: f64 = 2.0;

pub trait Env {
    fn now(&mut self) -> f64;
    /// createId(prefix, timestamp, seed)。
    fn id(&mut self, prefix: &str, timestamp: f64, seed: &str) -> String;
}

pub struct SystemEnv;
impl Env for SystemEnv {
    fn now(&mut self) -> f64 {
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as f64).unwrap_or(0.0)
    }
    fn id(&mut self, _prefix: &str, _timestamp: f64, _seed: &str) -> String {
        uuid::Uuid::new_v4().to_string()
    }
}

fn is_js_ws(c: char) -> bool {
    (c.is_whitespace() && c != '\u{85}') || c == '\u{feff}'
}

pub fn slice_utf16(s: &str, n: usize) -> String {
    if utf16_len(s) <= n {
        return s.to_string();
    }
    String::from_utf16_lossy(&s.encode_utf16().take(n).collect::<Vec<u16>>())
}

/// 逐行 `replace(/\s+/g,' ').trim()`、丢空行、用空格连接。
fn join_lines(s: &str) -> String {
    let unified = s.replace("\r\n", "\n").replace('\r', "\n");
    unified
        .split('\n')
        .map(|line| line.split(is_js_ws).filter(|p| !p.is_empty()).collect::<Vec<_>>().join(" "))
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// normalizeVoiceInputDictionaryEntryText（非字符串得到空串）。
pub fn normalize_text(v: Option<&Value>) -> String {
    match v.and_then(Value::as_str) {
        Some(s) => js_trim(&slice_utf16(&join_lines(s), MAX_ENTRY_CHARS)).to_string(),
        None => String::new(),
    }
}

pub fn normalize_str(s: &str) -> String {
    normalize_text(Some(&Value::String(s.to_string())))
}

pub fn text_key(s: &str) -> String {
    normalize_str(s).to_lowercase()
}

fn num(v: Option<&Value>) -> Option<f64> {
    v.and_then(Value::as_f64)
}

fn count_of(v: Option<&Value>) -> f64 {
    num(v).unwrap_or(1.0).floor().max(1.0)
}

/// JS 数字转字符串（只用于拼 id）。
fn js_num_str(n: f64) -> String {
    if n.fract() == 0.0 && n.abs() < 1e21 { format!("{}", n as i128) } else { format!("{n}") }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Alias {
    pub text: String,
    pub count: f64,
    pub last_seen_at: f64,
}

impl Alias {
    pub fn to_value(&self) -> Value {
        json!({ "text": self.text, "count": jnum(self.count), "lastSeenAt": jnum(self.last_seen_at) })
    }
}

fn normalize_alias(v: &Value, fallback: f64) -> Option<Alias> {
    let o = v.as_object()?;
    let text = normalize_text(o.get("text"));
    if text.is_empty() {
        return None;
    }
    Some(Alias { text, count: count_of(o.get("count")), last_seen_at: num(o.get("lastSeenAt")).unwrap_or(fallback) })
}

fn sort_aliases(mut v: Vec<Alias>) -> Vec<Alias> {
    v.sort_by(|a, b| {
        b.count.partial_cmp(&a.count).unwrap_or(std::cmp::Ordering::Equal).then(b.last_seen_at.partial_cmp(&a.last_seen_at).unwrap_or(std::cmp::Ordering::Equal))
    });
    v.truncate(MAX_ALIASES);
    v
}

pub fn normalize_aliases(v: Option<&Value>, fallback: f64) -> Vec<Alias> {
    let Some(arr) = v.and_then(Value::as_array) else { return vec![] };
    let mut by: Vec<(String, Alias)> = Vec::new();
    for raw in arr {
        let Some(alias) = normalize_alias(raw, fallback) else { continue };
        let key = text_key(&alias.text);
        if let Some((_, e)) = by.iter_mut().find(|(k, _)| *k == key) {
            e.count += alias.count;
            e.last_seen_at = e.last_seen_at.max(alias.last_seen_at);
        } else {
            by.push((key, alias));
        }
    }
    sort_aliases(by.into_iter().map(|(_, a)| a).collect())
}

fn aliases_to_values(a: &[Alias]) -> Value {
    Value::Array(a.iter().map(Alias::to_value).collect())
}

fn aliases_from_typed(a: &[Alias], ts: f64) -> Vec<Alias> {
    normalize_aliases(Some(&aliases_to_values(a)), ts)
}

fn merge_alias(aliases: &[Alias], text: &str, ts: f64) -> Vec<Alias> {
    let normalized = normalize_str(text);
    if normalized.is_empty() {
        return aliases_from_typed(aliases, ts);
    }
    let mut by: Vec<(String, Alias)> = aliases_from_typed(aliases, ts).into_iter().map(|a| (text_key(&a.text), a)).collect();
    let key = text_key(&normalized);
    if let Some((_, e)) = by.iter_mut().find(|(k, _)| *k == key) {
        e.count += 1.0;
        e.last_seen_at = ts;
    } else {
        by.push((key, Alias { text: normalized, count: 1.0, last_seen_at: ts }));
    }
    sort_aliases(by.into_iter().map(|(_, a)| a).collect())
}

fn merge_aliases(aliases: &[Alias], texts: &[String], ts: f64) -> Vec<Alias> {
    texts.iter().fold(aliases_from_typed(aliases, ts), |acc, t| merge_alias(&acc, t, ts))
}

#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub id: String,
    pub text: String,
    pub source: String,
    pub frequency: f64,
    pub aliases: Vec<Alias>,
    pub created_at: f64,
    pub updated_at: f64,
}

impl Entry {
    pub fn to_value(&self) -> Value {
        json!({
            "id": self.id, "text": self.text, "source": self.source, "frequency": jnum(self.frequency),
            "aliases": aliases_to_values(&self.aliases), "createdAt": jnum(self.created_at), "updatedAt": jnum(self.updated_at),
        })
    }
    /// 取自已规范化的 settings。
    pub fn from_normalized(v: &Value) -> Entry {
        let f = |k: &str| v[k].as_f64().unwrap_or(0.0);
        Entry {
            id: v["id"].as_str().unwrap_or("").into(),
            text: v["text"].as_str().unwrap_or("").into(),
            source: v["source"].as_str().unwrap_or("manual").into(),
            frequency: f("frequency"),
            aliases: v["aliases"].as_array().map(|a| a.iter().filter_map(|x| normalize_alias(x, 0.0)).collect()).unwrap_or_default(),
            created_at: f("createdAt"),
            updated_at: f("updatedAt"),
        }
    }
}

pub fn create_entry(env: &mut dyn Env, text: &str, source: &str, ts: f64, frequency: f64, aliases: &[Alias]) -> Entry {
    Entry {
        id: env.id("dict", ts, text),
        text: text.to_string(),
        source: source.to_string(),
        frequency: frequency.floor().max(1.0),
        aliases: aliases_from_typed(aliases, ts),
        created_at: ts,
        updated_at: ts,
    }
}

pub fn create_manual_entry(env: &mut dyn Env, text: &str) -> Option<Entry> {
    let n = normalize_str(text);
    if n.is_empty() {
        return None;
    }
    let now = env.now();
    Some(create_entry(env, &n, "manual", now, 1.0, &[]))
}

pub fn normalize_entries(env: &mut dyn Env, value: Option<&Value>) -> Vec<Entry> {
    match value {
        Some(Value::String(s)) => {
            let ts = env.now();
            let texts: Vec<String> = s.replace("\r\n", "\n").replace('\r', "\n").split('\n').map(normalize_str).filter(|t| !t.is_empty()).take(MAX_ENTRIES).collect();
            texts.iter().map(|t| create_entry(env, t, "manual", ts, 1.0, &[])).collect()
        }
        Some(Value::Array(arr)) => {
            let mut seen: Vec<String> = Vec::new();
            let mut out = Vec::new();
            for raw in arr {
                let Some(o) = raw.as_object() else { continue };
                let text = normalize_text(o.get("text"));
                if text.is_empty() {
                    continue;
                }
                let source = if o.get("source").and_then(Value::as_str) == Some("automatic") { "automatic" } else { "manual" };
                let now = env.now();
                let created = num(o.get("createdAt")).unwrap_or(now);
                let updated = num(o.get("updatedAt")).unwrap_or(created);
                let frequency = count_of(o.get("frequency"));
                let aliases = normalize_aliases(o.get("aliases"), updated);
                let id = match o.get("id").and_then(Value::as_str).filter(|s| !js_trim(s).is_empty()) {
                    Some(s) => js_trim(s).to_string(),
                    None => format!("dict-{}-{}", js_num_str(created), text),
                };
                let dedupe = format!("{source}:{}", text.to_lowercase());
                if seen.contains(&dedupe) {
                    continue;
                }
                seen.push(dedupe);
                out.push(Entry { id, text, source: source.into(), frequency, aliases, created_at: created, updated_at: updated });
            }
            out.truncate(MAX_ENTRIES);
            out
        }
        _ => vec![],
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Candidate {
    pub text: String,
    pub evidence_count: f64,
    pub aliases: Vec<Alias>,
    pub created_at: f64,
    pub updated_at: f64,
}

impl Candidate {
    pub fn to_value(&self) -> Value {
        json!({
            "text": self.text, "evidenceCount": jnum(self.evidence_count), "aliases": aliases_to_values(&self.aliases),
            "createdAt": jnum(self.created_at), "updatedAt": jnum(self.updated_at),
        })
    }
    pub fn from_normalized(v: &Value) -> Candidate {
        let f = |k: &str| v[k].as_f64().unwrap_or(0.0);
        Candidate {
            text: v["text"].as_str().unwrap_or("").into(),
            evidence_count: f("evidenceCount"),
            aliases: v["aliases"].as_array().map(|a| a.iter().filter_map(|x| normalize_alias(x, 0.0)).collect()).unwrap_or_default(),
            created_at: f("createdAt"),
            updated_at: f("updatedAt"),
        }
    }
}

pub fn normalize_candidates(env: &mut dyn Env, value: Option<&Value>) -> Vec<Candidate> {
    let Some(arr) = value.and_then(Value::as_array) else { return vec![] };
    let mut seen: Vec<String> = Vec::new();
    let mut out: Vec<Candidate> = Vec::new();
    for raw in arr {
        let Some(o) = raw.as_object() else { continue };
        let text = normalize_text(o.get("text"));
        if text.is_empty() {
            continue;
        }
        let key = text_key(&text);
        if seen.contains(&key) {
            continue;
        }
        seen.push(key);
        let now = env.now();
        let created = num(o.get("createdAt")).unwrap_or(now);
        let updated = num(o.get("updatedAt")).unwrap_or(created);
        let aliases = normalize_aliases(o.get("aliases"), updated);
        let evidence = match num(o.get("evidenceCount")) {
            Some(n) => n.floor().max(1.0),
            None => {
                let sum: f64 = aliases.iter().map(|a| a.count).sum();
                (if sum == 0.0 { 1.0 } else { sum }).floor().max(1.0)
            }
        };
        out.push(Candidate { text, evidence_count: evidence, aliases, created_at: created, updated_at: updated });
    }
    out.sort_by(|a, b| {
        b.evidence_count.partial_cmp(&a.evidence_count).unwrap_or(std::cmp::Ordering::Equal).then(b.updated_at.partial_cmp(&a.updated_at).unwrap_or(std::cmp::Ordering::Equal))
    });
    out.truncate(MAX_CANDIDATES);
    out
}

pub fn normalize_suppressed(value: Option<&Value>) -> Vec<String> {
    let Some(arr) = value.and_then(Value::as_array) else { return vec![] };
    let mut seen: Vec<String> = Vec::new();
    let mut out = Vec::new();
    for v in arr {
        let t = normalize_text(Some(v));
        if t.is_empty() {
            continue;
        }
        let key = text_key(&t);
        if seen.contains(&key) {
            continue;
        }
        seen.push(key);
        out.push(t);
    }
    out.truncate(MAX_ENTRIES);
    out
}

fn csv_term(s: &str) -> Option<String> {
    let n = js_trim(&join_lines(s)).to_string();
    if n.is_empty() || utf16_len(&n) > MAX_ENTRY_CHARS { None } else { Some(n) }
}

pub struct CsvMerge {
    pub entries: Vec<Entry>,
    pub imported: usize,
}

/// mergeVoiceInputDictionaryCsvTerms（只返回调用方用到的两项）。
pub fn merge_csv_terms(env: &mut dyn Env, existing: Option<&Value>, terms: &[String]) -> CsvMerge {
    let mut entries = normalize_entries(env, existing);
    let mut seen: Vec<String> = entries.iter().map(|e| text_key(&e.text)).collect();
    let mut imported = 0;
    let now = env.now();
    for raw in terms {
        let Some(term) = csv_term(raw) else { continue };
        let key = text_key(&term);
        if seen.contains(&key) || entries.len() >= MAX_ENTRIES {
            continue;
        }
        seen.push(key);
        imported += 1;
        let e = create_entry(env, &term, "manual", now, 1.0, &[]);
        entries.push(e);
    }
    CsvMerge { entries, imported }
}

pub fn new_automatic_entries(env: &mut dyn Env, prev: &[Entry], next: &[Entry]) -> Vec<Value> {
    let prev_v = Value::Array(prev.iter().map(Entry::to_value).collect());
    let next_v = Value::Array(next.iter().map(Entry::to_value).collect());
    let prev_keys: Vec<String> = normalize_entries(env, Some(&prev_v)).iter().filter(|e| e.source == "automatic").map(|e| text_key(&e.text)).collect();
    normalize_entries(env, Some(&next_v))
        .into_iter()
        .filter(|e| e.source == "automatic" && !prev_keys.contains(&text_key(&e.text)))
        .map(|e| json!({ "id": e.id, "text": e.text }))
        .collect()
}

// ── 学习动作 ──

struct Action {
    action: String,
    term: String,
    aliases: Vec<String>,
    confidence: Option<String>,
}

fn rank(a: &str) -> Option<i32> {
    match a {
        "add_candidate" => Some(0),
        "add_entry" => Some(1),
        "update_entry" => Some(2),
        _ => None,
    }
}

/// coalesceDictionaryLearningActions。输入元素需是对象且 term / action 为字符串（调用方已过滤）。
fn coalesce(actions: &[Value]) -> Vec<Action> {
    let mut words: Vec<(String, Action)> = Vec::new();
    for a in actions {
        let confidence = a.get("confidence").and_then(Value::as_str).map(String::from);
        if confidence.as_deref() == Some("low") {
            continue;
        }
        let term = normalize_text(a.get("term"));
        let key = text_key(&term);
        if key.is_empty() {
            continue;
        }
        let action = a.get("action").and_then(Value::as_str).unwrap_or("").to_string();
        let own: Vec<String> = a.get("aliases").and_then(Value::as_array).map(|x| x.iter().map(|v| normalize_text(Some(v))).collect()).unwrap_or_default();
        let idx = words.iter().position(|(k, _)| *k == key);
        let mut aliases: Vec<(String, String)> = Vec::new();
        let prev_aliases: Vec<String> = idx.map(|i| words[i].1.aliases.clone()).unwrap_or_default();
        for alias in prev_aliases.into_iter().chain(own) {
            let ak = text_key(&alias);
            if !ak.is_empty() && ak != key && !aliases.iter().any(|(k, _)| *k == ak) {
                aliases.push((ak, alias));
            }
        }
        let keep_prev = match idx {
            Some(i) => matches!((rank(&words[i].1.action), rank(&action)), (Some(p), Some(n)) if p >= n),
            None => false,
        };
        let (pref_action, pref_term, pref_conf) = if keep_prev {
            let p = &words[idx.unwrap()].1;
            (p.action.clone(), p.term.clone(), p.confidence.clone())
        } else {
            (action, term.clone(), confidence)
        };
        let next = Action { action: pref_action, term: normalize_str(&pref_term), aliases: aliases.into_iter().map(|(_, a)| a).collect(), confidence: pref_conf };
        match idx {
            Some(i) => words[i].1 = next,
            None => words.push((key, next)),
        }
    }
    words.into_iter().map(|(_, a)| a).collect()
}

pub struct Dict {
    pub entries: Vec<Entry>,
    pub candidates: Vec<Candidate>,
}

/// applyVoiceInputDictionaryLearningActions 的词典部分；None = 无变化。
pub fn apply_learning(env: &mut dyn Env, mut entries: Vec<Entry>, mut candidates: Vec<Candidate>, suppressed: &[String], actions: &[Value]) -> Option<Dict> {
    let suppressed: Vec<String> = suppressed.iter().map(|s| text_key(s)).collect();
    let now = env.now();
    let mut changed = false;
    for action in coalesce(actions) {
        let text = normalize_str(&action.term);
        let alias_texts: Vec<String> = action.aliases.iter().map(|a| normalize_str(a)).filter(|a| !a.is_empty() && *a != text).collect();
        if text.is_empty() || action.confidence.as_deref() == Some("low") {
            continue;
        }
        let key = text_key(&text);
        let entry_idx = entries.iter().position(|e| text_key(&e.text) == key);
        let cand_idx = candidates.iter().position(|c| text_key(&c.text) == key);
        let existing = cand_idx.map(|i| candidates[i].clone());
        let evidence = existing.as_ref().map(|c| c.evidence_count).unwrap_or(0.0);
        if entry_idx.is_some() || action.action == "add_entry" || action.action == "update_entry" || evidence + 1.0 >= CANDIDATE_PROMOTION_COUNT {
            if let Some(i) = entry_idx {
                let e = &mut entries[i];
                e.frequency += 1.0;
                e.aliases = merge_aliases(&e.aliases, &alias_texts, now);
                e.updated_at = now;
            } else {
                if suppressed.contains(&key) {
                    continue;
                }
                let aliases = merge_aliases(existing.as_ref().map(|c| c.aliases.as_slice()).unwrap_or(&[]), &alias_texts, now);
                let e = create_entry(env, &text, "automatic", now, evidence + 1.0, &aliases);
                entries.push(e);
            }
            if let Some(i) = cand_idx {
                candidates.remove(i);
            }
            changed = true;
            continue;
        }
        if suppressed.contains(&key) {
            continue;
        }
        let next = match existing {
            Some(c) => Candidate { evidence_count: c.evidence_count + 1.0, aliases: merge_aliases(&c.aliases, &alias_texts, now), updated_at: now, ..c },
            None => Candidate { text, evidence_count: 1.0, aliases: merge_aliases(&[], &alias_texts, now), created_at: now, updated_at: now },
        };
        match cand_idx {
            Some(i) => candidates[i] = next,
            None => candidates.push(next),
        }
        changed = true;
    }
    changed.then_some(Dict { entries, candidates })
}

/// deleteVoiceInputDictionaryEntriesFromSettings 的词典部分。None = 无变化。
pub fn delete_entries(entries: &[Entry], candidates: &[Candidate], suppressed: &[String], ids: &[String]) -> Option<(Vec<Entry>, Vec<Candidate>, Vec<String>)> {
    let id_set: Vec<String> = ids.iter().map(|i| js_trim(i).to_string()).filter(|i| !i.is_empty()).collect();
    if id_set.is_empty() {
        return None;
    }
    let hit: Vec<&Entry> = entries.iter().filter(|e| id_set.contains(&e.id)).collect();
    if hit.is_empty() {
        return None;
    }
    let deleted_keys: Vec<String> = hit.iter().map(|e| text_key(&e.text)).collect();
    let mut suppressed_keys: Vec<String> = suppressed.iter().map(|s| text_key(s)).collect();
    let mut next_suppressed = suppressed.to_vec();
    for e in hit.iter().filter(|e| e.source == "automatic") {
        let k = text_key(&e.text);
        if suppressed_keys.contains(&k) {
            continue;
        }
        suppressed_keys.push(k);
        next_suppressed.push(e.text.clone());
    }
    Some((
        entries.iter().filter(|e| !id_set.contains(&e.id)).cloned().collect(),
        candidates.iter().filter(|c| !deleted_keys.contains(&text_key(&c.text))).cloned().collect(),
        next_suppressed,
    ))
}

pub fn obj(v: &Value) -> Option<&Map<String, Value>> {
    v.as_object()
}
