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
import { CheckCircle2, Eraser, Loader2, Plug, RefreshCw, TriangleAlert } from 'lucide-react';
import PageHeader from '../components/PageHeader';
import { Badge } from '../components/ui';
import { api } from '../lib/tauri';
import { useAppStore } from '../store';
import type { AgentApplyResult, ApiKeyEntry, DetectedAgent } from '../types';

/** 单端点 agent：一个配置只认一把 Key（多 Key 无意义） */
const SINGLE_ENDPOINT = new Set<DetectedAgent['agent_id']>(['claude', 'codex', 'opencode']);

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
    </div>
  );
}
