//! SQLite 存储：用户、令牌（只存哈希）、设备。同步访问，自用规模足够。
use rusqlite::{params, Connection, OptionalExtension};
use sha2::{Digest, Sha256};
use std::sync::Mutex;

pub struct Db(Mutex<Connection>);

pub fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

pub fn hash(token: &str) -> String {
    Sha256::digest(token.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

pub fn random_hex(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    getrandom::fill(&mut buf).expect("系统随机数不可用");
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

/// 校验过的访问令牌对应的身份。
#[derive(Clone, Debug)]
pub struct Identity {
    pub user_id: String,
    pub email: String,
    pub device_id: String,
}

#[derive(Clone, Debug)]
pub struct DeviceRow {
    pub device_id: String,
    pub name: String,
    pub platform: String,
    pub app_version: String,
    pub last_seen: i64,
}

impl Db {
    pub fn open(path: &str) -> Result<Self, String> {
        let c = Connection::open(path).map_err(|e| e.to_string())?;
        c.execute_batch(
            "PRAGMA journal_mode = WAL;
             CREATE TABLE IF NOT EXISTS users (id TEXT PRIMARY KEY, email TEXT UNIQUE NOT NULL, created_at INTEGER NOT NULL);
             CREATE TABLE IF NOT EXISTS access_tokens (hash TEXT PRIMARY KEY, user_id TEXT NOT NULL, device_id TEXT NOT NULL, exp INTEGER NOT NULL);
             CREATE TABLE IF NOT EXISTS refresh_tokens (hash TEXT PRIMARY KEY, user_id TEXT NOT NULL, device_id TEXT NOT NULL, created_at INTEGER NOT NULL);
             CREATE TABLE IF NOT EXISTS devices (user_id TEXT NOT NULL, device_id TEXT NOT NULL, name TEXT NOT NULL DEFAULT '', platform TEXT NOT NULL DEFAULT '',
               app_version TEXT NOT NULL DEFAULT '', last_seen INTEGER NOT NULL DEFAULT 0, PRIMARY KEY (user_id, device_id));",
        )
        .map_err(|e| e.to_string())?;
        Ok(Self(Mutex::new(c)))
    }

    /// 邮箱对应的用户 id，不存在则创建。
    pub fn user_for_email(&self, email: &str) -> Result<String, String> {
        let c = self.0.lock().unwrap();
        if let Some(id) = c.query_row("SELECT id FROM users WHERE email = ?1", [email], |r| r.get::<_, String>(0)).optional().map_err(|e| e.to_string())? {
            return Ok(id);
        }
        let id = format!("u-{}", random_hex(8));
        c.execute("INSERT INTO users (id, email, created_at) VALUES (?1, ?2, ?3)", params![id, email, now_ms()]).map_err(|e| e.to_string())?;
        Ok(id)
    }

    /// 签发一对令牌：同一用户同一设备只保留最新的刷新令牌（轮换后旧的失效）。
    pub fn issue(&self, user_id: &str, device_id: &str, access: &str, exp_ms: i64, refresh: &str) -> Result<(), String> {
        let c = self.0.lock().unwrap();
        let now = now_ms();
        c.execute("DELETE FROM access_tokens WHERE exp < ?1", [now]).map_err(|e| e.to_string())?;
        c.execute("DELETE FROM refresh_tokens WHERE user_id = ?1 AND device_id = ?2", params![user_id, device_id]).map_err(|e| e.to_string())?;
        c.execute("INSERT INTO access_tokens (hash, user_id, device_id, exp) VALUES (?1, ?2, ?3, ?4)", params![hash(access), user_id, device_id, exp_ms])
            .map_err(|e| e.to_string())?;
        c.execute("INSERT INTO refresh_tokens (hash, user_id, device_id, created_at) VALUES (?1, ?2, ?3, ?4)", params![hash(refresh), user_id, device_id, now])
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn identity_for_access(&self, token: &str) -> Option<Identity> {
        let c = self.0.lock().unwrap();
        c.query_row(
            "SELECT t.user_id, u.email, t.device_id FROM access_tokens t JOIN users u ON u.id = t.user_id WHERE t.hash = ?1 AND t.exp > ?2",
            params![hash(token), now_ms()],
            |r| Ok(Identity { user_id: r.get(0)?, email: r.get(1)?, device_id: r.get(2)? }),
        )
        .optional()
        .ok()
        .flatten()
    }

    /// 消耗一枚刷新令牌（一次性）：返回它所属的身份，同时删除，调用方随后签发新的一对。
    pub fn take_refresh(&self, token: &str) -> Option<Identity> {
        let c = self.0.lock().unwrap();
        let h = hash(token);
        let id = c
            .query_row(
                "SELECT t.user_id, u.email, t.device_id FROM refresh_tokens t JOIN users u ON u.id = t.user_id WHERE t.hash = ?1",
                [&h],
                |r| Ok(Identity { user_id: r.get(0)?, email: r.get(1)?, device_id: r.get(2)? }),
            )
            .optional()
            .ok()
            .flatten()?;
        c.execute("DELETE FROM refresh_tokens WHERE hash = ?1", [&h]).ok()?;
        Some(id)
    }

    /// 登出：撤销该用户该设备的全部令牌。
    pub fn revoke(&self, user_id: &str, device_id: &str) {
        let c = self.0.lock().unwrap();
        let _ = c.execute("DELETE FROM access_tokens WHERE user_id = ?1 AND device_id = ?2", params![user_id, device_id]);
        let _ = c.execute("DELETE FROM refresh_tokens WHERE user_id = ?1 AND device_id = ?2", params![user_id, device_id]);
    }

    pub fn upsert_device(&self, user_id: &str, device_id: &str, name: &str, platform: &str, app_version: &str) {
        let c = self.0.lock().unwrap();
        let _ = c.execute(
            "INSERT INTO devices (user_id, device_id, name, platform, app_version, last_seen) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(user_id, device_id) DO UPDATE SET name = ?3, platform = ?4, app_version = ?5, last_seen = ?6",
            params![user_id, device_id, name, platform, app_version, now_ms()],
        );
    }

    pub fn touch_device(&self, user_id: &str, device_id: &str) {
        let c = self.0.lock().unwrap();
        let _ = c.execute("UPDATE devices SET last_seen = ?3 WHERE user_id = ?1 AND device_id = ?2", params![user_id, device_id, now_ms()]);
    }

    pub fn devices(&self, user_id: &str) -> Vec<DeviceRow> {
        let c = self.0.lock().unwrap();
        let Ok(mut st) = c.prepare("SELECT device_id, name, platform, app_version, last_seen FROM devices WHERE user_id = ?1 ORDER BY last_seen DESC") else {
            return vec![];
        };
        st.query_map([user_id], |r| Ok(DeviceRow { device_id: r.get(0)?, name: r.get(1)?, platform: r.get(2)?, app_version: r.get(3)?, last_seen: r.get(4)? }))
            .map(|rows| rows.flatten().collect())
            .unwrap_or_default()
    }
}
