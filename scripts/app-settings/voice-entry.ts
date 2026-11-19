// 语音输入数据的黄金样例：固定时钟 + 固定 id 序列下运行上游 voiceInputData.ts，输出（输入, 输出）对。
import {
  applyVoiceInputDictionaryLearningActions, compactVoiceInputHistoryIfNeeded, createManualVoiceInputDictionaryEntry,
  createVoiceInputHistoryEntry, deleteVoiceInputDictionaryEntriesFromSettings, getNewAutomaticDictionaryEntries,
  mergeVoiceInputDictionaryCsvTerms, normalizeVoiceInputDataSnapshot, normalizeVoiceInputDictionaryEntryText,
  normalizeVoiceInputHistory, normalizeVoiceInputSettings,
} from '@cindy-desktop-shared/voiceInputData';
import { DEFAULT_VOICE_INPUT_PROVIDER_KIND } from '@cindy-desktop-shared/voiceInputAsrProfiles';

const NOW = 1_700_000_000_000;
let idSeq = 0;
const realNow = Date.now;
const g = globalThis as any;
function fix() {
  idSeq = 0;
  Date.now = () => NOW;
  Object.defineProperty(g, 'crypto', { configurable: true, value: { randomUUID: () => `id-${++idSeq}` } });
}
const run = <T>(f: () => T): T => { fix(); try { return f(); } finally { Date.now = realNow; } };

const PLATFORMS = ['darwin', 'win32', 'linux'];
const H = (n: number) => 'x'.repeat(n);
const entry = (id: string, text: string, extra: any = {}) => ({ id, text, source: 'manual', frequency: 1, aliases: [], createdAt: 5, updatedAt: 6, ...extra });
const settingsInputs: any[] = [
  null, 3, [], {}, { language: 'ja' }, { language: 'es' }, { language: 'xx' }, { language: 'zh-Hans-x-lzh' },
  { microphoneDeviceId: '  ', muteSystemAudio: false, playInteractionSound: 'x', playStartSound: false, fastActivationEnabled: true, refinementEnabled: false },
  { refinementInstructions: '  ' }, { refinementInstructions: 'a\r\nb' + H(1100) }, { refinementInstructions: 5 },
  { dictionarySyncEnabledOverride: false }, { dictionarySyncEnabledOverride: null, dictionarySyncEnabled: false },
  { dictionarySyncEnabled: false }, { dictionarySyncEnabled: true },
  { composerLongPressEnabledOverride: false, composerLongPressEnabled: false }, { composerLongPressEnabled: false },
  { speakerFilterEnabled: true, speakerFilterStrictnessOverride: 'strict' }, { speakerFilterStrictnessOverride: 'weird', speakerFilterStrictness: 'strict' },
  { shortcut: null }, { shortcut: { code: 'KeyA', modifiers: { meta: 1, fn: 'y' } } }, { shortcut: { code: 'Fn', trigger: 'modifier' } },
  { shortcut: { code: 'KeyA', trigger: 'modifier' } }, { shortcut: { code: '', modifiers: {} } }, { shortcut: { code: 'KeyA' } },
  { shortcut: { code: 'F5', key: 7, modifiers: {} } },
  { dictionaryEntries: ' foo \r\n\nbar baz \n' }, { customDictionary: 'one\ntwo' },
  { dictionaryEntries: [entry('a', ' Foo  Bar '), entry('b', 'foo bar'), entry('c', 'FOO bar', { source: 'automatic', frequency: 3.7, aliases: [{ text: 'x', count: 2 }, { text: 'X', count: 1, lastSeenAt: 9 }, { text: '', count: 1 }, 5] }), { text: 'no id', createdAt: 11 }, { text: 'no id 2' }, 'x', { text: '' }, { id: ' ', text: 'blank id', createdAt: 3.5 }] },
  { dictionaryEntries: Array.from({ length: 1005 }, (_, i) => entry(`i${i}`, `term ${i}`)) },
  { dictionaryCandidates: [{ text: 'c1', evidenceCount: 1 }, { text: 'C1', evidenceCount: 5 }, { text: 'c2', aliases: [{ text: 'a', count: 3 }, { text: 'b', count: 4 }] }, { text: 'c3', evidenceCount: 5, updatedAt: 99 }, { text: 'c4', evidenceCount: 5, updatedAt: 100 }, { text: 'c5', evidenceCount: 0.4 }, 7] },
  { suppressedAutomaticDictionaryTexts: ['A', ' a ', 'b', 3, '', 'B'] },
  { conversationSelection: { modelId: ' m ', voice: ' v ', speed: 1.5 } }, { conversationSelection: { modelId: 'm', voice: 'a/b' } },
  { conversationSelection: { modelId: 'm', voice: 'v', speed: 9 } }, { conversationSelection: { modelId: 'm', providerId: 'p', voice: 'v' } },
  { conversationSelection: { modelId: 'm', providerId: 'p', agent: 'pi', protocol: 'minimax', voice: 'v' } },
  { conversationSelection: { modelId: 'm', providerId: 'xd', agent: 'pi', voice: 'v' } },
  { conversationSelection: { modelId: 'm', protocol: 'minimax', voice: 'v' } },
  { conversationSelection: { modelId: 'm', agent: 'codex', voice: 'v' } },
  { conversationSelections: { a: { modelId: 'm', voice: 'v' }, b: null, ' ': null, c: { modelId: '' }, [H(300)]: null }, conversationSentencePlayback: { a: true, b: 1, ' ': true } },
  { conversationSelections: [] , conversationSentencePlayback: [] },
];
const settings = PLATFORMS.flatMap((p) => settingsInputs.map((input) => ({ platform: p, input, output: run(() => normalizeVoiceInputSettings(input, p)) })));

const base = (extra: any = {}) => run(() => normalizeVoiceInputSettings({ dictionaryEntries: [entry('e1', 'Alpha', { aliases: [{ text: 'alfa', count: 2, lastSeenAt: 7 }] }), entry('e2', 'Beta', { source: 'automatic', frequency: 2 }), entry('e3', 'Gamma', { source: 'automatic' })], dictionaryCandidates: [{ text: 'Delta', evidenceCount: 1, createdAt: 1, updatedAt: 2 }], suppressedAutomaticDictionaryTexts: ['Zeta'], ...extra }, 'darwin'));
const act = (action: string, term: string, aliases: string[] = [], confidence = 'high') => ({ action, term, aliases, type: 'term', confidence });
const learningCases: any[] = [
  [], [act('add_candidate', 'New')], [act('add_candidate', 'Delta')], [act('add_candidate', 'Delta', ['dd'])], [act('add_entry', 'Fresh', ['f1', 'F1', 'fresh'])],
  [act('update_entry', 'alpha', ['al', 'alfa'])], [act('add_candidate', 'Zeta')], [act('add_entry', 'Zeta')], [act('add_entry', 'Low', [], 'low')],
  [act('add_candidate', 'Dup'), act('add_entry', 'dup', ['x']), act('add_candidate', 'DUP', ['y'])], [act('update_entry', 'Dup2'), act('add_candidate', 'dup2')],
  [act('add_candidate', '  '), act('add_entry', '')], [act('add_candidate', 'Beta'), act('add_candidate', 'Gamma', ['g'])],
  [act('add_candidate', 'A\r\nB  C', ['A B C', 'z'])],
];
const learning: any[] = [];
for (const refine of [true, false]) for (const auto of [true, false]) for (const actions of learningCases) {
  const cur = base({ refinementEnabled: refine, autoDictionaryEnabled: auto });
  const next = run(() => applyVoiceInputDictionaryLearningActions(cur, actions));
  const same = next === cur;
  learning.push({ current: cur, actions, changed: !same, next: same ? null : next, newAutomatic: same ? [] : run(() => getNewAutomaticDictionaryEntries(cur.dictionaryEntries, next.dictionaryEntries)) });
}
const deletes = [[], [''], ['nope'], ['e1'], ['e2'], ['e2', 'e3'], [' e3 '], ['e1', 'e2', 'e3']].map((ids) => {
  const cur = base({ dictionaryCandidates: [{ text: 'Beta', evidenceCount: 3 }, { text: 'Other', evidenceCount: 1 }] });
  const next = run(() => deleteVoiceInputDictionaryEntriesFromSettings(cur, ids as string[]));
  return { current: cur, ids, changed: next !== cur, next: next === cur ? null : next };
});
const csv = [
  { existing: [], terms: ['a', 'b', 'A', ' c  d ', '', 'x'.repeat(121), 'x'.repeat(120)] },
  { existing: [entry('e', 'Keep')], terms: ['keep', 'New', 'new'] },
  { existing: 'one\ntwo', terms: ['TWO', 'three\r\nfour'] },
  { existing: Array.from({ length: 1000 }, (_, i) => entry(`i${i}`, `term ${i}`)), terms: ['extra', 'term 5'] },
].map((c) => { const r = run(() => mergeVoiceInputDictionaryCsvTerms(c.existing as any, c.terms)); return { ...c, entries: r.entries, importedCount: r.importedCount }; });
const texts = ['', '  ', 'a  b\n\nc', ' x\r\ny ', 'ÀÉ İ I', 'x'.repeat(130), '😀'.repeat(70), 'tab\tsep\u00a0nbsp\u3000ideo', 5, null, undefined, 'a\u0085b'].map((t) => ({ input: t === undefined ? '__undef__' : t, output: normalizeVoiceInputDictionaryEntryText(t) }));
const manual = ['', ' ok ', 'Two  words', 'x'.repeat(200)].map((t) => ({ input: t, output: run(() => createManualVoiceInputDictionaryEntry(t)) }));
const historyIn: any[] = [
  null, [], [{ text: ' a ', createdAt: 5, id: 'h1' }, { text: 'b', createdAt: '7' }, { text: 'c', createdAt: 0 }, { text: '', createdAt: 9 }, { text: 'd', createdAt: 9, id: ' ' }, 4, { text: 'e', createdAt: 9.5, id: 'h5' }],
];
const history = historyIn.map((input) => ({ input, output: run(() => normalizeVoiceInputHistory(input)) }));
const create = ['', ' hi ', 'x'].map((t) => ({ input: t, output: run(() => createVoiceInputHistoryEntry(t)) }));
const mk = (n: number, len: number) => Array.from({ length: n }, (_, i) => ({ id: `h${i}`, text: 'w'.repeat(len), createdAt: 1000 - i }));
const compact = [mk(3, 10), mk(60, 300), mk(100, 400), mk(45, 100), mk(2, 5000)].map((input) => ({ input, output: compactVoiceInputHistoryIfNeeded(input) }));
const snapshots = PLATFORMS.flatMap((p) => [null, {}, { settings: { language: 'ja' }, history: [{ text: 'x', createdAt: 5, id: 'q' }] }].map((input) => ({ platform: p, input, output: run(() => normalizeVoiceInputDataSnapshot(input, p)) })));
process.stdout.write(JSON.stringify({ providerKind: DEFAULT_VOICE_INPUT_PROVIDER_KIND, settings, learning, deletes, csv, texts, manual, history, create, compact, snapshots }));
