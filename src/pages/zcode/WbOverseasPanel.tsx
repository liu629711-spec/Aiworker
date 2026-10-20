/**
 * ZCode 页 · WorkBuddy 海外版面板（用户裁决：海外版直接接入 ZCode 页）。
 *
 * 复用 buddy-sessions 的会话快照库（zcode_wb_* 命令族，wbSessionsApi）：
 * - 海外客户端（WorkBuddyAI.exe，D:\WorkBuddyAI）登录海外账号
 * - 「捕获当前登录」把整份会话文件存入快照库（region=overseas）
 * - 「切换」把指定会话写回并按区启动对应客户端（国内/海外 exe 自动选择）
 * 与 Buddy 账号池（workbuddy_* 凭证池）分工：本面板管登录态，凭证/积分在账号管理页。
 */
import { useCallback, useEffect, useState } from 'react';
import { Camera, Globe, Power, RefreshCw, SwitchCamera, Trash2 } from 'lucide-react';
import { Badge, EmptyState, Spinner } from '../../components/ui';
import { cn } from '../../lib/cn';
import { wbSessionsApi } from '../buddy/api/wbSessions';
import type { WbAccount, WbStatus } from '../../types';

function shortUid(uid: string): string {
  if (uid.length <= 10) return uid;
  return `${uid.slice(0, 4)}…${uid.slice(-4)}`;
}

function fmtTime(ts: number): string {
  if (!ts) return '—';
  const d = new Date(ts * 1000);
  const p = (n: number) => String(n).padStart(2, '0');
  return `${d.getMonth() + 1}/${d.getDate()} ${p(d.getHours())}:${p(d.getMinutes())}`;
}

export default function WbOverseasPanel() {
  const [accounts, setAccounts] = useState<WbAccount[]>([]);
  const [status, setStatus] = useState<WbStatus | null>(null);
  const [loading, setLoading] = useState(true);
  const [busy, setBusy] = useState<string | null>(null);
  const [err, setErr] = useState('');

  const refresh = useCallback(async () => {
    try {
      const [list, st] = await Promise.all([wbSessionsApi.list(), wbSessionsApi.status().catch(() => null)]);
      setAccounts(list);
      setStatus(st);
      setErr('');
    } catch (e) {
      setErr(String(e));
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  const run = async (key: string, fn: () => Promise<unknown>, ok: string) => {
    setBusy(key);
    setErr('');
    try {
      await fn();
      await refresh();
    } catch (e) {
      setErr(String(e));
    } finally {
      setBusy(null);
    }
  };

  const overseas = accounts.filter((a) => a.region === 'overseas');
  const currentRegion = status?.logged_in ? status.region ?? null : null;

  if (loading) {
    return (
      <div className="flex items-center gap-2 py-10 text-sm text-slate-400">
        <Spinner /> 加载中…
      </div>
    );
  }

  return (
    <div className="space-y-4">
      {/* 状态卡 */}
      <div className="grid grid-cols-2 gap-3 md:grid-cols-4">
        <StatBox label="海外客户端" value={status?.running ? '运行中' : '未运行'} tone={status?.running ? 'green' : 'slate'} />
        <StatBox label="当前登录区" value={currentRegion === 'overseas' ? '海外' : currentRegion === 'domestic' ? '国内' : '未登录'} tone={currentRegion === 'overseas' ? 'brand' : 'slate'} />
        <StatBox label="海外会话数" value={String(overseas.length)} hint="快照库" tone="green" />
        <StatBox label="客户端路径" value={status?.exe_found ? '已找到' : '未找到'} hint={status?.exe_found ?? '可在下方指定'} tone={status?.exe_found ? 'green' : 'amber'} />
      </div>

      {err && (
        <div className="rounded-lg border border-rose-200 bg-rose-50 px-3 py-2 text-xs text-rose-600 dark:border-rose-500/30 dark:bg-rose-500/10 dark:text-rose-400">
          {err}
        </div>
      )}

      {/* 操作行 */}
      <div className="flex flex-wrap items-center gap-2">
        <button className="btn-outline" onClick={() => void run('capture', () => wbSessionsApi.capture(), '已捕获')}>
          {busy === 'capture' ? <Spinner /> : <Camera size={15} />} 捕获当前登录
        </button>
        <button
          className="btn-outline"
          disabled={busy !== null}
          title="退出当前客户端 → 以海外配置重启（登录页指向 workbuddy.ai）→ 登录海外账号后回来「捕获」"
          onClick={() => void run('oversea', () => wbSessionsApi.launchOverseaLogin(), '已发起海外登录')}
        >
          {busy === 'oversea' ? <Spinner /> : <Globe size={15} />} 海外登录
        </button>
        <button className="btn-outline" onClick={() => void refresh()} disabled={loading}>
          <RefreshCw size={15} /> 刷新
        </button>
        {currentRegion === 'overseas' && (
          <Badge tone="green">
            <Globe size={10} className="mr-1" /> 当前会话 = 海外
          </Badge>
        )}
      </div>

      {/* 会话列表 */}
      {accounts.length === 0 ? (
        <EmptyState
          icon={<Globe size={28} />}
          title="还没有捕获任何会话"
          hint="先在 WorkBuddy / WorkBuddy AI 客户端里登录账号，再点「捕获当前登录」"
        />
      ) : (
        <div className="overflow-hidden rounded-xl border border-slate-200 dark:border-zinc-800">
          <table className="w-full text-sm">
            <thead>
              <tr className="bg-slate-50 text-left text-xs text-slate-500 dark:bg-zinc-900 dark:text-zinc-400">
                <th className="px-4 py-2 font-medium">账号</th>
                <th className="px-4 py-2 font-medium">区域</th>
                <th className="px-4 py-2 font-medium">端点</th>
                <th className="px-4 py-2 font-medium">捕获时间</th>
                <th className="px-4 py-2 text-right font-medium">操作</th>
              </tr>
            </thead>
            <tbody>
              {accounts.map((a) => (
                <tr key={a.id} className="border-t border-slate-200 first:border-t-0 dark:border-zinc-800">
                  <td className="px-4 py-2.5">
                    <div className="flex items-center gap-1.5">
                      <span className="font-medium">{a.label || shortUid(a.uid)}</span>
                      {a.is_current && <Badge tone="green">当前</Badge>}
                    </div>
                    <div className="font-mono text-xs text-slate-400">{shortUid(a.uid)}</div>
                  </td>
                  <td className="px-4 py-2.5">
                    {a.region === 'overseas' ? (
                      <Badge tone="brand">海外</Badge>
                    ) : (
                      <Badge tone="slate">国内</Badge>
                    )}
                  </td>
                  <td className="px-4 py-2.5 font-mono text-xs text-slate-400">{a.domain || '—'}</td>
                  <td className="px-4 py-2.5 text-xs text-slate-400">{fmtTime(a.captured_at)}</td>
                  <td className="px-4 py-2.5">
                    <div className="flex items-center justify-end gap-1.5">
                      <button
                        className="btn-outline !px-2 !py-1 text-xs"
                        disabled={busy !== null || a.is_current}
                        title={a.is_current ? '已是当前登录' : '退出客户端 → 写入此会话 → 重新启动'}
                        onClick={() => void run(`sw${a.id}`, () => wbSessionsApi.switchTo(a.id), '已切换')}
                      >
                        {busy === `sw${a.id}` ? <Spinner /> : <SwitchCamera size={13} />} 切换
                      </button>
                      <button
                        className="text-rose-400 transition hover:text-rose-600"
                        disabled={busy !== null}
                        title="删除此会话快照"
                        onClick={() => void run(`rm${a.id}`, () => wbSessionsApi.remove(a.id), '已删除')}
                      >
                        {busy === `rm${a.id}` ? <Spinner /> : <Trash2 size={14} />}
                      </button>
                    </div>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}

      <div className="rounded-lg border border-slate-200 bg-slate-50 px-3 py-2 text-xs leading-relaxed text-slate-500 dark:border-zinc-800 dark:bg-zinc-900 dark:text-zinc-400">
        <p>
          <Power size={11} className="mr-1 inline" />
          海外客户端 = <span className="font-mono">D:\WorkBuddyAI\WorkBuddyAI.exe</span>（官方 WorkBuddy AI
          5.8.4，登录体系 workbuddy.ai）；国内客户端 = <span className="font-mono">D:\Work\WorkBuddyAI\WorkBuddy.exe</span>。
          「切换」会退出当前客户端并按会话区域启动对应入口（海外/国内账号登录态互不覆盖）。
        </p>
        <p className="mt-1">
          会话快照与 API 网关账号池相互独立：需要海外号走网关（积分/签到/模型调用），请到「Buddy ·
          账号管理」页用 OAuth 登录选「海外版登录」入池。
        </p>
      </div>
    </div>
  );
}

function StatBox({
  label,
  value,
  hint,
  tone,
}: {
  label: string;
  value: string;
  hint?: string;
  tone: 'green' | 'amber' | 'brand' | 'slate';
}) {
  const toneCls = cn(
    'rounded-xl border px-3 py-2.5',
    tone === 'green' && 'border-emerald-200 bg-emerald-50 dark:border-emerald-500/30 dark:bg-emerald-500/10',
    tone === 'amber' && 'border-amber-200 bg-amber-50 dark:border-amber-500/30 dark:bg-amber-500/10',
    tone === 'brand' && 'border-sky-200 bg-sky-50 dark:border-sky-500/30 dark:bg-sky-500/10',
    tone === 'slate' && 'border-slate-200 bg-slate-50 dark:border-zinc-800 dark:bg-zinc-900',
  );
  return (
    <div className={toneCls}>
      <div className="text-xs text-slate-500 dark:text-zinc-400">{label}</div>
      <div className="mt-0.5 text-sm font-semibold text-slate-800 dark:text-zinc-100">{value}</div>
      {hint && <div className="truncate text-[10px] text-slate-400">{hint}</div>}
    </div>
  );
}
