// 布局与项目排序的黄金样例：运行上游 layoutTree.ts coerceLayout / projectOrderSync.ts，输出（输入, 输出）对。
import { coerceLayout, createDefaultLayout } from '@cindy-desktop-shared/layoutTree';
import { hostLocalProjectKeysOnly, parseSyncedProjectOrderMode } from '@cindy/maker-shared/project-order-sync';

const def: any = createDefaultLayout();
const clone = () => JSON.parse(JSON.stringify(def));
const pane = (id: string, panelKind: string, extra: any = {}) => ({ type: 'pane', id, panelKind, ...extra });
const split = (id: string, direction: string, children: any[]) => ({ type: 'split', id, direction, children });
const withContent = (content: any, extra: any = {}) => ({ ...clone(), content, ...extra });
const chat = pane('chat', 'chat-main', { minWidth: 400 });
const deep = (n: number): any => (n === 0 ? chat : split(`s${n}`, 'row', [{ fraction: 0.5, node: deep(n - 1) }, { fraction: 0.5, node: pane(`p${n}`, 'right-tabs') }]));
const layouts: any[] = [
  null, undefined, 3, 'x', [], {}, clone(),
  { ...clone(), schemaVersion: 2 }, { ...clone(), float: [{}] }, { ...clone(), float: 'x' },
  { ...clone(), sidebar: { ...def.sidebar, edge: 'right' } },
  { ...clone(), sidebar: { ...def.sidebar, panelKind: 'chat-main' } },
  { ...clone(), sidebar: { ...def.sidebar, id: '' } },
  { ...clone(), sidebar: { ...def.sidebar, type: 'split' } },
  { ...clone(), sidebar: null },
  withContent(split('root', 'row', [{ fraction: 0.5, node: chat }, { fraction: 0.5, node: pane('r', 'tab-group:right') }])),
  withContent(split('root', 'column', [{ fraction: 0.3, node: chat }, { fraction: 0.7, node: pane('r', 'ghost:abc', { collapsed: true, preferredWidth: 300 }) }])),
  withContent(split('root', 'diag', [{ fraction: 0.5, node: chat }, { fraction: 0.5, node: pane('r', 'x') }])),
  withContent(split('root', 'row', [{ fraction: 1, node: chat }])),
  withContent(split('root', 'row', [{ fraction: 0.5, node: chat }, { fraction: 0.4, node: pane('r', 'x') }])),
  withContent(split('root', 'row', [{ fraction: 0.5, node: chat }, { fraction: 0.505, node: pane('r', 'x') }])),
  withContent(split('root', 'row', [{ fraction: 0.5, node: chat }, { fraction: 0.52, node: pane('r', 'x') }])),
  withContent(split('root', 'row', [{ fraction: -1, node: chat }, { fraction: 2, node: pane('r', 'x') }])),
  withContent(split('root', 'row', [{ fraction: 0.5, node: chat }, { fraction: '0.5', node: pane('r', 'x') }])),
  withContent(split('root', 'row', [{ fraction: 0.5, node: chat }, null])),
  withContent(split('root', 'row', [{ fraction: 0.5, node: chat }, { fraction: 0.5, node: pane('chat', 'x') }])),
  withContent(split('root', 'row', [{ fraction: 0.5, node: chat }, { fraction: 0.5, node: pane('sessions', 'x') }])),
  withContent(split('root', 'row', [{ fraction: 0.5, node: chat }, { fraction: 0.5, node: pane('r2', 'chat-main') }])),
  withContent(split('root', 'row', [{ fraction: 0.5, node: pane('a', 'x') }, { fraction: 0.5, node: pane('b', 'y') }])),
  withContent(split('root', 'row', [{ fraction: 0.5, node: pane('c', 'chat-main', { collapsed: true }) }, { fraction: 0.5, node: pane('b', 'y') }])),
  withContent(split('root', 'row', [{ fraction: 0.5, node: chat }, { fraction: 0.5, node: pane('b', 'session-list') }])),
  withContent(split('root', 'row', [{ fraction: 0.5, node: chat }, { fraction: 0.5, node: pane('b', '') }])),
  withContent(split('root', 'row', [{ fraction: 0.5, node: chat }, { fraction: 0.5, node: pane('b', 'k'.repeat(128)) }])),
  withContent(split('root', 'row', [{ fraction: 0.5, node: chat }, { fraction: 0.5, node: pane('b', 'k'.repeat(129)) }])),
  withContent(split('root', 'row', [{ fraction: 0.5, node: chat }, { fraction: 0.5, node: pane('i'.repeat(129), 'k') }])),
  withContent(split('root', 'row', [{ fraction: 0.5, node: chat }, { fraction: 0.5, node: pane('b', 'k', { collapsed: 'yes' }) }])),
  withContent(split('root', 'row', [{ fraction: 0.5, node: chat }, { fraction: 0.5, node: pane('b', 'k', { minWidth: 0 }) }])),
  withContent(split('root', 'row', [{ fraction: 0.5, node: chat }, { fraction: 0.5, node: pane('b', 'k', { minWidth: 10.5 }) }])),
  withContent(split('root', 'row', [{ fraction: 0.5, node: chat }, { fraction: 0.5, node: pane('b', 'k', { minWidth: '5' }) }])),
  withContent(split('root', 'row', [{ fraction: 0.5, node: chat }, { fraction: 0.5, node: pane('b', 'k', { preferredWidth: 119 }) }])),
  withContent(split('root', 'row', [{ fraction: 0.5, node: chat }, { fraction: 0.5, node: pane('b', 'k', { preferredWidth: 120 }) }])),
  withContent(split('root', 'row', [{ fraction: 0.5, node: chat }, { fraction: 0.5, node: pane('b', 'k', { preferredWidth: 16000 }) }])),
  withContent(split('root', 'row', [{ fraction: 0.5, node: chat }, { fraction: 0.5, node: pane('b', 'k', { preferredWidth: 16001 }) }])),
  withContent(split('root', 'row', [{ fraction: 0.5, node: chat }, { fraction: 0.5, node: { type: 'weird', id: 'w' } }])),
  withContent(split('root', 'row', [{ fraction: 0.5, node: chat }, { fraction: 0.5, node: 'x' }])),
  withContent(chat), withContent(pane('only', 'right-tabs')), withContent(null), withContent(undefined),
  withContent(deep(6)), withContent(deep(7)), withContent(deep(8)),
  { ...clone(), extra: 'kept', content: split('root', 'row', [{ fraction: 0.5, node: { ...chat, unknown: 1 } }, { fraction: 0.5, node: pane('r', 'right-tabs') }]) },
];
const coerceCases = layouts.map((input) => {
  const r = coerceLayout(input);
  return { input: input === undefined ? { __undef: true } : input, output: r };
});
const keyInputs: any[] = [null, 3, 'x', {}, [], ['local:/a', 'device:x:/b', ' local:/c ', 'local:/a', '', '  ', 5, null, 'local:'], ['local:/' + 'k'.repeat(4100)],
  Array.from({ length: 10_005 }, (_, i) => `local:/p${i}`), ['  device:x  ', 'local: /sp ']];
const orderCases = keyInputs.map((input) => ({ input, keys: hostLocalProjectKeysOnly(input) }));
const modeCases = [null, undefined, 'custom', 'activity', 'x', 3, {}].map((input) => ({ input: input === undefined ? { __undef: true } : input, mode: parseSyncedProjectOrderMode(input) }));
process.stdout.write(JSON.stringify({ default: def, coerceCases, orderCases, modeCases }));
