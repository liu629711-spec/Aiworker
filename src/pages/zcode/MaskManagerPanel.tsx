/**
 * 马甲邮箱管理面板：列出 Relay 里的全部马甲，标注哪些已入池，可删除腾额度。
 *
 * 免费账号上限 50 个；删未入池的最安全（纯粹没用过）。
 * 删已入池的不影响本地凭证，但该地址以后不能再收信/找回。
 */
import { useCallback, useEffect, useState } from 'react';
import { RefreshCw, Trash2, Mail, CircleAlert, Check } from 'lucide-react';
import PageHeader from '../../components/PageHeader';
import GuidePanel from '../../components/GuidePanel';
import { Badge, Modal } from '../../components/ui';
import { cn } from '../../lib/cn';
import { registerApi, type RelayMaskList } from './api/register';

export default function MaskManagerPanel() {
  const [data, setData] = useState<RelayMaskList | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  // 待确认删除的马甲
  const [pendingDelete, setPendingDelete] = useState<{ id: number; address: string; pooled: boolean } | null>(
    null,
  );
  const [deleting, setDeleting] = useState(false);

  const reload = useCallback(() => {
    setLoading(true);
    setError(null);
    registerApi
      .relayMasks()
      .then(setData)
      .catch((e) => setError(String(e)))
      .finally(() => setLoading(false));
  }, []);

  useEffect(() => {
    reload();
  }, [reload]);

  const doDelete = async () => {
    if (!pendingDelete) return;
    setDeleting(true);
    try {
      await registerApi.deleteMask(pendingDelete.id);
      setNotice(`已删除 ${pendingDelete.address}，额度已释放`);
      setPendingDelete(null);
      reload();
    } catch (e) {
      setError(String(e));
    } finally {
      setDeleting(false);
    }
  };

  const pooled = data?.masks.filter((m) => m.pooled) ?? [];
  const unused = data?.masks.filter((m) => !m.pooled) ?? [];
  const remaining = data ? data.free_limit - data.total : 0;

  if (error && !data) {
    return (
      <div className="space-y-3">
        <div className="flex items-start gap-2 rounded-xl border border-rose-200 bg-rose-50 px-4 py-3 text-sm text-rose-600 dark:border-rose-500/30 dark:bg-rose-500/10 dark:text-rose-300">
          <CircleAlert size={15} className="mt-0.5 shrink-0" />
          <span className="min-w-0 break-all">{error}</span>
        </div>
        <button className="btn-outline" onClick={reload} disabled={loading}>
          <RefreshCw size={15} className={loading ? 'animate-spin' : ''} /> 重试
        </button>
      </div>
    );
  }

  return (
    <div className="animate-fade-in space-y-4">
      <PageHeader
        title="ZCode · 马甲邮箱"
        desc="Firefox Relay 马甲清单 · 已入池 / 未注册分类 · 删除腾额度"
        actions={
          <button className="btn-outline" onClick={reload} disabled={loading}>
            <RefreshCw size={15} className={loading ? 'animate-spin' : ''} />
            {loading ? '读取中…' : '刷新'}
          </button>
        }
      />

      <GuidePanel
        steps={[
          {
            title: '这个页面在做什么',
            detail:
              '「马甲邮箱」是 Firefox Relay 提供的邮箱别名：形如 xxx@mozmail.com。' +
              '别人往这个地址发信，会自动转发到你真实的 Gmail。' +
              '注册 z.ai 时用别名而不是真实邮箱，既能避免同邮箱被判重，又只需要维护一个收件箱。',
          },
          {
            title: '前置：登录 Firefox Relay',
            detail:
              'Firefox Relay 需要 Firefox 账号，免费额度约 5 个别名（页面顶部显示「剩余额度」）。' +
              '工具通过读取本机 Zen 浏览器的 cookie 来访问 Relay——所以先在本机用 Zen 浏览器登录 relay.firefox.com，再回来点「刷新」。',
            warning: '未登录时列表为空或报错，先去 Zen 浏览器登录一次。',
          },
          {
            title: '生成别名并注册入池',
            detail:
              '点「新建」生成若干别名，把它们复制到「自动注册」页的输入框里批量注册。' +
              '注册成功后这些别名的邮箱会出现在「已入池」分类，并标注对应的账号。',
          },
          {
            title: '删除腾额度',
            detail:
              '免费额度用完时可以删掉不再需要的别名（已入池的号删掉别名不影响账号本身，只是收不到后续邮件）。',
            warning: '删除是不可逆的：删掉的别名不能再用来收信，也可能无法重新创建同名地址。',
          },
        ]}
      />

      <div className="flex flex-wrap items-center gap-2">
        <button className="btn-outline" onClick={reload} disabled={loading}>
          <RefreshCw size={15} className={loading ? 'animate-spin' : ''} />
          {loading ? '读取中…' : '刷新'}
        </button>
        {data && (
          <span className="text-xs text-slate-400">
            {data.account_email} · 已用 {data.total}/{data.free_limit} · 剩余额度{' '}
            <b className={cn(remaining <= 5 ? 'text-rose-500' : 'text-slate-600 dark:text-zinc-200')}>
              {remaining}
            </b>
          </span>
        )}
        <span className="ml-auto text-xs text-slate-400">
          已入池 {pooled.length} · 未使用 {unused.length}
        </span>
      </div>

      {notice && (
        <div className="flex items-center justify-between gap-3 rounded-xl border border-emerald-200 bg-emerald-50 px-4 py-3 text-sm text-emerald-700 dark:border-emerald-500/30 dark:bg-emerald-500/10 dark:text-emerald-300">
          <span className="flex items-center gap-2">
            <Check size={15} /> {notice}
          </span>
          <button className="shrink-0 text-xs hover:opacity-70" onClick={() => setNotice(null)}>
            忽略
          </button>
        </div>
      )}
      {error && (
        <div className="flex items-center justify-between gap-3 rounded-xl border border-rose-200 bg-rose-50 px-4 py-3 text-sm text-rose-600 dark:border-rose-500/30 dark:bg-rose-500/10 dark:text-rose-300">
          <span className="min-w-0 break-all">{error}</span>
          <button className="shrink-0 text-xs hover:opacity-70" onClick={() => setError(null)}>
            忽略
          </button>
        </div>
      )}

      {/* 未使用的马甲：优先清理这批 */}
      {unused.length > 0 && (
        <section>
          <h3 className="mb-2 text-sm font-medium text-slate-700 dark:text-zinc-200">
            未使用的马甲（{unused.length}）—— 删这些最安全，纯粹没用过
          </h3>
          <div className="grid gap-2 md:grid-cols-2">
            {unused.map((m) => (
              <div
                key={m.id}
                className="flex items-center gap-2 rounded-xl border border-slate-200 bg-white px-3 py-2 dark:border-zinc-800 dark:bg-zinc-900/60"
              >
                <Mail size={14} className="shrink-0 text-slate-400" />
                <span className="truncate text-sm text-slate-600 dark:text-zinc-300">{m.address}</span>
                {!m.enabled && <Badge tone="slate">已停用</Badge>}
                <button
                  className="ml-auto shrink-0 rounded-lg p-1.5 text-rose-500 hover:bg-rose-50 dark:hover:bg-rose-500/10"
                  title="删除这个马甲，释放额度"
                  onClick={() => setPendingDelete({ id: m.id, address: m.address, pooled: false })}
                >
                  <Trash2 size={14} />
                </button>
              </div>
            ))}
          </div>
        </section>
      )}

      {/* 已入池的马甲 */}
      {pooled.length > 0 && (
        <section>
          <h3 className="mb-2 text-sm font-medium text-slate-500 dark:text-zinc-400">
            已入池的马甲（{pooled.length}）—— 对应账号已在工具里，删除不影响凭证，但地址以后不能再收信
          </h3>
          <div className="grid gap-2 md:grid-cols-2">
            {pooled.map((m) => (
              <div
                key={m.id}
                className="flex items-center gap-2 rounded-xl border border-slate-200 bg-white px-3 py-2 dark:border-zinc-800 dark:bg-zinc-900/60"
              >
                <Mail size={14} className="shrink-0 text-emerald-500" />
                <span className="truncate text-sm text-slate-600 dark:text-zinc-300">{m.address}</span>
                <Badge tone="green" title="本地已有该账号">
                  已入池
                </Badge>
                <button
                  className="ml-auto shrink-0 rounded-lg p-1.5 text-rose-500 hover:bg-rose-50 dark:hover:bg-rose-500/10"
                  title="删除这个马甲（账号凭证保留在工具里）"
                  onClick={() => setPendingDelete({ id: m.id, address: m.address, pooled: true })}
                >
                  <Trash2 size={14} />
                </button>
              </div>
            ))}
          </div>
        </section>
      )}

      {data && data.masks.length === 0 && (
        <p className="py-6 text-center text-sm text-slate-400">Relay 里还没有马甲。</p>
      )}

      <Modal
        open={pendingDelete != null}
        onClose={() => setPendingDelete(null)}
        title="删除马甲"
        footer={
          <>
            <button className="btn-outline" onClick={() => setPendingDelete(null)} disabled={deleting}>
              取消
            </button>
            <button className="btn-primary" onClick={doDelete} disabled={deleting}>
              {deleting ? '删除中…' : '确认删除'}
            </button>
          </>
        }
      >
        {pendingDelete && (
          <div className="space-y-2 text-sm">
            <p>
              确认删除 <b className="font-mono">{pendingDelete.address}</b>？
            </p>
            {pendingDelete.pooled ? (
              <p className="flex items-start gap-1.5 text-amber-600 dark:text-amber-400">
                <CircleAlert size={14} className="mt-0.5 shrink-0" />
                <span>
                  这个马甲对应的账号已在工具里。删除只影响 Relay
                  侧（该地址立即失效、以后不能再收信或找回），本地凭证不受影响。
                </span>
              </p>
            ) : (
              <p className="text-slate-500 dark:text-zinc-400">
                这个马甲没有对应账号，删除是安全的，纯粹释放一个额度。
              </p>
            )}
          </div>
        )}
      </Modal>
    </div>
  );
}
