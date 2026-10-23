/**
 * 邀请档案面板：把所有账号按邀请角色分类，集中展示邀请码、资格、邀请进度与奖励卡。
 *
 * 分类（与 ZCode 奖励中心口径一致）：
 * - 主号（inviter）：持有邀请码，可邀新号；展示已邀人数与三种卡进度
 * - 小号（invited）：经主号链接注册；展示三个新手任务完成度（决定主号能否拿到夜间卡）
 * - 未标记：既非主号也非小号
 *
 * 数据来自官方 referral/summary，点「同步资格」刷新。
 */
import { useCallback, useEffect, useState } from 'react';
import { RefreshCw, Link2, Copy, Check, Crown, Moon, CalendarDays, RotateCcw } from 'lucide-react';
import PageHeader from '../../components/PageHeader';
import { Badge, EmptyState } from '../../components/ui';
import { cn } from '../../lib/cn';
import { accountsApi, onAccountsChanged } from './api/accounts';
import { registerApi } from './api/register';
import InviteGraph from './InviteGraph';
import type { AccountLive } from './types';

/** 账号邀请档案：账号字段 + 归属主号邮箱（前端按 invited_by 关联得出） */
interface InviteArchiveRow extends AccountLive {
  inviter_label: string | null;
}

/** 新号三任务徽标 */
function TaskBadges({
  a,
  onComplete,
  completing,
}: {
  a: AccountLive;
  onComplete?: (includeLogin: boolean) => void;
  completing?: boolean;
}) {
  const tasks = [
    { key: 'task_login_device', label: '登录客户端', v: a.task_login_device },
    { key: 'task_bind_code', label: '绑邀请码', v: a.task_bind_code },
    { key: 'task_send_message', label: '发消息', v: a.task_send_message },
  ];
  const done = tasks.filter((t) => t.v === 1).length;
  const incomplete = done < 3;
  return (
    <span className="inline-flex flex-wrap items-center gap-1">
      <span
        className="inline-flex flex-wrap items-center gap-1"
        title={`新手任务 ${done}/3 完成（三个全完成，主号才拿到夜间蹬蹬卡）`}
      >
        <Badge tone={done === 3 ? 'green' : 'slate'}>任务 {done}/3</Badge>
        {tasks.map((t) => (
          <span
            key={t.key}
            className={cn(
              'rounded px-1.5 py-0.5 text-[11px]',
              t.v === 1
                ? 'bg-emerald-100 text-emerald-700 dark:bg-emerald-500/15 dark:text-emerald-300'
                : 'bg-slate-100 text-slate-400 dark:bg-zinc-800 dark:text-zinc-500',
            )}
            title={`${t.label}：${t.v === 1 ? '已完成' : '未完成'}`}
          >
            {t.label}
          </span>
        ))}
      </span>
      {incomplete && onComplete && (
        <span className="inline-flex items-center gap-1">
          <button
            className="chip bg-sky-100 text-sky-600 hover:bg-sky-200 dark:bg-sky-500/15 dark:text-sky-300"
            title="只补「发消息」（无副作用，任何时候都能做）"
            disabled={completing}
            onClick={() => onComplete(false)}
          >
            {completing ? '…' : '补发消息'}
          </button>
          <button
            className="chip bg-amber-100 text-amber-600 hover:bg-amber-200 dark:bg-amber-500/15 dark:text-amber-300"
            title="补「登录客户端」：会把本机当前登录账号切走并重启 ZCode 客户端（官方以此判定登录）"
            disabled={completing}
            onClick={() => onComplete(true)}
          >
            {completing ? '…' : '补登录'}
          </button>
        </span>
      )}
    </span>
  );
}

/** 主号的三种奖励卡进度 */
function RewardCards({ a }: { a: AccountLive }) {
  const nightDays = a.night_card_days ?? 0;
  const wkNow = a.weekend_progress ?? 0;
  const wkTarget = a.weekend_target ?? 2;
  const resetUsed = a.weekly_reset_used ?? 0;
  return (
    <div className="flex flex-wrap items-center gap-1.5">
      <Badge
        tone={nightDays > 0 ? 'amber' : 'slate'}
        title="夜间蹬蹬卡：每邀 1 个完成新手任务的新号 +1 天，本期上限 100 天"
      >
        <Moon size={11} /> 夜间卡 {nightDays} 天
      </Badge>
      <Badge
        tone={wkNow >= wkTarget ? 'green' : 'slate'}
        title={`周末蹬蹬卡：本周邀满 ${wkTarget} 个新号即可参与本周周末活动（每周重置）`}
      >
        <CalendarDays size={11} /> 周末卡 {wkNow}/{wkTarget}
      </Badge>
      {resetUsed > 0 && (
        <Badge tone="violet" title="周重置卡：Coding Plan 订阅用户，本期邀满 10 个发一次">
          <RotateCcw size={11} /> 周重置 {resetUsed}
        </Badge>
      )}
    </div>
  );
}

export default function InviteArchivePanel() {
  const [rows, setRows] = useState<InviteArchiveRow[]>([]);
  const [loading, setLoading] = useState(false);
  const [notice, setNotice] = useState<string | null>(null);
  const [copiedId, setCopiedId] = useState<number | null>(null);
  // 正在补新手任务的账号 id
  const [completingId, setCompletingId] = useState<number | null>(null);
  // 邀请关系画布展开状态（默认展开：关系是这一页最有信息量的部分）
  const [showGraph, setShowGraph] = useState(true);

  const reload = useCallback(() => {
    accountsApi
      .list()
      .then((list) => {
        const labelOf = new Map(list.map((a) => [a.id, a.label]));
        setRows(
          list.map((a) => ({
            ...a,
            nickname: null,
            email: a.label.includes('@') ? a.label : null,
            inviter_label: a.invited_by != null ? labelOf.get(a.invited_by) ?? null : null,
            plans: [],
            balances: [],
            claimables: [],
            balance_error: null,
            updated_at: null,
          })),
        );
      })
      .catch(() => {});
  }, []);

  useEffect(() => {
    reload();
    const unsub = onAccountsChanged(() => reload());
    return () => {
      unsub.then((f) => f());
    };
  }, [reload]);

  const sync = async () => {
    setLoading(true);
    setNotice(null);
    try {
      const r = await accountsApi.syncInviteQualification();
      setNotice(`✅ 已查询 ${r.synced} 个账号：可邀请 ${r.active}，暂未获得资格 ${r.locked}`);
      reload();
    } catch (e) {
      setNotice(`❌ ${String(e)}`);
    } finally {
      setLoading(false);
    }
  };

  const copyLink = async (a: InviteArchiveRow) => {
    if (!a.invite_code) return;
    const link = `https://zcode.z.ai/invite/${a.invite_code}`;
    try {
      await navigator.clipboard.writeText(link);
      setCopiedId(a.id);
      window.setTimeout(() => setCopiedId(null), 1500);
    } catch {
      /* 剪贴板不可用时静默 */
    }
  };

  // 补新手任务:includeLogin=true 会切换本机登录态并重启客户端
  const completeTasks = async (a: InviteArchiveRow, includeLogin: boolean) => {
    setNotice(null);
    setCompletingId(a.id);
    try {
      const r = await registerApi.completeTasks(a.id, includeLogin);
      const parts: string[] = [];
      if (r.send_message) parts.push('已发送消息');
      if (r.login_device) parts.push('已登录客户端');
      setNotice(`✅ ${a.label}：${parts.length > 0 ? parts.join(' + ') : '无操作'}${r.message ? `（${r.message}）` : ''}`);
      reload();
    } catch (e) {
      setNotice(`❌ ${String(e)}`);
    } finally {
      setCompletingId(null);
    }
  };

  // 分类
  const inviters = rows.filter((a) => a.invite_role === 'inviter');
  const invitees = rows.filter((a) => a.invite_role === 'invited');
  const unmarked = rows.filter((a) => a.invite_role !== 'inviter' && a.invite_role !== 'invited');
  // 可用作主号的账号:不限历史角色,只看当前是否有码 + 资格 active
  // (小号过段时间也会获得邀请资格,届时就出现在这里)
  const canInvite = rows.filter((a) => a.invite_state === 'active' && a.invite_code);
  // 合计
  const totalNight = rows.reduce((s, a) => s + (a.night_card_days ?? 0), 0);
  const totalInvited = rows.reduce((s, a) => s + (a.invited_count ?? 0), 0);
  // 还差几个新号就能拿周末卡的主号
  const weekendReady = canInvite.filter(
    (a) => (a.weekend_progress ?? 0) >= (a.weekend_target ?? 2),
  ).length;

  if (rows.length === 0) {
    return (
      <EmptyState
        icon={<Link2 size={26} />}
        title="暂无账号"
        hint="先在「ZCode 账号」页导入账号，再回来同步邀请资格。"
      />
    );
  }

  return (
    <div className="animate-fade-in space-y-4">
      <PageHeader
        title="ZCode · 邀请档案"
        desc="主号邀请码与奖励卡进度 · 小号新手任务 · 未标记账号归档"
        actions={
          <button className="btn-outline" onClick={sync} disabled={loading}>
            <RefreshCw size={15} className={loading ? 'animate-spin' : ''} />
            {loading ? '同步中…' : '同步资格'}
          </button>
        }
      />

      <div className="flex flex-wrap items-center gap-2">
        <button className="btn-outline" onClick={sync} disabled={loading}>
          <RefreshCw size={15} className={loading ? 'animate-spin' : ''} />
          {loading ? '同步中…' : '同步资格'}
        </button>
        <span className="text-xs text-slate-400">
          共 {rows.length} 个账号 · 主号 {inviters.length} · 小号 {invitees.length} · 未标记{' '}
          {unmarked.length}
        </span>
        <span className="ml-auto flex flex-wrap items-center gap-1.5 text-xs text-slate-400">
          <span title="所有账号已发放的夜间蹬蹬卡天数合计">
            夜间卡合计 <b className="text-slate-600 dark:text-zinc-200">{totalNight}</b> 天
          </span>
          <span title="所有账号已成功邀请的新号数合计">
            已邀新号合计 <b className="text-slate-600 dark:text-zinc-200">{totalInvited}</b>
          </span>
          <span title="当前有邀请资格的账号数（含已转正的小号）/ 周末卡已达标数">
            可邀请 <b className="text-slate-600 dark:text-zinc-200">{canInvite.length}</b> · 周末卡达标{' '}
            <b className="text-slate-600 dark:text-zinc-200">{weekendReady}</b>
          </span>
        </span>
      </div>

      {notice && (
        <div
          className={cn(
            'rounded-xl border px-4 py-3 text-sm',
            notice.startsWith('✅')
              ? 'border-emerald-200 bg-emerald-50 text-emerald-700 dark:border-emerald-500/30 dark:bg-emerald-500/10 dark:text-emerald-300'
              : 'border-rose-200 bg-rose-50 text-rose-600 dark:border-rose-500/30 dark:bg-rose-500/10 dark:text-rose-300',
          )}
        >
          {notice}
        </div>
      )}

      {/* 邀请关系画布（2026-10-22）：列表之上先给全局关系，再按角色给明细。
          画布用 invited_by 推导层级，能表达「小号转正后成为别人主号」的中间节点；
          列表视图保留（操作按钮都在那里，画布只做关系呈现与点选定位）。 */}
      <section>
        <div className="mb-2 flex items-center justify-between">
          <h3 className="flex items-center gap-1.5 text-sm font-medium text-slate-700 dark:text-zinc-200">
            <Link2 size={15} className="text-indigo-500" /> 邀请关系图
            <span className="ml-1 text-xs font-normal text-slate-400">
              共 {rows.length} 个节点 · 实线=已绑定，虚线=仅记录归属 · 可拖动节点
            </span>
          </h3>
          <button
            className="text-xs text-slate-400 hover:text-slate-600 dark:hover:text-zinc-300"
            onClick={() => setShowGraph((v) => !v)}
          >
            {showGraph ? '收起' : '展开'}
          </button>
        </div>
        {showGraph && <InviteGraph rows={rows} />}
      </section>

      {/* 主号 */}
      {inviters.length > 0 && (
        <section>
          <h3 className="mb-2 flex items-center gap-1.5 text-sm font-medium text-slate-700 dark:text-zinc-200">
            <Crown size={15} className="text-amber-500" /> 主号（邀请者）
          </h3>
          <div className="space-y-2">
            {inviters.map((a) => (
              <div
                key={a.id}
                className="rounded-xl border border-slate-200 bg-white p-3 dark:border-zinc-800 dark:bg-zinc-900/60"
              >
                <div className="flex flex-wrap items-center gap-2">
                  <span className="text-sm font-medium text-slate-800 dark:text-zinc-100">
                    {a.nickname ?? a.label}
                  </span>
                  {a.invite_state === 'active' ? (
                    <Badge tone="green" title="已获得邀请资格">
                      👑 可邀请
                    </Badge>
                  ) : a.invite_state === 'locked' ? (
                    <Badge tone="slate" title="该账号尚未获得邀请资格（官方按账号年龄/邀请人数判定）">
                      ⏳ 暂未拥有邀请资格
                    </Badge>
                  ) : (
                    <Badge tone="slate">资格未查</Badge>
                  )}
                  <span className="text-xs text-slate-400">已邀 {a.invited_count ?? 0} 个新号</span>
                  <span className="ml-auto flex items-center gap-1.5">
                    {a.invite_code && (
                      <>
                        <code className="rounded bg-slate-100 px-1.5 py-0.5 font-mono text-xs text-slate-600 dark:bg-zinc-800 dark:text-zinc-300">
                          {a.invite_code}
                        </code>
                        <button
                          className="chip bg-slate-100 text-slate-500 hover:bg-slate-200 dark:bg-zinc-800 dark:text-zinc-400"
                          title={`复制邀请链接 https://zcode.z.ai/invite/${a.invite_code}`}
                          onClick={() => copyLink(a)}
                        >
                          {copiedId === a.id ? <Check size={11} /> : <Copy size={11} />}
                          {copiedId === a.id ? '已复制' : '复制链接'}
                        </button>
                      </>
                    )}
                  </span>
                </div>
                <div className="mt-2">
                  <RewardCards a={a} />
                </div>
              </div>
            ))}
          </div>
        </section>
      )}

      {/* 小号 */}
      {invitees.length > 0 && (
        <section>
          <h3 className="mb-2 flex items-center gap-1.5 text-sm font-medium text-slate-700 dark:text-zinc-200">
            <Link2 size={15} className="text-sky-500" /> 小号（被邀请者）
          </h3>
          <p className="mb-2 text-xs text-slate-400">
            小号过段时间也会获得自己的邀请资格（官方按账号年龄/邀请数判定），届时即可当主号邀下一批。资格与卡进度同主号口径实时同步。
          </p>
          <div className="space-y-2">
            {invitees.map((a) => (
              <div
                key={a.id}
                className="rounded-xl border border-slate-200 bg-white p-3 dark:border-zinc-800 dark:bg-zinc-900/60"
              >
                <div className="flex flex-wrap items-center gap-2">
                  <span className="text-sm font-medium text-slate-800 dark:text-zinc-100">
                    {a.nickname ?? a.label}
                  </span>
                  <span className="text-xs text-slate-400" title="归属的主号">
                    归属 {a.inviter_label ?? '未知主号'}
                  </span>
                  {/* 小号自己的邀请能力：有码且 active 就能当主号 */}
                  {a.invite_state === 'active' && a.invite_code ? (
                    <Badge tone="amber" title="该小号已获得邀请资格，可当主号邀下一批">
                      👑 可邀请
                    </Badge>
                  ) : a.invite_state === 'locked' ? (
                    <Badge tone="slate" title="该小号尚未获得邀请资格（官方按账号年龄/邀请人数判定）">
                      ⏳ 暂未拥有邀请资格
                    </Badge>
                  ) : (
                    <Badge tone="slate">资格未查</Badge>
                  )}
                  <span className="ml-auto flex items-center gap-1.5">
                    {a.invite_code && a.invite_state === 'active' && (
                      <>
                        <code className="rounded bg-slate-100 px-1.5 py-0.5 font-mono text-xs text-slate-600 dark:bg-zinc-800 dark:text-zinc-300">
                          {a.invite_code}
                        </code>
                        <button
                          className="chip bg-slate-100 text-slate-500 hover:bg-slate-200 dark:bg-zinc-800 dark:text-zinc-400"
                          title={`复制邀请链接 https://zcode.z.ai/invite/${a.invite_code}`}
                          onClick={() => copyLink(a)}
                        >
                          {copiedId === a.id ? <Check size={11} /> : <Copy size={11} />}
                          {copiedId === a.id ? '已复制' : '复制链接'}
                        </button>
                      </>
                    )}
                    <TaskBadges
                      a={a}
                      completing={completingId === a.id}
                      onComplete={(includeLogin) => completeTasks(a, includeLogin)}
                    />
                  </span>
                </div>
                {/* 小号自己邀到人时也有卡进度 */}
                {(a.invited_count ?? 0) > 0 && (
                  <div className="mt-2">
                    <RewardCards a={a} />
                  </div>
                )}
              </div>
            ))}
          </div>
        </section>
      )}

      {/* 未标记 */}
      {unmarked.length > 0 && (
        <section>
          <h3 className="mb-2 text-sm font-medium text-slate-500 dark:text-zinc-400">
            未标记邀请关系（{unmarked.length}）
          </h3>
          <p className="mb-2 text-xs text-slate-400">
            这些账号没有官方邀请记录，但大多已有自己的邀请码——可直接当主号用（在「自动注册」里选它作为捆绑目标）。
          </p>
          <div className="grid gap-2 md:grid-cols-2">
            {unmarked.map((a) => (
              <div
                key={a.id}
                className="flex flex-wrap items-center gap-2 rounded-xl border border-slate-200 bg-white p-3 dark:border-zinc-800 dark:bg-zinc-900/60"
              >
                <span className="truncate text-sm text-slate-700 dark:text-zinc-200">
                  {a.nickname ?? a.label}
                </span>
                {a.invite_state === 'active' && a.invite_code ? (
                  <>
                    <Badge tone="amber" title="已获得邀请资格，可当主号">
                      👑 可邀请
                    </Badge>
                    <span className="ml-auto flex items-center gap-1.5">
                      <code className="rounded bg-slate-100 px-1.5 py-0.5 font-mono text-xs text-slate-500 dark:bg-zinc-800 dark:text-zinc-400">
                        {a.invite_code}
                      </code>
                      <button
                        className="chip bg-slate-100 text-slate-500 hover:bg-slate-200 dark:bg-zinc-800 dark:text-zinc-400"
                        title={`复制邀请链接 https://zcode.z.ai/invite/${a.invite_code}`}
                        onClick={() => copyLink(a)}
                      >
                        {copiedId === a.id ? <Check size={11} /> : <Copy size={11} />}
                        {copiedId === a.id ? '已复制' : '复制链接'}
                      </button>
                    </span>
                  </>
                ) : a.invite_code ? (
                  <code className="ml-auto rounded bg-slate-100 px-1.5 py-0.5 font-mono text-xs text-slate-500 dark:bg-zinc-800 dark:text-zinc-400">
                    {a.invite_code}
                  </code>
                ) : null}
              </div>
            ))}
          </div>
        </section>
      )}
    </div>
  );
}
