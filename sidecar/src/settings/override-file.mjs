// 「只存用户覆盖项」的设置文件，最小重写自 desktop main/maker-host/override-settings-file.ts。
// - 文件里只放和默认值不同的 key；全部回到默认时删除文件（同 desktop writeOverrides → reset）。
// - 写盘为 tmp + rename；全部用同步 fs，单线程下天然串行，不会交错写同一文件。
// - 读失败：preserveUnreadableFile=true 时保留原文件并拒绝覆盖写（同 desktop readWritableState），
//   否则删掉坏文件回默认。
import { existsSync, mkdirSync, readFileSync, renameSync, statSync, unlinkSync, writeFileSync } from 'node:fs';
import path from 'node:path';

const isPlainObject = (v) => !!v && typeof v === 'object' && !Array.isArray(v);
const clone = (v) => (v === undefined ? v : JSON.parse(JSON.stringify(v)));
const isEqual = (a, b) => JSON.stringify(a) === JSON.stringify(b);

/** 原子写 JSON：先写同目录 tmp 再 rename。 */
export function writeJsonAtomic(file, value, maxBytes) {
  const text = JSON.stringify(value, null, 2);
  if (maxBytes !== undefined && Buffer.byteLength(text, 'utf8') > maxBytes) {
    throw new Error(`file exceeds ${maxBytes} byte limit`);
  }
  mkdirSync(path.dirname(file), { recursive: true });
  const tmp = `${file}.${process.pid}.tmp`;
  try {
    writeFileSync(tmp, text, 'utf8');
    renameSync(tmp, file);
  } catch (err) {
    try { unlinkSync(tmp); } catch { /* 无 tmp */ }
    throw err;
  }
}

/** 读 JSON；文件不存在返回 undefined，超限或解析失败抛错。 */
export function readJson(file, maxBytes) {
  if (!existsSync(file)) return undefined;
  if (maxBytes !== undefined && statSync(file).size > maxBytes) throw new Error(`file exceeds ${maxBytes} byte limit`);
  return JSON.parse(readFileSync(file, 'utf8'));
}

/**
 * options: { file, defaults, normalize, mergeOverrides?, maxBytes?, preserveUnreadableFile? }
 * 返回 { read, readState, writePatch, reset }，语义同 desktop OverrideSettingsFile 的同名方法。
 */
export function createOverrideSettingsFile(options) {
  const defaults = () => clone(typeof options.defaults === 'function' ? options.defaults() : options.defaults);
  let cached = null;

  function load() {
    if (cached) return cached;
    let overrides = {};
    let unreadable = false;
    try {
      const parsed = readJson(options.file, options.maxBytes);
      if (parsed !== undefined) {
        if (options.preserveUnreadableFile && !isPlainObject(parsed)) throw new Error('settings file root must be an object');
        overrides = isPlainObject(parsed) ? parsed : {};
      }
    } catch (err) {
      process.stderr.write(`[settings] ${path.basename(options.file)} read failed, using defaults: ${err?.message ?? err}\n`);
      if (options.preserveUnreadableFile) unreadable = true;
      else try { unlinkSync(options.file); } catch { /* 已不存在 */ }
      overrides = {};
    }
    cached = { overrides, unreadable };
    return cached;
  }

  function readState() {
    const { overrides } = load();
    return {
      value: options.normalize({ ...defaults(), ...overrides }),
      isCustomized: Object.keys(overrides).length > 0,
      defaults: defaults(),
      customizedKeys: Object.keys(overrides),
    };
  }

  function writeOverrides(overrides) {
    if (Object.keys(overrides).length === 0) {
      reset();
      return;
    }
    writeJsonAtomic(options.file, overrides, options.maxBytes);
    cached = { overrides, unreadable: false };
  }

  function writePatch(patch, writeOptions = {}) {
    const state = load();
    if (state.unreadable) throw new Error('settings file is unreadable; refusing to overwrite it');
    const current = readState();
    const next = options.normalize({ ...current.value, ...patch });
    const d = defaults();
    let overrides;
    if (options.mergeOverrides) {
      overrides = options.mergeOverrides({ patch, next, defaults: d, overrides: { ...state.overrides } });
    } else {
      overrides = { ...state.overrides };
      for (const key of Object.keys(patch)) {
        if (!writeOptions.preserveDefaults && isEqual(next[key], d[key])) delete overrides[key];
        else overrides[key] = next[key];
      }
    }
    writeOverrides(overrides);
    return readState();
  }

  function reset() {
    try {
      if (existsSync(options.file)) unlinkSync(options.file);
    } catch (err) {
      throw err;
    }
    cached = { overrides: {}, unreadable: false };
    return readState().value;
  }

  return { read: () => readState().value, readState, writePatch, reset };
}

/** desktop 各 *Wire()：{...value, isCustomized, customizedKeys, defaults}。 */
export function overrideWire(state) {
  return { ...state.value, isCustomized: state.isCustomized, customizedKeys: state.customizedKeys, defaults: state.defaults };
}
