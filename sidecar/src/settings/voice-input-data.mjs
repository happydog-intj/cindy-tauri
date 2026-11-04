// 语音输入数据（设置 / 历史 / 词典）。对齐 desktop main/voice-input/VoiceInputDataStore.ts 的通道与返回形状。
// 和 desktop 的差别：没有跨设备词典同步（desktop 用 voiceDictionarySyncStore 的 CRDT 正本），
// 词典直接在本地 settings 上增删改；语音识别本身未迁移，readiness / 权限报告不可用。
import path from 'node:path';
import {
  MAX_VOICE_INPUT_DICTIONARY_ALIASES,
  MAX_VOICE_INPUT_DICTIONARY_ENTRIES,
  MAX_VOICE_INPUT_DICTIONARY_ENTRY_CHARS,
  applyVoiceInputDictionaryLearningActions,
  compactVoiceInputHistoryIfNeeded,
  createManualVoiceInputDictionaryEntry,
  createVoiceInputHistoryEntry,
  deleteVoiceInputDictionaryEntriesFromSettings,
  getNewAutomaticDictionaryEntries,
  mergeVoiceInputDictionaryCsvTerms,
  normalizeVoiceInputDataSnapshot,
  normalizeVoiceInputDictionaryEntryText,
  normalizeVoiceInputHistory,
  normalizeVoiceInputSettings,
} from '@cindy-desktop-shared/voiceInputData';
import { DEFAULT_VOICE_INPUT_PROVIDER_KIND } from '@cindy-desktop-shared/voiceInputAsrProfiles';
import { isSpeakerGateStrictness } from '@cindy/voice-input-core';
import { emitToRenderer, registerInvoke, registerSync } from '../ipc.mjs';
import { readJson, writeJsonAtomic } from './override-file.mjs';

const DATA_FILE_NAME = 'voice-input-data.v1.json';
const UNAVAILABLE = 'voice input is not available in the Tauri build yet';
const isRecord = (v) => !!v && typeof v === 'object' && !Array.isArray(v);
const clone = (v) => JSON.parse(JSON.stringify(v));

// desktop VoiceInputDataStore.ts:1021 stripDictionaryFields（词典只走语义化入口；开关只记 override）
function stripDictionaryFields(patch) {
  if (!isRecord(patch)) return {};
  const next = { ...patch };
  delete next.dictionaryEntries;
  delete next.dictionaryCandidates;
  delete next.suppressedAutomaticDictionaryTexts;
  if (typeof next.dictionarySyncEnabled === 'boolean' || next.dictionarySyncEnabled === null) {
    next.dictionarySyncEnabledOverride = next.dictionarySyncEnabled;
    delete next.dictionarySyncEnabled;
  }
  return next;
}
// desktop :1048 / :1065
function translateOverride(patch, field, valid) {
  if (!(field in patch)) return patch;
  const next = { ...patch };
  const value = next[field];
  delete next[field];
  if (valid(value)) next[`${field}Override`] = value;
  else if (value === null) next[`${field}Override`] = null;
  return next;
}

export function registerVoiceInputDataIpc({ settingsDir, platform = process.platform }) {
  const file = path.join(settingsDir, DATA_FILE_NAME);
  let state = null;

  const load = () => {
    if (state) return state;
    let raw;
    try {
      raw = readJson(file);
    } catch (err) {
      // desktop 同样按默认值继续（VoiceInputDataStore.ts:524），坏文件留在原处不覆盖，直到下一次写入
      process.stderr.write(`[voice-input] data read failed, using defaults: ${err.message}\n`);
    }
    const snapshot = normalizeVoiceInputDataSnapshot(raw ?? null, platform);
    state = { version: 1, legacyRendererStorageMigrated: isRecord(raw) && raw.legacyRendererStorageMigrated === true, ...snapshot };
    return state;
  };
  // desktop :961 projectVoiceInputDataForPersist：有效值字段不落盘，只存 *Override
  const save = (next) => {
    const settings = { ...next.settings };
    delete settings.composerLongPressEnabled;
    delete settings.dictionarySyncEnabled;
    delete settings.speakerFilterStrictness;
    writeJsonAtomic(file, { ...next, settings });
    state = next;
    emitToRenderer('voice-input:data-changed', clone({ settings: next.settings, history: next.history }));
  };
  const replaceSettings = (settings) => {
    const cur = load();
    save({ ...cur, settings: normalizeVoiceInputSettings(settings, platform) });
    return clone(load().settings);
  };
  const snapshot = () => clone({ settings: load().settings, history: load().history });

  registerSync('voice-input:data:get', () => snapshot());
  registerSync('voice-input:data:migrate-legacy', (payload) => {
    const cur = load();
    if (cur.legacyRendererStorageMigrated) return snapshot();
    let { settings, history } = cur;
    try {
      if (typeof payload?.settingsRaw === 'string') settings = normalizeVoiceInputSettings({ ...settings, ...JSON.parse(payload.settingsRaw) }, platform);
    } catch { /* 旧 localStorage 损坏时保留当前值 */ }
    try {
      if (typeof payload?.historyRaw === 'string') history = normalizeVoiceInputHistory(JSON.parse(payload.historyRaw));
    } catch { /* 同上 */ }
    save({ ...cur, settings, history, legacyRendererStorageMigrated: true });
    return snapshot();
  });
  registerInvoke('voice-input:settings:update', async (patch) => {
    const cur = load().settings;
    let p = stripDictionaryFields(patch);
    p = translateOverride(p, 'composerLongPressEnabled', (v) => typeof v === 'boolean');
    p = translateOverride(p, 'speakerFilterStrictness', isSpeakerGateStrictness);
    return replaceSettings({ ...cur, ...p });
  });

  // ── 词典 ──
  const entryIds = (input) => (Array.isArray(input) ? input.filter((x) => typeof x === 'string' && x).slice(0, MAX_VOICE_INPUT_DICTIONARY_ENTRIES) : []);
  const strList = (input, max) => (Array.isArray(input)
    ? input.filter((x) => typeof x === 'string').map((x) => x.slice(0, MAX_VOICE_INPUT_DICTIONARY_ENTRY_CHARS)).slice(0, max) : []);
  registerInvoke('voice-input:dictionary:delete-entries', async (ids) => {
    const cur = load().settings;
    const next = deleteVoiceInputDictionaryEntriesFromSettings(cur, entryIds(ids));
    return next === cur ? clone(cur) : replaceSettings(next);
  });
  registerInvoke('voice-input:dictionary:add-entry', async (text) => {
    const cur = load().settings;
    const entry = createManualVoiceInputDictionaryEntry(typeof text === 'string' ? text : '');
    if (!entry) return clone(cur);
    const key = entry.text.toLowerCase();
    if (cur.dictionaryEntries.some((e) => e.text.toLowerCase() === key)) return clone(cur);
    if (cur.dictionaryEntries.length >= MAX_VOICE_INPUT_DICTIONARY_ENTRIES) return clone(cur);
    return replaceSettings({ ...cur, dictionaryEntries: [...cur.dictionaryEntries, entry] });
  });
  registerInvoke('voice-input:dictionary:import-entries', async (texts) => {
    const cur = load().settings;
    const merged = mergeVoiceInputDictionaryCsvTerms(cur.dictionaryEntries, strList(texts, MAX_VOICE_INPUT_DICTIONARY_ENTRIES));
    return merged.importedCount === 0 ? clone(cur) : replaceSettings({ ...cur, dictionaryEntries: merged.entries });
  });
  const editEntry = (entryId, text, aliases) => {
    const cur = load().settings;
    const normalized = normalizeVoiceInputDictionaryEntryText(text);
    const entry = cur.dictionaryEntries.find((e) => e.id === entryId);
    if (!entry || !normalized) return clone(cur);
    const now = Date.now();
    const updated = {
      ...entry,
      text: normalized,
      updatedAt: now,
      ...(aliases ? { aliases: aliases.map((a) => normalizeVoiceInputDictionaryEntryText(a)).filter(Boolean).map((a) => ({ text: a, count: 1, lastSeenAt: now })) } : {}),
    };
    return replaceSettings({ ...cur, dictionaryEntries: cur.dictionaryEntries.map((e) => (e.id === entryId ? updated : e)) });
  };
  registerInvoke('voice-input:dictionary:rename-entry', async (payload) => {
    const entryId = typeof payload?.entryId === 'string' ? payload.entryId : '';
    if (!entryId) return clone(load().settings);
    return editEntry(entryId, typeof payload?.text === 'string' ? payload.text : '', null);
  });
  registerInvoke('voice-input:dictionary:edit-entry', async (payload) => {
    const entryId = typeof payload?.entryId === 'string' ? payload.entryId : '';
    if (!entryId) return clone(load().settings);
    const text = typeof payload?.text === 'string' ? payload.text.slice(0, MAX_VOICE_INPUT_DICTIONARY_ENTRY_CHARS) : '';
    return editEntry(entryId, text, strList(payload?.aliases, MAX_VOICE_INPUT_DICTIONARY_ALIASES));
  });
  registerInvoke('voice-input:dictionary-learning:record-actions', async (actions) => {
    const cur = load().settings;
    const list = Array.isArray(actions)
      ? actions.filter((a) => isRecord(a) && typeof a.action === 'string' && typeof a.term === 'string'
        && a.term.trim() && a.term.trim().length <= MAX_VOICE_INPUT_DICTIONARY_ENTRY_CHARS)
      : [];
    const next = applyVoiceInputDictionaryLearningActions(cur, list);
    if (next === cur) return { settings: clone(cur), newAutomaticEntries: [] };
    const newAutomaticEntries = getNewAutomaticDictionaryEntries(cur.dictionaryEntries, next.dictionaryEntries);
    return { settings: replaceSettings(next), newAutomaticEntries };
  });

  // ── 历史 ── desktop :345-387，sync 通道
  registerSync('voice-input:history:get', (limit) => {
    const h = load().history;
    return clone(typeof limit === 'number' ? h.slice(0, Math.max(0, limit)) : h);
  });
  registerSync('voice-input:history:get-for-refinement', () => {
    const cur = load();
    const compacted = compactVoiceInputHistoryIfNeeded(cur.history);
    if (compacted !== cur.history) save({ ...cur, history: compacted });
    return clone(compacted);
  });
  registerSync('voice-input:history:record', (text) => {
    const t = typeof text === 'string' ? text.trim() : '';
    if (!t) return null;
    const cur = load();
    const dup = cur.history.find((e) => e.text === t);
    if (dup) return dup.id;
    const entry = createVoiceInputHistoryEntry(t);
    if (!entry) return null;
    save({ ...cur, history: compactVoiceInputHistoryIfNeeded([entry, ...cur.history]) });
    return entry.id;
  });
  registerSync('voice-input:history:update', (payload) => {
    const id = typeof payload?.id === 'string' ? payload.id : '';
    const t = typeof payload?.text === 'string' ? payload.text.trim() : '';
    const cur = load();
    const entry = id && t ? cur.history.find((e) => e.id === id) : null;
    if (entry) save({ ...cur, history: normalizeVoiceInputHistory([{ ...entry, text: t }, ...cur.history.filter((e) => e.id !== id)]) });
    return true;
  });
  registerSync('voice-input:history:delete', (id) => {
    const cur = load();
    const history = cur.history.filter((e) => e.id !== id);
    if (history.length !== cur.history.length) save({ ...cur, history });
    return true;
  });

  // ── 识别能力：未迁移，报告不可用（形状对齐 desktop index.ts:1500 / :228）──
  const notRequired = platform !== 'darwin';
  const perm = (name) => (notRequired && name !== 'microphone'
    ? { ok: true, status: 'not-required' }
    : { ok: false, status: 'unknown', error: UNAVAILABLE });
  const permissions = () => ({ microphone: perm('microphone'), inputMonitoring: perm('inputMonitoring'), accessibility: perm('accessibility') });
  registerSync('voice-input:get-system-permissions-cached', () => permissions());
  registerInvoke('voice-input:get-system-permissions', async () => permissions());
  const readiness = () => ({
    ok: false, serviceMode: 'byok', provider: DEFAULT_VOICE_INPUT_PROVIDER_KIND, providerModel: '', auth: 'api-key',
    settingsTab: 'providers', error: UNAVAILABLE,
  });
  registerSync('voice-input:get-readiness-cached', () => readiness());
  registerInvoke('voice-input:get-readiness', async () => readiness());
  registerInvoke('voice-input:cancel', async () => ({ ok: true }));
}
