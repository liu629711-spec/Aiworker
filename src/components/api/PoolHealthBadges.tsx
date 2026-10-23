import { Badge } from '../ui';
import type { PoolStatus } from '../../types';

/**
 * 池账号运行时健康徽标（三池共用：Trae/Buddy/Qoder）
 *
 * 后端可观测字段：disabled（401/403 禁用）、hard_credit（积分耗尽硬冷却，
 * 次日 04:00 自动恢复探测 F-29 v1.2）、cooling + cooldown_reason（软冷却）、
 * credits / credits_expire_at（零积分 / 积分过期）、inflight（在途并发）、
 * per_model_cooldowns（按模型冷却：账号健康但某个模型被限）。
 * 可选口径对齐后端 pool::selectable——任一不可选条件命中即不显示「就绪」，
 * 杜绝「界面就绪、实际取不到号」的可观测盲点。
 */

/** 冷却原因中文映射（ErrKind.as_str 大驼峰 + consecutive_errors / hard_credit 小写蛇形） */
const REASON_LABELS: Record<string, string> = {
  sessiondead: '会话失效(401)',
  forbidden: '禁止访问(403)',
  hardcredit: '积分耗尽',
  hard_credit: '积分耗尽',
  planlimit: '套餐额度用尽',
  softrate: '上游限流(429)',
  notfound: '模型不存在(404)',
  server: '上游服务错误(5xx)',
  client: '上游请求错误(4xx)',
  consecutive_errors: '连续错误熔断',
  refresh_token_invalid: '凭证失效',
};

export function cooldownReasonLabel(reason: string | null | undefined): string {
  if (!reason) return '冷却中';
  return REASON_LABELS[reason.toLowerCase()] ?? reason;
}

/** Unix 秒 → "MM-DD HH:MM"（按查看者本地时区；与 cooldownReasonLabel 同侧格式化） */
function fmtUntil(ts: number): string {
  const d = new Date(ts * 1000);
  const pad = (n: number) => String(n).padStart(2, '0');
  return `${pad(d.getMonth() + 1)}-${pad(d.getDate())} ${pad(d.getHours())}:${pad(d.getMinutes())}`;
}

/**
 * 按模型冷却徽标（逐个列出，不合并计数）。
 *
 * 为什么必须逐个列模型名：一个账号同时冷却两个免费模型是常态（Qoder 的
 * qwen3.8-flash / deepseek-flash 各有每日免费额度）。合并成「2 个模型冷却」
 * 用户仍然不知道该打哪个、该换哪个——模型名才是可执行信息。
 * 徽标只说明「这个模型此刻不能打」，账号本身健康（积分与其他模型照常可用），
 * 故排在账号级冷却之后、不参与 selectable 判定（后端按模型过滤，不是整号停用）。
 */
function PerModelCooldownBadges({ map }: { map: Record<string, number> }) {
  // 排序只为展示稳定（后端 HashMap 无序）；截止时刻早的排前面
  const entries = Object.entries(map).sort((a, b) => a[1] - b[1]);
  if (entries.length === 0) return null;
  return (
    <>
      {entries.map(([model, until]) => (
        <Badge
          key={model}
          tone="amber"
          title={`${model} 被上游限到 ${fmtUntil(until)} 恢复（Qoder 免费额度耗尽 / Buddy 模型级限流）；该账号其他模型与积分仍可用`}
        >
          {model} 冷却至 {fmtUntil(until)}
        </Badge>
      ))}
    </>
  );
}

/**
 * ZCode 套餐画像徽标（2026-10-23 套餐池改造；仅 zcode 池有数据）。
 *
 * 与 PoolHealthBadges 的分工：那个回答「这个号现在能不能打」（运行态），
 * 这个回答「这个号凭什么能打、还能打什么、什么时候能打」（权益态）。
 * 网关选号的过滤维度就是这三条，摆到界面上用户才能解释「为什么这个号被跳过」。
 *
 * 形态 → 色调（与 zcode 页 PLAN_KIND_META 一致）：
 * new_account=绿 / task_card=琥珀 / global_build=蓝 / start_plan=紫 / other=灰
 */
const PLAN_TONE: Record<string, 'green' | 'amber' | 'blue' | 'violet' | 'slate'> = {
  new_account: 'green',
  task_card: 'amber',
  global_build: 'blue',
  start_plan: 'violet',
  other: 'slate',
  none: 'slate',
};

const PLAN_LABEL: Record<string, string> = {
  new_account: '新号赠送',
  task_card: '任务卡',
  global_build: 'Global Build',
  start_plan: 'Start Plan',
  other: '其他套餐',
  none: '无套餐',
};

export function PoolPlanBadges({ s }: { s?: PoolStatus }) {
  if (!s || !s.plan_kind) return null;
  const hour = new Date().getHours();
  const nightOpen = hour >= 23 || hour < 9;
  const models = s.plan_models ?? [];
  const kinds = s.plan_kinds?.length ? s.plan_kinds : [s.plan_kind];
  // 无套餐：一条红徽标说完就够（再叠「其他套餐」「无可用权益」只会让行变吵）
  if (kinds.length === 1 && kinds[0] === 'none') {
    return (
      <Badge tone="red" title="上游返回该账号没有任何套餐/资源包，网关选号会直接跳过它">
        无套餐·不会选中
      </Badge>
    );
  }
  return (
    <>
      {kinds.map((k) => (
        <Badge key={k} tone={PLAN_TONE[k] ?? 'slate'}>
          {PLAN_LABEL[k] ?? k}
        </Badge>
      ))}
      {models.length === 0 ? (
        <Badge tone="red" title="上游明确返回该账号没有任何可用权益，网关选号会跳过它">
          无可用权益
        </Badge>
      ) : (
        <Badge tone="slate" title="上游 capabilities 给出的可服务模型（请求其他模型会被跳过）">
          {models.join(' / ')}
        </Badge>
      )}
      {s.plan_night_only && (
        <Badge
          tone={nightOpen ? 'green' : 'amber'}
          title="夜间档额度只在 23:00→次日 09:00 生效；窗外不参与选号（不冷却、不计错）"
        >
          夜间档{nightOpen ? '·窗口内' : '·窗外跳过'}
        </Badge>
      )}
      {s.plan_remaining_units != null && s.plan_remaining_units > 0 && (
        <Badge tone="slate" title="该账号全部权益的剩余额度合计（token）">
          剩 {(s.plan_remaining_units / 1_000_000).toFixed(1)}M
        </Badge>
      )}
    </>
  );
}

export function PoolHealthBadges({
  s,
  running,
  tokenInvalid = false,
}: {
  s?: PoolStatus;
  running: boolean;
  /** 凭证已失效（RefreshTokenBadge 已示，避免「已禁用」徽标重复） */
  tokenInvalid?: boolean;
}) {
  if (!s) return null;
  const nowSec = Math.floor(Date.now() / 1000);
  const creditsExhausted = s.credits != null && s.credits <= 0;
  const creditsExpired =
    s.credits_expire_at != null && s.credits_expire_at > 0 && s.credits_expire_at < nowSec;
  const selectable =
    !s.disabled && !s.cooling && !s.hard_credit && !creditsExhausted && !creditsExpired;
  return (
    <>
      {s.disabled && !tokenInvalid && <Badge tone="red">已禁用</Badge>}
      {s.hard_credit && <Badge tone="red">积分耗尽·次日04:00恢复</Badge>}
      {s.cooling && (
        <Badge tone="amber">冷却中·{cooldownReasonLabel(s.cooldown_reason)}</Badge>
      )}
      {creditsExhausted && <Badge tone="amber">零积分</Badge>}
      {creditsExpired && <Badge tone="amber">积分已过期</Badge>}
      {/* 按模型冷却（账号健康但某个模型被限）：逐个列模型名 + 恢复时刻 */}
      <PerModelCooldownBadges map={s.per_model_cooldowns ?? {}} />
      {running &&
        selectable &&
        ((s.inflight ?? 0) > 0 ? (
          <Badge tone="amber">在途 {s.inflight}</Badge>
        ) : (
          <Badge tone="green">就绪</Badge>
        ))}
    </>
  );
}
