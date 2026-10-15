/**
 * WorkBuddy 多账号视图（按 AI Work Studio 设计语言）：
 * 国内/海外账号的捕获与一键切换。
 *
 * 用法：先在 WorkBuddy 客户端里登录账号 A → 本页「捕获当前登录」；
 * 再登录账号 B → 再捕获。之后任意时刻点「切换」即完成账号+站点环境（国内/海外）切换。
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
} from 'lucide-react';
import { Badge, EmptyState, StatCard } from '../../components/ui';
import { cn } from '../../lib/cn';
import { wbApi } from './api/workbuddy';
import type { WbAccount, WbStatus } from './types';

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
      🌍 海外
    </Badge>
  ) : (
    <Badge tone="amber" title="国内会话（www.workbuddy.cn）">
      🇨🇳 国内
    </Badge>
  );
}

export default function WorkBuddyView({
  onNotify,
}: {
  onNotify: (msg: string) => void;
}) {
  const [status, setStatus] = useState<WbStatus | null>(null);
  const [accounts, setAccounts] = useState<WbAccount[]>([]);
  const [busy, setBusy] = useState<string | null>(null);
  const [confirming, setConfirming] = useState<number | null>(null);

  const reload = useCallback(async () => {
    const [s, list] = await Promise.all([wbApi.status(), wbApi.list()]);
    setStatus(s);
    setAccounts(list);
  }, []);

  useEffect(() => {
    reload().catch((e) => onNotify(`加载失败: ${e}`));
  }, [reload, onNotify]);

  const capture = async () => {
    setBusy('capture');
    try {
      const acc = await wbApi.capture();
      onNotify(
        `已捕获: ${acc.label ?? shortUid(acc.uid)} (${
          acc.region === 'overseas' ? '海外' : '国内'
        })`,
      );
      await reload();
    } catch (e) {
      onNotify(`捕获失败: ${e}`);
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
      await wbApi.switchTo(id);
      onNotify('已写入会话并重启 WorkBuddy，请在其窗口确认登录状态');
      await reload();
    } catch (e) {
      onNotify(`切换失败: ${e}`);
    } finally {
      setBusy(null);
    }
  };

  const remove = async (id: number) => {
    try {
      await wbApi.remove(id);
      await reload();
    } catch (e) {
      onNotify(`删除失败: ${e}`);
    }
  };

  const rename = async (acc: WbAccount) => {
    const name = window.prompt('备注名（留空则清除）:', acc.label ?? '');
    if (name === null) return;
    try {
      await wbApi.rename(acc.id, name.trim() || null);
      await reload();
    } catch (e) {
      onNotify(`重命名失败: ${e}`);
    }
  };

  const currentUid = status?.logged_in ? status.uid : null;
  const overseaCount = accounts.filter((a) => a.region === 'overseas').length;

  return (
    <div>
      {/* 统计卡 */}
      <div className="grid grid-cols-2 gap-3 md:grid-cols-3">
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

      {status && !status.exe_found && (
        <div className="mt-4 flex items-center justify-between gap-3 rounded-xl border border-amber-200 bg-amber-50 px-4 py-3 text-sm text-amber-700 dark:border-amber-500/30 dark:bg-amber-500/10 dark:text-amber-300">
          <span className="flex items-center gap-2">
            <CircleAlert size={15} className="shrink-0" />
            未找到 WorkBuddy.exe（自动探测失败），切换前请先指定路径。
          </span>
          <button
            className="btn-outline shrink-0"
            onClick={async () => {
              const p = window.prompt(
                'WorkBuddy.exe 完整路径，如 D:\\Work\\WorkBuddyAI\\WorkBuddy.exe',
              );
              if (!p) return;
              try {
                await wbApi.setExePath(p.trim());
                onNotify('路径已保存');
                await reload();
              } catch (e) {
                onNotify(`保存失败: ${e}`);
              }
            }}
          >
            指定路径
          </button>
        </div>
      )}

      {/* 操作行 */}
      <div className="mt-4 flex flex-wrap items-center gap-2">
        <button className="btn-primary" onClick={capture} disabled={busy !== null}>
          <Camera size={15} />
          {busy === 'capture' ? '捕获中…' : '捕获当前登录'}
        </button>
        <button
          className="btn-outline"
          onClick={async () => {
            if (
              !window.confirm(
                '海外登录流程（共 4 步）:\n\n' +
                  '1. 退出 WorkBuddy 并登出当前账号（前提：当前账号已捕获）\n' +
                  '2. 以海外配置重启，登录页指向 www.workbuddy.ai\n' +
                  '3. 你在客户端登录海外账号（需先在浏览器注册，可能要挂代理）\n' +
                  '4. 回到本页点「捕获当前登录」\n\n' +
                  '之后国内/海外随时一键互切。确定继续吗？',
              )
            ) {
              return;
            }
            setBusy('oversea');
            try {
              await wbApi.launchOverseaLogin();
              onNotify(
                '已登出并以海外配置重启 WorkBuddy，请在客户端完成海外账号登录，然后回来点「捕获当前登录」',
              );
              await reload();
            } catch (e) {
              onNotify(`海外登录启动失败: ${e}`);
            } finally {
              setBusy(null);
            }
          }}
          disabled={busy !== null}
          title="登出当前账号并以海外配置（workbuddy.ai）重启 WorkBuddy，用于登录海外账号"
        >
          <Globe size={15} /> {busy === 'oversea' ? '启动中…' : '海外登录'}
        </button>
      </div>

      {/* 账号列表 */}
      <div className="mt-4">
        {accounts.length === 0 ? (
          <EmptyState
            icon={<TerminalSquare size={26} />}
            title="还没有捕获任何 WorkBuddy 账号"
            hint="在 WorkBuddy 客户端登录账号（国内或海外）→ 回到这里点「捕获当前登录」→ 换下一个账号登录 → 再捕获。之后即可在此一键切换，国内/海外站点环境随账号自动切换。"
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
                    onClick={() => rename(acc)}
                  >
                    <Pencil size={14} />
                  </button>
                  <button
                    className="rounded-lg px-2 py-1.5 text-rose-400 hover:bg-rose-50 hover:text-rose-600 dark:hover:bg-rose-500/10"
                    title="删除"
                    onClick={() => remove(acc.id)}
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
                  onClick={() => switchTo(acc.id)}
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
        说明：切换会重启 WorkBuddy 客户端；同一账号重复「捕获」即刷新其会话。国内/海外由官方会话文件的登录域决定，切换账号即自动切换站点环境。
      </p>
    </div>
  );
}
