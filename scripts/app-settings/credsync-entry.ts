// 凭据同步状态机的黄金样例：用脚本化依赖驱动上游 createCredentialsSync，记录状态推送、调用次数、写 key、退避延迟与各步结果。
import { createCredentialsSync } from '@cindy-desktop-main/model-access/credentialsSync.ts';

type Outcome = { ok?: any; err?: { code: string; statusCode: number }; gate?: string };
type Step = { op: string; [k: string]: any };
interface Scenario { name: string; fetch: Outcome[]; rotate?: Outcome[]; writes?: boolean[]; initialKey?: string | null; initialSource?: 'server' | 'manual' | null; steps: Step[] }

const good = (k = 'key-1', e = 'https://gw.example.com/v1') => ({ ok: { endpoint: e, apiKey: k } });
const E = (code: string, statusCode: number): Outcome => ({ err: { code, statusCode } });
const auth = (userId: string | null, realm: string | null = 'global'): Step => ({ op: 'auth', isAuthenticated: userId !== null, userId, realm });

const scenarios: Scenario[] = [
  { name: 'login ok', fetch: [good()], steps: [auth('u1')] },
  { name: 'same key is not rewritten', fetch: [good('same')], initialKey: 'same', steps: [auth('u1')] },
  { name: 'different key is rewritten', fetch: [good('new')], initialKey: 'old', steps: [auth('u1')] },
  { name: '503 disabled, sync no-ops, retry re-fetches', fetch: [E('MODEL_ACCESS_DISABLED', 503), good()], steps: [auth('u1'), { op: 'sync' }, { op: 'retry' }] },
  { name: '503 by code only', fetch: [E('MODEL_ACCESS_DISABLED', 0)], steps: [auth('u1')] },
  { name: '403 unsupported, logout resets, login again', fetch: [E('ORG_NOT_SUPPORTED', 403), good()], steps: [auth('u1'), { op: 'sync' }, { op: 'resync' }, auth(null), auth('u1')] },
  { name: 'transient failure exhausts retries', fetch: [E('INTERNAL_ERROR', 500)], steps: [auth('u1'), { op: 'retry' }] },
  { name: 'transient then ok', fetch: [E('NETWORK_ERROR', 0), good()], steps: [auth('u1')] },
  { name: 'two failures then ok', fetch: [E('X', 500), E('X', 500), good()], steps: [auth('u1')] },
  { name: 'unknown error shape', fetch: [{ err: { code: 5 as any, statusCode: 'x' as any } }, good()], steps: [auth('u1')] },
  { name: 'invalid payload: empty key', fetch: [{ ok: { endpoint: 'https://a.b', apiKey: '' } }], steps: [auth('u1')] },
  { name: 'invalid payload: blank key', fetch: [{ ok: { endpoint: 'https://a.b', apiKey: '  ' } }], steps: [auth('u1')] },
  { name: 'invalid payload: ftp endpoint', fetch: [{ ok: { endpoint: 'ftp://a.b', apiKey: 'k' } }], steps: [auth('u1')] },
  { name: 'invalid payload: blank endpoint', fetch: [{ ok: { endpoint: ' ', apiKey: 'k' } }], steps: [auth('u1')] },
  { name: 'invalid payload: not a url', fetch: [{ ok: { endpoint: 'not a url', apiKey: 'k' } }], steps: [auth('u1')] },
  { name: 'invalid payload: null', fetch: [{ ok: null }], steps: [auth('u1')] },
  { name: 'invalid payload: wrong types', fetch: [{ ok: { endpoint: 5, apiKey: {} } }, good()], steps: [auth('u1')] },
  { name: 'http endpoint accepted', fetch: [{ ok: { endpoint: 'http://127.0.0.1:9/x', apiKey: 'k' } }], steps: [auth('u1')] },
  { name: 'write failure is SAFE_STORAGE_UNAVAILABLE', fetch: [good()], writes: [false], steps: [auth('u1')] },
  { name: 'write failure then retry ok', fetch: [good()], writes: [false, true], steps: [auth('u1'), { op: 'retry' }] },
  { name: 'silent resync keeps ok on fetch failure', fetch: [good(), E('X', 500)], steps: [auth('u1'), { op: 'resync' }] },
  { name: 'silent resync keeps status on write failure', fetch: [good('a'), good('b')], writes: [true, false], steps: [auth('u1'), { op: 'resync' }] },
  { name: 'silent resync applies new key without syncing push', fetch: [good('a'), good('b')], steps: [auth('u1'), { op: 'resync' }] },
  { name: 'silent resync reaches disabled', fetch: [good('a'), E('MODEL_ACCESS_DISABLED', 503)], steps: [auth('u1'), { op: 'resync' }] },
  { name: 'rotate ok', fetch: [good('a')], rotate: [good('b')], steps: [auth('u1'), { op: 'rotate' }] },
  { name: 'rotate fetch error propagates', fetch: [good('a')], rotate: [E('RATE_LIMITED', 429)], steps: [auth('u1'), { op: 'rotate' }] },
  { name: 'rotate invalid payload', fetch: [good('a')], rotate: [{ ok: { endpoint: 'https://a.b', apiKey: '' } }], steps: [auth('u1'), { op: 'rotate' }] },
  { name: 'rotate write failure', fetch: [good('a')], rotate: [good('b')], writes: [true, false], steps: [auth('u1'), { op: 'rotate' }] },
  { name: 'rotate discarded when account changes', fetch: [good('a')], rotate: [{ ...good('b'), gate: 'r' }], steps: [auth('u1'), { op: 'rotate', wait: false }, auth('u2'), { op: 'release', gate: 'r' }] },
  { name: 'rotate waits for in-flight sync', fetch: [{ ...good('a'), gate: 'f' }], rotate: [good('b')], steps: [auth('u1'), { op: 'rotate', wait: false }, { op: 'release', gate: 'f' }] },
  { name: 'stale response of old user is dropped', fetch: [{ ...good('u1key'), gate: 'g1' }, good('u2key')], steps: [auth('u1'), auth('u2'), { op: 'release', gate: 'g1' }] },
  { name: 'logout during in-flight drops the result', fetch: [{ ...good(), gate: 'g' }], steps: [auth('u1'), auth(null), { op: 'release', gate: 'g' }] },
  { name: 'same user refresh does not invalidate', fetch: [{ ...good(), gate: 'g' }], steps: [auth('u1'), auth('u1'), auth('u1'), { op: 'release', gate: 'g' }] },
  { name: 'first identity is not a switch', fetch: [{ ...good(), gate: 'g' }], steps: [{ op: 'auth', isAuthenticated: true }, auth('u1'), { op: 'release', gate: 'g' }] },
  { name: 'realm change invalidates', fetch: [{ ...good('a'), gate: 'g' }, good('b')], steps: [auth('u1', 'global'), auth('u1', 'cn'), { op: 'release', gate: 'g' }] },
  { name: 'null identity keeps previous', fetch: [{ ...good('a'), gate: 'g' }, good('b')], steps: [auth('u1'), { op: 'auth', isAuthenticated: true, userId: null, realm: null }, { op: 'release', gate: 'g' }] },
  { name: 'concurrent sync calls share one fetch', fetch: [{ ...good(), gate: 'g' }], steps: [auth('u1'), { op: 'sync', wait: false }, { op: 'retry', wait: false }, { op: 'release', gate: 'g' }] },
  { name: 'silent in-flight turned loud by retry', fetch: [good('a'), { ...E('X', 500), gate: 'g' }], steps: [auth('u1'), { op: 'resync', wait: false }, { op: 'retry', wait: false }, { op: 'release', gate: 'g' }] },
  { name: 'silent in-flight stays silent', fetch: [good('a'), { ...E('X', 500), gate: 'g' }], steps: [auth('u1'), { op: 'resync', wait: false }, { op: 'release', gate: 'g' }] },
  { name: 'loud in-flight not made silent by resync', fetch: [good('a'), { ...E('X', 500), gate: 'g' }], steps: [auth('u1'), { op: 'retry', wait: false }, { op: 'resync', wait: false }, { op: 'release', gate: 'g' }] },
  { name: 'manual saved / removed', fetch: [good()], initialSource: 'manual', steps: [auth('u1'), { op: 'manualSaved' }, { op: 'manualRemoved' }] },
  { name: 'manual source overwritten by server credentials', fetch: [good()], initialSource: 'manual', steps: [auth('u1')] },
  { name: 'retry after unsupported', fetch: [E('ORG_NOT_SUPPORTED', 403), good()], steps: [auth('u1'), { op: 'retry' }] },
  { name: 'auth change while failed retries', fetch: [E('X', 500), good()], steps: [auth('u1'), auth('u1')] },
  { name: 'account switch while ok', fetch: [good('a'), good('b')], steps: [auth('u1'), auth('u2')] },
];

const flush = async () => { for (let i = 0; i < 8; i++) await new Promise((r) => setImmediate(r)); };
const norm = (v: any) => JSON.parse(JSON.stringify(v === undefined ? null : v));

async function run(sc: Scenario) {
  const events: any[] = [];
  const writes: any[] = [];
  const delays: number[] = [];
  let fetchN = 0, rotateN = 0, writeN = 0;
  let key: string | null = sc.initialKey ?? null;
  let state: any = sc.initialSource ? { source: sc.initialSource, endpoint: null } : { source: null, endpoint: null };
  const gates = new Map<string, { p: Promise<void>; res: () => void }>();
  const gate = (n: string) => {
    if (!gates.has(n)) { let res!: () => void; const p = new Promise<void>((r) => { res = r; }); gates.set(n, { p, res }); }
    return gates.get(n)!;
  };
  const pick = <T,>(q: T[], i: number) => q[Math.min(i, q.length - 1)];
  const outcome = async (o: Outcome) => { if (o.gate) await gate(o.gate).p; if (o.err) throw o.err; return o.ok; };
  const sync = createCredentialsSync({
    fetchCredentials: () => outcome(pick(sc.fetch, fetchN++)),
    rotateCredentials: () => outcome(pick(sc.rotate ?? sc.fetch, rotateN++)),
    readXdKey: () => key,
    writeXdKey: async (k: string) => {
      const ok = sc.writes ? pick(sc.writes, writeN++) : true;
      writes.push({ key: k, ok });
      if (ok) key = k;
      return ok;
    },
    store: {
      getSource: () => state.source,
      getServerEndpoint: () => (state.source === 'server' ? state.endpoint : null),
      setServerCredentials: (endpoint: string) => { state = { source: 'server', endpoint }; },
      setManualSource: () => { state = { source: 'manual', endpoint: null }; },
      clear: () => { state = { source: null, endpoint: null }; },
    } as any,
    onStatusChange: (s) => events.push(norm(s)),
    sleep: async (ms: number) => { delays.push(ms); },
  });
  const results: any[] = [];
  const track = (p: Promise<any>) => {
    const idx = results.length;
    results.push(null);
    p.then((v) => { results[idx] = { ok: norm(v) }; }, (e) => { results[idx] = { err: { code: e?.code ?? null, statusCode: e?.statusCode ?? null } }; });
  };
  for (const step of sc.steps) {
    switch (step.op) {
      case 'auth': sync.handleAuthChange({ isAuthenticated: step.isAuthenticated, userId: step.userId, realm: step.realm } as any); break;
      case 'sync': track(sync.sync()); break;
      case 'retry': track(sync.retry()); break;
      case 'resync': track(sync.resyncAfterKeyRejection()); break;
      case 'rotate': track(sync.rotate()); break;
      case 'manualSaved': sync.noteManualKeySaved(); break;
      case 'manualRemoved': sync.noteManualKeyRemoved(); break;
      case 'release': gate(step.gate).res(); break;
    }
    await flush();
  }
  return { name: sc.name, scenario: sc, events, writes, delays, fetchN, rotateN, results, finalStatus: norm(sync.getStatus()), finalKey: key, finalStore: state };
}

async function main() {
  const out = [];
  for (const sc of scenarios) out.push(await run(sc));
  console.log(JSON.stringify({ scenarios: out }));
}
main().catch((e) => { console.error(e); process.exit(1); });
