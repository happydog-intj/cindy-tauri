//! 远程可调用 / 可转发通道的白名单（数据由 `scripts/gen-app-settings-data.mjs` 从上游 allowlist.ts 生成）。
use serde::Deserialize;
use std::collections::HashSet;
use std::sync::OnceLock;

#[derive(Deserialize)]
struct Data {
    invoke: Vec<String>,
    push: Vec<String>,
    sessions: Vec<String>,
}

fn data() -> &'static Data {
    static D: OnceLock<Data> = OnceLock::new();
    D.get_or_init(|| serde_json::from_str(include_str!("allowlist.json")).expect("allowlist.json"))
}

fn set(cell: &'static OnceLock<HashSet<&'static str>>, pick: fn(&'static Data) -> &'static Vec<String>) -> &'static HashSet<&'static str> {
    cell.get_or_init(|| pick(data()).iter().map(String::as_str).collect())
}

pub fn invoke_allowed(channel: &str) -> bool {
    static S: OnceLock<HashSet<&'static str>> = OnceLock::new();
    set(&S, |d| &d.invoke).contains(channel)
}

pub fn push_forwarded(channel: &str) -> bool {
    static S: OnceLock<HashSet<&'static str>> = OnceLock::new();
    set(&S, |d| &d.push).contains(channel)
}

/// 属于列表级 `sessions` topic 的通道（上游 SESSION_LIST_CHANNELS ∪ ACCOUNT_CHANNELS 与推送白名单的交集）。
pub(super) fn is_sessions_channel(channel: &str) -> bool {
    static S: OnceLock<HashSet<&'static str>> = OnceLock::new();
    set(&S, |d| &d.sessions).contains(channel)
}

pub fn invoke_count() -> usize {
    data().invoke.len()
}

pub fn push_count() -> usize {
    data().push.len()
}

/// link-accept 回给控制端的指纹：FNV-1a 32 位，输入是排序后用换行拼接的通道列表（按 UTF-16 码元）。
pub fn allowlist_hash() -> String {
    static H: OnceLock<String> = OnceLock::new();
    H.get_or_init(|| {
        let mut sorted: Vec<&str> = data().invoke.iter().map(String::as_str).collect();
        sorted.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
        let joined = sorted.join("\n");
        let mut hash: u32 = 0x811c_9dc5;
        for unit in joined.encode_utf16() {
            hash ^= u32::from(unit);
            hash = hash.wrapping_mul(0x0100_0193);
        }
        format!("{hash:08x}")
    })
    .clone()
}
