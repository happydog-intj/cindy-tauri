//! 「只存用户覆盖项」的设置文件：sidecar/src/settings/override-file.mjs 的 Rust 版。
//!
//! - 文件里只放和默认值不同的 key；全部回到默认时删除文件。
//! - 写盘为 tmp + rename；整个对象在一把锁里，读改写不会交错。
//! - 读失败：`preserve_unreadable` 为真时保留原文件并拒绝覆盖写，否则删掉坏文件回默认。
//! - `customizedKeys` 的顺序是写入顺序（serde_json 开了 preserve_order，与 JS 对象一致）。

use serde_json::{json, Map, Value};
use std::path::PathBuf;
use std::sync::Mutex;

pub type Normalize = Box<dyn Fn(&Value) -> Value + Send + Sync>;
/// (patch, next, defaults, 当前覆盖项) → 新覆盖项。
pub type Merge = Box<dyn Fn(&Value, &Value, &Value, Map<String, Value>) -> Map<String, Value> + Send + Sync>;

#[derive(Debug)]
pub struct State {
    pub value: Value,
    pub is_customized: bool,
    pub defaults: Value,
    pub customized_keys: Vec<String>,
}

impl State {
    /// desktop 各 *Wire()：{...value, isCustomized, customizedKeys, defaults}。
    pub fn wire(&self) -> Value {
        let mut out = self.value.clone();
        if let Some(o) = out.as_object_mut() {
            o.insert("isCustomized".into(), json!(self.is_customized));
            o.insert("customizedKeys".into(), json!(self.customized_keys));
            o.insert("defaults".into(), self.defaults.clone());
        }
        out
    }
}

struct Cached {
    overrides: Map<String, Value>,
    unreadable: bool,
}

pub struct OverrideFile {
    file: PathBuf,
    defaults: Value,
    normalize: Normalize,
    merge: Option<Merge>,
    max_bytes: Option<u64>,
    preserve_unreadable: bool,
    cached: Mutex<Option<Cached>>,
}

/// 原子写 JSON：先写同目录 tmp 再 rename。
pub fn write_json_atomic(file: &std::path::Path, value: &Value, max_bytes: Option<u64>) -> Result<(), String> {
    let text = serde_json::to_string_pretty(value).map_err(|e| e.to_string())?;
    if let Some(max) = max_bytes {
        if text.len() as u64 > max {
            return Err(format!("file exceeds {max} byte limit"));
        }
    }
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let tmp = file.with_extension(format!("{}.tmp", std::process::id()));
    let res = std::fs::write(&tmp, text).and_then(|_| std::fs::rename(&tmp, file));
    if let Err(e) = res {
        let _ = std::fs::remove_file(&tmp);
        return Err(e.to_string());
    }
    Ok(())
}

/// 读 JSON；文件不存在返回 None，超限或解析失败返回 Err。
pub fn read_json(file: &std::path::Path, max_bytes: Option<u64>) -> Result<Option<Value>, String> {
    if !file.exists() {
        return Ok(None);
    }
    if let Some(max) = max_bytes {
        let len = std::fs::metadata(file).map_err(|e| e.to_string())?.len();
        if len > max {
            return Err(format!("file exceeds {max} byte limit"));
        }
    }
    let text = std::fs::read_to_string(file).map_err(|e| e.to_string())?;
    serde_json::from_str(&text).map(Some).map_err(|e| e.to_string())
}

fn merged(base: &Value, over: &Value) -> Value {
    let mut out = base.as_object().cloned().unwrap_or_default();
    if let Some(o) = over.as_object() {
        for (k, v) in o {
            out.insert(k.clone(), v.clone());
        }
    }
    Value::Object(out)
}

impl OverrideFile {
    pub fn new(file: PathBuf, defaults: Value, normalize: impl Fn(&Value) -> Value + Send + Sync + 'static) -> Self {
        Self { file, defaults, normalize: Box::new(normalize), merge: None, max_bytes: None, preserve_unreadable: false, cached: Mutex::new(None) }
    }

    pub fn with_merge(mut self, merge: impl Fn(&Value, &Value, &Value, Map<String, Value>) -> Map<String, Value> + Send + Sync + 'static) -> Self {
        self.merge = Some(Box::new(merge));
        self
    }

    #[allow(dead_code)]
    pub fn preserving_unreadable(mut self) -> Self {
        self.preserve_unreadable = true;
        self
    }

    #[allow(dead_code)]
    pub fn max_bytes(mut self, n: u64) -> Self {
        self.max_bytes = Some(n);
        self
    }

    fn load<'a>(&self, slot: &'a mut Option<Cached>) -> &'a mut Cached {
        if slot.is_none() {
            let mut overrides = Map::new();
            let mut unreadable = false;
            let parsed = read_json(&self.file, self.max_bytes).and_then(|p| match p {
                Some(v) if self.preserve_unreadable && !v.is_object() => Err("settings file root must be an object".to_string()),
                other => Ok(other),
            });
            match parsed {
                Ok(Some(Value::Object(o))) => overrides = o,
                Ok(_) => {}
                Err(e) => {
                    eprintln!("[settings] {} read failed, using defaults: {e}", self.file.file_name().and_then(|n| n.to_str()).unwrap_or("?"));
                    if self.preserve_unreadable {
                        unreadable = true;
                    } else {
                        let _ = std::fs::remove_file(&self.file);
                    }
                }
            }
            *slot = Some(Cached { overrides, unreadable });
        }
        slot.as_mut().unwrap()
    }

    fn state_of(&self, overrides: &Map<String, Value>) -> State {
        let value = (self.normalize)(&merged(&self.defaults, &Value::Object(overrides.clone())));
        State { value, is_customized: !overrides.is_empty(), defaults: self.defaults.clone(), customized_keys: overrides.keys().cloned().collect() }
    }

    pub fn read_state(&self) -> State {
        let mut g = self.cached.lock().unwrap();
        let c = self.load(&mut g);
        self.state_of(&c.overrides)
    }

    pub fn read(&self) -> Value {
        self.read_state().value
    }

    fn remove_file(&self) -> Result<(), String> {
        if self.file.exists() {
            std::fs::remove_file(&self.file).map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    /// `preserve_defaults`：补丁里值等于默认值的 key 也记成覆盖项（desktop 的 preserveDefaults）。
    pub fn write_patch(&self, patch: &Value, preserve_defaults: bool) -> Result<State, String> {
        let mut g = self.cached.lock().unwrap();
        let (unreadable, current_overrides) = {
            let c = self.load(&mut g);
            (c.unreadable, c.overrides.clone())
        };
        if unreadable {
            return Err("settings file is unreadable; refusing to overwrite it".into());
        }
        let current = self.state_of(&current_overrides).value;
        let next = (self.normalize)(&merged(&current, patch));
        let overrides = if let Some(m) = &self.merge {
            m(patch, &next, &self.defaults, current_overrides)
        } else {
            let mut o = current_overrides;
            for key in patch.as_object().map(|p| p.keys().cloned().collect::<Vec<_>>()).unwrap_or_default() {
                // 规范化后该 key 不存在（JS 里是 undefined，写盘时被丢弃）：不留覆盖项。
                let Some(nv) = next.get(&key).cloned() else {
                    o.shift_remove(&key);
                    continue;
                };
                if !preserve_defaults && Some(&nv) == self.defaults.get(&key) {
                    o.shift_remove(&key);
                } else {
                    o.insert(key, nv);
                }
            }
            o
        };
        if overrides.is_empty() {
            self.remove_file()?;
        } else {
            write_json_atomic(&self.file, &Value::Object(overrides.clone()), self.max_bytes)?;
        }
        *g = Some(Cached { overrides: overrides.clone(), unreadable: false });
        Ok(self.state_of(&overrides))
    }

    pub fn reset(&self) -> Result<Value, String> {
        let mut g = self.cached.lock().unwrap();
        self.remove_file()?;
        *g = Some(Cached { overrides: Map::new(), unreadable: false });
        Ok(self.state_of(&Map::new()).value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("override-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }
    fn store(file: PathBuf) -> OverrideFile {
        OverrideFile::new(file, json!({ "a": 1, "b": false }), |r| {
            json!({ "a": r["a"].as_i64().unwrap_or(1), "b": r["b"].as_bool().unwrap_or(false) })
        })
    }

    #[test]
    fn only_differences_are_stored_and_the_file_disappears_when_everything_is_default() {
        let d = dir("diff");
        let f = d.join("s.json");
        let s = store(f.clone());
        assert!(!s.read_state().is_customized);
        let st = s.write_patch(&json!({ "a": 5 }), false).unwrap();
        assert_eq!(st.value["a"], 5);
        assert_eq!(st.customized_keys, vec!["a".to_string()]);
        assert_eq!(read_json(&f, None).unwrap().unwrap(), json!({ "a": 5 }));
        // 改回默认值：覆盖项删掉，文件随之删除。
        let st = s.write_patch(&json!({ "a": 1 }), false).unwrap();
        assert!(!st.is_customized);
        assert!(!f.exists());
        // preserve_defaults：显式选了默认值也记下来。
        let st = s.write_patch(&json!({ "a": 1 }), true).unwrap();
        assert_eq!(st.customized_keys, vec!["a".to_string()]);
        assert!(f.exists());
        assert_eq!(s.reset().unwrap()["a"], 1);
        assert!(!f.exists());
    }

    #[test]
    fn customized_keys_keep_write_order_and_survive_a_reload() {
        let d = dir("order");
        let f = d.join("s.json");
        let s = store(f.clone());
        s.write_patch(&json!({ "b": true }), false).unwrap();
        s.write_patch(&json!({ "a": 9 }), false).unwrap();
        assert_eq!(s.read_state().customized_keys, vec!["b".to_string(), "a".to_string()]);
        let again = store(f);
        assert_eq!(again.read_state().customized_keys, vec!["b".to_string(), "a".to_string()]);
        assert_eq!(again.read()["a"], 9);
    }

    #[test]
    fn unreadable_files_are_deleted_by_default_or_preserved_and_protected_on_request() {
        let d = dir("bad");
        let f = d.join("s.json");
        std::fs::write(&f, "{not json").unwrap();
        assert_eq!(store(f.clone()).read()["a"], 1);
        assert!(!f.exists(), "默认：坏文件删掉回默认");

        std::fs::write(&f, "[1,2]").unwrap();
        let s = store(f.clone()).preserving_unreadable();
        assert_eq!(s.read()["a"], 1);
        assert!(s.write_patch(&json!({ "a": 2 }), false).unwrap_err().contains("unreadable"));
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "[1,2]", "保留原文件，不覆盖");
    }

    #[test]
    fn custom_merge_decides_the_stored_overrides() {
        let d = dir("merge");
        let s = store(d.join("s.json")).with_merge(|patch, next, _defaults, mut o| {
            if patch.get("a").is_some() {
                o.insert("a".into(), next["a"].clone());
            }
            o
        });
        // 等于默认值也会记下（自定义 merge 不做默认值比较）。
        assert!(s.write_patch(&json!({ "a": 1 }), false).unwrap().is_customized);
    }
}
