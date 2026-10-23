/**
 * ZCode 页 · 号池资源调度面板（2026-10-21 第四池接入配套）。
 *
 * 对齐 Buddy/Qoder「资源调度」页同构布局，按 ZCode 池实际能力裁剪：
 * - 资源开关 = zcodeEnabled 一个（z.ai 无区域概念、无思考代执行等 WB 同构特性）；
 * - 账号池选择：勾选白名单（清空 = 全部含 zai 凭证账号 fail-open 入池），
 *   uid = zcode accounts.id 字符串化（与网关 [ZCODE-SYNC] 同域）；无分组/标签概念；
 * - 调度参数：账号并发上限 + 池粘性 TTL（per-pool 专属，保存即热生效）；
 * - 模型目录：统一目录聚合过滤 zcode 源（GLM-5.3 / GLM-5.3-Flash，静态内置）；
 * - 环境配置：ZCode 号池专用 Key（bind_pool=zcode，启动自动预置）+ 网关基址，
 *   一键直写本机 ZCode 客户端 provider_config.json（写前自动备份）。
 * 网关级功能（服务启停 / API Keys / 用量统计）在全局 API 管理弹窗，页内不重复。
 */
import { useCallback, useEffect, useMemo, useState } from 'react';
import { Check, Copy, Layers, RefreshCw, Save, ToggleLeft } from 'lucide-react';
import { Badge, Spinner, StatCard } from '../../components/ui';
import { PoolHealthBadges, PoolPlanBadges } from '../../components/api/PoolHealthBadges';
import { api } from '../../lib/tauri';
import { useAppStore } from '../../store';
import { withMinDelay } from '../../lib/delay';
import { accountsApi } from './api/accounts';
import type { Account } from './types';
import type {
  ApiKeyEntry,
  ApiPoolFile,
  ApiServiceStatus,
  PoolStatus,
  UnifiedModel,
} from '../../types';

const ZCODE_POOL_KEY_ID = 'key-zcode-pool';

function CopyButton({ text, label }: { text: string; label: string }) {
  const [copied, setCopied] = useState(false);
  return (
    <button
      className="btn-outline !px-2 !py-1 text-xs"
      title={`复制${label}`}
      onClick={() => {
        void navigator.clipboard.writeText(text).then(() => {
          setCopied(true);
          setTimeout(() => setCopied(false), 1500);
        });
      }}
    >
      {copied ? <Check size={12} className="text-emerald-500" /> : <Copy size={12} />}
      {copied ? '已复制' : label}
    </button>
  );
}

export default function ZCodePoolPanel() {
  const pushToast = useAppStore((s) => s.pushToast);
  const [status, setStatus] = useState<ApiServiceStatus | null>(null);
  const [pool, setPool] = useState<ApiPoolFile | null>(null);
  const [zcodeEnabled, setZcodeEnabled] = useState(true);
  const [zcodeAccountConcurrencyLimit, setZcodeAccountConcurrencyLimit] = useState(1);
  const [zcodePoolStickyTtlSecs, setZcodePoolStickyTtlSecs] = useState(300);
  // 账号池白名单：null = 未自定义（fail-open 全量，显示为全选）；值域 = String(account.id)
  const [zcodeUids, setZcodeUids] = useState<string[] | null>(null);
  const [accounts, setAccounts] = useState<Account[]>([]);
  const [accountsError, setAccountsError] = useState(false);
  const [models, setModels] = useState<UnifiedModel[]>([]);
  const [poolStatus, setPoolStatus] = useState<PoolStatus[]>([]);
  const [key, setKey] = useState<ApiKeyEntry | null>(null);
  const [saving, setSaving] = useState(false);
  const [refreshing, setRefreshing] = useState(false);
  const [writingProvider, setWritingProvider] = useState(false);

  const refresh = useCallback(async () => {
    setRefreshing(true);
    try {
      const [st, pf, accs, cat, keys] = await Promise.all([
        api.apiServer.status().catch(() => null),
        api.apiServer.poolList().catch(() => null),
        accountsApi.list().catch(() => null as Account[] | null),
        api.apiServer
          .unifiedModels()
          .then((list) => list.filter((m) => m.sources.some((s) => s.pool === 'zcode')))
          .catch(() => [] as UnifiedModel[]),
        api.apiServer.keysList().catch(() => null),
      ]);
      setStatus(st);
      setPool(pf);
      if (accs === null) {
        // 失败哨兵（对齐 Qoder 页 F1）：失败置错误态，不把失败当空集误报「暂无账号」
        setAccountsError(true);
        pushToast('error', '账号列表读取失败：请点击刷新重试');
      } else {
        setAccountsError(false);
        setAccounts(accs);
      }
      setModels(cat);
      setKey(keys?.keys.find((k) => k.id === ZCODE_POOL_KEY_ID) ?? null);
      if (pf) {
        setZcodeEnabled(pf.zcode_enabled ?? true);
        setZcodeAccountConcurrencyLimit(pf.zcode_account_concurrency_limit ?? 1);
        setZcodePoolStickyTtlSecs(pf.zcode_pool_sticky_ttl_secs ?? 300);
        // 空数组 = fail-open（全部入池）→ 视为未自定义，显示为全选
        setZcodeUids(pf.zcode_enabled_uids?.length ? pf.zcode_enabled_uids : null);
      }
    } catch (err) {
      pushToast('error', `读取 ZCode 池状态失败：${String(err)}`);
    } finally {
      setRefreshing(false);
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  // 服务运行中轮询池实时状态（冷却/在途徽标；3s 同其他池页惯例）
  const refreshPoolStatus = useCallback(async () => {
    if (!status?.running) {
      setPoolStatus([]);
      return;
    }
    try {
      setPoolStatus(await api.apiServer.zcodePoolStatus());
    } catch {
      /* ignore */
    }
  }, [status?.running]);

  useEffect(() => {
    void refreshPoolStatus();
    if (!status?.running) return;
    const id = setInterval(() => void refreshPoolStatus(), 3000);
    return () => clearInterval(id);
  }, [refreshPoolStatus]);

  // 保存：uids/strategy/groupIds 原样回传（本页不改 Trae/Buddy/Qoder 池配置），
  // zcode 字段随本次保存提交（服务运行中热重载即时生效）
  const savePool = async () => {
    if (!pool) {
      pushToast('error', '池配置未加载，无法保存（请先刷新重试）');
      return;
    }
    setSaving(true);
    try {
      await withMinDelay(
        api.apiServer.poolSet(pool.enabled_uids ?? [], pool.strategy, pool.group_ids, {
          zcodeEnabled,
          zcodeAccountConcurrencyLimit,
          zcodePoolStickyTtlSecs,
          // null = 未自定义（后端保留原值保持 fail-open）；数组 = 白名单覆盖（[] = 清空恢复全量）
          ...(zcodeUids !== null ? { zcodeUids } : {}),
        }),
        600,
      );
      pushToast('success', 'ZCode 上游配置已保存（服务运行中即时生效）');
    } catch (err) {
      pushToast('error', `保存失败：${String(err)}`);
    } finally {
      setSaving(false);
    }
  };

  // 一键直写本机 ZCode 客户端 provider_config.json（网关命令侧备份 + 幂等更新）
  const writeProvider = async () => {
    if (!key) return;
    setWritingProvider(true);
    try {
      const r = await api.apiServer.zcodeProviderWrite(key.id);
      pushToast('success', `已写入 ZCode 客户端供应商：${r.provider_id}（${r.models} 个模型），重启 ZCode 生效`);
    } catch (err) {
      pushToast('error', `写入失败：${String(err).slice(0, 140)}`);
    } finally {
      setWritingProvider(false);
    }
  };

  const credAccounts = useMemo(
    () => accounts.filter((a) => !!a.zai_access_token),
    [accounts],
  );
  // 生效白名单：未自定义（null）= 全量含凭证账号（与后端 fail-open 默认一致）
  const selected = zcodeUids ?? credAccounts.map((a) => String(a.id));
  const toggleUid = (id: string) => {
    const base = zcodeUids ?? credAccounts.map((a) => String(a.id));
    setZcodeUids(base.includes(id) ? base.filter((u) => u !== id) : [...base, id]);
  };

  const running = status?.running ?? false;
  const baseUrl = `http://127.0.0.1:${status?.port ?? 7864}/v1`;
  // 池指标：可调度 = 未被禁用/冷却，且**有套餐权益**的账号数。
  // 2026-10-23 套餐池改造后，画像已同步但模型集为空的账号会被 selectable 直接挡掉
  // （上游明确说这号没权益），若仍按旧口径算「就绪」，界面会与实际取号结果不符。
  const schedulableCount = poolStatus.filter(
    (p) =>
      !p.disabled &&
      !p.cooling &&
      !p.hard_credit &&
      !(p.plan_kind != null && (p.plan_models?.length ?? 0) === 0),
  ).length;

  return (
    <div className="animate-fade-in">
      {/* 资源池说明 */}
      <div className="rounded-xl border border-amber-300/70 bg-amber-50/80 px-3.5 py-2.5 dark:border-amber-700/40 dark:bg-amber-900/10">
        <div className="flex flex-wrap items-center gap-2">
          <span className="text-xs font-semibold text-amber-800 dark:text-amber-200">资源池说明</span>
          <span className="rounded bg-amber-200 px-1.5 py-0.5 text-[10px] font-medium text-amber-700 dark:bg-amber-700/50 dark:text-amber-100">
            本服务消耗 z.ai 账号资源包
          </span>
        </div>
        <div className="mt-1 text-[11px] leading-4 text-slate-500 dark:text-zinc-400">
          ZCode 目录模型（GLM-5.3 系）经 z.ai anthropic 端点由 ZCode 账号池服务；账号 429-1113
          「无资源包」按账号冷却轮换，领取资源包/充值后自动恢复入池。池间调度序
          Buddy → Trae → Qoder → ZCode（尾部接管），ZCode 不参与裸名自动分流优先级竞争。
        </div>
      </div>

      {/* 池指标行 */}
      <div className="mt-4 grid grid-cols-3 gap-3">
        <StatCard
          label="含凭证账号"
          value={credAccounts.length}
          tone="green"
          hint="持有 zai_access_token，可参与池装配"
        />
        <StatCard
          label="可调度"
          value={running ? schedulableCount : '—'}
          tone="blue"
          hint="服务运行中未被禁用/冷却、且有套餐权益的池内账号（3s 轮询）"
        />
        <StatCard
          label="池内账号"
          value={selected.length}
          tone="violet"
          hint="勾选参与调度的账号数（清空勾选 = 全部含凭证账号自动入池）"
        />
      </div>

      <div className="mt-4 grid grid-cols-12 items-start gap-4">
        {/* 左列：账号池选择 + 资源开关与调度参数（合并面板，共用右上角「保存」） */}
        <div className="col-span-6">
          <div className="card p-4">
            <div className="mb-3 flex items-center justify-between gap-2">
              <div className="flex items-center gap-2">
                <Layers size={16} className="text-brand-500" />
                <span className="text-sm font-medium">账号池选择</span>
                <span className="text-xs text-slate-400">
                  {credAccounts.length > 0 && `已选 ${selected.length}/${credAccounts.length}`}
                </span>
              </div>
              <div className="flex items-center gap-1">
                {credAccounts.length > 0 && (
                  <>
                    <button
                      className="btn-ghost px-2 py-0.5 text-xs"
                      onClick={() => setZcodeUids(credAccounts.map((a) => String(a.id)))}
                    >
                      全选
                    </button>
                    <button className="btn-ghost px-2 py-0.5 text-xs" onClick={() => setZcodeUids([])}>
                      清空
                    </button>
                  </>
                )}
                <button className="btn-outline" onClick={() => void savePool()} disabled={saving || !pool}>
                  {saving ? <Spinner /> : <Save size={15} />} 保存
                </button>
                <button className="btn-outline" onClick={() => void refresh()} disabled={refreshing}>
                  <RefreshCw size={15} className={refreshing ? 'animate-spin' : ''} /> 刷新
                </button>
              </div>
            </div>
            <p className="mb-2 text-xs text-slate-400">
              {credAccounts.length === 0
                ? '暂无含凭证账号'
                : '勾选账号参与 ZCode 上游调度（清空 = 全部含凭证账号自动入池）；保存后即时生效'}
            </p>

            {accountsError ? (
              <p className="py-4 text-center text-xs text-rose-500 dark:text-rose-400">
                账号列表读取失败：请点击「刷新」重试。
              </p>
            ) : credAccounts.length === 0 ? (
              <p className="py-4 text-center text-xs text-slate-400">
                暂无含凭证账号：请先在「ZCode 账号」视图 OAuth 登录或导入本机客户端账号。
              </p>
            ) : (
              <div className="max-h-72 space-y-1 overflow-y-auto">
                {credAccounts.map((a) => {
                  const uid = String(a.id);
                  const p = poolStatus.find((x) => x.uid === uid);
                  return (
                    <div
                      key={a.id}
                      className="flex items-center gap-3 rounded-lg border border-slate-100 px-3 py-2 text-sm dark:border-zinc-800"
                    >
                      <input
                        type="checkbox"
                        checked={selected.includes(uid)}
                        onChange={() => toggleUid(uid)}
                      />
                      <div className="min-w-0 flex-1 break-all font-medium">{a.label || uid}</div>
                      {a.invite_role === 'inviter' && <Badge tone="violet">主号</Badge>}
                      {a.invite_role === 'invited' && <Badge tone="slate">小号</Badge>}
                      {a.is_current && <Badge tone="brand">客户端当前</Badge>}
                      {/* 套餐画像徽标：形态/可用模型/夜间档/剩余额度（仅 zcode 池） */}
                      <PoolPlanBadges s={p} />
                      {/* 运行时健康徽标：禁用/冷却原因/在途·就绪（三池共用组件） */}
                      <PoolHealthBadges s={p} running={running} />
                    </div>
                  );
                })}
              </div>
            )}

            {/* 资源开关与调度参数 */}
            <div className="mt-4 flex items-center gap-2 border-t border-slate-100 pt-3 dark:border-zinc-800">
              <ToggleLeft size={16} className="text-brand-500" />
              <span className="text-sm font-medium">资源开关与调度参数</span>
            </div>
            <div className="mt-2 space-y-2 rounded-lg bg-slate-50 p-3 dark:bg-zinc-800/50">
              <label className="flex cursor-pointer items-start gap-2.5 rounded-md px-1.5 py-1.5 transition hover:bg-slate-100/60 dark:hover:bg-zinc-800/60">
                <input
                  type="checkbox"
                  className="mt-0.5 h-3.5 w-3.5 rounded border-slate-300 text-brand-600 focus:ring-brand-500"
                  checked={zcodeEnabled}
                  onChange={() => setZcodeEnabled((v) => !v)}
                />
                <span className="min-w-0">
                  <span className="block text-xs text-slate-700 dark:text-zinc-200">启用 ZCode 上游</span>
                  <span className="block text-[11px] leading-4 text-slate-400 dark:text-zinc-500">
                    ZCode 目录模型（GLM-5.3 系）路由到 ZCode 账号池；关闭后仅 ZCode 显式绑定的
                    请求报错，其他池不受影响
                  </span>
                </span>
              </label>
              <div className="flex items-center justify-between gap-3 px-1.5">
                <span className="min-w-0">
                  <span className="block text-xs text-slate-700 dark:text-zinc-200">账号并发上限</span>
                  <span className="block text-[11px] leading-4 text-slate-400 dark:text-zinc-500">
                    单账号在途请求数达到上限即让位其他账号；0 = 不限
                  </span>
                </span>
                <span className="flex shrink-0 items-center gap-1.5">
                  <input
                    type="number"
                    min={0}
                    max={32}
                    step={1}
                    value={zcodeAccountConcurrencyLimit}
                    onChange={(e) =>
                      setZcodeAccountConcurrencyLimit(
                        Math.max(0, Math.min(32, Number(e.target.value) || 0)),
                      )
                    }
                    className="w-24 rounded-lg border border-slate-200 bg-white px-2 py-1.5 text-right text-xs tabular-nums text-slate-700 focus:border-brand-400 focus:outline-none dark:border-zinc-700 dark:bg-zinc-900 dark:text-zinc-200"
                  />
                  <span className="text-[11px] text-slate-400">并发</span>
                </span>
              </div>
              <div className="flex items-center justify-between gap-3 px-1.5">
                <span className="min-w-0">
                  <span className="block text-xs text-slate-700 dark:text-zinc-200">池粘性 TTL</span>
                  <span className="block text-[11px] leading-4 text-slate-400 dark:text-zinc-500">
                    TTL 内同会话落同一账号（上游 KV cache 复用）；0 = 关闭
                  </span>
                </span>
                <span className="flex shrink-0 items-center gap-1.5">
                  <input
                    type="number"
                    min={0}
                    max={3600}
                    step={30}
                    value={zcodePoolStickyTtlSecs}
                    onChange={(e) =>
                      setZcodePoolStickyTtlSecs(
                        Math.max(0, Math.min(3600, Number(e.target.value) || 0)),
                      )
                    }
                    className="w-24 rounded-lg border border-slate-200 bg-white px-2 py-1.5 text-right text-xs tabular-nums text-slate-700 focus:border-brand-400 focus:outline-none dark:border-zinc-700 dark:bg-zinc-900 dark:text-zinc-200"
                  />
                  <span className="text-[11px] text-slate-400">秒</span>
                </span>
              </div>
            </div>
          </div>
        </div>

        {/* 右列：模型目录 + 环境配置 */}
        <div className="col-span-6 space-y-4">
          <div className="card p-4">
            <div className="mb-3 flex items-center gap-2">
              <Layers size={16} className="text-brand-500" />
              <span className="text-sm font-medium">模型目录（ZCode）</span>
              <span className="text-xs text-slate-400">{models.length} 个模型</span>
              <span className="ml-auto text-[11px] text-slate-400">静态内置 · 无定时同步</span>
            </div>
            {models.length === 0 ? (
              <p className="py-4 text-center text-xs text-slate-400">
                统一目录未返回 ZCode 源（网关服务未运行？）。
              </p>
            ) : (
              <div className="space-y-1.5">
                {models.map((m) => (
                  <div
                    key={m.id}
                    className="flex flex-wrap items-center gap-2 rounded-lg border border-slate-100 px-3 py-2 text-sm dark:border-zinc-800"
                  >
                    <span className="font-medium">{m.display || m.id}</span>
                    <Badge tone={m.sources.some((s) => s.pool === 'zcode' && s.enabled) ? 'green' : 'amber'}>
                      {m.sources.some((s) => s.pool === 'zcode' && s.enabled) ? '可用' : '池关闭'}
                    </Badge>
                    {m.vendor && <Badge tone="slate">{m.vendor}</Badge>}
                    <span className="ml-auto text-xs tabular-nums text-slate-400">
                      {m.context_length ? `${(m.context_length / 1000).toFixed(0)}K 上下文` : ''}
                      {m.max_tokens ? ` / ${(m.max_tokens / 1000).toFixed(0)}K 输出` : ''}
                    </span>
                    {m.efforts.length > 0 && (
                      <span className="text-[11px] text-slate-400">思考档位 {m.efforts.join('·')}</span>
                    )}
                  </div>
                ))}
              </div>
            )}
            <p className="mt-2 text-[11px] text-slate-400 dark:text-zinc-500">
              客户端调用模型名带池前缀 <code className="rounded bg-slate-100 px-1 dark:bg-zinc-800">zcode/GLM-5.3</code>
              　锁定 ZCode 池；裸名按池间调度序竞争。
            </p>
          </div>

          {/* 环境配置：号池 Key + ZCode 客户端直写 */}
          <div className="card p-4">
            <div className="mb-3 flex items-center gap-2">
              <ToggleLeft size={16} className="text-brand-500" />
              <span className="text-sm font-medium">环境配置（接入 ZCode 客户端）</span>
            </div>
            {key ? (
              <>
                <div className="mb-2 flex items-center gap-2">
                  <span className="shrink-0 text-xs text-slate-500 dark:text-zinc-400">号池 Key</span>
                  <code className="flex-1 truncate rounded bg-slate-100 px-2 py-1 font-mono text-xs dark:bg-zinc-800">
                    {key.key}
                  </code>
                  <CopyButton text={key.key} label="Key" />
                  {!key.enabled && <Badge tone="amber">已停用</Badge>}
                </div>
                <div className="mb-2 flex items-center gap-2">
                  <span className="shrink-0 text-xs text-slate-500 dark:text-zinc-400">网关基址</span>
                  <code className="flex-1 truncate rounded bg-slate-100 px-2 py-1 font-mono text-xs dark:bg-zinc-800">
                    {baseUrl}
                  </code>
                  <CopyButton text={baseUrl} label="地址" />
                </div>
                <div className="flex items-center gap-2">
                  <button
                    className="btn-outline"
                    onClick={() => void writeProvider()}
                    disabled={writingProvider || !key.enabled}
                  >
                    {writingProvider ? <Spinner /> : <Save size={15} />} 写入 ZCode 供应商配置
                  </button>
                  <span className="text-[11px] text-slate-400">
                    幂等更新 provider_config.json（写前自动备份），重启 ZCode 客户端生效
                  </span>
                </div>
              </>
            ) : (
              <p className="text-xs text-slate-400">
                号池专用 Key（<code className="rounded bg-slate-100 px-1 dark:bg-zinc-800">{ZCODE_POOL_KEY_ID}</code>
                ，bind_pool=zcode）在网关服务启动时自动预置；当前未找到——请先启动 API 服务并刷新本页。
              </p>
            )}
          </div>
        </div>
      </div>
    </div>
  );
}
