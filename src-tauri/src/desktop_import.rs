// 从 Cindy 桌面版导入：取 Electron safeStorage 的密钥材料，交给 sidecar 解 .enc 文件。
// - macOS：钥匙串通用密码条目 service "Cindy Safe Storage" / account "Cindy Key"（Chromium os_crypt_mac，
//   条目名由 app.name 'Cindy' 派生）。首次读取 macOS 会弹授权框，用户拒绝时返回错误，只导入不含 key 的配置；
// - Windows：sidecar 读 profile 的 Local State，把 os_crypt.encrypted_key（去掉 "DPAPI" 前缀）以 hex 传来，
//   这里用 CryptUnprotectData 解出 32 字节 AES-GCM key（与桌面版同一 Windows 用户才能解开）。
// 只读，不写钥匙串，也不缓存材料。
use serde_json::{json, Value};

#[cfg(target_os = "macos")]
const MAC_SERVICE: &str = "Cindy Safe Storage";
#[cfg(target_os = "macos")]
const MAC_ACCOUNT: &str = "Cindy Key";

#[cfg(target_os = "macos")]
pub fn safe_storage_key(_params: &Value) -> Result<Value, String> {
    // 冒烟 / 测试不读真实钥匙串（开发机上就是用户自己的桌面版条目）。
    if std::env::var("CINDY_TAURI_SMOKE").as_deref() == Ok("1") {
        return Err("[UNSUPPORTED_CAPABILITY] desktop keychain is not read in smoke mode".into());
    }
    let entry = keyring::Entry::new(MAC_SERVICE, MAC_ACCOUNT).map_err(|e| format!("keychain entry: {e}"))?;
    match entry.get_password() {
        Ok(password) if !password.is_empty() => Ok(json!({ "scheme": "mac-v10", "password": password })),
        Ok(_) => Err("[NOT_FOUND] desktop safe storage key is empty".into()),
        Err(keyring::Error::NoEntry) => Err("[NOT_FOUND] desktop safe storage key not found".into()),
        Err(e) => Err(format!("[PERMISSION_DENIED] reading desktop safe storage key failed: {e}")),
    }
}

#[cfg(windows)]
pub fn safe_storage_key(params: &Value) -> Result<Value, String> {
    let hex = params.get("encryptedKey").and_then(Value::as_str).unwrap_or("");
    let blob = decode_hex(hex).ok_or("[INVALID_PARAMS] encryptedKey must be hex")?;
    if blob.is_empty() || blob.len() > 4096 {
        return Err("[INVALID_PARAMS] encryptedKey has an unexpected size".into());
    }
    let key = dpapi_unprotect(&blob)?;
    if key.len() != 32 {
        return Err("[INVALID_STATE] desktop safe storage key has an unexpected size".into());
    }
    let hex: String = key.iter().map(|b| format!("{b:02x}")).collect();
    Ok(json!({ "scheme": "win-v10", "key": hex }))
}

#[cfg(not(any(target_os = "macos", windows)))]
pub fn safe_storage_key(_params: &Value) -> Result<Value, String> {
    Err("[UNSUPPORTED_CAPABILITY] desktop import is not supported on this platform".into())
}

#[cfg(windows)]
fn decode_hex(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok()).collect()
}

#[cfg(windows)]
fn dpapi_unprotect(data: &[u8]) -> Result<Vec<u8>, String> {
    use windows::Win32::Foundation::{LocalFree, HLOCAL};
    use windows::Win32::Security::Cryptography::{CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB};
    let input = CRYPT_INTEGER_BLOB { cbData: data.len() as u32, pbData: data.as_ptr() as *mut u8 };
    let mut output = CRYPT_INTEGER_BLOB::default();
    // SAFETY：input 指向调用期间有效的切片；output 由系统分配，复制后用 LocalFree 释放。
    unsafe {
        CryptUnprotectData(&input, None, None, None, None, CRYPTPROTECT_UI_FORBIDDEN, &mut output)
            .map_err(|e| format!("[PERMISSION_DENIED] DPAPI could not unlock the desktop key: {e}"))?;
        let out = std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec();
        let _ = LocalFree(Some(HLOCAL(output.pbData as *mut core::ffi::c_void)));
        Ok(out)
    }
}

// Windows CI 跑：用 CryptProtectData 造一个和 Chromium 同类的 DPAPI 包，确认解包得到原 key。
#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use windows::Win32::Foundation::{LocalFree, HLOCAL};
    use windows::Win32::Security::Cryptography::{CryptProtectData, CRYPT_INTEGER_BLOB};

    fn protect(data: &[u8]) -> Vec<u8> {
        let input = CRYPT_INTEGER_BLOB { cbData: data.len() as u32, pbData: data.as_ptr() as *mut u8 };
        let mut output = CRYPT_INTEGER_BLOB::default();
        unsafe {
            CryptProtectData(&input, None, None, None, None, 0, &mut output).expect("CryptProtectData");
            let out = std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec();
            let _ = LocalFree(Some(HLOCAL(output.pbData as *mut core::ffi::c_void)));
            out
        }
    }

    #[test]
    fn desktop_import_dpapi_roundtrip() {
        let key: Vec<u8> = (0u8..32).collect();
        let hex: String = protect(&key).iter().map(|b| format!("{b:02x}")).collect();
        let v = safe_storage_key(&json!({ "encryptedKey": hex })).expect("unwrap");
        assert_eq!(v["scheme"], "win-v10");
        assert_eq!(v["key"], key.iter().map(|b| format!("{b:02x}")).collect::<String>());
        assert!(safe_storage_key(&json!({ "encryptedKey": "zz" })).is_err());
        assert!(safe_storage_key(&json!({ "encryptedKey": "00ff" })).is_err());
    }
}
