/**
 * buddy-sessions 多账号会话（原 ZCode 页 WorkBuddy 子视图移植，2026-10-16）：
 * 国内/海外登录态的捕获与一键切换。
 *
 * 用法：先在 WorkBuddy 客户端里登录账号 A → 本页「捕获当前登录」；
 * 再登录账号 B → 再捕获。之后任意时刻点「切换」即完成账号+站点环境（国内/海外）切换。
 * 海外账号：点「海外登录」→ 客户端以 workbuddy.ai 配置重启 → 登录后回来「捕获」。
 *
 * 与「账号管理」页的分工：本页管登录态快照库（整份会话文件，zcode_wb_* 命令族）；
 * 账号池管凭证（签到/积分/网关）。捕获后如需这些能力，到账号管理页「扫描本机账号」入池。
 */
import { useCallback, useEffect, useState } from 'react';
import {
  Camera,
  Globe,
  RefreshCw,
  Trash2,
  SwitchCamera,
  Pencil,
  TerminalSquare,
  CircleAlert,
  Save,
} from 'lucide-react';
import PageHeader from '../../components/PageHeader';
import { Badge, EmptyState, Modal, Spinner, StatCard } from '../../components/ui';
import { cn } from '../../lib/cn';
import { useAppStore } from '../../store';
import { wbSessionsApi } from './api/wbSessions';
import type { WbAccount, WbStatus } from '../../types';

/** uid 短标识：d6a4fbf9-93ae-49bd-a42b-25839cf194d8 → d6a4…9d8 */
function shortUid(uid: string): string {
  if (uid.length <= 10) return uid;
  return `${uid.slice(0, 4)}…${uid.slice(-4)}`;
}

function fmtTime(ts: number): string {
  if (!ts) return '—';
  const d = new Date(ts * 1000);
  const p = (n: number) => String(n).padStart(2, '0');
  return `${p(d.getMonth() + 1)}-${p(d.getDate())} ${p(d.getHours())}:${p(d.getMinutes())}`;
}

function RegionBadge({ region }: { region: string }) {
  return region === 'overseas' ? (
    <Badge tone="blue" title="海外会话（www.workbuddy.ai）">
      海外
    </Badge>
  ) : (
    <Badge tone="amber" title="国内会话（www.workbuddy.cn）">
      国内
    </Badge>
  );
}

export default function BuddySessions() {
  const pushToast = useAppStore((s) => s.pushToast);
  const [status, setStatus] = useState<WbStatus | null>(null);
  const [accounts, setAccounts] = useState<WbAccount[]>([]);
  // 进行中的操作：capture / oversea / switch-<id>（互斥所有主按钮，对齐原视图）
  const [busy, setBusy] = useState<string | null>(null);
  // 切换两步确认：3 秒内再点一次才执行（切换会重启客户端，防误触）
  const [confirming, setConfirming] = useState<number | null>(null);
  // 海外登录向导说明/确认弹框（禁 window.confirm，红线）
  const [overseaOpen, setOverseaOpen] = useState(false);
  // 指定 WorkBuddy.exe 路径弹框（禁 window.prompt，红线）
  const [exeOpen, setExeOpen] = useState(false);
  const [exePath, setExePath] = useState('');
  const [exeSaving, setExeSaving] = useState(false);
  // 重命名弹框
  const [renameFor, setRenameFor] = useState<WbAccount | null>(null);
  const [renameText, setRenameText] = useState('');
  const [renameBusy, setRenameBusy] = useState(false);
  // 删除确认弹框
  const [deleteFor, setDeleteFor] = useState<WbAccount | null>(null);
  const [deleteBusy, setDeleteBusy] = useState(false);

  const reload = useCallback(async () => {
    try {
      const [s, list] = await Promise.all([wbSessionsApi.status(), wbSessionsApi.list()]);
      setStatus(s);
      setAccounts(list);
    } catch (e) {
      pushToast('error', `加载失败: ${e}`);
    }
  }, [pushToast]);

  useEffect(() => {
    void reload();
  }, [reload]);

  const capture = async () => {
    setBusy('capture');
    try {
      const acc = await wbSessionsApi.capture();
      pushToast(
        'success',
        `已捕获: ${acc.label ?? shortUid(acc.uid)}（${acc.region === 'overseas' ? '海外' : '国内'}）`,
      );
      await reload();
    } catch (e) {
      pushToast('error', `捕获失败: ${e}`);
    } finally {
      setBusy(null);
    }
  };

  const switchTo = async (id: number) => {
    if (confirming !== id) {
      setConfirming(id);
      window.setTimeout(() => setConfirming((c) => (c === id ? null : c)), 3000);
      return;
    }
    setConfirming(null);
    setBusy(`switch-${id}`);
    try {
      await wbSessionsApi.switchTo(id);
      pushToast('info', '已写入会话并重启 WorkBuddy，请在其窗口确认登录状态');
      await reload();
    } catch (e) {
      pushToast('error', `切换失败: ${e}`);
    } finally {
      setBusy(null);
    }
  };

  const confirmOversea = async () => {
    setOverseaOpen(false);
    setBusy('oversea');
    try {
      await wbSessionsApi.launchOverseaLogin();
      pushToast(
        'info',
        '已登出并以海外配置重启 WorkBuddy，请在客户端完成海外账号登录，然后回来点「捕获当前登录」',
      );
      await reload();
    } catch (e) {
      pushToast('error', `海外登录启动失败: ${e}`);
    } finally {
      setBusy(null);
    }
  };

  const saveExePath = async () => {
    const p = exePath.trim();
    if (!p) return;
    setExeSaving(true);
    try {
      await wbSessionsApi.setExePath(p);
      pushToast('success', '路径已保存');
      setExeOpen(false);
      await reload();
    } catch (e) {
      pushToast('error', `保存失败: ${e}`);
    } finally {
      setExeSaving(false);
    }
  };

  const confirmRename = async () => {
    if (!renameFor) return;
    setRenameBusy(true);
    try {
      await wbSessionsApi.rename(renameFor.id, renameText.trim() || null);
      setRenameFor(null);
      await reload();
    } catch (e) {
      pushToast('error', `重命名失败: ${e}`);
    } finally {
      setRenameBusy(false);
    }
  };

  const confirmDelete = async () => {
    if (!deleteFor) return;
    setDeleteBusy(true);
    try {
      await wbSessionsApi.remove(deleteFor.id);
      setDeleteFor(null);
      await reload();
    } catch (e) {
      pushToast('error', `删除失败: ${e}`);
    } finally {
      setDeleteBusy(false);
    }
  };

  const currentUid = status?.logged_in ? status.uid : null;
  const overseaCount = accounts.filter((a) => a.region === 'overseas').length;

  return (
    <div className="animate-fade-in">
      <PageHeader
        title="Buddy · 多账号会话"
        desc="国内/海外登录态捕获 · 一键切换（切换即切换站点环境）"
        actions={
          <>
            <button onClick={() => void reload()} className="btn-outline" disabled={busy !== null}>
              <RefreshCw size={15} className={busy === 'capture' ? 'animate-spin' : ''} /> 刷新
            </button>
            <button className="btn-primary" onClick={() => void capture()} disabled={busy !== null}>
              <Camera size={15} />
              {busy === 'capture' ? '捕获中…' : '捕获当前登录'}
            </button>
            <button
              className="btn-outline"
              onClick={() => setOverseaOpen(true)}
              disabled={busy !== null}
              title="登出当前账号并以海外配置（workbuddy.ai）重启 WorkBuddy，用于登录海外账号"
            >
              <Globe size={15} /> {busy === 'oversea' ? '启动中…' : '海外登录'}
            </button>
          </>
        }
      />

      {/* 统计卡 */}
      <div className="mt-5 grid grid-cols-2 gap-3 md:grid-cols-3">
        <StatCard
          label="WorkBuddy 当前登录"
          value={
            currentUid ? (
              <span className="font-mono text-lg" title={currentUid}>
                {shortUid(currentUid)}
              </span>
            ) : (
              '未登录'
            )
          }
          hint={status?.domain ?? '未检测到登录会话'}
          tone={currentUid ? 'brand' : 'slate'}
        />
        <StatCard
          label="客户端状态"
          value={
            <span className="flex items-center gap-2">
              <span
                className={cn(
                  'inline-block h-2.5 w-2.5 rounded-full',
                  status?.running
                    ? 'bg-emerald-500 shadow-[0_0_6px_rgba(16,185,129,0.6)]'
                    : 'bg-slate-300 dark:bg-zinc-600',
                )}
              />
              {status?.running ? '运行中' : '未运行'}
            </span>
          }
          hint="WorkBuddy 桌面客户端"
          tone={status?.running ? 'green' : 'slate'}
        />
        <StatCard
          label="已捕获账号"
          value={accounts.length}
          hint={overseaCount > 0 ? `含海外 ${overseaCount} 个` : '暂无海外账号'}
          tone="violet"
        />
      </div>

      {/* 未找到客户端 exe：切换前需先指定路径 */}
      {status && !status.exe_found && (
        <div className="mt-4 flex items-center justify-between gap-3 rounded-xl border border-amber-200 bg-amber-50 px-4 py-3 text-sm text-amber-700 dark:border-amber-500/30 dark:bg-amber-500/10 dark:text-amber-300">
          <span className="flex items-center gap-2">
            <CircleAlert size={15} className="shrink-0" />
            未找到 WorkBuddy.exe（自动探测失败），切换前请先指定路径。
          </span>
          <button
            className="btn-outline shrink-0"
            onClick={() => {
              setExePath('');
              setExeOpen(true);
            }}
          >
            指定路径
          </button>
        </div>
      )}

      {/* 账号列表 */}
      <div className="mt-4">
        {accounts.length === 0 ? (
          <EmptyState
            icon={<TerminalSquare size={26} />}
            title="还没有捕获任何 WorkBuddy 账号"
            hint="在 WorkBuddy 客户端登录账号（国内或海外）→ 点上方「捕获当前登录」→ 换下一个账号登录 → 再捕获。之后即可在此一键切换，国内/海外站点环境随账号自动切换。"
          />
        ) : (
          <div className="grid gap-3 md:grid-cols-2 xl:grid-cols-3">
            {accounts.map((acc) => (
              <div
                key={acc.id}
                className={cn(
                  'card p-4',
                  acc.is_current && 'ring-2 ring-emerald-500/40 dark:ring-emerald-500/30',
                )}
              >
                <div className="flex items-center gap-2">
                  <RegionBadge region={acc.region} />
                  {acc.is_current && <Badge tone="green">使用中</Badge>}
                  <span className="ml-auto shrink-0" />
                  <button
                    className="rounded-lg px-2 py-1.5 text-slate-400 hover:bg-slate-100 hover:text-slate-600 dark:hover:bg-zinc-800"
                    title="重命名"
                    disabled={busy !== null}
                    onClick={() => {
                      setRenameText(acc.label ?? '');
                      setRenameFor(acc);
                    }}
                  >
                    <Pencil size={14} />
                  </button>
                  <button
                    className="rounded-lg px-2 py-1.5 text-rose-400 hover:bg-rose-50 hover:text-rose-600 dark:hover:bg-rose-500/10"
                    title="删除"
                    disabled={busy !== null}
                    onClick={() => setDeleteFor(acc)}
                  >
                    <Trash2 size={14} />
                  </button>
                </div>
                <div
                  className="mt-2 truncate text-sm font-semibold text-slate-800 dark:text-zinc-100"
                  title={acc.uid}
                >
                  {acc.label ?? shortUid(acc.uid)}
                </div>
                <div className="mt-0.5 truncate text-xs text-slate-400">
                  {acc.uin ? `uin ${acc.uin} · ` : ''}
                  {acc.domain ?? '未知域'} · 捕获 {fmtTime(acc.captured_at)}
                </div>
                <button
                  className={cn(
                    'btn mt-3 w-full',
                    acc.is_current
                      ? 'cursor-default bg-emerald-50 text-emerald-600 dark:bg-emerald-500/10 dark:text-emerald-300'
                      : 'btn-outline',
                  )}
                  disabled={acc.is_current || busy !== null}
                  onClick={() => void switchTo(acc.id)}
                  title={
                    acc.is_current
                      ? '当前正在使用此账号'
                      : '退出 WorkBuddy → 写入此账号会话 → 重新启动（3 秒内再点一次确认）'
                  }
                >
                  <SwitchCamera size={14} />
                  {busy === `switch-${acc.id}`
                    ? '切换中…'
                    : acc.is_current
                      ? '✓ 使用中'
                      : confirming === acc.id
                        ? '再点一次确认切换'
                        : '切换到此账号'}
                </button>
              </div>
            ))}
          </div>
        )}
      </div>

      <p className="mt-4 flex items-start gap-1.5 text-xs leading-relaxed text-slate-400">
        <RefreshCw size={12} className="mt-0.5 shrink-0" />
        说明：切换会重启 WorkBuddy 客户端；同一账号重复「捕获」即刷新其会话。国内/海外由官方会话文件的登录域决定，切换账号即自动切换站点环境。捕获后如需签到/积分/网关能力，请到「账号管理」页扫描入池。
      </p>

      {/* 海外登录向导说明/确认弹框（原 window.confirm 改 Modal，红线） */}
      <Modal
        open={overseaOpen}
        onClose={() => setOverseaOpen(false)}
        title="海外登录向导"
        footer={
          <>
            <button className="btn-outline" onClick={() => setOverseaOpen(false)}>
              取消
            </button>
            <button className="btn-primary" onClick={() => void confirmOversea()}>
              确认启动
            </button>
          </>
        }
      >
        <div className="space-y-3 text-sm">
          <div className="rounded-lg bg-amber-50 p-3 text-xs text-amber-700 dark:bg-amber-500/10 dark:text-amber-300">
            将退出 WorkBuddy 并登出当前账号。前提：当前账号已捕获（否则会话丢失）。
          </div>
          <ol className="list-decimal space-y-1.5 pl-5 text-xs text-slate-600 dark:text-zinc-300">
            <li>退出 WorkBuddy 并登出当前账号（前提：当前账号已捕获）</li>
            <li>以海外配置重启，登录页指向 www.workbuddy.ai</li>
            <li>在客户端登录海外账号（需先在浏览器注册，可能要挂代理）</li>
            <li>回到本页点「捕获当前登录」</li>
          </ol>
          <p className="text-xs text-slate-400">之后国内/海外随时一键互切。</p>
        </div>
      </Modal>

      {/* 指定 WorkBuddy.exe 路径弹框（原 window.prompt 改 Modal，红线） */}
      <Modal
        open={exeOpen}
        onClose={() => !exeSaving && setExeOpen(false)}
        title="指定 WorkBuddy.exe 路径"
        footer={
          <>
            <button className="btn-outline" onClick={() => setExeOpen(false)} disabled={exeSaving}>
              取消
            </button>
            <button
              className="btn-primary"
              onClick={() => void saveExePath()}
              disabled={exeSaving || !exePath.trim()}
            >
              {exeSaving ? <Spinner /> : <Save size={14} />} 保存
            </button>
          </>
        }
      >
        <label className="block text-sm">
          <span className="mb-1 block text-xs font-medium text-slate-500">
            WorkBuddy.exe 完整路径
          </span>
          <input
            className="input w-full"
            value={exePath}
            onChange={(e) => setExePath(e.target.value)}
            placeholder="D:\Work\WorkBuddyAI\WorkBuddy.exe"
          />
        </label>
      </Modal>

      {/* 重命名弹框（原 window.prompt 改 Modal，红线） */}
      <Modal
        open={renameFor != null}
        onClose={() => !renameBusy && setRenameFor(null)}
        title="重命名备注"
        footer={
          <>
            <button className="btn-outline" onClick={() => setRenameFor(null)} disabled={renameBusy}>
              取消
            </button>
            <button className="btn-primary" onClick={() => void confirmRename()} disabled={renameBusy}>
              {renameBusy ? <Spinner /> : null} 保存
            </button>
          </>
        }
      >
        <label className="block text-sm">
          <span className="mb-1 block text-xs font-medium text-slate-500">备注名（留空则清除）</span>
          <input
            className="input w-full"
            value={renameText}
            onChange={(e) => setRenameText(e.target.value)}
            placeholder="账号备注名"
          />
        </label>
      </Modal>

      {/* 删除确认弹框（禁 window.confirm，红线） */}
      <Modal
        open={deleteFor != null}
        onClose={() => !deleteBusy && setDeleteFor(null)}
        title="删除已捕获账号"
        footer={
          <>
            <button className="btn-outline" onClick={() => setDeleteFor(null)} disabled={deleteBusy}>
              取消
            </button>
            <button
              className="btn-primary !bg-rose-600 hover:!bg-rose-500"
              onClick={() => void confirmDelete()}
              disabled={deleteBusy}
            >
              {deleteBusy ? <Spinner /> : null} 确认删除
            </button>
          </>
        }
      >
        <div className="text-sm">
          确定删除「{deleteFor?.label ?? shortUid(deleteFor?.uid ?? '')}」的捕获记录？
          <div className="mt-1 text-xs text-slate-400">
            仅删除本工具的会话快照，不影响账号池与客户端当前登录态。
          </div>
        </div>
      </Modal>
    </div>
  );
}
