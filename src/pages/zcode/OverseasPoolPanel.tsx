/**
 * ZCode 页 · 海外号池接入面板（2026-10-17 海外接入）。
 *
 * 展示两条预置的海外号池专用 Key（Qoder/Buddy 各一把，bind_pool + region=global
 * 双重钉死——启动时自动预置，见 api_keys::ensure_overseas_pool_keys）+ 网关基址 +
 * 两池海外模型清单，供 ZCode CLI 自定义供应商配置直接复制：
 * - 国内账号用国内 Key、海外账号用海外 Key（Key 区域约束在取号侧硬隔离，跨区请求
 *   会被快速拒绝并给出明确文案，不空转）；
 * - Key 删除后不会自动复活（kv 标记幂等），可在「API 管理」弹窗重建。
 */
import { useCallback, useEffect, useState } from 'react';
import { Check, Copy, Globe, RefreshCw } from 'lucide-react';
import { Badge, EmptyState } from '../../components/ui';
import { api, type ApiKeyEntry, type OverseasAccessInfo } from '../../lib/tauri';
import { useAppStore } from '../../store';

const OVERSEAS_KEY_IDS = ['key-qoder-overseas', 'key-buddy-overseas'] as const;

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

function KeyCard({ k, baseUrl, models }: { k: ApiKeyEntry; baseUrl: string; models: string }) {
  return (
    <div className="rounded-xl border border-slate-200 bg-white p-4 dark:border-zinc-800 dark:bg-zinc-900">
      <div className="mb-2 flex items-center gap-2">
        <Globe size={14} className="text-violet-500" />
        <span className="font-medium">{k.name}</span>
        {k.bind_pool === 'qoder' && <Badge tone="blue">Qoder 池</Badge>}
        {k.bind_pool === 'buddy' && <Badge tone="green">Buddy 池</Badge>}
        {k.region === 'global' && <Badge tone="violet">仅海外</Badge>}
        {!k.enabled && <Badge tone="amber">已停用</Badge>}
      </div>
      <div className="mb-2 flex items-center gap-2">
        <code className="flex-1 truncate rounded bg-slate-100 px-2 py-1 font-mono text-xs dark:bg-zinc-800">
          {k.key}
        </code>
        <CopyButton text={k.key} label="Key" />
      </div>
      <div className="mb-2 flex items-center gap-2">
        <code className="flex-1 truncate rounded bg-slate-100 px-2 py-1 font-mono text-xs dark:bg-zinc-800">
          {baseUrl}
        </code>
        <CopyButton text={baseUrl} label="地址" />
      </div>
      <div className="flex items-center gap-2">
        <span className="text-xs text-slate-500 dark:text-zinc-400">
          海外模型 {models ? models.split(',').length : 0} 个
        </span>
        <CopyButton text={models} label="模型名" />
      </div>
    </div>
  );
}

export default function OverseasPoolPanel() {
  const pushToast = useAppStore((s) => s.pushToast);
  const [keys, setKeys] = useState<ApiKeyEntry[]>([]);
  const [info, setInfo] = useState<OverseasAccessInfo | null>(null);
  const [loading, setLoading] = useState(false);

  const refresh = useCallback(async () => {
    setLoading(true);
    try {
      const [keysView, access] = await Promise.all([
        api.apiServer.keysList(),
        api.apiServer.overseasAccess(),
      ]);
      setKeys(keysView.keys.filter((k) => (OVERSEAS_KEY_IDS as readonly string[]).includes(k.id)));
      setInfo(access);
    } catch (err) {
      pushToast('error', `海外号池信息加载失败：${String(err).slice(0, 120)}`);
    } finally {
      setLoading(false);
    }
  }, [pushToast]);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  const qoderKey = keys.find((k) => k.id === 'key-qoder-overseas');
  const buddyKey = keys.find((k) => k.id === 'key-buddy-overseas');
  const qoderModels = (info?.qoder ?? []).map((m) => m.id).join(',');
  const buddyModels = (info?.buddy ?? []).map((m) => m.id).join(',');

  return (
    <div>
      <div className="mb-3 flex items-center justify-between">
        <div>
          <div className="text-sm font-medium">海外号池专用 Key</div>
          <div className="text-xs text-slate-500 dark:text-zinc-400">
            Qoder / Buddy 各一把，区域约束钉死海外账号——国内账号走国内 Key、海外账号走海外 Key，
            跨区请求直接拒绝（不空转）。ZCode CLI 自定义供应商按下方 Key + 地址 + 模型名配置。
          </div>
        </div>
        <button className="btn-outline" onClick={() => void refresh()} disabled={loading}>
          <RefreshCw size={14} className={loading ? 'animate-spin' : ''} /> 刷新
        </button>
      </div>

      {!qoderKey && !buddyKey ? (
        <EmptyState
          icon={<Globe size={28} />}
          title="未找到海外号池 Key"
          hint="网关启动时会自动预置；若已删除，可在「API 管理」弹窗手工重建（区域选「仅海外」）"
        />
      ) : (
        <div className="grid gap-3 md:grid-cols-2">
          {qoderKey && <KeyCard k={qoderKey} baseUrl={info?.base_url ?? ''} models={qoderModels} />}
          {buddyKey && <KeyCard k={buddyKey} baseUrl={info?.base_url ?? ''} models={buddyModels} />}
        </div>
      )}

      {info && (
        <div className="mt-4 rounded-xl border border-slate-200 bg-white p-4 dark:border-zinc-800 dark:bg-zinc-900">
          <div className="mb-2 text-sm font-medium">海外模型清单</div>
          <div className="grid gap-4 md:grid-cols-2">
            <div>
              <div className="mb-1 flex items-center justify-between">
                <span className="text-xs text-slate-500 dark:text-zinc-400">
                  Qoder 海外（{info.qoder.length} 个）
                </span>
                <CopyButton text={qoderModels} label="全部模型名" />
              </div>
              <div className="max-h-44 overflow-auto rounded-lg bg-slate-50 p-2 font-mono text-[11px] leading-5 dark:bg-zinc-800/60">
                {info.qoder.map((m) => (
                  <div key={m.id} className="flex justify-between gap-2">
                    <span className="truncate">{m.id}</span>
                    <span className="shrink-0 text-slate-400">{m.credits || m.name}</span>
                  </div>
                ))}
              </div>
            </div>
            <div>
              <div className="mb-1 flex items-center justify-between">
                <span className="text-xs text-slate-500 dark:text-zinc-400">
                  Buddy 海外（{info.buddy.length} 个）
                </span>
                <CopyButton text={buddyModels} label="全部模型名" />
              </div>
              <div className="max-h-44 overflow-auto rounded-lg bg-slate-50 p-2 font-mono text-[11px] leading-5 dark:bg-zinc-800/60">
                {info.buddy.map((m) => (
                  <div key={m.id} className="flex justify-between gap-2">
                    <span className="truncate">{m.id}</span>
                    <span className="shrink-0 text-slate-400">{m.rate === 0 ? '免费' : `${m.rate}x`}</span>
                  </div>
                ))}
              </div>
            </div>
          </div>
          <div className="mt-3 rounded-lg bg-slate-50 p-3 text-xs leading-5 text-slate-500 dark:bg-zinc-800/60 dark:text-zinc-400">
            <div className="mb-1 font-medium text-slate-600 dark:text-zinc-300">ZCode CLI 配置要点</div>
            <div>Base URL：<code className="font-mono">{info.base_url}</code></div>
            <div>API Key：Qoder 模型用「Qoder 海外号池」Key，Buddy 模型用「Buddy 海外号池」Key</div>
            <div>模型名：点上方「全部模型名」复制，逐个填入自定义模型</div>
          </div>
        </div>
      )}
    </div>
  );
}
