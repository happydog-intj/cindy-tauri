// 主密钥：32 字节随机值，存系统钥匙串（macOS Keychain / Windows 凭据管理器）。
// sidecar 用它加密 <dataDir>/secrets.v1.json 里的 API key；主密钥只经 stdin RPC 交给 sidecar，不进环境变量。
// 冒烟模式用单独的 service 和按数据目录区分的 account，verify 阶段结束时删掉，避免在钥匙串里留垃圾。
use keyring::{Entry, Error};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::Path;

const SERVICE: &str = "com.cindy.tauri.p1";
const SMOKE_SERVICE: &str = "com.cindy.tauri.p1.smoke";
const ACCOUNT: &str = "secret-store-master-key";

fn entry(smoke: bool, data_dir: &Path) -> Result<Entry, Error> {
    if smoke {
        let mut h = DefaultHasher::new();
        data_dir.to_string_lossy().hash(&mut h);
        Entry::new(SMOKE_SERVICE, &format!("{ACCOUNT}-{:016x}", h.finish()))
    } else {
        Entry::new(SERVICE, ACCOUNT)
    }
}

fn is_hex_key(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn random_hex() -> Result<String, String> {
    let mut buf = [0u8; 32];
    getrandom::fill(&mut buf).map_err(|e| format!("random source failed: {e}"))?;
    Ok(buf.iter().map(|b| format!("{b:02x}")).collect())
}

/// 读主密钥；没有就新建。已存在但格式不对时报错，绝不覆盖（覆盖会让已存 key 永久无法解密）。
pub fn load_or_create(smoke: bool, data_dir: &Path) -> Result<String, String> {
    let e = entry(smoke, data_dir).map_err(|e| format!("keychain entry: {e}"))?;
    match e.get_password() {
        Ok(v) if is_hex_key(&v) => Ok(v),
        Ok(_) => Err("keychain master key has an unexpected format".into()),
        Err(Error::NoEntry) => {
            let key = random_hex()?;
            e.set_password(&key).map_err(|e| format!("keychain write: {e}"))?;
            Ok(key)
        }
        Err(err) => Err(format!("keychain read: {err}")),
    }
}

/// 冒烟 verify 阶段结束时清掉本次用的钥匙串条目。
pub fn delete_smoke_entry(data_dir: &Path) {
    if let Ok(e) = entry(true, data_dir) {
        match e.delete_credential() {
            Ok(()) | Err(Error::NoEntry) => {}
            Err(err) => eprintln!("[secrets] smoke keychain cleanup failed: {err}"),
        }
    }
}
