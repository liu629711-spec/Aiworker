/**
 * 免费档模型额度的「此刻是否仍然耗尽」判定（Qoder / Buddy 两域共用）。
 *
 * 为什么不能只看 `exhausted_at` 有没有值：那是**历史事实**——这个模型曾经用完过。
 * 上游按日重置，到了 `reset_at` 之后额度就回来了，但快照里的 `exhausted_at`
 * 不会自动消失（要等下一次真的耗尽才会覆盖）。
 *
 * 实测反例（2026-10-23 用户反馈「免费模型还不能用」）：库内 8 个账号的
 * `free_models` 全是 `exhausted_at=10-22 22:04` + `reset_at=10-23 00:00`，
 * 而当前时刻 19:49 —— 期间同一账号在 19:42:39 用 Qwen3.8-Flash 成功返回 200。
 * 只按 exhausted_at 判会让界面永远显示「已用完」，与真实可用性相反。
 *
 * `reset_at` 缺失时保守判为仍然耗尽：确实用完过，且没有恢复时刻证明已恢复。
 */
export function isQuotaStillExhausted(m: {
  exhausted_at?: string | null;
  reset_at?: string | null;
}): boolean {
  if (!m.exhausted_at) return false;
  if (!m.reset_at) return true;
  // reset_at 形如 "2026-10-23 00:00"（本地时间，无秒），补秒后可稳定解析
  const t = Date.parse(`${m.reset_at.replace(' ', 'T')}:00`);
  if (Number.isNaN(t)) return true;
  return Date.now() < t;
}

/**
 * 免费模型额度徽标的状态与文案（两域口径统一，避免各写一套判断）。
 *
 * 返回 { used, label }：
 * - used=true  → 此刻仍耗尽（琥珀）
 * - used=false → 可用（绿）
 * - 曾耗尽但已过恢复时刻 → used=false 且 label 说明「已恢复」
 */
export function freeQuotaState(m: {
  exhausted_at?: string | null;
  reset_at?: string | null;
}): { used: boolean; recovered: boolean } {
  const used = isQuotaStillExhausted(m);
  return { used, recovered: !!m.exhausted_at && !used };
}
