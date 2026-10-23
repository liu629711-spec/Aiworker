/**
 * accounts 业务的前端接口层:页面只允许从这里拿数据/调接口,禁止直接 invoke。
 */
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import type { Account, BatchDone, BatchProgress } from "../types";

export const ACCOUNTS_CHANGED_EVENT = "accounts:changed";
export const BATCH_PROGRESS_EVENT = "batch:progress";
export const BATCH_DONE_EVENT = "batch:done";

export const accountsApi = {
  list: () => invoke<Account[]>("zcode_accounts_list"),
  /** 打开浏览器登录 z.ai(OAuth)并导入新账号 */
  addViaOAuth: () => invoke<Account>("zcode_accounts_add_oauth"),
  /** 导入本机 ZCode 客户端当前登录的账号(重复导入即刷新凭证) */
  importLocal: () => invoke<Account>("zcode_accounts_import_local"),
  /** 取消进行中的 OAuth 轮询登录 */
  cancelOAuth: () => invoke<void>("zcode_accounts_cancel_oauth"),
  /** 切换到此账号:写回本机 credentials.json 并重启 ZCode 客户端 */
  switchTo: (id: number) => invoke<void>("zcode_accounts_switch", { id }),
  remove: (id: number) => invoke<void>("zcode_accounts_delete", { id }),
  /** 设置邀请关系标识:"inviter"/"invited"/空=清除;invitedBy 为被邀请者归属的主号 id */
  setInviteRole: (
    id: number,
    role: "" | "inviter" | "invited",
    invitedBy?: number | null,
  ) =>
    invoke<void>("zcode_accounts_set_invite_role", {
      id,
      role: role === "" ? null : role,
      invitedBy: role === "invited" ? (invitedBy ?? null) : null,
    }),
  /** 自动同步邀请关系(官方奖励中心接口):返回统计 */
  syncInvite: () =>
    invoke<{
      inviters: number;
      linked: number;
      unmatched: string[];
      errors: string[];
      conflicts: string[];
    }>("zcode_accounts_sync_invite"),
  /** 刷新各账号邀请资格(邀请码/active·locked/预计开放时间/已邀请人数) */
  syncInviteQualification: () =>
    invoke<{
      synced: number;
      active: number;
      locked: number;
      errors: string[];
    }>("zcode_accounts_sync_invite_qualification"),
  /** 生成某账号的邀请链接(无资格/无码会报错) */
  inviteLink: (id: number) => invoke<string>("zcode_accounts_invite_link", { id }),
  /** 发起邀请捆绑:浏览器打开本地验证码页,成功后自动标记 invited + invited_by */
  bindInvite: (accountId: number, inviterId: number) =>
    invoke<void>("zcode_accounts_bind_invite", { accountId, inviterId }),
  /** 批量账密登录:按 secret 条目 id 逐个经内嵌窗口导入,返回接受的条目数 */
  batchLogin: (secretIds: number[]) =>
    invoke<number>("zcode_accounts_batch_login", { secretIds }),
  /** 取消进行中的批量登录队列 */
  cancelBatchLogin: () => invoke<void>("zcode_accounts_batch_login_cancel"),
  /** 批量登录是否在进行中(窗口重开时恢复按钮态) */
  batchLoginStatus: () => invoke<boolean>("zcode_accounts_batch_login_status"),
};

/** 订阅账号变化事件,返回取消订阅函数 */
export function onAccountsChanged(handler: () => void): Promise<() => void> {
  return listen(ACCOUNTS_CHANGED_EVENT, () => handler());
}

/** 订阅批量登录进度事件,返回取消订阅函数 */
export function onBatchProgress(
  handler: (p: BatchProgress) => void,
): Promise<() => void> {
  return listen<BatchProgress>(BATCH_PROGRESS_EVENT, (e) => handler(e.payload));
}

/** 订阅批量登录结束事件,返回取消订阅函数 */
export function onBatchDone(handler: (d: BatchDone) => void): Promise<() => void> {
  return listen<BatchDone>(BATCH_DONE_EVENT, (e) => handler(e.payload));
}
