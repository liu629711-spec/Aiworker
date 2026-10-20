/**
 * ZCode 账号工作台（zai-claim 功能，按 AI Work Studio 设计语言重写）。
 *
 * 子视图：ZCode 账号（余额监控/切换/邀请关系分组） | WorkBuddy（多账号捕获/切换/海外登录）。
 * 数据逻辑与 Rust 命令（zcode_* / zcode_wb_*）一一对应。
 */
import { useCallback, useEffect, useRef, useState } from 'react';
import {
  Users,
  RefreshCw,
  Plus,
  Download,
  Camera,
  Trash2,
  SwitchCamera,
  Link2,
  Tag,
  ChevronDown,
  ChevronRight,
  MonitorCheck,
  CircleAlert,
  Info,
  KeyRound,
  Globe,
} from 'lucide-react';
import PageHeader from '../../components/PageHeader';
import { Badge, EmptyState, StatCard } from '../../components/ui';
import { cn } from '../../lib/cn';
import { accountsApi, onAccountsChanged } from './api/accounts';
import { monitorApi, onRefreshing, onSnapshot } from './api/monitor';
import type { Refreshing } from './api/monitor';
import SecretsPanel from './SecretsPanel';
import WbOverseasPanel from './WbOverseasPanel';
import OverseasPoolPanel from './OverseasPoolPanel';
import { fmtExpire, fmtTime, fmtUnits } from './format';
import type { Account, AccountLive, Snapshot } from './types';

type Tone = 'slate' | 'brand' | 'green' | 'amber' | 'red' | 'blue' | 'violet';

/** 邀请关系徽标：已标记（官方同步绑定）为纯展示；仅未标记可点击设为邀请者 */
function RoleBadge({
  role,
  onMark,
}: {
  role: string | null;
  onMark?: () => void;
}) {
  if (role === 'inviter') {
    return (
      <Badge tone="amber" title="邀请者（主号，持有邀请链接）——关系由官方同步锁定">
        👑 邀请者
      </Badge>
    );
  }
  if (role === 'invited') {
    return (
      <Badge tone="blue" title="被邀请者（经主号链接注册）——关系由官方同步锁定">
        🔗 被邀请
      </Badge>
    );
  }
  return (
    <button
      className="chip bg-slate-100 text-slate-500 hover:bg-slate-200 dark:bg-zinc-800 dark:text-zinc-400"
      title="标记邀请关系：点击设为邀请者（主号）"
      onClick={onMark}
    >
      <Tag size={11} /> 标记
    </button>
  );
}

/** 图标小按钮（行内操作） */
function IconBtn({
  icon,
  title,
  onClick,
  disabled,
  danger,
  active,
  label,
}: {
  icon: React.ReactNode;
  title: string;
  onClick: (e: React.MouseEvent) => void;
  disabled?: boolean;
  danger?: boolean;
  active?: boolean;
  label?: string;
}) {
  return (
    <button
      className={cn(
        'inline-flex items-center gap-1 rounded-lg px-2 py-1.5 text-xs font-medium transition active:scale-95 disabled:cursor-not-allowed disabled:opacity-50',
        danger
          ? 'text-rose-500 hover:bg-rose-50 dark:hover:bg-rose-500/10'
          : active
            ? 'bg-emerald-100 text-emerald-700 dark:bg-emerald-500/15 dark:text-emerald-300'
            : 'text-slate-500 hover:bg-slate-100 dark:text-zinc-400 dark:hover:bg-zinc-800',
      )}
      title={title}
      onClick={onClick}
      disabled={disabled}
    >
      {icon}
      {label && <span>{label}</span>}
    </button>
  );
}

/** 剩余量进度条：满额=满槽，用完=空槽 */
function UsageBar({ b }: { b: AccountLive['balances'][number] }) {
  const pct =
    b.total_units > 0 ? Math.round((b.used_units / b.total_units) * 100) : 0;
  const rem =
    b.total_units > 0 ? Math.round((b.remaining_units / b.total_units) * 100) : 0;
  const width = Math.min(Math.max(rem, 0), 100);
  return (
    <div>
      <div className="flex items-center justify-between text-xs">
        <span className="font-medium text-slate-700 dark:text-zinc-200">
          {b.show_name}
        </span>
        <span className="tabular-nums text-slate-400">
          {pct >= 100
            ? '已用完'
            : `剩 ${fmtUnits(b.remaining_units)} / ${fmtUnits(b.total_units)}（已用 ${pct}%）`}
        </span>
      </div>
      <div className="mt-1 h-1.5 overflow-hidden rounded-full bg-slate-100 dark:bg-zinc-800">
        <div
          className={cn(
            'h-full rounded-full transition-all',
            width <= 10
              ? 'bg-rose-400'
              : width <= 30
                ? 'bg-amber-400'
                : 'bg-emerald-500',
          )}
          style={{ width: `${width}%` }}
        />
      </div>
    </div>
  );
}

/** 单个账号卡片：自管套餐 tab 选中态、进度条区滚动与切换确认 */
function AccountCard({
  account,
  checking,
  switching,
  onCheck,
  onRemove,
  onSetRole,
  onSwitch,
}: {
  account: AccountLive;
  checking: boolean;
  switching: boolean;
  onCheck: (id: number) => void;
  onRemove: (id: number) => void;
  onSetRole: (id: number, role: '' | 'inviter' | 'invited', invitedBy?: number) => void;
  onSwitch: (id: number) => void;
}) {
  const [activePlanId, setActivePlanId] = useState<string | null>(null);
  const [confirming, setConfirming] = useState(false);
  const confirmTimer = useRef<number | undefined>(undefined);
  const barsRef = useRef<HTMLDivElement>(null);

  // 切换是重操作（重启 ZCode），两步确认：3 秒内再点一次才执行
  const switchClick = () => {
    if (!confirming) {
      setConfirming(true);
      confirmTimer.current = window.setTimeout(() => setConfirming(false), 3000);
      return;
    }
    window.clearTimeout(confirmTimer.current);
    setConfirming(false);
    onSwitch(account.id);
  };

  // 当前选中的套餐 tab（未选/失效时取第一个）
  const plan = account.plans.find((p) => p.plan_id === activePlanId) ?? account.plans[0];

  // 该套餐下的模型用量条：优先按 entitlement_id 归属精确匹配；旧快照退回按模型名过滤
  const planEntIds = new Set(
    (plan?.entitlements ?? [])
      .map((e) => e.entitlement_id)
      .filter((x): x is string => !!x),
  );
  const matchById =
    planEntIds.size > 0 &&
    account.balances.some((b) => b.entitlement_id && planEntIds.has(b.entitlement_id));
  const planModels = new Set(plan?.entitlements.map((e) => e.show_name) ?? []);
  const bars = !plan
    ? account.balances
    : matchById
      ? account.balances.filter((b) => b.entitlement_id && planEntIds.has(b.entitlement_id))
      : account.balances.filter((b) => planModels.has(b.show_name));

  // 快照更新/切换 tab 后，滚动到当前正在使用的模型（有用量者）
  useEffect(() => {
    const el = barsRef.current;
    if (!el) return;
    const inUse = Array.from(el.children).find(
      (c) => c.getAttribute('data-used') === '1',
    );
    if (inUse) {
      el.scrollTop = (inUse as HTMLElement).offsetTop;
    } else {
      el.scrollTop = 0;
    }
  }, [account.balances, plan?.plan_id]);

  return (
    <div
      className={cn(
        'card p-4',
        account.is_current && 'ring-2 ring-emerald-500/40 dark:ring-emerald-500/30',
      )}
    >
      {/* 头部：徽标 + 名称 + 操作 */}
      <div className="flex items-center gap-2">
        <RoleBadge
          role={account.invite_role}
          onMark={() => onSetRole(account.id, 'inviter')}
        />
        <span
          className="truncate text-sm font-semibold text-slate-800 dark:text-zinc-100"
          title={account.label}
        >
          {account.nickname ?? account.label}
        </span>
        {account.is_current && <Badge tone="green">使用中</Badge>}
        <span className="ml-auto shrink-0 text-xs text-slate-400">
          {fmtTime(account.updated_at)}
        </span>
        <IconBtn
          icon={<SwitchCamera size={14} />}
          label={switching ? '…' : confirming ? '确认?' : '切换'}
          title={
            confirming
              ? '再次点击确认：写入凭证并重启 ZCode'
              : '切换到此账号（重启 ZCode）'
          }
          onClick={switchClick}
          disabled={switching}
          active={confirming}
        />
        <IconBtn
          icon={<RefreshCw size={14} className={checking ? 'animate-spin' : ''} />}
          title="检测此账号"
          onClick={() => onCheck(account.id)}
          disabled={checking}
        />
        <IconBtn
          icon={<Trash2 size={14} />}
          title="删除"
          danger
          onClick={() => onRemove(account.id)}
        />
      </div>

      {/* 登录邮箱：与显示名不同时展示一行小字 */}
      {account.email && account.email !== (account.nickname ?? account.label) && (
        <div className="mt-0.5 truncate text-xs text-slate-400" title={account.email}>
          {account.email}
        </div>
      )}

      {account.balance_error ? (
        <div className="mt-3 flex items-center gap-1.5 text-xs text-rose-500">
          <CircleAlert size={13} /> 查询失败：{account.balance_error}
        </div>
      ) : (
        <>
          {plan && account.plans.length > 0 && (
            <div className="mt-3 flex flex-wrap items-center gap-1.5">
              {account.plans.map((p) => (
                <button
                  key={p.plan_id}
                  className={cn(
                    'rounded-full px-2.5 py-0.5 text-xs font-medium transition',
                    p.plan_id === plan.plan_id
                      ? 'bg-zinc-900 text-white dark:bg-white dark:text-zinc-900'
                      : 'bg-slate-100 text-slate-500 hover:bg-slate-200 dark:bg-zinc-800 dark:text-zinc-400',
                  )}
                  onClick={() => setActivePlanId(p.plan_id)}
                >
                  {p.name}
                </button>
              ))}
              <span className="ml-1 text-xs text-slate-400">
                到期 {fmtExpire(plan.ends_at)}
              </span>
            </div>
          )}

          {/* 进度条区定高（两根条）：条数多则滚动并自动定位到使用中的模型 */}
          <div
            ref={barsRef}
            className={cn(
              'mt-3 space-y-3 overflow-y-auto pr-1',
              bars.length > 2 ? 'max-h-32' : '',
            )}
          >
            {bars.length === 0 ? (
              <p className="py-2 text-xs text-slate-400">
                {account.plans.length === 0 ? '暂无任何套餐' : '暂无用量数据'}
              </p>
            ) : (
              bars.map((b) => (
                <div
                  key={b.bucket_id ?? `${b.show_name}-${b.entitlement_id ?? b.total_units}`}
                  data-used={b.used_units > 0 ? '1' : '0'}
                >
                  <UsageBar b={b} />
                </div>
              ))
            )}
          </div>
        </>
      )}
    </div>
  );
}

/** 小号紧凑单行：默认一行（名称+剩余量+操作），点开箭头展开完整卡片 */
function InviteeRow({
  account,
  expanded,
  onToggle,
  checking,
  switching,
  onCheck,
  onRemove,
  onSetRole,
  onSwitch,
}: {
  account: AccountLive;
  expanded: boolean;
  onToggle: () => void;
  checking: boolean;
  switching: boolean;
  onCheck: (id: number) => void;
  onRemove: (id: number) => void;
  onSetRole: (id: number, role: '' | 'inviter' | 'invited', invitedBy?: number) => void;
  onSwitch: (id: number) => void;
}) {
  const [confirming, setConfirming] = useState(false);
  const confirmTimer = useRef<number | undefined>(undefined);

  // 与主卡片一致：切换是重操作（重启 ZCode），3 秒内两步确认
  const switchClick = () => {
    if (!confirming) {
      setConfirming(true);
      confirmTimer.current = window.setTimeout(() => setConfirming(false), 3000);
      return;
    }
    window.clearTimeout(confirmTimer.current);
    setConfirming(false);
    onSwitch(account.id);
  };

  const remaining = account.balances.reduce((s, b) => s + b.remaining_units, 0);

  return (
    <div
      className={cn(
        'rounded-xl border bg-white dark:bg-zinc-900/60',
        account.is_current
          ? 'border-emerald-500/40 dark:border-emerald-500/30'
          : 'border-slate-200/70 dark:border-zinc-800',
      )}
    >
      {/* 整行可点展开/收起；操作按钮上阻断冒泡避免误触发 */}
      <div
        className="row-hover flex cursor-pointer items-center gap-2 rounded-xl px-3 py-2"
        onClick={onToggle}
      >
        {expanded ? (
          <ChevronDown size={14} className="shrink-0 text-slate-400" />
        ) : (
          <ChevronRight size={14} className="shrink-0 text-slate-400" />
        )}
        <Link2 size={13} className="shrink-0 text-sky-500" />
        <span
          className="truncate text-sm text-slate-700 dark:text-zinc-200"
          title={account.email ?? account.label}
        >
          {account.nickname ?? account.label}
        </span>
        {account.balance_error ? (
          <span className="shrink-0 text-xs text-rose-500" title={account.balance_error}>
            查询失败
          </span>
        ) : (
          <span className="shrink-0 text-xs tabular-nums text-slate-400" title="剩余总量">
            剩 {fmtUnits(remaining)}
          </span>
        )}
        {account.is_current && <Badge tone="green">使用中</Badge>}
        <span className="ml-auto shrink-0" />
        <IconBtn
          icon={<SwitchCamera size={14} />}
          title={
            confirming
              ? '再次点击确认：写入凭证并重启 ZCode'
              : '切换到此账号（重启 ZCode）'
          }
          onClick={(e) => {
            e.stopPropagation();
            switchClick();
          }}
          disabled={switching}
          active={confirming}
        />
        <IconBtn
          icon={<RefreshCw size={14} className={checking ? 'animate-spin' : ''} />}
          title="检测此账号"
          onClick={(e) => {
            e.stopPropagation();
            onCheck(account.id);
          }}
          disabled={checking}
        />
        <IconBtn
          icon={<Trash2 size={14} />}
          title="删除"
          danger
          onClick={(e) => {
            e.stopPropagation();
            onRemove(account.id);
          }}
        />
      </div>
      {expanded && (
        <div className="border-t border-slate-100 p-3 dark:border-zinc-800">
          <AccountCard
            account={account}
            checking={checking}
            switching={switching}
            onCheck={onCheck}
            onRemove={onRemove}
            onSetRole={onSetRole}
            onSwitch={onSwitch}
          />
        </div>
      )}
    </div>
  );
}

export default function ZCodePage() {
  const [accounts, setAccounts] = useState<Account[]>([]);
  const [snapshot, setSnapshot] = useState<Snapshot | null>(null);
  const [importing, setImporting] = useState<'idle' | 'oauth' | 'local'>('idle');
  const [actionError, setActionError] = useState<string | null>(null);
  // 余额刷新进行中（启动首轮与手动刷新共用，Rust 每轮开始/逐账号/结束广播）
  const [refreshing, setRefreshing] = useState<Refreshing>({
    active: false,
    done: 0,
    total: 0,
  });
  // OAuth 添加账号流程进行中的注册提示条（可手动忽略）
  const [oauthNotice, setOauthNotice] = useState(false);
  const [checking, setChecking] = useState<Set<number>>(new Set());
  const [switching, setSwitching] = useState<Set<number>>(new Set());
  const [syncingInvite, setSyncingInvite] = useState(false);
  // 顶层视图切换：zcode=账号管理 / secrets=账密与批量登录
  const [view, setView] = useState<'zcode' | 'secrets' | 'wb' | 'overseas'>('zcode');

  const refreshAccounts = useCallback(() => {
    accountsApi.list().then(setAccounts).catch((e) => console.error(e));
  }, []);

  useEffect(() => {
    refreshAccounts();
    monitorApi.snapshot().then(setSnapshot).catch((e) => console.error(e));
    const unsubs = [
      onAccountsChanged(() => refreshAccounts()),
      onSnapshot((s) => setSnapshot(s)),
      onRefreshing(setRefreshing),
    ];
    return () => {
      unsubs.forEach((p) => p.then((unlisten) => unlisten()));
    };
  }, [refreshAccounts]);

  const importAccount = async (mode: 'oauth' | 'local') => {
    setImporting(mode);
    if (mode === 'oauth') {
      setOauthNotice(true);
    }
    setActionError(null);
    try {
      if (mode === 'oauth') {
        await accountsApi.addViaOAuth();
      } else {
        await accountsApi.importLocal();
      }
    } catch (e) {
      const msg = String(e);
      if (!msg.includes('已取消')) {
        setActionError(msg);
      }
    } finally {
      setImporting('idle');
      setOauthNotice(false);
    }
  };

  const cancelOAuth = () => {
    accountsApi.cancelOAuth().catch((e) => console.error(e));
  };

  // 触发一轮全量刷新即可：各账号探完即经快照事件逐个更新卡片
  const refreshAll = () => {
    monitorApi.refreshNow().catch((e) => setActionError(String(e)));
  };

  const check = async (id: number) => {
    setChecking((s) => new Set(s).add(id));
    try {
      setSnapshot(await monitorApi.checkAccount(id));
    } catch (e) {
      setActionError(String(e));
    } finally {
      setChecking((s) => {
        const next = new Set(s);
        next.delete(id);
        return next;
      });
    }
  };

  const removeAccount = async (id: number) => {
    try {
      await accountsApi.remove(id);
    } catch (e) {
      setActionError(String(e));
    }
  };

  // 标记邀请关系，落库后事件驱动刷新
  const setInviteRole = (
    id: number,
    role: '' | 'inviter' | 'invited',
    invitedBy?: number,
  ) => {
    accountsApi
      .setInviteRole(id, role, invitedBy ?? null)
      .catch((e) => setActionError(String(e)));
  };

  // 自动同步邀请关系：官方接口拉邀请记录，主号/小号自动标记与绑定
  const syncInvite = async () => {
    setActionError(null);
    setSyncingInvite(true);
    try {
      const r = await accountsApi.syncInvite();
      const parts = [`确认主号 ${r.inviters} 个`, `自动绑定 ${r.linked} 个`];
      if (r.unmatched.length > 0) {
        parts.push(`未匹配（小号未导入）: ${r.unmatched.join('、')}`);
      }
      if (r.errors.length > 0) {
        parts.push(`失败: ${r.errors.join('、')}`);
      }
      setActionError(`✅ ${parts.join(' | ')}`);
    } catch (e) {
      setActionError(String(e));
    } finally {
      setSyncingInvite(false);
    }
  };

  const switchAccount = async (id: number) => {
    setActionError(null);
    setSwitching((s) => new Set(s).add(id));
    try {
      await accountsApi.switchTo(id);
    } catch (e) {
      setActionError(String(e));
    } finally {
      setSwitching((s) => {
        const next = new Set(s);
        next.delete(id);
        return next;
      });
    }
  };

  // 渲染以快照为准；首个快照到达前用 DB 行占位
  const source: AccountLive[] =
    snapshot?.accounts ??
    accounts.map((a) => ({
      id: a.id,
      label: a.label,
      nickname: null,
      email: null,
      invite_role: a.invite_role ?? null,
      invited_by: null,
      is_current: a.is_current,
      plans: [],
      balances: [],
      claimables: [],
      balance_error: null,
      updated_at: null,
    }));
  // 分组视图：邀请者（主号）为组头，其被邀请的小号以紧凑单行挂在组内
  const totalRemaining = (x: AccountLive) =>
    x.balances.reduce((s, b) => s + b.remaining_units, 0);
  const inviters = source.filter((a) => a.invite_role === 'inviter');
  const families = inviters
    .map((inv) => ({
      inviter: inv,
      invitees: source
        .filter((a) => a.invite_role === 'invited' && a.invited_by === inv.id)
        .sort((a, b) => totalRemaining(b) - totalRemaining(a) || a.id - b.id),
    }))
    .sort(
      (x, y) =>
        Number(y.inviter.is_current) - Number(x.inviter.is_current) ||
        totalRemaining(y.inviter) - totalRemaining(x.inviter) ||
        x.inviter.id - y.inviter.id,
    );
  const inviterIds = new Set(inviters.map((i) => i.id));
  const looseInvitees = source
    .filter(
      (a) =>
        a.invite_role === 'invited' &&
        (!a.invited_by || !inviterIds.has(a.invited_by)),
    )
    .sort((a, b) => totalRemaining(b) - totalRemaining(a) || a.id - b.id);
  const unmarked = source
    .filter((a) => a.invite_role !== 'inviter' && a.invite_role !== 'invited')
    .sort((a, b) => totalRemaining(b) - totalRemaining(a) || a.id - b.id);
  // 小号紧凑行的展开态（展开后显示完整卡片）
  const [expanded, setExpanded] = useState<Set<number>>(new Set());
  const toggleExpand = (id: number) =>
    setExpanded((s) => {
      const n = new Set(s);
      if (n.has(id)) n.delete(id);
      else n.add(id);
      return n;
    });

  const currentCount = source.filter((a) => a.is_current).length;

  return (
    <div className="animate-fade-in">
      <PageHeader
        title="ZCode 账号工作台"
        desc="ZCode 多账号余额监控 · 一键切换 · 邀请关系分组"
        actions={
          <>
            <button
              className="btn-outline"
              disabled={syncingInvite}
              onClick={syncInvite}
              title="从官方奖励中心接口自动拉取邀请记录：标记邀请者并自动绑定被邀请账号"
            >
              <Link2 size={15} /> {syncingInvite ? '同步中…' : '同步邀请'}
            </button>
            {view === 'zcode' && (
              <button className="btn-outline" onClick={refreshAll} disabled={refreshing.active}>
                <RefreshCw size={15} className={refreshing.active ? 'animate-spin' : ''} />
                {refreshing.active
                  ? `刷新中 ${refreshing.done}/${refreshing.total}`
                  : '刷新用量'}
              </button>
            )}
          </>
        }
      />

      {/* 子视图切换 */}
      <div className="mb-4 flex items-center gap-1 rounded-xl bg-slate-100 p-1 dark:bg-zinc-900">
        {(
          [
            { key: 'zcode', label: 'ZCode 账号', icon: Users },
            { key: 'secrets', label: '账密 · 批量登录', icon: KeyRound },
            { key: 'wb', label: 'Buddy 海外', icon: Globe },
            { key: 'overseas', label: '海外号池', icon: KeyRound },
          ] as const
        ).map(({ key, label, icon: Icon }) => (
          <button
            key={key}
            className={cn(
              'flex items-center gap-1.5 rounded-lg px-3 py-1.5 text-sm font-medium transition',
              view === key
                ? 'bg-white text-slate-800 shadow-sm dark:bg-zinc-800 dark:text-zinc-100'
                : 'text-slate-500 hover:text-slate-700 dark:text-zinc-400',
            )}
            onClick={() => setView(key)}
          >
            <Icon size={14} /> {label}
          </button>
        ))}
      </div>

      {view === 'secrets' ? (
        <SecretsPanel />
      ) : view === 'wb' ? (
        <WbOverseasPanel />
      ) : view === 'overseas' ? (
        <OverseasPoolPanel />
      ) : (
        <>
          {/* 统计卡 */}
          <div className="grid grid-cols-2 gap-3 md:grid-cols-4">
            <StatCard label="账号总数" value={source.length} hint="z.ai 账号" tone="brand" />
            <StatCard
              label="主号组"
              value={families.length}
              hint={`${looseInvitees.length} 个小号未归属`}
              tone="amber"
            />
            <StatCard
              label="使用中账号"
              value={currentCount}
              hint="当前 ZCode 登录"
              tone="green"
            />
            <StatCard
              label="最近余额更新"
              value={fmtTime(snapshot?.last_balance_at ?? null)}
              hint={`活动检测 ${fmtTime(snapshot?.last_preview_at ?? null)}`}
              tone="slate"
            />
          </div>

          {actionError && (
            <div className="mt-4 flex items-center justify-between gap-3 rounded-xl border border-slate-200 bg-white px-4 py-3 text-sm text-slate-600 dark:border-zinc-800 dark:bg-zinc-900 dark:text-zinc-300">
              <span className="min-w-0 truncate">{actionError}</span>
              <button
                className="shrink-0 text-xs text-slate-400 hover:text-slate-600"
                onClick={() => setActionError(null)}
              >
                忽略
              </button>
            </div>
          )}

          {oauthNotice && (
            <div className="mt-4 flex items-start justify-between gap-3 rounded-xl border border-sky-200 bg-sky-50 px-4 py-3 text-sm text-sky-700 dark:border-sky-500/30 dark:bg-sky-500/10 dark:text-sky-300">
              <span className="flex items-start gap-2">
                <Info size={15} className="mt-0.5 shrink-0" />
                如果你是在此页面注册新账号：注册完成后请点「取消」结束本次流程，再重新点一遍「添加账号」走授权，才能成功添加。
              </span>
              <button
                className="shrink-0 text-xs text-sky-500 hover:text-sky-700"
                onClick={() => setOauthNotice(false)}
              >
                忽略
              </button>
            </div>
          )}

          {/* 操作行 */}
          <div className="mt-4 flex flex-wrap items-center gap-2">
            <button
              className="btn-primary"
              onClick={() => importAccount('oauth')}
              disabled={importing !== 'idle'}
            >
              <Plus size={15} />
              {importing === 'oauth' ? '已打开系统浏览器，请在其中完成登录…' : '添加账号'}
            </button>
            {importing === 'oauth' && (
              <button className="btn-outline" onClick={cancelOAuth}>
                取消
              </button>
            )}
            <button
              className="btn-outline"
              onClick={() => importAccount('local')}
              disabled={importing !== 'idle'}
            >
              <Download size={15} /> 导入当前账号
            </button>
            <span className="ml-auto text-xs text-slate-400">
              共 {source.length} 个账号 · {families.length} 个主号组
            </span>
          </div>

          {/* 账号分组区 */}
          <div className="mt-4 space-y-4">
            {source.length === 0 ? (
              <EmptyState
                icon={<Users size={26} />}
                title="暂无账号"
                hint="点击「添加账号」登录 z.ai 导入，或「导入当前账号」导入 ZCode 客户端当前登录的账号。"
              />
            ) : (
              <>
                {families.map(({ inviter, invitees }) => (
                  <div
                    key={inviter.id}
                    className={cn(
                      'rounded-2xl border p-3',
                      inviter.is_current
                        ? 'border-emerald-500/40 bg-emerald-50/40 dark:border-emerald-500/25 dark:bg-emerald-500/5'
                        : 'border-slate-200/70 bg-slate-50/50 dark:border-zinc-800 dark:bg-zinc-900/40',
                    )}
                  >
                    <div className="grid gap-3 lg:grid-cols-2">
                      <AccountCard
                        account={inviter}
                        checking={checking.has(inviter.id)}
                        switching={switching.has(inviter.id)}
                        onCheck={check}
                        onRemove={removeAccount}
                        onSetRole={setInviteRole}
                        onSwitch={switchAccount}
                      />
                      {invitees.length > 0 && (
                        <div className="space-y-2 lg:border-l lg:border-slate-200/70 lg:pl-3 lg:dark:border-zinc-800">
                          {invitees.map((a) => (
                            <InviteeRow
                              key={a.id}
                              account={a}
                              expanded={expanded.has(a.id)}
                              onToggle={() => toggleExpand(a.id)}
                              checking={checking.has(a.id)}
                              switching={switching.has(a.id)}
                              onCheck={check}
                              onRemove={removeAccount}
                              onSetRole={setInviteRole}
                              onSwitch={switchAccount}
                            />
                          ))}
                        </div>
                      )}
                    </div>
                  </div>
                ))}

                {looseInvitees.length > 0 && (
                  <div>
                    <div className="mb-2 flex items-center gap-1.5 text-xs font-medium text-slate-500 dark:text-zinc-400">
                      <Link2 size={13} /> 被邀请 · 未归属主号
                    </div>
                    <div className="space-y-2">
                      {looseInvitees.map((a) => (
                        <InviteeRow
                          key={a.id}
                          account={a}
                          expanded={expanded.has(a.id)}
                          onToggle={() => toggleExpand(a.id)}
                          checking={checking.has(a.id)}
                          switching={switching.has(a.id)}
                          onCheck={check}
                          onRemove={removeAccount}
                          onSetRole={setInviteRole}
                          onSwitch={switchAccount}
                        />
                      ))}
                    </div>
                  </div>
                )}

                {unmarked.length > 0 && (
                  <div>
                    {(families.length > 0 || looseInvitees.length > 0) && (
                      <div className="mb-2 flex items-center gap-1.5 text-xs font-medium text-slate-500 dark:text-zinc-400">
                        <Tag size={13} /> 未标记邀请关系
                      </div>
                    )}
                    <div className="grid gap-3 lg:grid-cols-2">
                      {unmarked.map((a) => (
                        <AccountCard
                          key={a.id}
                          account={a}
                          checking={checking.has(a.id)}
                          switching={switching.has(a.id)}
                          onCheck={check}
                          onRemove={removeAccount}
                          onSetRole={setInviteRole}
                          onSwitch={switchAccount}
                        />
                      ))}
                    </div>
                  </div>
                )}
              </>
            )}
          </div>
        </>
      )}
    </div>
  );
}
