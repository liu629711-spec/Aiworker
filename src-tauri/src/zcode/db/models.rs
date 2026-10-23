use serde::Serialize;

/// 账号(对应 accounts 表)。
/// jwt / refresh_token 当前为明文落库(个人工具、库文件在用户目录);如需加密再引入 DPAPI。
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct Account {
    pub id: i64,
    pub label: String,
    pub user_id: Option<String>,
    pub jwt: String,
    pub refresh_token: String,
    /// 每账号固定一个稳定 UUID,接口请求头 X-Device-Mid 用
    pub device_mid: String,
    /// stored / active / expired ...
    pub status: String,
    /// 是否为 ZCode 客户端当前登录的账号(由本机 credentials.json 比对得出)
    pub is_current: bool,
    /// 切换账号时写回 credentials.json 的身份信息(JSON 字符串)
    pub user_info: Option<String>,
    pub zai_access_token: Option<String>,
    pub zai_refresh_token: Option<String>,
    /// 邀请关系标识:inviter=邀请者(主号), invited=被邀请者(小号), NULL=未标记
    pub invite_role: Option<String>,
    /// 被邀请者归属的邀请者账号 id(绑定关系;邀请者本身为 NULL)
    pub invited_by: Option<i64>,
    /// 该账号自己的邀请码(官方 referral/summary 的 inviter.invite_code)
    pub invite_code: Option<String>,
    /// 邀请资格状态:active=已获得 / locked=冷却中 / NULL=未查过
    pub invite_state: Option<String>,
    /// locked 时官方给出的预计开放时间(unix 秒)
    pub invite_unlock_at: Option<i64>,
    /// 已成功邀请的人数(累计,inviter.invited.total)
    pub invited_count: Option<i64>,
    /// 本周已邀请人数(inviter.invited.weekly)——分配名额的真实依据
    pub invited_weekly: Option<i64>,
    /// 本周邀请上限(inviter.invited.reward_cap;官方当前给 100)
    pub invite_cap: Option<i64>,
    /// 官方是否已判定本期封顶(inviter.invited.capped)
    pub invite_capped: Option<i64>,
    /// 邀请资格查询时间(unix 秒)
    pub invite_synced_at: Option<i64>,
    /// 已发放的夜间蹬蹬卡天数(每邀 1 个完成新手任务的新号 +1)
    pub night_card_days: Option<i64>,
    /// 夜间卡状态:in_progress / achieved / issued
    pub night_card_state: Option<String>,
    /// 周末蹬蹬卡进度(每周邀满 weekend_target 个即可参与周末活动)
    pub weekend_progress: Option<i64>,
    /// 周末卡目标(官方给 2)
    pub weekend_target: Option<i64>,
    /// 周重置卡已用次数(Coding Plan 订阅用户;本期邀满 10 个发一次)
    pub weekly_reset_used: Option<i64>,
    /// 新号任务:登录客户端(被邀请者侧)
    pub task_login_device: Option<i64>,
    /// 新号任务:绑定邀请码(被邀请者侧)
    pub task_bind_code: Option<i64>,
    /// 新号任务:发送消息(被邀请者侧)
    pub task_send_message: Option<i64>,
    /// 新号任务整体状态:done=三个任务全完成(夜间卡已发)
    pub invitee_state: Option<String>,
    /// 套餐形态归一化结果:new_account|task_card|global_build|start_plan|other
    /// (见 zcode::plan_profile;NULL = 从未同步过画像,网关按"不可用于推理"处理)
    #[serde(default)]
    pub plan_kind: Option<String>,
    /// 该账号可服务的模型(逗号分隔,catalog id)。**空/None = 没有任何可用权益,不该入池**
    #[serde(default)]
    pub plan_servable_models: Option<String>,
    /// 权益发放总量(token)
    #[serde(default)]
    pub plan_grant_units: Option<i64>,
    /// 权益已消耗量
    #[serde(default)]
    pub plan_used_units: Option<i64>,
    /// 权益剩余量
    #[serde(default)]
    pub plan_remaining_units: Option<i64>,
    /// 聚合时段口径:daily|daily_night|…
    #[serde(default)]
    pub plan_period: Option<String>,
    /// 1 = 只有夜间档权益(白天不能选号)
    #[serde(default)]
    pub plan_night_only: Option<i64>,
    /// 最早到期时间(unix 秒)
    #[serde(default)]
    pub plan_ends_at: Option<i64>,
    /// 完整画像 JSON(含 grants 明细,UI 展开与选号共用)
    #[serde(default)]
    pub plan_profile_json: Option<String>,
    /// 画像同步时间(unix 秒)
    #[serde(default)]
    pub profile_synced_at: Option<i64>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Default)]
pub struct NewAccount {
    pub label: String,
    pub device_mid: String,
    pub user_id: Option<String>,
    pub jwt: String,
    pub refresh_token: String,
    pub user_info: Option<String>,
    pub zai_access_token: Option<String>,
    pub zai_refresh_token: Option<String>,
    /// 新建账号即可确定的邀请归属(注册入池时由调用方传入,通常为 None)
    pub invite_role: Option<String>,
    /// 被邀请者归属的邀请者账号 id
    pub invited_by: Option<i64>,
}

/// 账密管理行:account_id 非空 = 关联已导入账号(title 取账号 label),NULL = 独立录入条目
#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct SecretItem {
    pub id: i64,
    pub account_id: Option<i64>,
    pub title: String,
    pub login_name: String,
    pub password: String,
    pub updated_at: i64,
}
