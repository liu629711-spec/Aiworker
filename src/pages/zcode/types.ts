/**
 * 与 Rust 侧模型一一对应的类型。
 * 注意:serde 默认序列化保持 snake_case,故字段名与 Rust 结构体一致。
 */

export interface Account {
  id: number;
  label: string;
  user_id: string | null;
  jwt: string;
  refresh_token: string;
  device_mid: string;
  status: string;
  is_current: boolean;
  /** 切换账号写回 credentials.json 所需字段 */
  user_info: string | null;
  zai_access_token: string | null;
  zai_refresh_token: string | null;
  /** 邀请关系标识:inviter=邀请者(主号), invited=被邀请者(小号), null=未标记 */
  invite_role: string | null;
  /** 被邀请者归属的邀请者账号 id(绑定关系) */
  invited_by: number | null;
  created_at: number;
  updated_at: number;
}

export interface BalanceItem {
  show_name: string;
  /** 归属套餐的 entitlement(跨套餐同名模型靠它区分;旧快照可能没有) */
  entitlement_id: string | null;
  bucket_id: string | null;
  total_units: number;
  used_units: number;
  remaining_units: number;
}

export interface EntitlementInfo {
  entitlement_id: string | null;
  show_name: string;
  grant_units: number;
  period: string;
}

export interface PlanInfo {
  plan_id: string;
  name: string;
  /** 套餐到期时间(unix 秒;旧快照可能没有) */
  ends_at: number | null;
  entitlements: EntitlementInfo[];
}

export interface ClaimablePlan {
  plan_id: string;
  name: string;
}

export interface AccountLive {
  id: number;
  label: string;
  /** 自定义昵称(user_info.name,与邮箱不同时才有) */
  nickname: string | null;
  /** 登录邮箱(user_info.email;早期数据缺失时可能为 null) */
  email: string | null;
  /** 邀请关系标识:inviter=邀请者(主号), invited=被邀请者(小号), null=未标记 */
  invite_role: string | null;
  /** 被邀请者归属的邀请者账号 id(绑定关系;邀请者/未标记为 null) */
  invited_by: number | null;
  is_current: boolean;
  plans: PlanInfo[];
  balances: BalanceItem[];
  claimables: ClaimablePlan[];
  balance_error: string | null;
  updated_at: number | null;
}

export interface Snapshot {
  accounts: AccountLive[];
  current_label: string | null;
  last_balance_at: number | null;
  last_preview_at: number | null;
}

export interface ClaimDetected {
  account_id: number;
  label: string;
  plans: ClaimablePlan[];
}

export interface ClaimStateEvent {
  account_id: number;
  plan_id: string;
  phase: "started" | "finished";
  ok: boolean | null;
  message: string | null;
}

/** 账密管理行:account_id 非空 = 关联已导入账号(title 为账号名),null = 独立录入条目 */
export interface SecretItem {
  id: number;
  account_id: number | null;
  title: string;
  login_name: string;
  password: string;
  updated_at: number;
}

/** 批量登录队列的单条进度(Rust batch:progress 事件) */
export interface BatchProgress {
  index: number;
  total: number;
  secret_id: number;
  login_name: string;
  state: "running" | "success" | "failed" | "cancelled";
  message: string;
}

/** 批量登录队列结束汇总(Rust batch:done 事件) */
export interface BatchDone {
  total: number;
  ok: number;
  fail: number;
}

/* ===== WorkBuddy 多账号(国内/海外) ===== */

/** 从会话文件明文字段解析出的登录信息 */
export interface WbSessionMeta {
  uid: string;
  uin: string | null;
  domain: string | null;
  /** domestic=国内(www.workbuddy.cn) / overseas=海外(www.workbuddy.ai) */
  region: "domestic" | "overseas";
}

/** 已捕获的 WorkBuddy 账号(一份官方会话文件快照) */
export interface WbAccount {
  id: number;
  uid: string;
  label: string | null;
  uin: string | null;
  region: string;
  domain: string | null;
  captured_at: number;
  is_current: boolean;
}

/** WorkBuddy 当前状态(serde flatten:登录时 meta 字段平铺在顶层) */
export interface WbStatus {
  logged_in: boolean;
  running: boolean;
  exe_found: string | null;
  uid?: string;
  uin?: string | null;
  domain?: string | null;
  region?: "domestic" | "overseas";
}
