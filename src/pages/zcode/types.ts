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
  /** 该账号自己的邀请码(官方 referral/summary 的 inviter.invite_code) */
  invite_code: string | null;
  /** 邀请资格状态:active=已获得 / locked=冷却中 / null=未查过 */
  invite_state: string | null;
  /** locked 时官方给出的预计开放时间(unix 秒) */
  invite_unlock_at: number | null;
  /** 已成功邀请的人数(累计) */
  invited_count: number | null;
  /** 本周已邀人数(invited.weekly,分配名额依据) */
  invited_weekly: number | null;
  /** 本周邀请上限(invited.reward_cap,官方当前 100) */
  invite_cap: number | null;
  /** 官方是否已判本期封顶(invited.capped) */
  invite_capped: number | null;
  /** 已发放的夜间蹬蹬卡天数 */
  night_card_days: number | null;
  /** 夜间卡状态:in_progress / achieved / issued */
  night_card_state: string | null;
  /** 周末蹬蹬卡本周进度 */
  weekend_progress: number | null;
  /** 周末卡目标(官方给 2) */
  weekend_target: number | null;
  /** 周重置卡已用次数 */
  weekly_reset_used: number | null;
  /** 新号任务:登录客户端 */
  task_login_device: number | null;
  /** 新号任务:绑定邀请码 */
  task_bind_code: number | null;
  /** 新号任务:发送消息 */
  task_send_message: number | null;
  /** 新号任务整体状态:done=三个任务全完成 */
  invitee_state: string | null;
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
  /** 该权益覆盖的模型(capabilities 归一化后的短名;旧快照为空) */
  capabilities?: string[];
  /** 权益优先级(官方给,越大越优先) */
  priority?: number;
}

export interface PlanInfo {
  plan_id: string;
  name: string;
  /** 套餐到期时间(unix 秒;旧快照可能没有) */
  ends_at: number | null;
  entitlements: EntitlementInfo[];
  /** 归一化形态:new_account|task_card|global_build|start_plan|other(旧快照为空) */
  plan_kind?: string;
  /** 官方套餐权重(越大越优先) */
  priority?: number;
  /** active / expired / …(旧快照为空) */
  status?: string;
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
  /** 该账号自己的邀请码 */
  invite_code: string | null;
  /** 邀请资格状态:active=已获得 / locked=冷却中 / null=未查过 */
  invite_state: string | null;
  /** locked 时官方给出的预计开放时间(unix 秒) */
  invite_unlock_at: number | null;
  /** 已成功邀请的人数(累计) */
  invited_count: number | null;
  /** 本周已邀人数(invited.weekly,分配名额依据) */
  invited_weekly: number | null;
  /** 本周邀请上限(invited.reward_cap,官方当前 100) */
  invite_cap: number | null;
  /** 官方是否已判本期封顶(invited.capped) */
  invite_capped: number | null;
  /** 已发放的夜间蹬蹬卡天数(每邀 1 个完成新手任务的新号 +1) */
  night_card_days: number | null;
  /** 夜间卡状态:in_progress / achieved / issued */
  night_card_state: string | null;
  /** 周末蹬蹬卡本周进度 */
  weekend_progress: number | null;
  /** 周末卡目标(官方给 2) */
  weekend_target: number | null;
  /** 周重置卡已用次数 */
  weekly_reset_used: number | null;
  /** 新号任务:登录客户端 */
  task_login_device: number | null;
  /** 新号任务:绑定邀请码 */
  task_bind_code: number | null;
  /** 新号任务:发送消息 */
  task_send_message: number | null;
  /** 新号任务整体状态:done=三个任务全完成 */
  invitee_state: string | null;
  // ── 套餐画像(2026-10-23 套餐池改造;后端由 billing/balance 归一化) ──
  /** 归一化套餐形态(主口径:燃烧顺序最靠前的那一个) */
  plan_kind?: string | null;
  /** 该账号全部形态(new_account/task_card/global_build/start_plan/other) */
  plan_kinds?: string[];
  /** 可服务模型(空 = 没有任何可用权益,网关不会选它) */
  servable_models?: string[];
  /** 当前时刻可服务模型(夜间档按当时窗口裁剪) */
  servable_now?: string[];
  /** 聚合时段口径:daily | daily_night */
  plan_period?: string | null;
  /** 只有夜间档权益(白天不可用,窗口 23:00→09:00) */
  plan_night_only?: boolean;
  /** 剩余额度(token) */
  plan_remaining_units?: number | null;
  /** 发放总额度(token) */
  plan_grant_units?: number | null;
  /** 最早到期时间(unix 秒) */
  plan_ends_at?: number | null;
  /** 画像同步时间(unix 秒);null/undefined = 从未同步 */
  profile_synced_at?: number | null;
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
