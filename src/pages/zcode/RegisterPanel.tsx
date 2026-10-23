/**
 * 马甲邮箱全自动注册面板（ZCode 账号工作台子视图）。
 *
 * 流程：填马甲邮箱/密码 → 选主号（可选捆绑）→ 发起 →
 *   Rust 打开内嵌注册窗自动填表过验证码 → IMAP 收验证邮件 →
 *   完成邮箱验证 → OAuth 授权入池 → 邀请捆绑。
 * 进度经 register:progress / register:done 事件实时展示。
 */
import { useCallback, useEffect, useState } from 'react';
import {
  MailPlus,
  Play,
  Square,
  Settings2,
  Check,
  Loader2,
  CircleAlert,
  Info,
  Layers,
} from 'lucide-react';
import PageHeader from '../../components/PageHeader';
import GuidePanel from '../../components/GuidePanel';
import { Badge, Modal } from '../../components/ui';
import { cn } from '../../lib/cn';
import { accountsApi, onAccountsChanged } from './api/accounts';
import {
  onBindResult,
  onRegisterDone,
  onRegisterProgress,
  registerApi,
  type BindResult,
  type ImapState,
  type MaskPoolItem,
  type RegisterDone,
  type RegisterProgress,
  type RegisterStep,
  type RelayState,
} from './api/register';
import type { AccountLive } from './types';

/** 阶段中文名与顺序（与 Rust flow.rs 的 step 对应） */
const STEPS: { key: RegisterStep; label: string }[] = [
  { key: 'open', label: '打开注册页' },
  { key: 'fill', label: '填表过验证码' },
  { key: 'mail', label: '收取验证邮件' },
  { key: 'verify', label: '完成邮箱验证' },
  { key: 'oauth', label: '授权并入池' },
  { key: 'bind', label: '邀请捆绑' },
];

function StepBadge({ state }: { state: RegisterProgress['state'] }) {
  if (state === 'running') {
    return (
      <span className="inline-flex items-center gap-1 text-sky-600 dark:text-sky-400">
        <Loader2 size={12} className="animate-spin" /> 进行中
      </span>
    );
  }
  if (state === 'success') {
    return (
      <span className="inline-flex items-center gap-1 text-emerald-600 dark:text-emerald-400">
        <Check size={12} /> 完成
      </span>
    );
  }
  if (state === 'failed') {
    return (
      <span className="inline-flex items-center gap-1 text-rose-500">
        <CircleAlert size={12} /> 失败
      </span>
    );
  }
  return <span className="text-slate-400">已取消</span>;
}

/**
 * 单个账号的进度卡:7 个步骤一行排开(点=状态),失败步骤标红并显示原因。
 * 「明细」展开按时间顺序的完整事件流——批量跑完后复盘"哪几个失败、各自卡在哪一步"
 * 全靠这个,前端只留当前态是不够的(同一 step 的事件会互相覆盖)。
 */
function ProgressCard({
  tag,
  events,
  progress,
}: {
  tag: string;
  events: RegisterProgress[];
  progress: Record<string, RegisterProgress>;
}) {
  const [open, setOpen] = useState(false);
  const lastEvent = events[events.length - 1];
  const hasFail = events.some((e) => e.state === 'failed');
  const allDone = events.some((e) => e.step === 'done' && e.state === 'success');
  // 捆绑步骤的真实结果(经 zcode:bind-result 回传,不是"已打开页面"的假成功)
  const bindEv = progress[`${tag}::bind`];
  const bindMsg = bindEv?.message ?? '';

  return (
    <div
      className={cn(
        'rounded-xl border p-3',
        hasFail
          ? 'border-rose-300 bg-rose-50/50 dark:border-rose-500/30 dark:bg-rose-500/5'
          : allDone
            ? 'border-emerald-300 bg-emerald-50/50 dark:border-emerald-500/30 dark:bg-emerald-500/5'
            : 'border-slate-200 bg-white dark:border-zinc-800 dark:bg-zinc-900/60',
      )}
    >
      <div className="flex flex-wrap items-center gap-2">
        <span className="truncate font-mono text-xs text-slate-600 dark:text-zinc-300">{tag}</span>
        {allDone && <Badge tone="green">已入池</Badge>}
        {hasFail && <Badge tone="red">有步骤失败</Badge>}
        {/* 捆绑是异步的:真实结果未回传前只能显示"等待验证码",不能算成功 */}
        {bindEv?.state === 'running' && (
          <Badge tone="amber" title="已在系统浏览器打开验证页,完成验证码后自动回传结果">
            捆绑待验证
          </Badge>
        )}
        <span className="ml-auto flex items-center gap-2">
          {/* 步骤点:一眼看出走到哪、哪步红了 */}
          <span className="flex items-center gap-1">
            {STEPS.map((s) => {
              const p = progress[`${tag}::${s.key}`];
              return (
                <span
                  key={s.key}
                  title={`${s.label}: ${p ? p.message : '等待'}`}
                  className={cn(
                    'h-2 w-2 rounded-full',
                    p?.state === 'success'
                      ? 'bg-emerald-500'
                      : p?.state === 'failed'
                        ? 'bg-rose-500'
                        : p?.state === 'running'
                          ? 'animate-pulse bg-sky-500'
                          : 'bg-slate-300 dark:bg-zinc-700',
                  )}
                />
              );
            })}
          </span>
          <button
            className="chip bg-slate-100 text-slate-500 hover:bg-slate-200 dark:bg-zinc-800 dark:text-zinc-400"
            onClick={() => setOpen((v) => !v)}
          >
            {open ? '收起' : '明细'}
          </button>
        </span>
      </div>

      {/* 当前态一行:最新事件的消息 */}
      {lastEvent && !open && (
        <div className="mt-1.5 truncate text-xs text-slate-500 dark:text-zinc-400">
          {lastEvent.message}
        </div>
      )}
      {/* 捆绑提示单独一行(比最新事件更重要,容易被忽略) */}
      {bindEv?.state === 'running' && bindMsg && (
        <div className="mt-1.5 truncate text-xs text-amber-600 dark:text-amber-400">
          ⚠️ {bindMsg}
        </div>
      )}

      {/* 明细:完整时间线 */}
      {open && (
        <ol className="mt-3 space-y-1.5 border-t border-slate-100 pt-3 dark:border-zinc-800">
          {events.map((e, i) => (
            <li key={`${e.step}-${i}`} className="flex items-start gap-2 text-xs">
              <span className="w-24 shrink-0 text-slate-400">
                {STEPS.find((s) => s.key === e.step)?.label ?? e.step}
              </span>
              <StepBadge state={e.state} />
              <span className="min-w-0 flex-1 text-slate-600 dark:text-zinc-300">{e.message}</span>
            </li>
          ))}
        </ol>
      )}
    </div>
  );
}

/** IMAP 配置弹窗：Gmail 地址 + App Password */
function ImapConfigModal({
  open,
  initial,
  onClose,
  onSaved,
}: {
  open: boolean;
  initial: ImapState;
  onClose: () => void;
  onSaved: (msg: string) => void;
}) {
  const [host, setHost] = useState(initial.host);
  const [port, setPort] = useState('993');
  const [user, setUser] = useState(initial.user ?? '');
  const [pass, setPass] = useState('');
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    if (open) {
      setHost(initial.host);
      setUser(initial.user ?? '');
      setPass('');
      setError(null);
    }
  }, [open, initial]);

  const save = async () => {
    setSaving(true);
    setError(null);
    try {
      const msg = await registerApi.saveImap(host, Number(port) || 993, user, pass);
      onSaved(msg);
      onClose();
    } catch (e) {
      setError(String(e));
    } finally {
      setSaving(false);
    }
  };

  return (
    <Modal
      open={open}
      onClose={onClose}
      title="马甲邮箱配置"
      footer={
        <>
          <button className="btn-outline" onClick={onClose} disabled={saving}>
            取消
          </button>
          <button className="btn-primary" onClick={save} disabled={saving}>
            {saving ? '测试中…' : '保存并测试连接'}
          </button>
        </>
      }
    >
      <div className="space-y-3">
        <p className="flex items-start gap-2 rounded-xl bg-sky-50 px-3 py-2 text-xs text-sky-700 dark:bg-sky-500/10 dark:text-sky-300">
          <Info size={14} className="mt-0.5 shrink-0" />
          <span>
            用 Gmail 收 z.ai 的注册验证邮件：需先在 Google 账号开启两步验证，再生成
            16 位 App Password 填到下面。Firefox Relay 马甲邮箱的邮件都会转投到这个
            Gmail。
          </span>
        </p>
        <label className="block">
          <span className="text-xs font-medium text-slate-500 dark:text-zinc-400">Gmail 地址</span>
          <input
            className="input mt-1 w-full"
            value={user}
            placeholder="your@gmail.com"
            onChange={(e) => setUser(e.target.value)}
          />
        </label>
        <label className="block">
          <span className="text-xs font-medium text-slate-500 dark:text-zinc-400">
            App Password（16 位）
          </span>
          <input
            className="input mt-1 w-full font-mono"
            value={pass}
            placeholder="xxxx xxxx xxxx xxxx"
            onChange={(e) => setPass(e.target.value)}
          />
        </label>
        <div className="flex gap-3">
          <label className="block flex-1">
            <span className="text-xs font-medium text-slate-500 dark:text-zinc-400">IMAP 主机</span>
            <input
              className="input mt-1 w-full"
              value={host}
              onChange={(e) => setHost(e.target.value)}
            />
          </label>
          <label className="block w-24">
            <span className="text-xs font-medium text-slate-500 dark:text-zinc-400">端口</span>
            <input
              className="input mt-1 w-full"
              value={port}
              onChange={(e) => setPort(e.target.value)}
            />
          </label>
        </div>
        {error && (
          <p className="flex items-start gap-1.5 text-xs text-rose-500">
            <CircleAlert size={13} className="mt-0.5 shrink-0" />
            {error}
          </p>
        )}
      </div>
    </Modal>
  );
}

export default function RegisterPanel() {
  const [accounts, setAccounts] = useState<AccountLive[]>([]);
  // 账密库里现成的未注册马甲邮箱(优先复用)
  const [maskPool, setMaskPool] = useState<MaskPoolItem[]>([]);
  const [selectedSecretId, setSelectedSecretId] = useState<number | null>(null);
  const [email, setEmail] = useState('');
  const [password, setPassword] = useState('');
  // 批量注册
  const [batchCount, setBatchCount] = useState(3);
  const [batchSource, setBatchSource] = useState<'generate' | 'pool'>('generate');
  // Relay 状态
  const [relay, setRelay] = useState<RelayState | null>(null);
  const [relayError, setRelayError] = useState<string | null>(null);
  // 批量进度:已完成/失败计数
  const [batchDone, setBatchDone] = useState<{ ok: number; fail: number; total: number } | null>(null);
  const [inviterId, setInviterId] = useState<number | null>(null);
  const [running, setRunning] = useState(false);
  // 进度按「归属标签」分组:批量模式下每个账号一张卡([2/5] a@mozmail.com),
  // 单条模式归到空串那一组。同一个 step 在同一张卡内只保留最新态,
  // 但跨卡不覆盖——这是修掉"批量只看到最后一个号"的关键。
  const [progress, setProgress] = useState<Record<string, RegisterProgress>>({});
  // 每张卡的步骤历史(按到达顺序),用于展开看完整时间线
  const [history, setHistory] = useState<Record<string, RegisterProgress[]>>({});
  const [notice, setNotice] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [imap, setImap] = useState<ImapState>({ configured: false, host: 'imap.gmail.com', user: null });
  const [imapOpen, setImapOpen] = useState(false);

  const reloadAccounts = useCallback(() => {
    accountsApi
      .list()
      .then((rows) =>
        setAccounts(
          rows.map((a) => ({
            id: a.id,
            label: a.label,
            nickname: null,
            email: a.label.includes('@') ? a.label : null,
            invite_role: a.invite_role,
            invited_by: a.invited_by,
            invite_code: a.invite_code,
            invite_state: a.invite_state,
            invite_unlock_at: a.invite_unlock_at,
            invited_count: a.invited_count,
            invited_weekly: a.invited_weekly,
            invite_cap: a.invite_cap,
            invite_capped: a.invite_capped,
            night_card_days: a.night_card_days,
            night_card_state: a.night_card_state,
            weekend_progress: a.weekend_progress,
            weekend_target: a.weekend_target,
            weekly_reset_used: a.weekly_reset_used,
            task_login_device: a.task_login_device,
            task_bind_code: a.task_bind_code,
            task_send_message: a.task_send_message,
            invitee_state: a.invitee_state,
            is_current: a.is_current,
            plans: [],
            balances: [],
            claimables: [],
            balance_error: null,
            updated_at: null,
          })),
        ),
      )
      .catch(() => {});
    // 现成马甲邮箱池:注册成功后该条目会被关联到新账号,这里随之消失
    registerApi
      .maskPool()
      .then(setMaskPool)
      .catch(() => {});
  }, []);

  useEffect(() => {
    reloadAccounts();
    registerApi.imapState().then(setImap).catch(() => {});
    // Relay 状态:读不到登录态时给出可操作提示(不是致命错误,还能用手输)
    registerApi
      .relayStatus()
      .then(setRelay)
      .catch((e) => setRelayError(String(e)));
    registerApi.status().then(setRunning).catch(() => {});
    const unsubs: Array<() => void> = [];
    onRegisterProgress((p) => {
      // tag 为空串时归一成"(单条)",避免空 key 混在一起
      const key = p.tag || '(单条)';
      setProgress((prev) => ({ ...prev, [`${key}::${p.step}`]: p }));
      setHistory((prev) => {
        const list = prev[key] ? [...prev[key]] : [];
        // 同一步骤的重复事件:替换最后一条同 step 记录,不无限追加
        const lastSame = list.length > 0 && list[list.length - 1].step === p.step;
        const next = lastSame ? [...list.slice(0, -1), p] : [...list, p];
        return { ...prev, [key]: next };
      });
      if (p.state === 'running') setRunning(true);
    }).then((u) => unsubs.push(u));
    onBindResult((r) => {
      // 捆绑成功意味着邀请关系变了,账号列表要刷新(分组视图立即可见)。
      // 结果本身已由 register:progress 的 bind 步骤消息展示,这里只做数据刷新。
      if (r.ok) reloadAccounts();
    }).then((u) => unsubs.push(u));
    onRegisterDone((d: RegisterDone) => {
      setRunning(false);
      setNotice(
        d.ok ? `✅ ${d.message}` : `❌ ${d.message}`,
      );
      // 批量模式下从汇总文案里解析出成功/失败数
      const m = /成功\s*(\d+)\s*\/\s*失败\s*(\d+)\s*\/\s*共\s*(\d+)/.exec(d.message);
      if (m) {
        setBatchDone({ ok: Number(m[1]), fail: Number(m[2]), total: Number(m[3]) });
      }
      reloadAccounts();
      registerApi.relayStatus().then(setRelay).catch(() => {});
    }).then((u) => unsubs.push(u));
    onAccountsChanged(() => reloadAccounts()).then((u) => unsubs.push(u));
    return () => unsubs.forEach((u) => u());
  }, [reloadAccounts]);

  // 可作捆绑目标的主号:只看当前是否有邀请资格 + 邀请码,不限历史角色
  // (小号过段时间也会获得资格,届时同样出现在这里)
  const canInvite = accounts.filter((a) => a.invite_state === 'active' && a.invite_code);

  /** 本周剩余邀请名额(官方 weekly 已用 / reward_cap 上限);未同步过资格时返回 null */
  const remaining = (a: AccountLive): number | null => {
    if (a.invite_capped) return 0;
    const cap = a.invite_cap;
    const used = a.invited_weekly;
    if (cap == null || used == null) return null;
    return Math.max(0, cap - used);
  };

  const start = async () => {
    setError(null);
    setNotice(null);
    setProgress({});
    setHistory({});
    // 有现成马甲邮箱就用它(账密从库里取,不经过前端);否则校验手输
    if (!selectedSecretId) {
      if (!email.includes('@')) {
        setError('请选择或填写马甲邮箱地址');
        return;
      }
      if (!password) {
        setError('请填写注册密码');
        return;
      }
    }
    try {
      await registerApi.start({
        secretId: selectedSecretId,
        email: selectedSecretId ? undefined : email,
        password: selectedSecretId ? undefined : password,
        inviterId,
      });
      setRunning(true);
      setNotice('已发起：注册窗口将自动打开并按步骤执行');
    } catch (e) {
      setError(String(e));
    }
  };

  const startBatch = async () => {
    setError(null);
    setNotice(null);
    setProgress({});
    setHistory({});
    setBatchDone(null);
    if (batchSource === 'pool' && maskPool.length < batchCount) {
      setError(`账密库里只有 ${maskPool.length} 个未注册马甲，不够 ${batchCount} 个`);
      return;
    }
    if (batchSource === 'generate' && relay && relay.mask_count + batchCount > relay.free_limit) {
      setError(
        `免费账号最多 ${relay.free_limit} 个马甲，已有 ${relay.mask_count} 个，再生成 ${batchCount} 个会超限`,
      );
      return;
    }
    try {
      const n = await registerApi.batch({ count: batchCount, source: batchSource, inviterId });
      setRunning(true);
      setNotice(
        batchSource === 'generate'
          ? `已生成 ${n} 个新马甲并开始逐个注册，跑完自动汇报`
          : `已接受 ${n} 个马甲，开始逐个注册`,
      );
    } catch (e) {
      setError(String(e));
    }
  };

  const cancel = async () => {
    try {
      await registerApi.cancel();
      setNotice('已请求取消（当前步骤会尽快中止）');
    } catch (e) {
      setError(String(e));
    }
  };

  return (
    <div className="animate-fade-in space-y-4">
      <PageHeader title="ZCode · 自动注册" desc="马甲邮箱注册入池 · 自动邀请捆绑 · 新手任务" />

      <GuidePanel
        defaultOpen={!imap.configured}
        steps={[
          {
            title: '准备一个能自动收信的邮箱（推荐 Gmail）',
            detail:
              '注册 z.ai 时官方会往注册邮箱发一封验证邮件，工具要通过 IMAP 自动读这封信拿验证码。' +
              '所以必须有一个「程序能登录读取」的邮箱——Gmail 开启两步验证后生成 16 位 App Password 即可。',
            warning:
              '普通邮箱密码不能直接用于 IMAP；未开两步验证的 Gmail 也拿不到 App Password。',
          },
          {
            title: '把邮箱凭证填进「配置马甲邮箱」',
            detail:
              '点下方「配置马甲邮箱」，填 IMAP 服务器（Gmail 为 imap.gmail.com）、端口 993、' +
              '邮箱地址、16 位 App Password，保存后点「测试连接」确认能登录成功。',
            warning: '这一步失败时后面的注册一定跑不通——先确保测试连接成功。',
          },
          {
            title: '生成马甲邮箱（可选，但强烈建议）',
            detail:
              '去左侧「马甲邮箱」页连上 Firefox Relay，一键生成一批随机别名（形如 xxx@mozmail.com），' +
              '它们会自动转发到上面那个 Gmail。这样每个新号有独立邮箱，又都收进同一个收件箱。',
            warning: '同一个邮箱注册多个 z.ai 账号会被官方判重；马甲邮箱就是为了避开这一点。',
          },
          {
            title: '选主号（决定新号捆绑给谁）',
            detail:
              '页面下方「捆绑到主号」选一个有邀请码的账号；留空则只注册不捆绑。' +
              '不填主号时新号仍会入池，之后可在「邀请档案」页手动补绑。',
          },
          {
            title: '粘贴马甲邮箱列表并开始',
            detail:
              '把上一步生成的一批 @mozmail.com 地址按行粘贴进输入框，点「开始注册」。' +
              '进度会逐步显示：开窗填表 → 等邮件 → 验证 → OAuth 授权 → 入池 → 捆绑 → 发消息。',
            warning:
              '注册会弹出一个内嵌浏览器窗口，请勿关闭；遇到阿里云滑块验证需要手动滑一下。',
          },
        ]}
      />

      {/* 操作行 */}
      <div className="flex flex-wrap items-center gap-2">
        <span className="text-sm font-medium text-slate-700 dark:text-zinc-200">
          <MailPlus size={15} className="mr-1 inline align-[-2px]" />
          马甲邮箱注册入池
        </span>
        <span className="ml-auto flex items-center gap-2">
          {imap.configured ? (
            <Badge tone="green" title={`${imap.host} · ${imap.user ?? ''}`}>
              <Check size={11} /> 邮箱已配置
            </Badge>
          ) : (
            <button className="btn-outline" onClick={() => setImapOpen(true)}>
              <Settings2 size={14} /> 配置马甲邮箱
            </button>
          )}
          {imap.configured && (
            <button
              className="btn-outline"
              onClick={() => setImapOpen(true)}
              title="修改 Gmail / IMAP 配置"
            >
              <Settings2 size={14} />
            </button>
          )}
        </span>
      </div>

      {/* 提示条 */}
      {!imap.configured && (
        <div className="flex items-start gap-2 rounded-xl border border-amber-200 bg-amber-50 px-4 py-3 text-sm text-amber-700 dark:border-amber-500/30 dark:bg-amber-500/10 dark:text-amber-300">
          <CircleAlert size={15} className="mt-0.5 shrink-0" />
          <span>
            尚未配置收件邮箱。自动注册需要读取 z.ai 发来的验证邮件，请先点右上角「配置马甲邮箱」。
          </span>
        </div>
      )}
      {notice && (
        <div className="flex items-center justify-between gap-3 rounded-xl border border-emerald-200 bg-emerald-50 px-4 py-3 text-sm text-emerald-700 dark:border-emerald-500/30 dark:bg-emerald-500/10 dark:text-emerald-300">
          <span className="min-w-0">{notice}</span>
          <button className="shrink-0 text-xs hover:opacity-70" onClick={() => setNotice(null)}>
            忽略
          </button>
        </div>
      )}
      {error && (
        <div className="flex items-center justify-between gap-3 rounded-xl border border-rose-200 bg-rose-50 px-4 py-3 text-sm text-rose-600 dark:border-rose-500/30 dark:bg-rose-500/10 dark:text-rose-300">
          <span className="min-w-0">{error}</span>
          <button className="shrink-0 text-xs hover:opacity-70" onClick={() => setError(null)}>
            忽略
          </button>
        </div>
      )}

      {/* 批量注册（唯一入口）：一次跑完 N 个号 */}
      <div className="card space-y-3 p-4">
        <div className="flex flex-wrap items-center gap-2">
          <span className="text-sm font-medium text-slate-700 dark:text-zinc-200">
            <Layers size={15} className="mr-1 inline align-[-2px]" />
            批量注册（无人值守）
          </span>
          {relay ? (
            <Badge tone="green" title={`Relay 账号 ${relay.account_email}`}>
              Relay 已连接 · {relay.mask_count}/{relay.free_limit} 马甲
            </Badge>
          ) : relayError ? (
            <Badge tone="amber" title={relayError}>
              Relay 未连接
            </Badge>
          ) : (
            <Badge tone="slate">检测中…</Badge>
          )}
        </div>
        {relayError && (
          <p className="flex items-start gap-1.5 text-xs text-amber-600 dark:text-amber-400">
            <CircleAlert size={13} className="mt-0.5 shrink-0" />
            <span className="min-w-0 break-all">
              {relayError}（不影响手输注册；如需自动生成马甲，请先在 Zen 浏览器登录
              relay.firefox.com）
            </span>
          </p>
        )}
        <div className="grid gap-3 md:grid-cols-3">
          <label className="block">
            <span className="text-xs font-medium text-slate-500 dark:text-zinc-400">注册数量</span>
            <input
              className="input mt-1 w-full"
              type="number"
              min={1}
              max={20}
              value={batchCount}
              onChange={(e) => setBatchCount(Math.max(1, Math.min(20, Number(e.target.value) || 1)))}
              disabled={running}
            />
          </label>
          <label className="block">
            <span className="text-xs font-medium text-slate-500 dark:text-zinc-400">马甲来源</span>
            <select
              className="input mt-1 w-full"
              value={batchSource}
              onChange={(e) => setBatchSource(e.target.value as 'generate' | 'pool')}
              disabled={running}
            >
              <option value="generate">自动生成新马甲（Relay）</option>
              <option value="pool">用账密库里现成的（{maskPool.length} 个可用）</option>
            </select>
          </label>
          <label className="block">
            <span className="text-xs font-medium text-slate-500 dark:text-zinc-400">
              捆绑到主号（=邀请人）
            </span>
            <select
              className="input mt-1 w-full"
              value={inviterId ?? ''}
              onChange={(e) => setInviterId(e.target.value ? Number(e.target.value) : null)}
              disabled={running}
            >
              <option value="">不捆绑（新号照常入池，之后可在「邀请档案」补绑）</option>
              {canInvite.map((a) => (
                <option key={a.id} value={a.id}>
                  {a.label}（{a.invite_code}
                  {remaining(a) != null ? ` · 本周还剩 ${remaining(a)} 个名额` : ''}）
                </option>
              ))}
            </select>
          </label>
        </div>
        <div className="flex flex-wrap items-center gap-2">
          <button
            className="btn-primary"
            onClick={startBatch}
            disabled={running || !imap.configured}
            title={imap.configured ? '一次跑完，跑完自动汇报' : '请先配置马甲邮箱（收验证码用）'}
          >
            <Play size={15} /> {running ? '批量注册进行中…' : `开始批量注册 ${batchCount} 个`}
          </button>
          {running && (
            <button className="btn-outline" onClick={cancel}>
              <Square size={14} /> 取消
            </button>
          )}
          <span className="text-xs text-slate-400">
            每个号自动完成：注册 → 收验证码 → 授权入池 → 捆绑；弹滑块时需手动滑一下
          </span>
        </div>
        {batchDone && (
          <p className="text-xs text-slate-500 dark:text-zinc-400">
            本轮结果：成功 {batchDone.ok} / 失败 {batchDone.fail} / 共 {batchDone.total}
          </p>
        )}
      </div>

      {/* 单个注册（手动指定） */}
      <div className="card space-y-3 p-4">
        <label className="block">
          <span className="text-xs font-medium text-slate-500 dark:text-zinc-400">
            马甲邮箱（优先选账密库里现成的）
          </span>
          <select
            className="input mt-1 w-full"
            value={selectedSecretId ?? ''}
            onChange={(e) => setSelectedSecretId(e.target.value ? Number(e.target.value) : null)}
            disabled={running}
          >
            <option value="">
              {maskPool.length === 0 ? '（无现成的，改用手动输入）' : '手动输入新邮箱…'}
            </option>
            {maskPool.map((m) => (
              <option key={m.id} value={m.id}>
                {m.email}
              </option>
            ))}
          </select>
        </label>
        {maskPool.length > 0 && !selectedSecretId && (
          <p className="text-xs text-slate-400">
            账密库里有 {maskPool.length} 个尚未注册的马甲邮箱，选中后注册成功会自动关联账密。
          </p>
        )}
        <div className="grid gap-3 md:grid-cols-2">
          <label className="block">
            <span className="text-xs font-medium text-slate-500 dark:text-zinc-400">
              马甲邮箱（Firefox Relay）
            </span>
            <input
              className="input mt-1 w-full disabled:cursor-not-allowed disabled:opacity-50"
              value={selectedSecretId ? maskPool.find((m) => m.id === selectedSecretId)?.email ?? '' : email}
              placeholder="xxxx@mozmail.com"
              onChange={(e) => setEmail(e.target.value)}
              disabled={running || selectedSecretId != null}
            />
          </label>
          <label className="block">
            <span className="text-xs font-medium text-slate-500 dark:text-zinc-400">注册密码</span>
            <input
              className="input mt-1 w-full disabled:cursor-not-allowed disabled:opacity-50"
              type="password"
              value={selectedSecretId ? '（用账密库里保存的密码）' : password}
              placeholder="给新号设的密码"
              onChange={(e) => setPassword(e.target.value)}
              disabled={running || selectedSecretId != null}
            />
          </label>
        </div>
        <label className="block">
          <span className="text-xs font-medium text-slate-500 dark:text-zinc-400">
            捆绑到主号（可选，仅列出已获得邀请资格的账号）
          </span>
          <select
            className="input mt-1 w-full"
            value={inviterId ?? ''}
            onChange={(e) => setInviterId(e.target.value ? Number(e.target.value) : null)}
            disabled={running}
          >
            <option value="">不捆绑（稍后手动捆）</option>
            {canInvite.map((a) => (
              <option key={a.id} value={a.id}>
                {a.label}（{a.invite_code}
                {remaining(a) != null ? ` · 本周还剩 ${remaining(a)} 个名额` : ''}）
              </option>
            ))}
          </select>
        </label>
        <div className="flex items-center gap-2">
          <button
            className="btn-primary"
            onClick={start}
            disabled={running || !imap.configured}
            title={imap.configured ? '发起注册流水线' : '请先配置马甲邮箱'}
          >
            <Play size={15} /> {running ? '注册进行中…' : '开始注册'}
          </button>
          {running && (
            <button className="btn-outline" onClick={cancel}>
              <Square size={14} /> 取消
            </button>
          )}
          <span className="ml-auto text-xs text-slate-400">
            注册窗口会自动打开；弹滑块时请在窗口里手动滑一下
          </span>
        </div>
      </div>

      {/* 进度：按账号分卡片，每张卡可展开看完整步骤时间线 */}
      {Object.keys(history).length > 0 && (
        <div className="space-y-3">
          <div className="flex items-center justify-between">
            <div className="text-sm font-medium text-slate-700 dark:text-zinc-200">执行进度</div>
            <span className="text-xs text-slate-400">
              共 {Object.keys(history).length} 个账号 · 点「明细」看每步发生了什么
            </span>
          </div>
          {Object.entries(history).map(([tag, events]) => (
            <ProgressCard key={tag} tag={tag} events={events} progress={progress} />
          ))}
        </div>
      )}

      <ImapConfigModal
        open={imapOpen}
        initial={imap}
        onClose={() => setImapOpen(false)}
        onSaved={(msg) => {
          setNotice(msg);
          registerApi.imapState().then(setImap).catch(() => {});
        }}
      />
    </div>
  );
}
