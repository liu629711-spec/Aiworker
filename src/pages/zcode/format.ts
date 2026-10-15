/** 共享格式化工具 */

export function fmtUnits(n: number): string {
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(1)}M`;
  if (n >= 1_000) return `${(n / 1_000).toFixed(1)}K`;
  return String(n);
}

export function fmtTime(unix: number | null): string {
  return unix ? new Date(unix * 1000).toLocaleTimeString("zh-CN") : "-";
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
