/**
 * Agent 接入中心（2026-10-23）：检测本机 AI agent，把网关 API Key 的生效模型
 * 一键写入各 agent 的配置文件（复用 zcode_provider_write 的备份/幂等 upsert 范式）。
 *
 * 支持写入（有文件级自定义模型入口）：
 * - WorkBuddy   ~/.workbuddy/models.json      每模型一条，含上下文上限 + 思考深度
 * - ZCode CLI   ~/.zcode/v2/provider_config.json  每 Key 一个 provider
 * - Claude Code ~/.claude/settings.json       env 块（单端点）
 * - Codex CLI   ~/.codex/config.toml          [model_providers.aiwork] + [profiles.aiwork]
 * - opencode    ~/.config/opencode/opencode.jsonc  provider 段（单端点）
 *
 * 仅检测（无自定义渠道入口）：Gemini CLI / Cursor / Trae / Qoder / CodeBuddy
 *
 * 「清除」按各 agent 的 AIWork 标记精确定位，不动用户自己的配置。
 */
import { useCallback, useEffect, useMemo, useState } from 'react';
import { CheckCircle2, Eraser, Loader2, Plug, RefreshCw, TriangleAlert, Layers } from 'lucide-react';
import PageHeader from '../components/PageHeader';
import { Badge, Modal } from '../components/ui';
import { api } from '../lib/tauri';
import { useAppStore } from '../store';
import type {
  AgentApplyResult,
  ApiKeyEntry,
  DetectedAgent,
  PoolAvailability,
  PoolModelOption,
} from '../types';

/** 单端点 agent：一个配置只认一把 Key（多 Key 无意义） */
const SINGLE_ENDPOINT = new Set<DetectedAgent['agent_id']>(['claude', 'codex', 'opencode']);

/** 池展示名与顺序（弹框分组标题用） */
const POOL_META: Record<string, { label: string; hint: string }> = {
  trae: { label: 'Trae', hint: '账号套餐体系' },
  buddy: { label: 'WorkBuddy', hint: '国内 / 海外双区' },
  qoder: { label: 'Qoder', hint: '国内 / 海外双区' },
  zcode: { label: 'ZCode', hint: 'z.ai 账号（无区域）' },
};

/**
 * 「导入模型」弹框：按号池分组，逐池挑要写进 WorkBuddy 的模型。
 *
 * 为什么要有它（2026-10-23 用户诉求）：
 * 1. 原来只能「按 Key 勾选」，模型由 Key 的池归属全量展开——想少要几个只能写进去再删；
 * 2. 更糟的是**没有任何账号校验**：网关首启会自动预置两把 region=global 的海外
 *    Key，前端又默认全选，于是「一个海外号都没有」的用户照样被写进 @global 模型，
 *    请求打出去必然 no_healthy。
 *
 * 本弹框把两件事一起解决：
 * - 每池模型多选（只写勾选的）
 * - 无账号的池/区**置灰 + 不可勾 + 显式标「未配置账号池」**（默认就不勾）
 */
function ImportModelsModal({
  open,
  agentId,
  agentLabel,
  keys,
  onClose,
  onDone,
}: {
  open: boolean;
  agentId: string;
  agentLabel: string;
  keys: ApiKeyEntry[];
  onClose: () => void;
  onDone: (r: AgentApplyResult) => void;
}) {
  const [loading, setLoading] = useState(false);
  const [pools, setPools] = useState<PoolAvailability[]>([]);
  const [options, setOptions] = useState<PoolModelOption[]>([]);
  /** 已勾选的模型 id */
  const [picked, setPicked] = useState<Set<string>>(new Set());
  /** 每池选用的 Key（空 = 后端按池绑定自动挑） */
  const [keyOfPool, setKeyOfPool] = useState<Record<string, string>>({});
  const [saving, setSaving] = useState(false);
  const [err, setErr] = useState<string | null>(null);

  const load = useCallback(async () => {
    setLoading(true);
    setErr(null);
    try {
      const r = await api.apiServer.agentPoolCatalog();
      setPools(r.pools);
      setOptions(r.models);
      // 默认只勾「有账号」的池的模型——这正是「自动排除海外」的默认态
      setPicked(new Set(r.models.filter((m) => m.enabled).map((m) => m.id)));
      // 每池默认选绑定该池的启用 Key
      const nextKey: Record<string, string> = {};
      for (const p of ['trae', 'buddy', 'qoder', 'zcode']) {
        const bound = keys.find((k) => k.enabled && k.bind_pool === p);
        if (bound) nextKey[p] = bound.id;
      }
      setKeyOfPool(nextKey);
    } catch (e) {
      setErr(String(e));
    } finally {
      setLoading(false);
    }
  }, [keys]);

  useEffect(() => {
    if (open) void load();
  }, [open, load]);

  // 按池分组（顺序固定，空池也显示——用户需要看到「这个池没账号」而不是它消失）
  const grouped = useMemo(() => {
    const order = ['trae', 'buddy', 'qoder', 'zcode'];
    return order.map((pool) => ({
      pool,
      meta: POOL_META[pool],
      avail: pools.find((p) => p.pool === pool),
      items: options.filter((m) => m.pool === pool),
    }));
  }, [pools, options]);

  // 有账号的池数（不是账号总数——文案说的是「几个池有账号」）
  const poolsWithAccounts = useMemo(
    () => grouped.filter((g) => (g.avail?.cn ?? 0) + (g.avail?.global ?? 0) > 0).length,
    [grouped],
  );

  const toggle = (id: string) => {
    setPicked((prev) => {
      const next = new Set(prev);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      return next;
    });
  };

  /** 整池全选/全不选（只对可勾的模型生效） */
  const togglePool = (pool: string) => {
    const usable = options.filter((m) => m.pool === pool && m.enabled).map((m) => m.id);
    const allPicked = usable.length > 0 && usable.every((id) => picked.has(id));
    setPicked((prev) => {
      const next = new Set(prev);
      for (const id of usable) {
        if (allPicked) next.delete(id);
        else next.add(id);
      }
      return next;
    });
  };

  const save = async () => {
    const list = [...picked];
    if (list.length === 0) {
      setErr('至少勾选一个模型');
      return;
    }
    setSaving(true);
    setErr(null);
    try {
      // 每池的 Key 逐池传：多池混选时各池凭证可能不同（单个 keyId 表达不了），
      // 空值不传 → 后端按池绑定的启用 Key 自动挑。
      const distinctPools = [...new Set(list.map((id) => id.split('/')[0]))];
      const poolKeys: Record<string, string> = {};
      for (const p of distinctPools) {
        const id = keyOfPool[p];
        if (id) poolKeys[p] = id;
      }
      const r = await api.apiServer.agentsApplyModels(
        agentId,
        list,
        undefined,
        Object.keys(poolKeys).length > 0 ? poolKeys : undefined,
      );
      onDone(r);
      onClose();
    } catch (e) {
      setErr(String(e));
    } finally {
      setSaving(false);
    }
  };

  return (
    <Modal
      open={open}
      onClose={onClose}
      title="导入模型"
      size="lg"
      bodyClass="max-h-[calc(100vh-560px)] min-h-[320px] overflow-y-auto"
      footer={
        <>
          <span className="mr-auto text-xs text-slate-400">
            {loading
              ? '正在读取各池账号与模型…'
              : `已选 ${picked.size} 个模型 · ${poolsWithAccounts}/${grouped.length} 个池有账号`}
          </span>
          <button className="btn-outline" onClick={onClose} disabled={saving}>
            取消
          </button>
          <button className="btn-primary" onClick={() => void save()} disabled={saving || loading}>
            {saving ? '写入中…' : `写入所选 ${picked.size} 个模型`}
          </button>
        </>
      }
    >
      <div className="space-y-3">
        <p className="flex items-start gap-2 rounded-xl bg-sky-50 px-3 py-2 text-xs leading-relaxed text-sky-700 dark:bg-sky-500/10 dark:text-sky-300">
          <Layers size={14} className="mt-0.5 shrink-0" />
          <span>
            按号池映射：只写你勾选的模型；<b>没有账号的池/区自动置灰且不可勾</b>
            ——写进去的请求会因无可用账号而直接失败。
          </span>
        </p>

        {err && (
          <p className="flex items-start gap-1.5 text-xs text-rose-500">
            <TriangleAlert size={13} className="mt-0.5 shrink-0" />
            <span className="min-w-0 break-all">{err}</span>
          </p>
        )}

        {grouped.map(({ pool, meta, avail, items }) => {
          const cn = avail?.cn ?? 0;
          const gl = avail?.global ?? 0;
          const hasAny = cn + gl > 0;
          const usable = items.filter((m) => m.enabled);
          const allPicked = usable.length > 0 && usable.every((m) => picked.has(m.id));
          return (
            <div
              key={pool}
              className={`rounded-xl border p-3 ${
                hasAny
                  ? 'border-slate-200 dark:border-zinc-700'
                  : 'border-slate-200/60 bg-slate-50/60 dark:border-zinc-800 dark:bg-zinc-900/40'
              }`}
            >
              <div className="flex flex-wrap items-center gap-2">
                <span className="text-sm font-semibold text-slate-700 dark:text-zinc-200">
                  {meta?.label ?? pool}
                </span>
                <span className="text-xs text-slate-400">{meta?.hint}</span>
                {hasAny ? (
                  <>
                    <Badge tone="green">
                      有账号 {cn + gl}
                      {gl > 0 ? `（海外 ${gl}）` : ''}
                    </Badge>
                    <button
                      className="ml-auto chip bg-slate-100 text-slate-500 hover:bg-slate-200 dark:bg-zinc-800 dark:text-zinc-400"
                      onClick={() => togglePool(pool)}
                      disabled={usable.length === 0}
                    >
                      {allPicked ? '全不选' : '全选'}
                    </button>
                  </>
                ) : (
                  <Badge tone="slate">未配置账号池 · 模型将跳过</Badge>
                )}
              </div>

              {/* 该池用哪把 Key（多池混选时逐池指定；单池也可覆盖） */}
              {hasAny && usable.length > 0 && (
                <label className="mt-2 flex items-center gap-2 text-xs text-slate-500 dark:text-zinc-400">
                  对应 Key
                  <select
                    className="min-w-0 flex-1 rounded border border-slate-200 bg-white px-2 py-1 text-xs dark:border-zinc-700 dark:bg-zinc-800"
                    value={keyOfPool[pool] ?? ''}
                    onChange={(e) =>
                      setKeyOfPool((prev) => ({ ...prev, [pool]: e.target.value }))
                    }
                  >
                    <option value="">自动（按池绑定挑）</option>
                    {keys
                      .filter((k) => k.enabled)
                      .map((k) => (
                        <option key={k.id} value={k.id}>
                          {k.name}
                          {k.bind_pool ? `（${k.bind_pool}）` : '（全局）'}
                        </option>
                      ))}
                  </select>
                </label>
              )}

              {items.length === 0 ? (
                <p className="mt-2 text-xs text-slate-400">该池目录为空（可在对应资源调度页同步模型）</p>
              ) : (
                <div className="mt-2 grid max-h-40 grid-cols-1 gap-1 overflow-y-auto md:grid-cols-2">
                  {items.map((m) => {
                    const checked = picked.has(m.id);
                    return (
                      <label
                        key={m.id}
                        className={`flex items-center gap-2 rounded px-1.5 py-1 text-xs ${
                          m.enabled
                            ? 'cursor-pointer hover:bg-slate-50 dark:hover:bg-zinc-800/60'
                            : 'cursor-not-allowed opacity-45'
                        }`}
                        title={
                          m.enabled
                            ? m.id
                            : `该池该区没有账号（${m.region === 'global' ? '海外' : m.region === 'cn' ? '国内' : '无区域'}），写入也会 no_healthy`
                        }
                      >
                        <input
                          type="checkbox"
                          className="h-3.5 w-3.5 accent-indigo-500"
                          checked={checked}
                          disabled={!m.enabled}
                          onChange={() => toggle(m.id)}
                        />
                        <span className="min-w-0 flex-1 truncate font-mono">
                          {m.id.split('/').slice(1).join('/')}
                        </span>
                        {m.region === 'global' && <Badge tone="violet">海外</Badge>}
                        {m.region === 'cn' && <Badge tone="blue">国内</Badge>}
                      </label>
                    );
                  })}
                </div>
              )}
            </div>
          );
        })}

        <p className="text-[11px] leading-relaxed text-slate-400 dark:text-slate-500">
          写入目标：{agentLabel}。
          {agentId === 'zcode'
            ? '按 Key 切成 provider（每 Key 一个，清单 = 该 Key 下勾选的模型），重复写入同一 Key 只替换该 provider 的模型清单。'
            : '每模型一条配置，已按目录写入最大上下文与思考档位。'}
          同名模型带 @cn / @global 后缀区分号池。
        </p>
      </div>
    </Modal>
  );
}

export default function AgentIntegration() {
  const pushToast = useAppStore((s) => s.pushToast);
  const [agents, setAgents] = useState<DetectedAgent[]>([]);
  const [keys, setKeys] = useState<ApiKeyEntry[]>([]);
  const [scanning, setScanning] = useState(true);
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [results, setResults] = useState<Partial<Record<string, AgentApplyResult>>>({});
  // 每 agent 勾选的 Key 集合；默认全选启用 Key
  const [selected, setSelected] = useState<Record<string, Set<string>>>({});
  // 单端点 agent 的接入 Key
  const [primaryKey, setPrimaryKey] = useState<Record<string, string>>({});
  // 「导入模型」弹框（按池选模型；仅 WorkBuddy 走这个入口）
  const [importFor, setImportFor] = useState<DetectedAgent | null>(null);

  const enabledKeys = useMemo(() => keys.filter((k) => k.enabled), [keys]);

  const load = useCallback(async () => {
    setScanning(true);
    setError(null);
    try {
      const [scan, file] = await Promise.all([api.apiServer.agentsScan(), api.apiServer.keysList()]);
      setAgents(scan.agents);
      setKeys(file.keys);
      const enabled = file.keys.filter((k) => k.enabled);
      setSelected((prev) => {
        const next = { ...prev };
        for (const a of scan.agents) {
          if (!next[a.agent_id]) next[a.agent_id] = new Set(enabled.map((k) => k.id));
        }
        return next;
      });
      setPrimaryKey((prev) => {
        const next = { ...prev };
        for (const a of scan.agents) {
          if (!next[a.agent_id] && enabled.length) next[a.agent_id] = enabled[0].id;
        }
        return next;
      });
    } catch (e) {
      setError(String(e));
    } finally {
      setScanning(false);
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  const toggleKey = (agentId: string, keyId: string) => {
    setSelected((prev) => {
      const set = new Set(prev[agentId]);
      if (set.has(keyId)) set.delete(keyId);
      else set.add(keyId);
      return { ...prev, [agentId]: set };
    });
  };

  const apply = async (agent: DetectedAgent) => {
    const single = SINGLE_ENDPOINT.has(agent.agent_id);
    const keyIds = single
      ? primaryKey[agent.agent_id]
        ? [primaryKey[agent.agent_id]]
        : []
      : [...(selected[agent.agent_id] ?? [])];
    if (keyIds.length === 0) {
      pushToast('warn', '请先选择 Key');
      return;
    }
    setBusy(agent.agent_id);
    try {
      const r = await api.apiServer.agentsApply(agent.agent_id, keyIds, keyIds[0]);
      setResults((prev) => ({ ...prev, [agent.agent_id]: r }));
      const detail =
        r.added != null
          ? `写入 ${r.added} 个模型（上下文+思考深度已带上；保留自定义 ${r.kept_foreign ?? 0} 条）`
          : r.applied != null
            ? `写入 ${r.applied}/${r.total} 个供应商`
            : '已写入';
      pushToast('success', `${agent.label}：${detail}`);
    } catch (e) {
      setResults((prev) => ({ ...prev, [agent.agent_id]: { ok: false, error: String(e) } }));
      pushToast('error', `写入失败：${String(e)}`);
    } finally {
      setBusy(null);
    }
  };

  const clear = async (agent: DetectedAgent) => {
    setBusy(agent.agent_id);
    try {
      const r = await api.apiServer.agentsClear(agent.agent_id);
      setResults((prev) => ({ ...prev, [agent.agent_id]: r }));
      pushToast('success', `${agent.label}：已清除 ${r.removed ?? 0} 项 AIWork 配置`);
    } catch (e) {
      pushToast('error', `清除失败：${String(e)}`);
    } finally {
      setBusy(null);
    }
  };

  const detected = agents.filter((a) => a.detected);
  const missing = agents.filter((a) => !a.detected);

  return (
    <div className="animate-fade-in space-y-4">
      <PageHeader
        title="Agent 接入中心"
        desc="把网关 API Key 与生效模型一键写入本机 AI agent（自动识别已安装的客户端）"
        actions={
          <button
            onClick={() => void load()}
            disabled={scanning}
            className="flex items-center gap-1.5 rounded-lg border border-slate-200 bg-white px-3 py-1.5 text-sm font-medium text-slate-600 transition hover:bg-slate-50 active:scale-95 disabled:opacity-50 dark:border-zinc-700 dark:bg-zinc-900 dark:text-zinc-300 dark:hover:bg-zinc-800"
          >
            {scanning ? <Loader2 size={14} className="animate-spin" /> : <RefreshCw size={14} />}
            重新检测
          </button>
        }
      />

      {/* 区域区分说明（2026-10-23）：Qoder/Buddy 两池同 id 跨区重名，靠 @cn/@global 后缀区分 */}
      <div className="rounded-xl border border-sky-200 bg-sky-50 p-3 text-xs leading-relaxed text-sky-800 dark:border-sky-900 dark:bg-sky-950 dark:text-sky-300">
        <div className="font-medium">国内 / 海外号池如何区分</div>
        <div className="mt-1 space-y-1 text-sky-700 dark:text-sky-400">
          <p>
            Qoder 与 Buddy 都有国内、海外两个号池，其中 <b>Qoder 13 个模型里有 11 个两区同名</b>
            （如 GLM-5.3、Qwen3.8-Flash）。所以写入时同名模型会各出一条并带区域后缀：
          </p>
          <p className="font-mono text-[11px]">
            qoder/GLM-5.3<b className="text-emerald-600 dark:text-emerald-400">@cn</b>（国内号池） · qoder/GLM-5.3
            <b className="text-violet-600 dark:text-violet-400">@global</b>（海外号池）
          </p>
          <p>
            只有一边有的模型直接出裸名（无歧义）。若你勾选的 Key 已限定区域，则只出该区、不加后缀，
            避免重复。
          </p>
        </div>
      </div>

      {error && (
        <div className="flex items-center gap-2 rounded-lg border border-red-200 bg-red-50 p-3 text-sm text-red-700 dark:border-red-900 dark:bg-red-950 dark:text-red-300">
          <TriangleAlert size={15} className="shrink-0" />
          {error}
        </div>
      )}

      {scanning && agents.length === 0 && (
        <div className="flex items-center justify-center gap-2 rounded-xl border border-dashed border-slate-200 p-10 text-sm text-slate-400 dark:border-zinc-800">
          <Loader2 size={15} className="animate-spin" />
          正在检测本机 agent…
        </div>
      )}

      {detected.length > 0 && (
        <div className="grid grid-cols-1 gap-4 xl:grid-cols-3">
          {detected.map((agent) => {
            const single = SINGLE_ENDPOINT.has(agent.agent_id);
            const chosen = selected[agent.agent_id] ?? new Set<string>();
            const result = results[agent.agent_id];
            const isBusy = busy === agent.agent_id;
            const canWrite = agent.writable;
            return (
              <div
                key={agent.agent_id}
                className="flex flex-col gap-3 rounded-xl border border-slate-200 bg-white p-4 dark:border-zinc-700 dark:bg-zinc-900"
              >
                <div>
                  <div className="flex items-center gap-2">
                    <Plug size={16} className={canWrite ? 'text-indigo-500' : 'text-slate-400'} />
                    <span className="text-sm font-semibold">{agent.label}</span>
                    {canWrite ? <Badge tone="green">可写入</Badge> : <Badge tone="slate">仅检测</Badge>}
                  </div>
                  <p className="mt-1 text-xs leading-relaxed text-slate-500 dark:text-zinc-400">{agent.desc}</p>
                  <p className="mt-0.5 break-all font-mono text-[11px] text-slate-400 dark:text-zinc-500">
                    {agent.config_path}
                  </p>
                </div>

                {!canWrite ? (
                  <div className="rounded-lg bg-slate-50 p-2.5 text-xs leading-relaxed text-slate-400 dark:bg-zinc-800/60 dark:text-zinc-500">
                    该客户端没有文件级自定义模型入口，无法自动写入。可在客户端内手动添加 OpenAI 兼容端点：
                    <span className="ml-1 font-mono text-slate-500 dark:text-zinc-400">http://127.0.0.1:端口/v1</span>
                  </div>
                ) : (
                  <>
                    {single ? (
                      <label className="flex items-center gap-2 text-xs text-slate-500 dark:text-zinc-400">
                        接入 Key（单端点只认一把）
                        <select
                          value={primaryKey[agent.agent_id] ?? ''}
                          onChange={(e) =>
                            setPrimaryKey((prev) => ({ ...prev, [agent.agent_id]: e.target.value }))
                          }
                          className="flex-1 rounded border border-slate-200 bg-white px-2 py-1 text-xs dark:border-zinc-700 dark:bg-zinc-800"
                        >
                          {enabledKeys.map((k) => (
                            <option key={k.id} value={k.id}>
                              {k.name}
                              {k.bind_pool ? `（${k.bind_pool}）` : '（全局）'}
                            </option>
                          ))}
                        </select>
                      </label>
                    ) : (
                      <div className="space-y-1">
                        <div className="text-xs font-medium text-slate-500 dark:text-zinc-400">
                          选择 Key（{chosen.size}/{enabledKeys.length} 启用；模型按池归属自动分配到对应 Key）
                        </div>
                        <div className="max-h-44 space-y-1 overflow-auto rounded-lg bg-slate-50 p-2 dark:bg-zinc-800/60">
                          {keys.length === 0 && (
                            <span className="text-xs text-slate-400">尚无 API Key，请先在 API 管理创建</span>
                          )}
                          {keys.map((k) => (
                            <label
                              key={k.id}
                              className={`flex cursor-pointer items-center gap-2 rounded px-1.5 py-1 text-sm ${
                                k.enabled ? 'hover:bg-white dark:hover:bg-zinc-800' : 'cursor-not-allowed opacity-50'
                              }`}
                              title={k.enabled ? undefined : '该 Key 已停用，勾选无效'}
                            >
                              <input
                                type="checkbox"
                                disabled={!k.enabled}
                                checked={chosen.has(k.id)}
                                onChange={() => toggleKey(agent.agent_id, k.id)}
                                className="h-3.5 w-3.5 accent-indigo-500"
                              />
                              <span className="min-w-0 flex-1 truncate">{k.name}</span>
                              <Badge tone={k.bind_pool ? 'blue' : 'slate'}>
                                {k.bind_pool || '全局'}
                                {k.region && k.region !== 'cn' ? `·${k.region === 'global' ? '海外' : k.region}` : ''}
                              </Badge>
                            </label>
                          ))}
                        </div>
                      </div>
                    )}

                    <div className="flex items-center gap-2">
                      <button
                        onClick={() => void apply(agent)}
                        disabled={isBusy || (single ? !primaryKey[agent.agent_id] : chosen.size === 0)}
                        className="flex flex-1 items-center justify-center gap-1.5 rounded-lg bg-zinc-900 px-3 py-1.5 text-sm font-medium text-white transition hover:bg-zinc-700 active:scale-95 disabled:cursor-not-allowed disabled:opacity-40 dark:bg-zinc-100 dark:text-zinc-900 dark:hover:bg-white"
                      >
                        {isBusy ? <Loader2 size={14} className="animate-spin" /> : <CheckCircle2 size={14} />}
                        一键写入
                      </button>
                      {/* 支持逐模型选择的 agent：WorkBuddy（每模型一条）与
                          ZCode（每 Key 一个 provider，清单按勾选生成）。
                          其余是单端点（Claude/Codex/opencode），逐模型选择无意义。 */}
                      {(agent.agent_id === 'workbuddy' || agent.agent_id === 'zcode') && (
                        <button
                          onClick={() => setImportFor(agent)}
                          disabled={isBusy}
                          title="按号池挑要写入的模型；没有账号的池自动置灰（不会写进海外模型）"
                          className="flex items-center justify-center gap-1.5 rounded-lg border border-indigo-200 bg-indigo-50 px-3 py-1.5 text-sm font-medium text-indigo-600 transition hover:bg-indigo-100 active:scale-95 disabled:opacity-40 dark:border-indigo-500/30 dark:bg-indigo-500/10 dark:text-indigo-400"
                        >
                          <Layers size={14} />
                          导入模型
                        </button>
                      )}
                      <button
                        onClick={() => void clear(agent)}
                        disabled={isBusy}
                        title="仅清除本工具写入的配置（AIWork 标记），不影响你自己的配置"
                        className="flex items-center justify-center gap-1.5 rounded-lg border border-slate-200 px-3 py-1.5 text-sm text-slate-600 transition hover:bg-slate-50 active:scale-95 disabled:opacity-40 dark:border-zinc-700 dark:text-zinc-300 dark:hover:bg-zinc-800"
                      >
                        <Eraser size={14} />
                        清除
                      </button>
                    </div>

                    {agent.agent_id === 'workbuddy' && (
                      <p className="text-[11px] leading-relaxed text-slate-400 dark:text-zinc-500">
                        已按模型目录写入最大上下文与思考档位；同名模型带 @cn / @global 后缀区分号池；WorkBuddy
                        需重开对话生效
                      </p>
                    )}
                    {agent.agent_id === 'codex' && (
                      <p className="text-[11px] leading-relaxed text-slate-400 dark:text-zinc-500">
                        写入独立 provider 段，不改你的默认模型；用 <span className="font-mono">codex --profile aiwork</span> 使用
                      </p>
                    )}
                    {(agent.agent_id === 'claude' || agent.agent_id === 'opencode') && (
                      <p className="text-[11px] leading-relaxed text-slate-400 dark:text-zinc-500">
                        重启终端/客户端生效
                      </p>
                    )}

                    {result && (
                      <div className="rounded-lg bg-slate-50 p-2.5 text-xs leading-relaxed dark:bg-zinc-800/60">
                        {result.ok === false ? (
                          <span className="text-red-600 dark:text-red-400">{result.error ?? '失败'}</span>
                        ) : (
                          <div className="space-y-1">
                            {result.added != null && (
                              <div className="text-emerald-700 dark:text-emerald-400">
                                ✓ 写入 {result.added} 个模型；保留用户自定义 {result.kept_foreign ?? 0} 条
                                {result.removed_aiwork ? `；覆盖旧 AIWork 条目 ${result.removed_aiwork} 条` : ''}
                              </div>
                            )}
                            {result.added != null &&
                              ((result.cn_count ?? 0) > 0 || (result.global_count ?? 0) > 0) && (
                                <div className="text-slate-500 dark:text-zinc-400">
                                  其中 国内 {result.cn_count ?? 0} 个 · 海外 {result.global_count ?? 0} 个
                                </div>
                              )}
                            {/* 因无账号被跳过的池：必须明示——否则用户以为全写进去了 */}
                            {result.skipped_pools && result.skipped_pools.length > 0 && (
                              <div className="text-amber-600 dark:text-amber-400">
                                已跳过（无可用账号）：{result.skipped_pools.join('、')}
                              </div>
                            )}
                            {result.applied != null && (
                              <div className="text-emerald-700 dark:text-emerald-400">
                                ✓ 写入 {result.applied}/{result.total} 个供应商
                              </div>
                            )}
                            {result.base_url && (
                              <div className="text-emerald-700 dark:text-emerald-400">
                                ✓ 端点 {result.base_url}
                                {result.model ? `（模型 ${result.model}）` : ''}
                                {result.key_name ? `，Key「${result.key_name}」` : ''}
                              </div>
                            )}
                            {result.hint && (
                              <div className="text-slate-500 dark:text-zinc-400">{result.hint}</div>
                            )}
                            {result.models && result.models.length > 0 && (
                              <details>
                                <summary className="cursor-pointer text-slate-500 dark:text-zinc-400">
                                  模型归属明细（{result.models.length}）
                                </summary>
                                <ul className="mt-1 max-h-32 space-y-0.5 overflow-auto text-[11px] text-slate-400 dark:text-zinc-500">
                                  {result.models.map((m) => (
                                    <li key={m.id} className="flex items-center gap-1.5">
                                      <span className="font-mono">{m.id}</span>
                                      {m.region === 'cn' && <Badge tone="green">国内</Badge>}
                                      {m.region === 'global' && <Badge tone="violet">海外</Badge>}
                                      <span className="text-slate-400 dark:text-zinc-600">→ {m.key_name}</span>
                                    </li>
                                  ))}
                                </ul>
                              </details>
                            )}
                            {result.details && result.details.length > 0 && (
                              <details>
                                <summary className="cursor-pointer text-slate-500 dark:text-zinc-400">
                                  每 Key 结果（{result.details.length}）
                                </summary>
                                <ul className="mt-1 max-h-32 space-y-0.5 overflow-auto text-[11px]">
                                  {result.details.map((d) => {
                                    const ok = d.result?.ok === true;
                                    return (
                                      <li
                                        key={d.key_id}
                                        className={ok ? 'text-slate-400 dark:text-zinc-500' : 'text-red-500'}
                                      >
                                        {d.key_name}：
                                        {ok
                                          ? `${(d.result as { models?: number }).models ?? 0} 模型`
                                          : String((d.result as { error?: string }).error ?? '失败')}
                                      </li>
                                    );
                                  })}
                                </ul>
                              </details>
                            )}
                          </div>
                        )}
                      </div>
                    )}
                  </>
                )}
              </div>
            );
          })}
        </div>
      )}

      {missing.length > 0 && (
        <details className="rounded-xl border border-slate-200 bg-white p-3 dark:border-zinc-800 dark:bg-zinc-900">
          <summary className="cursor-pointer text-sm font-medium text-slate-500 dark:text-zinc-400">
            未检测到的 agent（{missing.length}）
          </summary>
          <ul className="mt-2 space-y-1 text-xs text-slate-400 dark:text-zinc-500">
            {missing.map((a) => (
              <li key={a.agent_id}>
                {a.label} — 未找到 <span className="font-mono">{a.config_path}</span>
              </li>
            ))}
          </ul>
        </details>
      )}

      {/* 「导入模型」弹框：按池选模型 + 无账号池自动置灰 */}
      {importFor && (
        <ImportModelsModal
          open={importFor != null}
          agentId={importFor.agent_id}
          agentLabel={importFor.label}
          keys={keys}
          onClose={() => setImportFor(null)}
          onDone={(r) => {
            setResults((prev) => ({ ...prev, [importFor.agent_id]: r }));
            // WorkBuddy 返回 added（模型条数）；ZCode 返回 applied/total（provider 数）
            // 与 added（模型总数）——两种口径都要能读
            const detail =
              r.applied != null
                ? `写入 ${r.applied}/${r.total} 个 provider（${r.added ?? 0} 个模型）`
                : r.added != null
                  ? `写入 ${r.added} 个模型（国内 ${r.cn_count ?? 0} / 海外 ${r.global_count ?? 0}）`
                  : '已写入';
            const skip =
              r.skipped_pools && r.skipped_pools.length > 0
                ? `；跳过：${r.skipped_pools.join('、')}`
                : '';
            pushToast(r.ok === false ? 'warn' : 'success', `${importFor.label}：${detail}${skip}`);
          }}
        />
      )}
    </div>
  );
}
