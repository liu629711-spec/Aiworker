/**
 * register 业务的前端接口层:马甲邮箱全自动注册流水线。
 * 页面只允许从这里拿数据/调接口,禁止直接 invoke。
 */
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

export const REGISTER_PROGRESS_EVENT = "register:progress";
export const REGISTER_DONE_EVENT = "register:done";
/** 捆绑真实结果(浏览器里完成验证码后由 Rust 广播) */
export const BIND_RESULT_EVENT = "zcode:bind-result";

/** 流水线阶段标识(与 Rust flow.rs 的 step 一一对应) */
export type RegisterStep =
  | "open"
  | "fill"
  | "mail"
  | "verify"
  | "oauth"
  | "bind"
  | "done";

/** 单步进度(Rust register:progress 事件) */
export interface RegisterProgress {
  step: RegisterStep;
  /** running | success | failed | cancelled */
  state: "running" | "success" | "failed" | "cancelled";
  message: string;
  /**
   * 归属标签:批量模式下形如 "[2/5] a@mozmail.com",单条为空串。
   * 前端必须把它并入 key,否则同一 step 的后一条事件会覆盖前一条,
   * 批量跑完只能看到最后一个号的当前态。
   */
  tag: string;
  account_id?: number;
}

/** 捆绑真实结果(Rust zcode:bind-result 事件)。
 * 捆绑的 bind-code 由系统浏览器里的页面异步触发,Rust 收到结果后才广播——
 * 在此之前流水线只能报"已打开验证页",不能算成功。 */
export interface BindResult {
  account_id: number;
  inviter_id: number;
  ok: boolean;
  message: string;
}

/** 结束汇总(Rust register:done 事件) */
export interface RegisterDone {
  ok: boolean;
  message: string;
  account_id: number | null;
  bound: boolean;
}

/** 已保存的 IMAP 配置(密码不回显) */
export interface ImapState {
  configured: boolean;
  host: string;
  user: string | null;
}

/** 可复用的现成马甲邮箱(账密库里尚未入池的条目) */
export interface MaskPoolItem {
  id: number;
  email: string;
}

/** Relay(马甲邮箱)状态 */
export interface RelayState {
  ok: boolean;
  account_email: string;
  mask_count: number;
  free_limit: number;
}

/** Relay 里的一个马甲（含是否已入池） */
export interface RelayMask {
  id: number;
  address: string;
  enabled: boolean;
  /** 本地是否已有对应账号（已入池的删了不影响凭证，但地址不能再收信） */
  pooled: boolean;
}

/** Relay 马甲全量列表 */
export interface RelayMaskList {
  account_email: string;
  total: number;
  free_limit: number;
  masks: RelayMask[];
}

export const registerApi = {
  /** 列出账密库里尚未入池的马甲邮箱(优先复用,不必手输) */
  maskPool: () => invoke<MaskPoolItem[]>("zcode_register_mask_pool"),
  /** Relay 状态:能否复用本机浏览器登录态、账号邮箱、现有马甲数 */
  relayStatus: () => invoke<RelayState>("zcode_relay_status"),
  /** Relay 全部马甲(标注哪些已入池),供管理/清理 */
  relayMasks: () => invoke<RelayMaskList>("zcode_relay_list_masks"),
  /** 删除一个马甲(腾出免费额度) */
  deleteMask: (maskId: number) => invoke<void>("zcode_relay_delete_mask", { maskId }),
  /** 自动生成 N 个新马甲(直接调 Relay 接口,无需人工过验证码) */
  createMasks: (count: number) => invoke<string[]>("zcode_relay_create_masks", { count }),
  /**
   * 批量注册:一次把 N 个号跑完(注册 → 收码 → 入池 → 捆绑)。
   * `source` 为 "generate"(自动生成新马甲)或 "pool"(用账密库里现成的)。
   */
  batch: (params: {
    count: number;
    source: "generate" | "pool";
    inviterId?: number | null;
    mailTimeoutSec?: number;
  }) =>
    invoke<number>("zcode_register_batch", {
      count: params.count,
      source: params.source,
      inviterId: params.inviterId ?? null,
      mailTimeoutSec: params.mailTimeoutSec ?? null,
    }),
  /**
   * 发起单个注册流水线:注册 → 邮箱验证 → 授权入池 →(可选)邀请捆绑。
   * `secretId` 传账密条目 id 时复用现成账密(优先);否则用手输的 email/password。
   */
  start: (params: {
    email?: string;
    password?: string;
    username?: string;
    inviterId?: number | null;
    mailTimeoutSec?: number;
    secretId?: number | null;
  }) =>
    invoke<void>("zcode_register_start", {
      email: params.email ?? null,
      password: params.password ?? null,
      username: params.username ?? null,
      inviterId: params.inviterId ?? null,
      mailTimeoutSec: params.mailTimeoutSec ?? null,
      secretId: params.secretId ?? null,
    }),
  /**
   * 补齐某账号缺失的新手任务（登录客户端 + 发送消息）。
   * includeLogin=false 时只发消息，不打断当前登录态。
   */
  completeTasks: (accountId: number, includeLogin: boolean) =>
    invoke<{ login_device: boolean; send_message: boolean; message: string }>(
      "zcode_register_complete_tasks",
      { accountId, includeLogin },
    ),
  /** 取消进行中的注册流水线 */
  cancel: () => invoke<void>("zcode_register_cancel"),
  /** 注册流水线是否在进行中 */
  status: () => invoke<boolean>("zcode_register_status"),
  /** 保存并测试马甲邮箱(Gmail)IMAP 配置 */
  saveImap: (host: string, port: number, user: string, pass: string) =>
    invoke<string>("zcode_register_save_imap", { host, port, user, pass }),
  /** 读取已保存的 IMAP 配置(不含密码) */
  imapState: () => invoke<ImapState>("zcode_register_imap_state"),
};

/** 订阅注册进度事件,返回取消订阅函数 */
export function onRegisterProgress(
  handler: (p: RegisterProgress) => void,
): Promise<() => void> {
  return listen<RegisterProgress>(REGISTER_PROGRESS_EVENT, (e) => handler(e.payload));
}

/** 订阅注册结束事件,返回取消订阅函数 */
export function onRegisterDone(
  handler: (d: RegisterDone) => void,
): Promise<() => void> {
  return listen<RegisterDone>(REGISTER_DONE_EVENT, (e) => handler(e.payload));
}

/** 订阅捆绑真实结果事件,返回取消订阅函数 */
export function onBindResult(
  handler: (r: BindResult) => void,
): Promise<() => void> {
  return listen<BindResult>(BIND_RESULT_EVENT, (e) => handler(e.payload));
}
