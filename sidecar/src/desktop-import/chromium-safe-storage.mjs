// 解 Electron safeStorage（Chromium os_crypt）写出的 .enc 文件，只用于从 desktop 版导入 API key。
// 文件内容是 base64(密文)，密文以 'v10' 开头：
// - macOS：钥匙串 "<app> Safe Storage" 里的口令 → PBKDF2-SHA1(salt 'saltysalt', 1003 次, 16 字节)
//   → AES-128-CBC，IV 是 16 个空格（Chromium os_crypt_mac.mm）；
// - Windows：Local State 里 os_crypt.encrypted_key 经 DPAPI 解出的 32 字节 key → AES-256-GCM，
//   v10 后 12 字节 nonce，末尾 16 字节 tag（Chromium os_crypt_win.cc）。
// 钥匙串口令 / DPAPI 解包由 Rust 做（native desktopImport.safeStorageKey），这里只做对称解密。
// 明文只在内存里，不写日志。
import { createDecipheriv, pbkdf2Sync } from 'node:crypto';

const PREFIX = Buffer.from('v10');
// desktop main/secrets/chunkedSecretFile.ts：大 secret 的分块格式，provider key 不会用到。
const CHUNKED_HEADER = 'CINDY_COMPANION_SECRET_V2';

/** Rust 回来的 key 材料 → 解密函数。无法识别时返回 null。 */
export function createDecryptor(material) {
  if (!material || typeof material !== 'object') return null;
  if (material.scheme === 'mac-v10' && typeof material.password === 'string' && material.password) {
    const key = pbkdf2Sync(material.password, 'saltysalt', 1003, 16, 'sha1');
    return (blob) => {
      if (blob.length <= PREFIX.length || !blob.subarray(0, 3).equals(PREFIX)) return null;
      const d = createDecipheriv('aes-128-cbc', key, Buffer.alloc(16, 0x20));
      return Buffer.concat([d.update(blob.subarray(3)), d.final()]).toString('utf8');
    };
  }
  if (material.scheme === 'win-v10' && typeof material.key === 'string' && /^[0-9a-f]{64}$/.test(material.key)) {
    const key = Buffer.from(material.key, 'hex');
    return (blob) => {
      if (blob.length < 3 + 12 + 16 || !blob.subarray(0, 3).equals(PREFIX)) return null;
      const nonce = blob.subarray(3, 15);
      const tag = blob.subarray(blob.length - 16);
      const d = createDecipheriv('aes-256-gcm', key, nonce);
      d.setAuthTag(tag);
      return Buffer.concat([d.update(blob.subarray(15, blob.length - 16)), d.final()]).toString('utf8');
    };
  }
  return null;
}

/** 读一个 .enc 文件的文本内容并解密；格式不对或解密失败返回 null。 */
export function decryptEncFile(text, decrypt) {
  if (typeof text !== 'string' || !text.trim() || text.startsWith(CHUNKED_HEADER)) return null;
  try {
    return decrypt(Buffer.from(text.trim(), 'base64'));
  } catch {
    return null;
  }
}
