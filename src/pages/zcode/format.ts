/** 共享格式化工具 */

export function fmtUnits(n: number): string {
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(1)}M`;
  if (n >= 1_000) return `${(n / 1_000).toFixed(1)}K`;
  return String(n);
}

export function fmtTime(unix: number | null): string {
  return unix ? new Date(unix * 1000).toLocaleTimeString("zh-CN") : "-";
}

/**
 * Unix 秒 → 「10-25 00:54」（跨天时带日期）。
 *
 * 为什么不用 fmtTime:它只输出时刻(00:54:10),用户会误读成"还剩 54 分钟"的
 * 倒计时,而实际含义是"10-25 那天 00:54 才解锁"。凡是表达"某时刻之后才能用"
 * 的场景一律用本函数。
 */
export function fmtDateTime(unix: number | null): string {
  if (!unix) return "-";
  const d = new Date(unix * 1000);
  const pad = (n: number) => String(n).padStart(2, '0');
  return `${pad(d.getMonth() + 1)}-${pad(d.getDate())} ${pad(d.getHours())}:${pad(d.getMinutes())}`;
}

/** 套餐到期时间:当天到期的显示"今日 HH:mm",否则"MM-DD HH:mm" */
export function fmtExpire(unix: number | null | undefined): string {
  if (!unix) return "-";
  const d = new Date(unix * 1000);
  const hm = d.toLocaleTimeString("zh-CN", { hour: "2-digit", minute: "2-digit" });
  const today = new Date();
  if (d.toDateString() === today.toDateString()) return `今日 ${hm}`;
  const md = d.toLocaleDateString("zh-CN", { month: "2-digit", day: "2-digit" });
  return `${md} ${hm}`;
}

/**
 * 套餐形态展示元数据（2026-10-23 套餐池改造）。
 *
 * 后端口径（`zcode::plan_profile::PlanKind`）：
 * - `new_account`  新号赠送：短期高额度，到期即失效
 * - `task_card`    任务卡：邀请奖励发放（夜间卡/周末卡/周重置卡），一次性
 * - `global_build` ZCode Global Build：夜间档，只在 23:00→09:00 可用
 * - `start_plan`   ZCode Start Plan：日内额度，长期订阅底座
 * - `other`        未识别形态
 *
 * `burnOrder` 是网关节奏里的燃烧顺序（用户口径「临时额度先烧」）：
 * 数值越小越先被调度消耗，长期订阅留底。
 */
export const PLAN_KIND_META: Record<
  string,
  { label: string; tone: 'green' | 'amber' | 'blue' | 'violet' | 'slate'; burnOrder: number; hint: string }
> = {
  new_account: {
    label: '新号赠送',
    tone: 'green',
    burnOrder: 0,
    hint: '新注册赠送额度，先烧它（到期即失效）',
  },
  task_card: {
    label: '任务卡',
    tone: 'amber',
    burnOrder: 1,
    hint: '邀请奖励发放的任务卡/夜间卡，一次性，优先烧',
  },
  global_build: {
    label: 'Global Build',
    tone: 'blue',
    burnOrder: 2,
    hint: '夜间档额度，仅 23:00→次日 09:00 可用',
  },
  start_plan: {
    label: 'Start Plan',
    tone: 'violet',
    burnOrder: 3,
    hint: '长期订阅底座，最后烧（保留给日常使用）',
  },
  other: { label: '其他套餐', tone: 'slate', burnOrder: 4, hint: '有套餐但未识别出形态' },
  // 'none' 不是 PlanKind 的一种，而是"上游根本没给任何套餐"（grants 为空）。
  // 与 other 分开：other 仍可能有可用权益，none 一定不可选。
  none: { label: '无套餐', tone: 'slate', burnOrder: 9, hint: '上游返回没有任何套餐，网关选号会跳过' },
};

export function planKindLabel(kind?: string | null): string {
  if (!kind) return '';
  return (PLAN_KIND_META[kind] ?? PLAN_KIND_META.other).label;
}

/** 夜间档窗口（与后端 `plan_profile::NIGHT_START_HOUR/END_HOUR` 保持一致） */
export const NIGHT_WINDOW_TEXT = '23:00→次日 09:00';

/** 当前本地时刻是否在夜间档窗口内（跨零点） */
export function inNightWindow(d = new Date()): boolean {
  const h = d.getHours();
  return h >= 23 || h < 9;
}
