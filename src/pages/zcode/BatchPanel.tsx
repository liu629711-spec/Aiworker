/**
 * ZCode 页 · 夜间/周末批处理（2026-10-23）
 *
 * 为什么需要这个页面：ZCode 的 Start Plan / Global Build 额度**没有对外 API**，
 * 只能由官方客户端消费；而单次 agent 回合要吃掉约 26 万 input token，所以
 * 「在线网关」不成立，「把任务排在队列里、只在夜间窗口内批量跑」成立。
 *
 * 页面三块：
 * ① 窗口与容量：现在是否在窗口内、下一个窗口几点开、CLI 是否就绪；
 * ② 归因卡片：最近一次执行真实用了哪个 provider —— 这是判断「到底烧的是免费
 *    套餐（account:zai-start-plan）还是自有中转池（aiwork-*）」的唯一证据；
 * ③ 任务队列：入队 / 立即跑 / 取消 / 重跑 / 结果查看。
 */
import { useCallback, useEffect, useMemo, useState } from 'react';
import {
  Moon,
  Play,
  Plus,
  RefreshCw,
  Trash2,
  RotateCcw,
  X,
  CircleAlert,
  CheckCircle2,
  Clock,
  Zap,
} from 'lucide-react';
import PageHeader from '../../components/PageHeader';
import { Badge, EmptyState, Spinner, StatCard } from '../../components/ui';
import { cn } from '../../lib/cn';
import { api } from '../../lib/tauri';
import { NIGHT_WINDOW_TEXT, fmtDateTime, fmtExpire } from './format';
import type { BatchStatus, BatchTask } from '../../types';

const STATUS_META: Record<string, { label: string; tone: 'slate' | 'green' | 'amber' | 'red' | 'blue' }> = {
  pending: { label: '排队中', tone: 'slate' },
  running: { label: '执行中', tone: 'blue' },
  cancel_requested: { label: '取消中', tone: 'amber' },
  done: { label: '完成', tone: 'green' },
  failed: { label: '失败', tone: 'red' },
  canceled: { label: '已取消', tone: 'slate' },
};

const WINDOW_META: Record<string, { label: string; hint: string }> = {
  night: { label: '夜间档', hint: '命中 23:00→09:00 窗口' },
  forced: { label: '手动', hint: '忽略窗口立即执行' },
  off: { label: '窗外', hint: '不在窗口内，不会自动消费' },
};

/**
 * provider 归因展示。
 *
 * 这是整个页面的关键判断依据：
 * - `account:zai-start-plan` / `account:*start-plan` → 真正烧到了 ZCode 免费套餐
 * - `aiwork-*` → 落到用户自己的 Trae/Buddy/Qoder 中转池（免费额度没动）
 * - 空 → 日志里没读到（CLI 可能没落盘，或执行前就失败了）
 */
function ProviderBadge({ provider }: { provider: string | null }) {
  if (!provider) return <span className="text-xs text-slate-400">未归因</span>;
  const isPlan = provider.startsWith('account:');
  const isOwn = provider.startsWith('aiwork-');
  return (
    <Badge
      tone={isPlan ? 'green' : isOwn ? 'amber' : 'slate'}
      title={
        isPlan
          ? '真实走的是 ZCode 免费套餐通道，消耗套餐额度'
          : isOwn
            ? '落到本工具自己的上游池（Trae/Buddy/Qoder 中转），ZCode 免费额度没有被消耗'
            : undefined
      }
    >
      {provider}
    </Badge>
  );
}

function fmtDuration(ms: number | null): string {
  if (!ms || ms <= 0) return '-';
  const s = Math.round(ms / 1000);
  return s >= 60 ? `${Math.floor(s / 60)}m${s % 60}s` : `${s}s`;
}

function fmtTokens(n: number | null): string {
  if (n == null) return '-';
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(2)}M`;
  if (n >= 1000) return `${(n / 1000).toFixed(1)}K`;
  return String(n);
}

export default function BatchPanel() {
  const [status, setStatus] = useState<BatchStatus | null>(null);
  const [tasks, setTasks] = useState<BatchTask[]>([]);
  const [input, setInput] = useState('');
  const [busy, setBusy] = useState<string | null>(null);
  const [err, setErr] = useState<string | null>(null);
  const [note, setNote] = useState<string | null>(null);
  const [expanded, setExpanded] = useState<number | null>(null);

  const refresh = useCallback(async () => {
    try {
      const [s, l] = await Promise.all([api.apiServer.zcodeBatchStatus(), api.apiServer.zcodeBatchList(200)]);
      setStatus(s);
      setTasks(l);
      setErr(null);
    } catch (e) {
      setErr(String(e).slice(0, 200));
    }
  }, []);

  useEffect(() => {
    void refresh();
    const t = window.setInterval(() => void refresh(), 5000);
    return () => window.clearInterval(t);
  }, [refresh]);

  // 多行输入 → 一条一行任务（空行/纯空白丢弃）
  const lines = useMemo(
    () => input.split('\n').map((s) => s.trim()).filter(Boolean),
    [input],
  );

  const enqueue = async () => {
    if (lines.length === 0) return;
    setBusy('enqueue');
    try {
      const n = await api.apiServer.zcodeBatchEnqueue(lines, 'GLM-5.3');
      setInput('');
      await refresh();
      if (n === 0) setErr('没有可入队的内容');
    } catch (e) {
      setErr(`入队失败：${String(e).slice(0, 160)}`);
    } finally {
      setBusy(null);
    }
  };

  const runNow = async () => {
    setBusy('run');
    try {
      // 后端只置「强制唤醒」标志就返回；真正的执行在后台协程里，
      // 单条约 1–2 分钟，靠 5s 轮询看状态变化。
      const started = await api.apiServer.zcodeBatchRunNow();
      if (started) {
        setErr(null);
        setNote('已触发后台执行：单条约 1–2 分钟，状态会自动刷新');
        window.setTimeout(() => setNote(null), 5000);
      } else {
        setErr('队列里没有排队中的任务');
      }
      await refresh();
    } catch (e) {
      setErr(`触发失败：${String(e).slice(0, 200)}`);
    } finally {
      setBusy(null);
    }
  };

  const act = async (name: string, fn: () => Promise<unknown>) => {
    setBusy(`${name}`);
    try {
      await fn();
      await refresh();
    } catch (e) {
      setErr(String(e).slice(0, 200));
    } finally {
      setBusy(null);
    }
  };

  const windowOpen = status?.window_open ?? false;
  const lastProvider = status?.last_provider ?? null;
  const burningPlan = !!lastProvider && lastProvider.startsWith('account:');

  return (
    <div className="animate-fade-in">
      <PageHeader
        title="夜间批处理"
        desc={`ZCode 免费套餐没有对外 API，只能在夜间档 ${NIGHT_WINDOW_TEXT} 由客户端消费。把任务排队，窗口内自动跑。`}
        actions={
          <div className="flex items-center gap-2">
            <button className="btn-outline" onClick={() => void refresh()}>
              <RefreshCw size={14} /> 刷新
            </button>
            <button
              className="btn-outline"
              onClick={() => void act('run', runNow)}
              disabled={busy === 'run' || (status?.pending ?? 0) === 0}
              title="忽略时间窗，立刻取一条任务执行（白天验通道用）"
            >
              {busy === 'run' ? <Spinner /> : <Play size={14} />} 立即跑一条
            </button>
          </div>
        }
      />

      {note && (
        <div className="mb-3 flex items-center gap-2 rounded-lg border border-emerald-200 bg-emerald-50 px-3 py-2 text-xs text-emerald-700 dark:border-emerald-800/60 dark:bg-emerald-900/20 dark:text-emerald-300">
          <CheckCircle2 size={14} className="shrink-0" />
          <span>{note}</span>
        </div>
      )}

      {err && (
        <div className="mb-3 flex items-start gap-2 rounded-lg border border-rose-200 bg-rose-50 px-3 py-2 text-xs text-rose-600 dark:border-rose-800/60 dark:bg-rose-900/20 dark:text-rose-300">
          <CircleAlert size={14} className="mt-0.5 shrink-0" />
          <span className="break-all">{err}</span>
          <button className="ml-auto shrink-0 text-rose-400" onClick={() => setErr(null)}>
            <X size={13} />
          </button>
        </div>
      )}

      {/* 经济性提示（交互 #4）：入队前先估量——数字来自实测（docs 方案第七八节） */}
      <div className="mb-3 rounded-lg border border-slate-100 bg-slate-50 px-3 py-2 text-[11px] leading-relaxed text-slate-500 dark:border-zinc-800 dark:bg-zinc-900/60 dark:text-zinc-400">
        额度参考：单回合 headless 执行约消耗 <b>26 万 input tokens</b>（系统提示+skills+MCP 全量）；
        夜间档（23:00–09:00）额度理论可跑 <b>~380 单/夜</b>，日间额度约 <b>11 单</b>。
        排队前先估量，任务结果以 CLI 日志归因（provider_used）为准。
      </div>

      {/* 窗口 / 容量 */}
      <div className="grid grid-cols-4 gap-3">
        <StatCard
          label="夜间窗口"
          value={windowOpen ? '已开启' : '未开启'}
          tone={windowOpen ? 'green' : 'slate'}
          hint={
            windowOpen
              ? `${NIGHT_WINDOW_TEXT} 内，队列会自动消费`
              : status?.next_window_at
                ? `下次开启：${fmtExpire(status.next_window_at)}`
                : NIGHT_WINDOW_TEXT
          }
        />
        <StatCard label="排队中" value={status?.pending ?? 0} tone="blue" hint="等待执行的任务数" />
        <StatCard
          label="今日完成"
          value={status?.done_today ?? 0}
          tone="green"
          hint={status?.failed_today ? `另有 ${status.failed_today} 条失败` : '今日已成功执行'}
        />
        <StatCard
          label="最近归因"
          value={lastProvider ? (burningPlan ? '免费套餐' : '自有池') : '—'}
          tone={burningPlan ? 'green' : lastProvider ? 'amber' : 'slate'}
          hint={lastProvider ?? '还没有执行记录'}
        />
      </div>

      {/* 归因警示：跑在自己池子上时明确提示，避免误以为在烧免费额度 */}
      {lastProvider && !burningPlan && (
        <div className="mt-3 flex items-start gap-2 rounded-xl border border-amber-300/70 bg-amber-50/80 px-3.5 py-2.5 text-xs text-amber-800 dark:border-amber-700/40 dark:bg-amber-900/10 dark:text-amber-200">
          <Zap size={14} className="mt-0.5 shrink-0" />
          <div className="leading-5">
            最近一次执行真实走的是 <b>{lastProvider}</b>，也就是本工具自己的上游池，
            <b>ZCode 免费额度没有被消耗</b>。CLI 会静默回退到 providerOrder 里的第一个 provider；
            要看是否命中套餐，以每行任务的「归因」列（真实 providerId）为准。
          </div>
        </div>
      )}

      {!status?.cli_entry && (
        <div className="mt-3 flex items-start gap-2 rounded-xl border border-rose-200 bg-rose-50 px-3.5 py-2.5 text-xs text-rose-600 dark:border-rose-800/60 dark:bg-rose-900/20 dark:text-rose-300">
          <CircleAlert size={14} className="mt-0.5 shrink-0" />
          没找到 <code className="mx-1">zcode.cjs</code>
          ，批处理无法执行。请确认 ZCode 客户端已安装，或设置环境变量
          <code className="mx-1">ZCODE_CLI_ENTRY</code> 指向它的绝对路径。
        </div>
      )}

      <div className="mt-4 grid grid-cols-12 items-start gap-4">
        {/* 左列：入队 */}
        <div className="col-span-5">
          <div className="card p-4">
            <div className="mb-2 flex items-center gap-2">
              <Plus size={15} className="text-brand-500" />
              <span className="text-sm font-medium">新建任务</span>
              <span className="text-xs text-slate-400">一行一条</span>
            </div>
            <textarea
              className="h-40 w-full resize-y rounded-lg border border-slate-200 bg-white p-2.5 text-xs leading-5 outline-none focus:border-brand-400 dark:border-zinc-700 dark:bg-zinc-900"
              placeholder={'每行一条 prompt，例如：\n总结这份周报的要点\n把这段日志里的错误归类'}
              value={input}
              onChange={(e) => setInput(e.target.value)}
            />
            <div className="mt-2 flex items-center justify-between">
              <span className="text-xs text-slate-400">
                待入队 {lines.length} 条 · 单条约吃 26 万 token，夜间档约 380 条/夜
              </span>
              <button
                className="btn-primary"
                onClick={() => void enqueue()}
                disabled={lines.length === 0 || busy === 'enqueue'}
              >
                {busy === 'enqueue' ? <Spinner /> : <Plus size={14} />} 入队
              </button>
            </div>
            <p className="mt-2 text-[11px] leading-4 text-slate-400">
              入队后不会立刻执行：调度器只在夜间窗口（{NIGHT_WINDOW_TEXT}）内消费。
              想立刻试一条，用右上角「立即跑一条」。
            </p>
          </div>
        </div>

        {/* 右列：队列 */}
        <div className="col-span-7">
          <div className="card p-4">
            <div className="mb-3 flex items-center gap-2">
              <Moon size={15} className="text-brand-500" />
              <span className="text-sm font-medium">任务队列</span>
              <span className="text-xs text-slate-400">{tasks.length} 条</span>
              <button
                className="btn-ghost ml-auto px-2 py-0.5 text-xs"
                onClick={() => void act('clear', () => api.apiServer.zcodeBatchClear())}
                disabled={busy === 'clear'}
              >
                <Trash2 size={12} /> 清空已结束
              </button>
            </div>

            {tasks.length === 0 ? (
              <EmptyState icon={<Clock size={22} />} title="队列为空" hint="左侧粘贴任务后点「入队」" />
            ) : (
              <div className="max-h-[520px] space-y-1.5 overflow-y-auto pr-1">
                {tasks.map((t) => {
                  const meta = STATUS_META[t.status] ?? STATUS_META.pending;
                  const w = t.window_tag ? WINDOW_META[t.window_tag] : null;
                  const open = expanded === t.id;
                  return (
                    <div
                      key={t.id}
                      className="rounded-lg border border-slate-100 px-3 py-2 dark:border-zinc-800"
                    >
                      <div className="flex items-center gap-2">
                        <span className="shrink-0 text-xs tabular-nums text-slate-400">#{t.id}</span>
                        <Badge tone={meta.tone}>{meta.label}</Badge>
                        {w && (
                          <Badge tone={t.window_tag === 'night' ? 'violet' : 'slate'} title={w.hint}>
                            {w.label}
                          </Badge>
                        )}
                        <span className="min-w-0 flex-1 truncate text-xs text-slate-700 dark:text-zinc-200" title={t.prompt}>
                          {t.prompt}
                        </span>
                        {t.status === 'failed' && t.error && (
                          <span
                            className="min-w-0 shrink truncate text-[11px] text-rose-500"
                            title={t.error}
                          >
                            {t.error.split('\n')[0]}
                          </span>
                        )}
                        {t.status === 'running' && t.started_at && (
                          <span className="shrink-0 text-[11px] tabular-nums text-blue-500">
                            已运行 {Math.max(0, Math.round((Date.now() - t.started_at * 1000) / 1000))}s
                          </span>
                        )}
                        {t.status === 'done' && <CheckCircle2 size={13} className="shrink-0 text-emerald-500" />}
                        <span className="shrink-0 text-[11px] text-slate-400">{fmtDuration(t.duration_ms)}</span>
                        <button
                          className="shrink-0 text-xs text-slate-400 hover:text-slate-600"
                          onClick={() => setExpanded(open ? null : t.id)}
                        >
                          {open ? '收起' : '详情'}
                        </button>
                        {(t.status === 'pending' || t.status === 'running' || t.status === 'cancel_requested') && (
                          <button
                            className="shrink-0 text-slate-400 hover:text-rose-500 disabled:opacity-40"
                            title={
                              t.status === 'pending'
                                ? '取消'
                                : t.status === 'cancel_requested'
                                  ? '已请求取消，等待本轮执行结束'
                                  : '请求取消（CLI 无法安全中断，本轮执行完成后生效，最长等 5 分钟）'
                            }
                            disabled={t.status === 'cancel_requested'}
                            onClick={() => void act(`c${t.id}`, () => api.apiServer.zcodeBatchCancel(t.id))}
                          >
                            <X size={13} />
                          </button>
                        )}
                        {(t.status === 'failed' || t.status === 'canceled') && (
                          <button
                            className="shrink-0 text-slate-400 hover:text-brand-500"
                            title="重跑"
                            onClick={() => void act(`r${t.id}`, () => api.apiServer.zcodeBatchRequeue(t.id))}
                          >
                            <RotateCcw size={13} />
                          </button>
                        )}
                      </div>

                      {/* 归因与用量：折叠时也显示，这是最关键的判断信息 */}
                      {(t.provider_used || t.input_tokens != null) && (
                        <div className="mt-1 flex flex-wrap items-center gap-x-3 gap-y-1 pl-6 text-[11px] text-slate-400">
                          <span className="flex items-center gap-1">
                            归因 <ProviderBadge provider={t.provider_used} />
                          </span>
                          {t.model_used && <span>模型 {t.model_used}</span>}
                          <span>
                            in {fmtTokens(t.input_tokens)} / out {fmtTokens(t.output_tokens)}
                          </span>
                          {t.finished_at && <span>{fmtDateTime(t.finished_at)}</span>}
                        </div>
                      )}

                      {open && (
                        <div className="mt-2 space-y-2 border-t border-slate-100 pt-2 dark:border-zinc-800">
                          <div>
                            <div className="text-[11px] font-medium text-slate-500">Prompt</div>
                            <pre className="mt-0.5 whitespace-pre-wrap break-all rounded bg-slate-50 p-2 text-[11px] leading-4 text-slate-600 dark:bg-zinc-900 dark:text-zinc-300">
                              {t.prompt}
                            </pre>
                          </div>
                          {t.result && (
                            <div>
                              <div className="text-[11px] font-medium text-slate-500">结果</div>
                              <pre className="mt-0.5 max-h-64 overflow-y-auto whitespace-pre-wrap break-all rounded bg-slate-50 p-2 text-[11px] leading-4 text-slate-600 dark:bg-zinc-900 dark:text-zinc-300">
                                {t.result}
                              </pre>
                            </div>
                          )}
                          {t.error && (
                            <div>
                              <div className="text-[11px] font-medium text-rose-500">
                                错误（首行已带分类前缀：超时/输出异常/CLI异常/其他）
                              </div>
                              <pre className="mt-0.5 max-h-48 overflow-y-auto whitespace-pre-wrap break-all rounded bg-rose-50 p-2 text-[11px] leading-4 text-rose-600 dark:bg-rose-900/20 dark:text-rose-300">
                                {t.error}
                              </pre>
                            </div>
                          )}
                          {t.attempts > 1 && (
                            <div className="text-[11px] text-slate-400">已尝试 {t.attempts} 次</div>
                          )}
                        </div>
                      )}
                    </div>
                  );
                })}
              </div>
            )}
          </div>
        </div>
      </div>
    </div>
  );
}
