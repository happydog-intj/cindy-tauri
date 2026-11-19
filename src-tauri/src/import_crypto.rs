//! 解 Electron safeStorage（Chromium os_crypt v10）写出的 .enc 文件，只用于从桌面版导入 API key。
//! 文件内容是 base64(密文)，密文以 "v10" 开头：
//! - macOS：钥匙串 "<app> Safe Storage" 里的口令 → PBKDF2-SHA1(salt "saltysalt", 1003 次, 16 字节) → AES-128-CBC，
//!   IV 是 16 个空格（Chromium os_crypt_mac.mm）；
//! - Windows：Local State 里 os_crypt.encrypted_key 经 DPAPI 解出的 32 字节 key → AES-256-GCM，
//!   v10 后 12 字节 nonce，末尾 16 字节 tag（Chromium os_crypt_win.cc）。
//! 钥匙串口令 / DPAPI 解包在 desktop_import.rs，这里只做对称解密。明文只在内存里，不写日志。

use aes::cipher::{block_padding::Pkcs7, BlockDecryptMut, KeyIvInit};
use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::Engine;
use serde_json::Value;

const PREFIX: &[u8] = b"v10";
/// desktop main/secrets/chunkedSecretFile.ts：大 secret 的分块格式，provider key 不会用到。
const CHUNKED_HEADER: &str = "CINDY_COMPANION_SECRET_V2";

pub enum Decryptor {
    MacV10 { key: [u8; 16] },
    WinV10 { key: [u8; 32] },
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 || !s.is_ascii() {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok()).collect()
}

impl Decryptor {
    /// Rust 钥匙串 / DPAPI 那边给出的 key 材料 → 解密器。无法识别返回 None。
    pub fn from_material(material: &Value) -> Option<Decryptor> {
        match material.get("scheme").and_then(Value::as_str)? {
            "mac-v10" => {
                let password = material.get("password").and_then(Value::as_str).filter(|p| !p.is_empty())?;
                let mut key = [0u8; 16];
                pbkdf2::pbkdf2_hmac::<sha1::Sha1>(password.as_bytes(), b"saltysalt", 1003, &mut key);
                Some(Decryptor::MacV10 { key })
            }
            "win-v10" => {
                let hex = material.get("key").and_then(Value::as_str).filter(|k| k.len() == 64 && k.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()))?;
                let bytes = unhex(hex)?;
                Some(Decryptor::WinV10 { key: bytes.try_into().ok()? })
            }
            _ => None,
        }
    }

    fn decrypt(&self, blob: &[u8]) -> Option<String> {
        if blob.len() <= PREFIX.len() || &blob[..3] != PREFIX {
            return None;
        }
        match self {
            Decryptor::MacV10 { key } => {
                let iv = [0x20u8; 16];
                let dec = cbc::Decryptor::<aes::Aes128>::new(key.into(), &iv.into());
                let plain = dec.decrypt_padded_vec_mut::<Pkcs7>(&blob[3..]).ok()?;
                String::from_utf8(plain).ok()
            }
            Decryptor::WinV10 { key } => {
                if blob.len() < 3 + 12 + 16 {
                    return None;
                }
                let cipher = Aes256Gcm::new(key.into());
                // aes-gcm 要求 密文||tag，文件里正是这个顺序。
                let plain = cipher.decrypt(Nonce::from_slice(&blob[3..15]), &blob[15..]).ok()?;
                String::from_utf8(plain).ok()
            }
        }
    }

    /// 读一个 .enc 文件的文本内容并解密；格式不对或解密失败返回 None。
    pub fn decrypt_enc_text(&self, text: &str) -> Option<String> {
        let t = text.trim();
        if t.is_empty() || text.starts_with(CHUNKED_HEADER) {
            return None;
        }
        let blob = base64::engine::general_purpose::STANDARD.decode(t).ok()?;
        self.decrypt(&blob)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // 参考向量由 Node crypto 生成（与 desktop 端 Electron 的输出格式一致），不是本模块自己加密出来的。
    const MAC_PW: &str = "vector-password-Zk3";
    const MAC_ENC: &str = "djEwa/+7n0XBGB+8M/hSbtmdaiCJGZpV5dyNIX9l1JZIWgE=";
    const WIN_KEY: &str = "00070e151c232a31383f464d545b626970777e858c939aa1a8afb6bdc4cbd2d9";
    const WIN_ENC: &str = "djEwAQIDBAUGBwgJCgsMtMpolntl4PMrd3+76PLH70bqb3pJR6tnaH8f6mp6nom0";

    #[test]
    fn decrypts_what_electron_wrote_on_both_platforms() {
        let mac = Decryptor::from_material(&json!({ "scheme": "mac-v10", "password": MAC_PW })).unwrap();
        assert_eq!(mac.decrypt_enc_text(MAC_ENC).as_deref(), Some("sk-vector-mac-123"));
        assert_eq!(mac.decrypt_enc_text(&format!("{MAC_ENC}\n")).as_deref(), Some("sk-vector-mac-123"), "文件末尾换行");
        let win = Decryptor::from_material(&json!({ "scheme": "win-v10", "key": WIN_KEY })).unwrap();
        assert_eq!(win.decrypt_enc_text(WIN_ENC).as_deref(), Some("sk-vector-win-456"));
    }

    #[test]
    fn wrong_keys_and_damaged_files_give_nothing_instead_of_garbage() {
        let bad_mac = Decryptor::from_material(&json!({ "scheme": "mac-v10", "password": "other" })).unwrap();
        assert_eq!(bad_mac.decrypt_enc_text(MAC_ENC), None);
        let win = Decryptor::from_material(&json!({ "scheme": "win-v10", "key": WIN_KEY })).unwrap();
        // 篡改一个字节：GCM 校验失败
        let mut raw = base64::engine::general_purpose::STANDARD.decode(WIN_ENC).unwrap();
        raw[20] ^= 1;
        assert_eq!(win.decrypt_enc_text(&base64::engine::general_purpose::STANDARD.encode(raw)), None);
        // 平台不对的文件、分块格式、空文件、非 base64
        let mac = Decryptor::from_material(&json!({ "scheme": "mac-v10", "password": MAC_PW })).unwrap();
        assert_eq!(mac.decrypt_enc_text(WIN_ENC), None);
        assert_eq!(mac.decrypt_enc_text("CINDY_COMPANION_SECRET_V2\nxx"), None);
        assert_eq!(mac.decrypt_enc_text("  \n"), None);
        assert_eq!(mac.decrypt_enc_text("%%%not base64%%%"), None);
        assert_eq!(mac.decrypt_enc_text("YWJj"), None, "没有 v10 前缀");
    }

    #[test]
    fn unusable_key_material_is_rejected() {
        for m in [
            json!(null),
            json!({ "scheme": "mac-v10" }),
            json!({ "scheme": "mac-v10", "password": "" }),
            json!({ "scheme": "win-v10", "key": "abcd" }),
            json!({ "scheme": "win-v10", "key": WIN_KEY.to_uppercase() }),
            json!({ "scheme": "linux-v11", "password": "x" }),
        ] {
            assert!(Decryptor::from_material(&m).is_none(), "{m}");
        }
    }
}
