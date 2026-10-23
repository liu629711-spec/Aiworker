import { Clock } from 'lucide-react';
import { Badge } from '../ui';
import type { ModelCooldown } from '../../types';

/**
 * 进程级模型冷却横幅（Buddy / Qoder 资源调度页共用）。
 *
 * 与账号行的 `per_model_cooldowns` 徽标是**两个维度**，别混：
 * - 账号级 = 「这个账号的这个模型没额度」→ 换账号就能绕过（徽标在账号行）
 * - 模型级 = 「这个模型整体在排队/被限流」→ 换账号照样排队，只能等或换模型
 *   （Qoder 海外实测：换号=新号从头排，原号队列位置作废）
 *
 * 只读快照，无交互；剩余秒数由父级轮询驱动刷新（3s）。
 */

/** 秒 → 「Ns / Nm Ns」（冷却都是秒/分钟级，不必精确到日） */
function fmtRemaining(secs: number): string {
  if (secs < 60) return `${secs}s`;
  const m = Math.floor(secs / 60);
  const s = secs % 60;
  return s === 0 ? `${m}m` : `${m}m${s}s`;
}

export function ModelCooldownBanner({ items }: { items: ModelCooldown[] }) {
  if (items.length === 0) return null;
  // 剩余时间长的排后面（用户先看快恢复的）；同秒按模型名稳定排序
  const sorted = [...items].sort((a, b) => a.remaining_secs - b.remaining_secs || a.model.localeCompare(b.model));
  return (
    <div className="flex flex-wrap items-center gap-2 rounded-lg border border-amber-200 bg-amber-50 px-3 py-2 text-xs dark:border-amber-900/50 dark:bg-amber-950/30">
      <span className="flex items-center gap-1.5 font-medium text-amber-700 dark:text-amber-400">
        <Clock size={13} />
        模型冷却中
      </span>
      <span className="text-amber-600/80 dark:text-amber-500/70">
        （该模型整体排队/限流，换账号同样要排，建议换其他模型）
      </span>
      {sorted.map((c) => (
        <Badge
          key={`${c.global ? 'g' : 'cn'}:${c.model}`}
          tone="amber"
          title={`${c.model}${c.global ? '（海外区）' : ''} 冷却剩余约 ${fmtRemaining(c.remaining_secs)}${c.fails ? `；连续失败 ${c.fails} 次（渐进冷却会翻倍）` : '（排队退避）'}`}
        >
          {c.model}
          {c.global ? '·海外' : ''} {fmtRemaining(c.remaining_secs)}
        </Badge>
      ))}
    </div>
  );
}
