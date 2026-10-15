/**
 * claim 业务的前端接口层:页面只允许从这里拿数据/调接口,禁止直接 invoke。
 */
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import type { Event } from "@tauri-apps/api/event";
import type { ClaimStateEvent } from "../types";

export const CLAIM_STATE_EVENT = "claim:state";

export const claimApi = {
  /** 打开 Playwright 领取窗口(用户在弹出的浏览器中完成验证码) */
  start: (accountId: number, planId: string) =>
    invoke<void>("zcode_claim_start", { accountId, planId }),
};

/** 订阅领取流程状态(started/finished) */
export function onClaimState(
  handler: (payload: ClaimStateEvent) => void,
): Promise<() => void> {
  return listen<ClaimStateEvent>(CLAIM_STATE_EVENT, (event: Event<ClaimStateEvent>) =>
    handler(event.payload),
  );
}
