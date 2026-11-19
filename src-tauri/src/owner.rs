//! 应用会话（desktop main/appSessionState.ts 的单数据集版，原 sidecar owner.mjs）。
//! Tauri 只有一份本地数据：本机模式和云端登录都用 dataOwnerId='local-v1'，未登录为 null。
//! 每次模式切换 generation 递增（renderer 据此区分“边界已提交”和“边界进行中”），持久化到
//! <dataDir>/app-session.json，内容只有 mode / generation / 非敏感的用户展示信息。

use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

pub const LOCAL_DATA_OWNER_ID: &str = "local-v1";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    Local,
    SignedOut,
    Cloud,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Local => "local",
            Mode::SignedOut => "signed-out",
            Mode::Cloud => "cloud",
        }
    }
    pub fn parse(s: &str) -> Option<Mode> {
        match s {
            "local" => Some(Mode::Local),
            "signed-out" => Some(Mode::SignedOut),
            "cloud" => Some(Mode::Cloud),
            _ => None,
        }
    }
}

struct Inner {
    mode: Mode,
    generation: u64,
    cached_user: Option<Value>,
}

pub struct Owner {
    file: PathBuf,
    inner: Mutex<Inner>,
    listeners: Mutex<Vec<Box<dyn Fn(&Value) + Send + Sync>>>,
}

/// JS `slice(0, n)` 按 UTF-16 单元；这里按字符，差别只在代理对被切断时，展示字段可以接受。
fn cut(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

fn sanitize_cached_user(u: Option<&Value>) -> Option<Value> {
    let u = u?.as_object()?;
    let id = u.get("id")?.as_str().filter(|s| !s.is_empty())?;
    let s = |k: &str| u.get(k).and_then(Value::as_str).map(|v| cut(v, 500));
    Some(json!({
        "id": cut(id, 200),
        "name": s("name").unwrap_or_default(),
        "email": s("email"),
        "membershipKind": if u.get("membershipKind").and_then(Value::as_str) == Some("org") { "org" } else { "personal" },
    }))
}

impl Owner {
    /// 读入持久化的会话；没有文件（v0.0.1 升级上来）= 本机模式、generation 1，和旧行为一致。
    pub fn new(data_dir: &Path) -> Self {
        let file = data_dir.join("app-session.json");
        let mut inner = Inner { mode: Mode::Local, generation: 1, cached_user: None };
        if let Ok(text) = std::fs::read_to_string(&file) {
            match serde_json::from_str::<Value>(&text) {
                Ok(raw) => {
                    if let Some(m) = raw.get("mode").and_then(Value::as_str).and_then(Mode::parse) {
                        inner.mode = m;
                    }
                    // JS Number.isSafeInteger：|n| <= 2^53-1；这里只接受非负整数。
                    if let Some(g) = raw.get("generation").and_then(Value::as_u64).filter(|g| (1..=(1u64 << 53) - 1).contains(g)) {
                        inner.generation = g;
                    }
                    if inner.mode == Mode::Cloud {
                        inner.cached_user = sanitize_cached_user(raw.get("cachedUser"));
                    }
                }
                Err(e) => eprintln!("[owner] app-session.json unreadable ({e}); using local mode"),
            }
        }
        Self { file, inner: Mutex::new(inner), listeners: Mutex::new(Vec::new()) }
    }

    pub fn mode(&self) -> Mode {
        self.inner.lock().unwrap().mode
    }

    pub fn cached_user(&self) -> Option<Value> {
        self.inner.lock().unwrap().cached_user.clone()
    }

    fn stamp_of(i: &Inner) -> Value {
        let owner = if i.mode == Mode::SignedOut { Value::Null } else { json!(LOCAL_DATA_OWNER_ID) };
        json!({ "dataOwnerId": owner, "ownerGeneration": i.generation })
    }

    pub fn stamp(&self) -> Value {
        Self::stamp_of(&self.inner.lock().unwrap())
    }

    pub fn is_current(&self, data_owner_id: &Value, generation: &Value) -> bool {
        let s = self.stamp();
        s["dataOwnerId"] == *data_owner_id && s["ownerGeneration"] == *generation
    }

    /// 提交新模式：generation +1 并落盘。同模式只更新缓存的用户展示信息，不动 generation。
    pub fn commit(&self, next: Mode, user: Option<&Value>) -> Result<Value, String> {
        let (stamp, changed) = {
            let mut g = self.inner.lock().unwrap();
            let changed = next != g.mode;
            let mut candidate = Inner { mode: next, generation: g.generation + u64::from(changed), cached_user: if next == Mode::Cloud { sanitize_cached_user(user) } else { None } };
            // 先落盘再改内存：写失败时状态不前进。
            let doc = json!({ "version": 1, "mode": next.as_str(), "generation": candidate.generation, "cachedUser": candidate.cached_user });
            self.persist(&doc)?;
            std::mem::swap(&mut *g, &mut candidate);
            (Self::stamp_of(&g), changed)
        };
        if changed {
            for f in self.listeners.lock().unwrap().iter() {
                f(&stamp);
            }
        }
        Ok(stamp)
    }

    fn persist(&self, doc: &Value) -> Result<(), String> {
        use std::io::Write;
        if let Some(dir) = self.file.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        let tmp = PathBuf::from(format!("{}.tmp-{}", self.file.display(), std::process::id()));
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let res = opts.open(&tmp).and_then(|mut f| f.write_all(doc.to_string().as_bytes())).and_then(|_| std::fs::rename(&tmp, &self.file));
        if let Err(e) = res {
            let _ = std::fs::remove_file(&tmp);
            return Err(e.to_string());
        }
        Ok(())
    }

    pub fn on_change(&self, f: impl Fn(&Value) + Send + Sync + 'static) {
        self.listeners.lock().unwrap().push(Box::new(f));
    }

    /// sidecar 经 localdb 协议访问：owner.get / owner.commit。
    pub fn rpc(&self, op: &str, req: &Value) -> Result<Value, String> {
        match op {
            "owner.get" => {
                let i = self.inner.lock().unwrap();
                Ok(json!({ "mode": i.mode.as_str(), "stamp": Self::stamp_of(&i), "cachedUser": i.cached_user }))
            }
            "owner.commit" => {
                let mode = req["mode"].as_str().and_then(Mode::parse).ok_or_else(|| format!("invalid app mode: {}", req["mode"]))?;
                let stamp = self.commit(mode, req.get("user"))?;
                Ok(json!({ "stamp": stamp, "cachedUser": self.cached_user() }))
            }
            _ => Err(format!("unknown op: {op}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn dir(n: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("owner-{n}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn defaults_commit_generation_and_persistence() {
        let d = dir("basic");
        let o = Owner::new(&d);
        assert_eq!((o.mode(), o.stamp()), (Mode::Local, json!({ "dataOwnerId": "local-v1", "ownerGeneration": 1 })));
        let seen = Arc::new(Mutex::new(Vec::<Value>::new()));
        let sink = seen.clone();
        o.on_change(move |s| sink.lock().unwrap().push(s.clone()));
        let user = json!({ "id": "u1", "name": "N", "email": "a@b.c", "membershipKind": "org", "token": "leak" });
        let s = o.commit(Mode::Cloud, Some(&user)).unwrap();
        assert_eq!(s, json!({ "dataOwnerId": "local-v1", "ownerGeneration": 2 }));
        assert_eq!(o.cached_user().unwrap(), json!({ "id": "u1", "name": "N", "email": "a@b.c", "membershipKind": "org" }));
        // 同模式：只更新展示信息，generation 不动，不通知
        o.commit(Mode::Cloud, Some(&json!({ "id": "u1", "name": "M" }))).unwrap();
        assert_eq!(o.stamp()["ownerGeneration"], 2);
        assert_eq!(o.cached_user().unwrap(), json!({ "id": "u1", "name": "M", "email": null, "membershipKind": "personal" }));
        assert_eq!(seen.lock().unwrap().len(), 1);
        let text = std::fs::read_to_string(d.join("app-session.json")).unwrap();
        assert!(!text.contains("leak"));
        // 重启后恢复
        let o2 = Owner::new(&d);
        assert_eq!((o2.mode(), o2.stamp()["ownerGeneration"].clone()), (Mode::Cloud, json!(2)));
        assert_eq!(o2.cached_user().unwrap()["name"], "M");
        // signed-out：owner 为 null，缓存用户清掉
        let s = o2.commit(Mode::SignedOut, Some(&user)).unwrap();
        assert_eq!(s, json!({ "dataOwnerId": null, "ownerGeneration": 3 }));
        assert!(o2.cached_user().is_none());
        assert!(o2.is_current(&Value::Null, &json!(3)));
        assert!(!o2.is_current(&json!("local-v1"), &json!(3)));
        // 无效 user 在 cloud 模式下被丢弃
        o2.commit(Mode::Cloud, Some(&json!({ "name": "no id" }))).unwrap();
        assert!(o2.cached_user().is_none());
    }

    #[test]
    fn tolerates_bad_files_and_validates_rpc() {
        let d = dir("bad");
        std::fs::write(d.join("app-session.json"), "{not json").unwrap();
        let o = Owner::new(&d);
        assert_eq!((o.mode(), o.stamp()["ownerGeneration"].clone()), (Mode::Local, json!(1)));
        std::fs::write(d.join("app-session.json"), r#"{"mode":"weird","generation":-3,"cachedUser":{"id":"x"}}"#).unwrap();
        let o = Owner::new(&d);
        assert_eq!((o.mode(), o.stamp()["ownerGeneration"].clone(), o.cached_user()), (Mode::Local, json!(1), None));
        // cloud 以外的模式不带缓存用户
        std::fs::write(d.join("app-session.json"), r#"{"mode":"signed-out","generation":7,"cachedUser":{"id":"x"}}"#).unwrap();
        let o = Owner::new(&d);
        assert_eq!((o.mode(), o.stamp(), o.cached_user()), (Mode::SignedOut, json!({ "dataOwnerId": null, "ownerGeneration": 7 }), None));
        assert!(o.rpc("owner.commit", &json!({ "mode": "bogus" })).unwrap_err().contains("invalid app mode"));
        let r = o.rpc("owner.commit", &json!({ "mode": "local" })).unwrap();
        assert_eq!(r["stamp"], json!({ "dataOwnerId": "local-v1", "ownerGeneration": 8 }));
        assert_eq!(o.rpc("owner.get", &json!({})).unwrap()["mode"], "local");
    }

    #[test]
    fn failed_write_does_not_advance_state() {
        let d = dir("fail");
        let o = Owner::new(&d);
        // 把目标文件占成目录：rename 必然失败
        std::fs::create_dir_all(d.join("app-session.json")).unwrap();
        assert!(o.commit(Mode::Cloud, Some(&json!({ "id": "u" }))).is_err());
        assert_eq!((o.mode(), o.stamp()["ownerGeneration"].clone()), (Mode::Local, json!(1)));
    }
}
