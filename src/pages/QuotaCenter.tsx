/**
 * 额度中心（2026-10-17）：跨家族积分 / 免费档用量 / 健康体检 聚合页。
 *
 * - 额度总览：Buddy/Qoder/Trae/ZCode 四家族账号的积分余额、到期、免费模型
 *   本周期已用次数（网关记账，见 api_server/free_usage.rs）、耗尽/恢复状态；
 *   上游不开放免费模型剩余额度查询（实测），「已用次数 + 恢复时间」为可观测口径。
 * - 体检：跨家族问题清单（需重登 / 低额度 / 即将到期 / 免费档耗尽），
 *   能自动修的给一键动作（Qoder 刷新凭证），其余给处理指引。
 */
import { useCallback, useEffect, useMemo, useState } from 'react';
import { AlertTriangle, Gauge, RefreshCw, ShieldCheck, Stethoscope } from 'lucide-react';
import PageHeader from '../components/PageHeader';
import { Badge } from '../components/ui';
import { api, type ApiKeyEntry, type FreeUsageSnapshot, type QoderAccountView, type WorkBuddyAccountView } from '../lib/tauri';
import { useAppStore } from '../store';

const CN = 'sec-east';

type Issue = {
  family: 'Buddy' | 'Qoder' | 'Trae' | 'ZCode';
  account: string;
  detail: string;
  /** 可一键处理的动作 id（qoder-refresh） */
  action?: 'qoder-refresh';
  keyId?: string;
};

function fmtDate(v: string | number | null | undefined): string {
  if (!v) return '—';
  const d = typeof v === 'number' ? new Date(v * 1000) : new Date(String(v).replace(' ', 'T'));
  if (Number.isNaN(d.getTime())) return String(v);
  return `${d.getMonth() + 1}/${d.getDate()}`;
}

function FreeUsageCell({ usage }: { usage?: { count?: number; exhausted_at?: string; reset_at?: string } }) {
  if (!usage || (usage.count == null && !usage.exhausted_at)) {
    return <span className="text-xs text-slate-300 dark:text-zinc-600">—</span>;
  }
  const used = usage.count ?? 0;
  if (usage.exhausted_at) {
    return (
      <span className="text-xs text-amber-600 dark:text-amber-400">
        已用完（{usage.exhausted_at.slice(5, 16)}）{usage.reset_at ? ` · ${usage.reset_at.slice(5, 16)} 恢复` : ''}
      </span>
    );
  }
  return <span className="text-xs text-slate-500 dark:text-zinc-400">本周期已用 {used} 次</span>;
}

export default function QuotaCenter() {
  const pushToast = useAppStore((s) => s.pushToast);
  const [tab, setTab] = useState<'overview' | 'health'>('overview');
  const [loading, setLoading] = useState(false);
  const [wbAccounts, setWbAccounts] = useState<WorkBuddyAccountView[]>([]);
  const [qoderAccounts, setQoderAccounts] = useState<QoderAccountView[]>([]);
  const [freeUsage, setFreeUsage] = useState<FreeUsageSnapshot>({});
  const [keys, setKeys] = useState<ApiKeyEntry[]>([]);
  const [refreshingQoder, setRefreshingQoder] = useState('');

  const refresh = useCallback(async () => {
    setLoading(true);
    try {
      const [wb, q, fu, ks] = await Promise.all([
        api.workbuddy.accountsList(),
        api.qoder.accountsList(),
        api.apiServer.freeUsageSnapshot(),
        api.apiServer.keysList(),
      ]);
      setWbAccounts(wb);
      setQoderAccounts(q);
      setFreeUsage(fu);
      setKeys(ks.keys);
    } catch (err) {
      pushToast('error', `额度中心加载失败：${String(err).slice(0, 120)}`);
    } finally {
      setLoading(false);
    }
  }, [pushToast]);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  // 体检问题清单（跨家族）
  const issues = useMemo<Issue[]>(() => {
    const out: Issue[] = [];
    for (const a of wbAccounts) {
      if (a.needs_relogin) {
        out.push({ family: 'Buddy', account: a.nickname || a.id, detail: a.relogin_reason || '登录态失效，需重新登录' });
      }
      if (a.credits_balance != null && a.credits_balance < 50) {
        out.push({ family: 'Buddy', account: a.nickname || a.id, detail: `积分余额偏低：${a.credits_balance}` });
      }
      const fm = a.free_models ?? [];
      if (fm.length > 0 && fm.every((m) => m.exhausted_at)) {
        out.push({ family: 'Buddy', account: a.nickname || a.id, detail: `免费档全部耗尽（${fm.map((m) => m.name).join('、')}）` });
      }
    }
    for (const a of qoderAccounts) {
      if (a.needs_relogin) {
        out.push({ family: 'Qoder', account: a.nickname || a.id, detail: a.relogin_reason || '登录态失效，需重新登录', action: 'qoder-refresh', keyId: a.id });
      }
      if (a.credits_balance != null && a.credits_balance < 50) {
        out.push({ family: 'Qoder', account: a.nickname || a.id, detail: `积分余额偏低：${a.credits_balance}` });
      }
      const fm = a.free_models ?? [];
      if (fm.length > 0 && fm.every((m) => m.exhausted_at)) {
        out.push({ family: 'Qoder', account: a.nickname || a.id, detail: `免费档全部耗尽（${fm.map((m) => m.name).join('、')}）` });
      }
    }
    return out;
  }, [wbAccounts, qoderAccounts]);

  const refreshQoderCreds = async (keyId: string, name: string) => {
    setRefreshingQoder(keyId);
    try {
      await api.qoder.accountRefreshToken(keyId);
      pushToast('success', `「${name}」凭证已续期`);
      void refresh();
    } catch (err) {
      pushToast('error', `刷新失败：${String(err).slice(0, 120)}`);
    } finally {
      setRefreshingQoder('');
    }
  };

  const freeOf = (family: string, uid: string, model: string) =>
    freeUsage[family]?.[uid]?.[model?.toLowerCase() ?? ''];

  return (
    <div className="p-6">
      <PageHeader
        title="额度中心"
        desc="跨家族积分 / 免费档用量 / 账号健康（免费模型剩余额度上游不开放，展示「本周期已用次数 + 恢复时间」口径）"
        actions={
          <button className="btn-outline" onClick={() => void refresh()} disabled={loading}>
            <RefreshCw size={15} className={loading ? 'animate-spin' : ''} /> 刷新
          </button>
        }
      />

      <div className="mb-4 flex items-center gap-1 rounded-xl bg-slate-100 p-1 dark:bg-zinc-900">
        {([
          { key: 'overview', label: '额度总览', icon: Gauge },
          { key: 'health', label: '账号体检', icon: Stethoscope },
        ] as const).map(({ key, label, icon: Icon }) => (
          <button
            key={key}
            className={`flex items-center gap-1.5 rounded-lg px-3 py-1.5 text-sm font-medium transition ${
              tab === key
                ? 'bg-white text-slate-800 shadow-sm dark:bg-zinc-800 dark:text-zinc-100'
                : 'text-slate-500 hover:text-slate-700 dark:text-zinc-400'
            }`}
            onClick={() => setTab(key)}
          >
            <Icon size={14} /> {label}
            {key === 'health' && issues.length > 0 && (
              <span className="ml-1 inline-flex h-4 min-w-4 items-center justify-center rounded-full bg-amber-500 px-1 text-[10px] font-bold text-white">
                {issues.length}
              </span>
            )}
          </button>
        ))}
      </div>

      {tab === 'overview' ? (
        <div className="space-y-5">
          {/* Buddy 家族 */}
          <section>
            <div className="mb-2 flex items-center gap-2 text-sm font-medium">
              <ShieldCheck size={14} className="text-emerald-500" /> Buddy（WorkBuddy）
            </div>
            <div className="overflow-hidden rounded-xl border border-slate-200 dark:border-zinc-800">
              <table className="w-full text-sm">
                <thead className="bg-slate-50 text-left text-xs text-slate-500 dark:bg-zinc-900 dark:text-zinc-400">
                  <tr>
                    <th className="px-4 py-2 font-medium">账号</th>
                    <th className="px-4 py-2 font-medium">区域</th>
                    <th className="px-4 py-2 font-medium">积分余额</th>
                    <th className="px-4 py-2 font-medium">免费模型 · 本周期已用</th>
                  </tr>
                </thead>
                <tbody>
                  {wbAccounts.map((a) => (
                    <tr key={a.id} className="border-t border-slate-200 dark:border-zinc-800">
                      <td className="px-4 py-2">{a.nickname || a.id}</td>
                      <td className="px-4 py-2">
                        <Badge tone={a.region === 'global' ? 'violet' : 'blue'}>
                          {a.region === 'global' ? '海外' : '国内'}
                        </Badge>
                      </td>
                      <td className="px-4 py-2 tabular-nums">{a.credits_balance ?? '—'}</td>
                      <td className="px-4 py-2">
                        {(a.free_models ?? []).length === 0 ? (
                          <span className="text-xs text-slate-300 dark:text-zinc-600">无免费档</span>
                        ) : (
                          <div className="flex flex-col gap-0.5">
                            {a.free_models!.map((m) => (
                              <div key={m.id} className="flex items-center gap-2">
                                <span className="text-xs">{m.name}</span>
                                <FreeUsageCell usage={freeOf('buddy', a.uid, m.id)} />
                                {m.exhausted_at && !freeOf('buddy', a.uid, m.id)?.exhausted_at && (
                                  <span className="text-xs text-amber-600 dark:text-amber-400">
                                    已用完{m.reset_at ? ` · ${m.reset_at.slice(5, 16)} 恢复` : ''}
                                  </span>
                                )}
                              </div>
                            ))}
                          </div>
                        )}
                      </td>
                    </tr>
                  ))}
                  {wbAccounts.length === 0 && (
                    <tr><td colSpan={4} className="px-4 py-3 text-center text-xs text-slate-400">无账号</td></tr>
                  )}
                </tbody>
              </table>
            </div>
          </section>

          {/* Qoder 家族 */}
          <section>
            <div className="mb-2 flex items-center gap-2 text-sm font-medium">
              <ShieldCheck size={14} className="text-violet-500" /> Qoder
            </div>
            <div className="overflow-hidden rounded-xl border border-slate-200 dark:border-zinc-800">
              <table className="w-full text-sm">
                <thead className="bg-slate-50 text-left text-xs text-slate-500 dark:bg-zinc-900 dark:text-zinc-400">
                  <tr>
                    <th className="px-4 py-2 font-medium">账号</th>
                    <th className="px-4 py-2 font-medium">区域</th>
                    <th className="px-4 py-2 font-medium">积分余额</th>
                    <th className="px-4 py-2 font-medium">免费模型 · 本周期已用</th>
                  </tr>
                </thead>
                <tbody>
                  {qoderAccounts.map((a) => (
                    <tr key={a.id} className="border-t border-slate-200 dark:border-zinc-800">
                      <td className="px-4 py-2">{a.nickname || a.id}</td>
                      <td className="px-4 py-2">
                        <Badge tone={a.region === 'global' ? 'violet' : 'blue'}>
                          {a.region === 'global' ? '海外' : '国内'}
                        </Badge>
                      </td>
                      <td className="px-4 py-2 tabular-nums">{a.credits_balance ?? '—'}</td>
                      <td className="px-4 py-2">
                        {(a.free_models ?? []).length === 0 ? (
                          <span className="text-xs text-slate-300 dark:text-zinc-600">无免费档</span>
                        ) : (
                          <div className="flex flex-col gap-0.5">
                            {a.free_models!.map((m) => (
                              <div key={m.id} className="flex items-center gap-2">
                                <span className="text-xs">{m.name}</span>
                                <FreeUsageCell usage={freeOf('qoder', a.uid, m.id)} />
                                {m.exhausted_at && !freeOf('qoder', a.uid, m.id)?.exhausted_at && (
                                  <span className="text-xs text-amber-600 dark:text-amber-400">已用完</span>
                                )}
                              </div>
                            ))}
                          </div>
                        )}
                      </td>
                    </tr>
                  ))}
                  {qoderAccounts.length === 0 && (
                    <tr><td colSpan={4} className="px-4 py-3 text-center text-xs text-slate-400">无账号</td></tr>
                  )}
                </tbody>
              </table>
            </div>
          </section>

          {/* 每把 Key 今日用量 */}
          <section>
            <div className="mb-2 flex items-center gap-2 text-sm font-medium">
              <Gauge size={14} className="text-indigo-500" /> Key 今日用量
            </div>
            <div className="flex flex-wrap gap-2">
              {keys.map((k) => (
                <div
                  key={k.id}
                  className="rounded-lg border border-slate-200 bg-white px-3 py-2 text-xs dark:border-zinc-800 dark:bg-zinc-900"
                >
                  <div className="font-medium">{k.name}</div>
                  <div className="text-slate-400">今日 {k.used_today} 次{k.daily_limit > 0 ? ` / ${k.daily_limit}` : ''}</div>
                </div>
              ))}
            </div>
          </section>
        </div>
      ) : (
        <div className="rounded-xl border border-slate-200 dark:border-zinc-800">
          {issues.length === 0 ? (
            <div className="flex flex-col items-center gap-2 p-8 text-center">
              <ShieldCheck size={28} className="text-emerald-500" />
              <div className="text-sm font-medium">全部健康</div>
              <div className="text-xs text-slate-400">Buddy / Qoder 账号未发现需处理的问题</div>
            </div>
          ) : (
            <table className="w-full text-sm">
              <thead className="bg-slate-50 text-left text-xs text-slate-500 dark:bg-zinc-900 dark:text-zinc-400">
                <tr>
                  <th className="px-4 py-2 font-medium">家族</th>
                  <th className="px-4 py-2 font-medium">账号</th>
                  <th className="px-4 py-2 font-medium">问题</th>
                  <th className="px-4 py-2 font-medium">处理</th>
                </tr>
              </thead>
              <tbody>
                {issues.map((i, idx) => (
                  <tr key={idx} className="border-t border-slate-200 dark:border-zinc-800">
                    <td className="px-4 py-2"><Badge tone="slate">{i.family}</Badge></td>
                    <td className="px-4 py-2">{i.account}</td>
                    <td className="px-4 py-2">
                      <span className="inline-flex items-center gap-1.5">
                        <AlertTriangle size={13} className="text-amber-500" />
                        {i.detail}
                      </span>
                    </td>
                    <td className="px-4 py-2">
                      {i.action === 'qoder-refresh' && i.keyId ? (
                        <button
                          className="btn-outline !px-2 !py-1 text-xs"
                          disabled={refreshingQoder === i.keyId}
                          onClick={() => void refreshQoderCreds(i.keyId!, i.account)}
                        >
                          <RefreshCw size={12} className={refreshingQoder === i.keyId ? 'animate-spin' : ''} />
                          刷新凭证
                        </button>
                      ) : i.family === 'Buddy' && i.detail.includes('登录') ? (
                        <span className="text-xs text-slate-400">Buddy 页 → 重新登录</span>
                      ) : (
                        <span className="text-xs text-slate-400">关注或补充额度</span>
                      )}
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          )}
        </div>
      )}
    </div>
  );
}
