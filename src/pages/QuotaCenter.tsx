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
import { Activity, AlertTriangle, Gauge, RefreshCw, ShieldCheck, Stethoscope } from 'lucide-react';
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

/** 单个免费模型的用量/额度单元格。
 *
 * 双数据源口径：`usage` 是网关记账（free_model_usage kv，只记成功调用），
 * `snapshot` 是账号池快照（free_models[]，由上游错误码驱动标注 exhausted_at）。
 * 两者谁先到不一定——「第一个请求就撞限额」的模型只有快照有标注、kv 里没条目
 * （旧实现 kv 侧静默失败，2026-10-21 已改 upsert，但存量数据仍可能缺）。
 * 故展示取并集：任一为耗尽即显示「已用完」，不互相等待。
 */
function FreeUsageCell({
  usage,
  snapshot,
}: {
  usage?: { count?: number; exhausted_at?: string; reset_at?: string };
  snapshot?: { exhausted_at?: string | null; reset_at?: string | null };
}) {
  const exhaustedAt = usage?.exhausted_at ?? snapshot?.exhausted_at ?? null;
  const resetAt = usage?.reset_at ?? snapshot?.reset_at ?? null;
  const used = usage?.count;
  if (exhaustedAt) {
    return (
      <span className="text-xs text-amber-600 dark:text-amber-400">
        已用完（{exhaustedAt.slice(5, 16)}）{resetAt ? ` · ${resetAt.slice(5, 16)} 恢复` : ''}
      </span>
    );
  }
  if (used == null) {
    return <span className="text-xs text-slate-300 dark:text-zinc-600">—</span>;
  }
  return <span className="text-xs text-slate-500 dark:text-zinc-400">本周期已用 {used} 次</span>;
}

export default function QuotaCenter() {
  const pushToast = useAppStore((s) => s.pushToast);
  const [tab, setTab] = useState<'overview' | 'health' | 'events'>('overview');
  const [gwEvents, setGwEvents] = useState<string[]>([]);
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

  // 网关事件时间线（观测性 #1）：进入页签拉一次 + 页签打开期间每 5s 轮询；
  // 数据是内存环形缓冲（最近 300 条调度级事件），服务未运行时为空
  const refreshEvents = useCallback(async () => {
    try {
      setGwEvents(await api.apiServer.apiRecentEvents(120));
    } catch {
      setGwEvents([]);
    }
  }, []);
  useEffect(() => {
    if (tab !== 'events') return;
    void refreshEvents();
    const t = setInterval(() => void refreshEvents(), 5000);
    return () => clearInterval(t);
  }, [tab, refreshEvents]);

  // 体检问题清单（跨家族）：免费档**按模型逐条**上报——一个账号的两个免费
  // 模型一个用完一个可用是常态，合并成「全部耗尽」会同时掩盖两种事实：
  // 已耗尽的看不出是哪个、部分耗尽的根本不出现
  const issues = useMemo<Issue[]>(() => {
    const out: Issue[] = [];
    for (const a of wbAccounts) {
      if (a.needs_relogin) {
        out.push({ family: 'Buddy', account: a.nickname || a.id, detail: a.relogin_reason || '登录态失效，需重新登录' });
      }
      if (a.credits_balance != null && a.credits_balance < 50) {
        out.push({ family: 'Buddy', account: a.nickname || a.id, detail: `积分余额偏低：${a.credits_balance}` });
      }
      for (const m of a.free_models ?? []) {
        if (!m.exhausted_at) continue;
        out.push({
          family: 'Buddy',
          account: a.nickname || a.id,
          detail: `免费模型 ${m.name} 已用完${m.reset_at ? ` · ${m.reset_at.slice(5, 16)} 恢复` : ''}`,
        });
      }
    }
    for (const a of qoderAccounts) {
      if (a.needs_relogin) {
        out.push({ family: 'Qoder', account: a.nickname || a.id, detail: a.relogin_reason || '登录态失效，需重新登录', action: 'qoder-refresh', keyId: a.id });
      }
      if (a.credits_balance != null && a.credits_balance < 50) {
        out.push({ family: 'Qoder', account: a.nickname || a.id, detail: `积分余额偏低：${a.credits_balance}` });
      }
      for (const m of a.free_models ?? []) {
        if (!m.exhausted_at) continue;
        out.push({
          family: 'Qoder',
          account: a.nickname || a.id,
          detail: `免费模型 ${m.name} 已用完${m.reset_at ? ` · ${m.reset_at.slice(5, 16)} 恢复` : ''}`,
        });
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
          { key: 'events', label: '网关事件', icon: Activity },
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
                                <FreeUsageCell usage={freeOf('buddy', a.uid, m.id)} snapshot={m} />
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
                                <FreeUsageCell usage={freeOf('qoder', a.uid, m.id)} snapshot={m} />
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
      ) : tab === 'health' ? (
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
      ) : (
        /* 网关事件时间线（观测性 #1）：调度级事件（轮换/降权/让位/落池）
           内存环形缓冲直读，5s 轮询——「号为什么被换/被罚」从此一屏可见 */
        <div className="rounded-xl border border-slate-200 dark:border-zinc-800">
          <div className="flex items-center justify-between border-b border-slate-100 px-4 py-2 dark:border-zinc-800">
            <span className="text-xs text-slate-500 dark:text-zinc-400">
              最近调度级事件（新→旧，5s 自动刷新；仅本次启动后，上限 300 条）
            </span>
            <button
              className="flex items-center gap-1 text-xs text-slate-400 hover:text-brand-500"
              onClick={() => void refreshEvents()}
            >
              <RefreshCw size={12} /> 刷新
            </button>
          </div>
          {gwEvents.length === 0 ? (
            <div className="flex flex-col items-center gap-2 p-8 text-center">
              <Activity size={28} className="text-slate-300 dark:text-zinc-600" />
              <div className="text-sm font-medium">暂无事件</div>
              <div className="text-xs text-slate-400">
                网关未运行，或启动后还没有轮换/降权类调度动作
              </div>
            </div>
          ) : (
            <div className="max-h-[420px] overflow-y-auto p-3">
              {gwEvents.map((e, i) => (
                <div
                  key={i}
                  className="whitespace-pre-wrap break-all rounded px-2 py-1 font-mono text-[11px] leading-relaxed text-slate-600 hover:bg-slate-50 dark:text-zinc-300 dark:hover:bg-zinc-800/60"
                >
                  {e}
                </div>
              ))}
            </div>
          )}
        </div>
      )}
    </div>
  );
}
