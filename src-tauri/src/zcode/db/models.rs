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
