/**
 * monitor 业务的前端接口层:页面只允许从这里拿数据/调接口,禁止直接 invoke。
 */
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import type { Event } from "@tauri-apps/api/event";
import type { ClaimDetected, Snapshot } from "../types";

export const SNAPSHOT_EVENT = "monitor:snapshot";
export const CLAIM_DETECTED_EVENT = "monitor:claim-detected";
/** 一轮余额刷新开始/每探完一个账号/结束时广播,启动首轮与手动刷新都会触发 */
export const REFRESHING_EVENT = "monitor:refreshing";

export interface Refreshing {
  active: boolean;
  done: number;
  total: number;
}

export const monitorApi = {
  snapshot: () => invoke<Snapshot>("zcode_monitor_snapshot"),
  /** 手动检测单个账号(卡片「检测」按钮),返回最新快照 */
  checkAccount: (accountId: number) =>
    invoke<Snapshot>("zcode_monitor_check_account", { accountId }),
  /** 立即刷新全部账号剩余用量 */
  refreshNow: () => invoke<Snapshot>("zcode_monitor_refresh_now"),
};

/** 订阅全局快照(余额/套餐/活动/当前账号),返回取消订阅函数 */
export function onSnapshot(handler: (payload: Snapshot) => void): Promise<() => void> {
  return listen<Snapshot>(SNAPSHOT_EVENT, (event: Event<Snapshot>) =>
    handler(event.payload),
  );
}

/** 订阅余额刷新进行中状态(按钮 loading 与进度用) */
export function onRefreshing(handler: (p: Refreshing) => void): Promise<() => void> {
  return listen<Refreshing>(REFRESHING_EVENT, (event: Event<Refreshing>) =>
    handler(event.payload),
  );
}

/** 订阅"发现新可领活动"提醒 */
export function onClaimDetected(
  handler: (payload: ClaimDetected) => void,
): Promise<() => void> {
  return listen<ClaimDetected>(CLAIM_DETECTED_EVENT, (event: Event<ClaimDetected>) =>
    handler(event.payload),
  );
}
