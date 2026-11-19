// 余额 / 额度用量投影的黄金样例：运行上游 billing/projection.ts，输出（输入, 输出或错误）。
import { projectModelAccessBalance, projectModelAccessCreditUsage } from '@cindy-desktop-main/billing/projection.ts';

const run = (fn: (v: unknown) => unknown, input: unknown) => {
  try { return { input, ok: true, output: JSON.parse(JSON.stringify(fn(input))) }; }
  catch { return { input, ok: false }; }
};

const T = '2026-05-01T12:00:00.000Z';
const bal = (o: Record<string, unknown> = {}) => ({
  scale: 9, planCredits: '1.5', purchasedCredits: '2', promotionalCredits: '0.5', available: '4', observedAt: T, ...o,
});
const balances: unknown[] = [
  bal(), bal({ planCredits: '0', purchasedCredits: '0', promotionalCredits: '0', available: '0' }),
  bal({ purchasedCredits: '-1', available: '1' }), bal({ planCredits: '-1', available: '1' }), bal({ promotionalCredits: '-0.5', available: '3' }),
  bal({ available: '5' }), bal({ scale: 8 }), bal({ scale: '9' }), bal({ observedAt: '2026-02-30T00:00:00Z' }),
  bal({ observedAt: '2024-02-29T23:59:59.123456789Z' }), bal({ observedAt: '2025-02-29T00:00:00Z' }), bal({ observedAt: '2026-05-01T12:00:00+08:00' }),
  bal({ observedAt: '2026-05-01T24:00:00Z' }), bal({ observedAt: '2026-05-01T12:60:00Z' }), bal({ observedAt: '2026-05-01T12:00:60Z' }), bal({ observedAt: 5 }),
  bal({ planCredits: '1.0000000001', available: '4.0000000001' }), bal({ planCredits: '01', available: '3.5' }), bal({ planCredits: '1e3' }),
  bal({ planCredits: 1.5 }), bal({ planCredits: '9999999999.999999999', purchasedCredits: '0', promotionalCredits: '0', available: '9999999999.999999999' }),
  bal({ planCredits: '9999999999', purchasedCredits: '-9999999999', promotionalCredits: '0', available: '0' }),
  bal({ planCredits: '10000000000', available: '12.5' }), bal({ planCredits: '-0', available: '2.5' }), bal({ planCredits: '1.', available: '3.5' }),
  null, [], 'x', 3, {}, bal({ extra: 1 }), bal({ planCredits: undefined }),
];

const pool = (remaining: string, used: string | null, total: string | null) => ({ remaining, used, total });
const grant = (o: Record<string, unknown> = {}) => ({
  grantId: 'g1', displayName: 'Welcome', originalAmount: '1', usedAmount: '0.5', remainingAmount: '0.5',
  expiresAt: '2027-01-01T00:00:00.000Z', state: 'active', ...o,
});
const usage = (o: Record<string, unknown> = {}) => ({
  scale: 9, available: '4', plan: pool('1.5', '0.5', '2'), purchased: pool('2', null, null), promotional: pool('0.5', '0.5', '1'),
  promotionalGrants: [grant()], promotionalGrantsComplete: true, promotionalGrantConsistency: 'OBSERVED', ledgerUpdatedAt: null, observedAt: T, ...o,
});
const usages: unknown[] = [
  usage(), usage({ ledgerUpdatedAt: '2026-04-30T00:00:00Z' }), usage({ ledgerUpdatedAt: 'bad' }), usage({ ledgerUpdatedAt: 5 }),
  usage({ promotionalGrantsComplete: false, promotional: pool('0.5', null, null) }),
  usage({ promotionalGrantsComplete: false }),
  usage({ promotionalGrants: [] }), usage({ promotionalGrants: [], promotional: pool('0', '0', '0'), available: '3.5' }),
  usage({ promotionalGrants: [grant(), grant()] }),
  usage({ promotionalGrants: [grant({ grantId: 'g2', expiresAt: '2020-01-01T00:00:00.000Z' })].concat([grant()]) }),
  usage({ promotionalGrants: [grant({ state: 'depleted', usedAmount: '1', remainingAmount: '0' })], promotional: pool('0', '1', '1'), available: '3.5' }),
  usage({ promotionalGrants: [grant({ state: 'depleted', usedAmount: '0.9', remainingAmount: '0' })] }),
  usage({ promotionalGrants: [grant({ state: 'expired', usedAmount: '0.3', remainingAmount: '0' })], promotional: pool('0', '0', '0'), available: '3.5' }),
  usage({ promotionalGrants: [grant({ state: 'expired', usedAmount: '0.3', remainingAmount: '0.1' })] }),
  usage({ promotionalGrants: [grant({ state: 'voided', usedAmount: '2', remainingAmount: '0' })] }),
  usage({ promotionalGrants: [grant({ state: 'weird' })] }), usage({ promotionalGrants: [grant({ state: 'active', remainingAmount: '0', usedAmount: '1' })] }),
  usage({ promotionalGrants: [grant({ displayName: null })] }), usage({ promotionalGrants: [grant({ displayName: '' })] }),
  usage({ promotionalGrants: [grant({ displayName: 'x'.repeat(128) })] }), usage({ promotionalGrants: [grant({ displayName: 'x'.repeat(129) })] }),
  usage({ promotionalGrants: [grant({ grantId: '' })] }), usage({ promotionalGrants: [grant({ grantId: 'g'.repeat(129) })] }),
  usage({ promotionalGrants: [grant({ originalAmount: '0', usedAmount: '0', remainingAmount: '0' })] }),
  usage({ promotionalGrants: [grant({ expiresAt: 'nope' })] }),
  usage({ promotionalGrants: new Array(1001).fill(0).map((_, i) => grant({ grantId: 'g' + i })) }),
  usage({ promotionalGrants: 'x' }), usage({ promotionalGrantsComplete: 'yes' }), usage({ promotionalGrantConsistency: 'STALE' }), usage({ scale: 6 }),
  usage({ purchased: pool('-1', null, null), available: '0.5', plan: pool('1.5', '0.5', '2'), promotional: pool('0', '0', '0'), promotionalGrants: [], promotionalGrantsComplete: true }),
  usage({ plan: pool('-1', null, null), available: '0.5' }), usage({ plan: pool('1.5', '0.5', null) }), usage({ plan: pool('1.5', null, '2') }),
  usage({ plan: pool('1.5', '0.5', '3') }), usage({ plan: pool('1.5', '-0.5', '1') }), usage({ plan: 'x' }), usage({ plan: null }),
  usage({ available: '5' }), usage({ observedAt: 'x' }), null, [], 7, {},
];

console.log(JSON.stringify({
  balanceCases: balances.map((i) => run(projectModelAccessBalance, i)),
  usageCases: usages.map((i) => run(projectModelAccessCreditUsage, i)),
}));
