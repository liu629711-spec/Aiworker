/**
 * 将 AI Work Studio 网关模型目录按池（trae/buddy/qoder）拆分为三个 ZCode 自定义供应商。
 *
 * 用法：node --experimental-sqlite scripts/sync_zcode_pools.mjs
 * 行为：
 *  - 从网关 /v1/models 拉取统一模型目录（含倍率/归属池）
 *  - 从本地库 kv（api_models / wb_model_catalog）读取各池声明的上下文，
 *    每个模型取「各号池声明的最大上下文」（应用户要求不做保守口径）
 *  - 在 ~/.zcode/v2/provider_config.json 中 upsert 三个 provider：
 *      aiwork-trae / aiwork-buddy / aiwork-qoder（各自独立 API Key）
 *    模型 ID 使用池前缀（如 trae/glm-5.3），网关按前缀锁池，请求互不串池
 *  - 不再生成 -max 变体（应用户要求精简；Max Mode 需要时在模型名后手动加 -max 即可）
 *  - 写入前自动备份原文件（.aiwork-backup-<时间戳>）
 *  - 幂等：重复执行先清除旧的同名 provider 再重建，不影响其它供应商
 */
import { readFileSync, writeFileSync, copyFileSync } from 'node:fs';
import { homedir } from 'node:os';
import { join } from 'node:path';
import { DatabaseSync } from 'node:sqlite';

const GATEWAY = 'http://127.0.0.1:7864';
const BASE_URL = 'http://127.0.0.1:7864/v1';
const API_TYPE = 'anthropic-messages';
const GROUP = 'standard-personal';
// 任一把有效的网关 Key（用于拉目录；三池各自的 Key 见 POOLS）
const READ_KEY = 'ck_ab9cdf27868e7075d271ce6c3aae4517';

const POOLS = [
  { id: 'aiwork-trae', name: 'AIWork·Trae池', prefix: 'trae', apiKey: 'ck_ab9cdf27868e7075d271ce6c3aae4517' },
  { id: 'aiwork-buddy', name: 'AIWork·Buddy池', prefix: 'buddy', apiKey: 'ck_d7de161f0eeba8b2a33567242966c4c6' },
  { id: 'aiwork-qoder', name: 'AIWork·Qoder池', prefix: 'qoder', apiKey: 'ck_179d02037e8684d90e8addd08d451f82' },
];

// ---- 1. 拉取网关统一目录 ----
const res = await fetch(`${GATEWAY}/v1/models`, {
  headers: { Authorization: `Bearer ${READ_KEY}` },
});
if (!res.ok) {
  throw new Error(`拉取网关模型目录失败: HTTP ${res.status}（请确认 AI Work Studio 网关已启动、Key 有效）`);
}
const { data } = await res.json();
console.log(`网关模型目录: ${data.length} 个模型`);

// ---- 2. 读取本地库各池上下文声明，取每模型最大值 ----
const maxCtx = new Map(); // canonical id -> max context
const bump = (id, ctx) => {
  if (!id || !Number.isFinite(ctx) || ctx <= 0) return;
  const key = id.trim().toLowerCase();
  maxCtx.set(key, Math.max(maxCtx.get(key) ?? 0, ctx));
};
for (const m of data) bump(m.id, m.context_length);

try {
  const db = new DatabaseSync(join(process.env.APPDATA, 'AIWorkAssistant', 'data', 'aiwork.sqlite'), { readOnly: true });
  const trae = JSON.parse(db.prepare("SELECT content FROM kv WHERE key='api_models'").get()?.content ?? '{}');
  const traeArr = Array.isArray(trae) ? trae : (trae.models ?? []);
  // context_length = 实际请求槽位；context_length_max = 官方能力上限（应用户要求按上限口径）
  for (const m of traeArr) {
    bump(m.id, m.context_length);
    bump(m.id, m.context_length_max);
  }
  const wb = JSON.parse(db.prepare("SELECT content FROM kv WHERE key='wb_model_catalog'").get()?.content ?? '{}');
  const wbArr = Array.isArray(wb) ? wb : (wb.models ?? []);
  for (const m of wbArr) bump(m.id, m.context_length);
  console.log(`本地池目录: trae ${traeArr.length} 个 / buddy ${wbArr.length} 个（已并入最大上下文计算）`);
} catch (e) {
  console.warn(`读取本地池目录失败（仅用网关目录口径）: ${e.message}`);
}

// ---- 3. 写入 ZCode 配置 ----
const cfgPath = join(homedir(), '.zcode', 'v2', 'provider_config.json');
const cfg = JSON.parse(readFileSync(cfgPath, 'utf8'));
const backup = `${cfgPath}.aiwork-backup-${new Date().toISOString().replace(/[:.]/g, '-').slice(0, 19)}`;
copyFileSync(cfgPath, backup);
console.log(`已备份原配置 → ${backup}`);

const conf = cfg.config;
conf.providerConfigRules ??= { providerRules: [] };
conf.modelConfigRules ??= { providerModelRules: [], manualProviderModelRules: [] };

// 幂等：清掉旧的同名 provider 及其模型规则
conf.providerConfigRules.providerRules = conf.providerConfigRules.providerRules.filter(
  (r) => !POOLS.some((p) => p.id === r.providerId),
);
conf.modelConfigRules.providerModelRules = conf.modelConfigRules.providerModelRules.filter(
  (r) => !POOLS.some((p) => p.id === r.providerId),
);

for (const pool of POOLS) {
  const models = data.filter((m) => (m.sources ?? []).some((s) => s.pool === pool.prefix));
  const ids = [];
  const rules = [];
  for (const m of models) {
    const mid = `${pool.prefix}/${m.id}`;
    ids.push(mid);
    const ctx = maxCtx.get(m.id.trim().toLowerCase()) ?? m.context_length;
    const config = { enabled: true };
    const properties = {};
    if (ctx) properties.contextWindow = ctx;
    if (m.supports_image === true) properties.inputFormat = { supportsImage: true };
    if (Object.keys(properties).length) config.properties = properties;
    if (m.max_tokens) config.optionSpecs = { maxOutputTokens: { max: m.max_tokens } };
    rules.push({ modelId: mid, providerId: pool.id, config });
  }
  conf.providerConfigRules.providerRules.push({
    providerId: pool.id,
    providerName: pool.name,
    config: {
      group: GROUP,
      access: { type: 'api-key', apiKey: pool.apiKey },
      api: { type: API_TYPE, baseUrl: BASE_URL },
      personalModelIds: ids,
      modelOrder: ids,
    },
  });
  conf.modelConfigRules.providerModelRules.push(...rules);
  console.log(`${pool.name}: ${models.length} 个模型`);
}

// 三个池供应商置顶，便于在 ZCode 模型选择器中优先看到
conf.providerOrder = [
  ...POOLS.map((p) => p.id),
  ...(conf.providerOrder ?? []).filter((id) => !POOLS.some((p) => p.id === id)),
];

writeFileSync(cfgPath, JSON.stringify(cfg, null, 2) + '\n');
console.log(`已写入 ${cfgPath}`);
console.log('注意：若 ZCode 正在运行，请完全退出并重新打开后生效；期间勿在 ZCode 设置中改动模型配置（会被内存态覆盖）。');
