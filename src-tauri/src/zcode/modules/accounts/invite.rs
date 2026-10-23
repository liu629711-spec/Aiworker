//! 邀请资格与邀请捆绑(逆向自官方奖励中心):
//! - 资格查询:`GET /api/v1/incentive/referral/summary` 的 `data.inviter` 节点,
//!   含 state(active/locked) / invite_code / unlock_at / invited.{total,weekly,reward_cap,capped}
//! - 邀请链接:`https://zcode.z.ai/invite/{CODE}`(8 位码,字符集 2345679ABCDEFGHJKLMNPQRSTUVWXYZ)
//! - 捆绑:`POST /zcode-api/incentive/referral/bind-code`,body `{invite_code}`,
//!   必须由被邀请者持自己的 token 调用,并带阿里云验证码(与 claim 同体系)
//!
//! 说明:邀请落地页 `/invite/{code}` 只把码写进 zcode.z.ai 域名的 sessionStorage,
//! 而注册发生在 chat.z.ai,跨域不互通——所以工具侧不依赖落地页,自己显式调 bind-code。

use crate::zcode::api::ZaiClient;
use crate::zcode::db::{models, repo};
use crate::zcode::error::{AppError, AppResult};
use sqlx::SqlitePool;

/// 邀请码字符集(官方落地页校验用):去掉易混淆的 0/1/I/O
const CODE_ALPHABET: &str = "23456789ABCDEFGHJKLMNPQRSTUVWXYZ";
/// 邀请码长度
pub const CODE_LEN: usize = 8;
/// 邀请落地页基址
pub const INVITE_BASE: &str = "https://zcode.z.ai/invite/";

/// 邀请资格状态:已获得 / 尚未获得(官方按账号年龄判定)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InviteState {
    Active,
    Locked,
}

impl InviteState {
    pub fn as_str(&self) -> &'static str {
        match self {
            InviteState::Active => "active",
            InviteState::Locked => "locked",
        }
    }
}

/// 校验并归一把邀请码:去空白 + 转大写;不合法返回 None。
/// 容错处理用户从链接里手抄、复制到小写、或带上空格的情况。
pub fn normalize_code(raw: &str) -> Option<String> {
    let code: String = raw
        .trim()
        .to_uppercase()
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    if code.len() != CODE_LEN {
        return None;
    }
    if !code.chars().all(|c| CODE_ALPHABET.contains(c)) {
        return None;
    }
    Some(code)
}

/// 从邀请链接或裸码中提取邀请码(兼容 https://zcode.z.ai/invite/XXXX、/cn/invite/XXXX、
/// 带 query/hash、以及直接粘贴码三种输入)。
pub fn extract_code(input: &str) -> Option<String> {
    let s = input.trim();
    if s.is_empty() {
        return None;
    }
    // 含路径段时取 invite/ 后面那一段
    if let Some(idx) = s.find("/invite/") {
        let tail = &s[idx + "/invite/".len()..];
        let code = tail
            .split(|c| c == '/' || c == '?' || c == '#')
            .next()
            .unwrap_or("");
        return normalize_code(code);
    }
    normalize_code(s)
}

/// 构造邀请落地页链接
pub fn invite_link(code: &str) -> Option<String> {
    normalize_code(code).map(|c| format!("{INVITE_BASE}{c}"))
}

/// 从 referral/summary 响应的 `data.inviter` 节点解析邀请码
pub fn parse_inviter_code(summary: &serde_json::Value) -> Option<String> {
    normalize_code(summary["data"]["inviter"]["invite_code"].as_str().unwrap_or(""))
}

/// 解析邀请资格状态;`visible=false` 视为未开放(与官方页面同口径:visible 控制是否展示邀请入口)
pub fn parse_invite_state(summary: &serde_json::Value) -> Option<InviteState> {
    let inviter = &summary["data"]["inviter"];
    let state = inviter["state"].as_str()?;
    match state {
        "active" => Some(InviteState::Active),
        "locked" => Some(InviteState::Locked),
        _ => None,
    }
}

/// 解析预计开放时间(unix 秒;非 locked 或缺失时 None)
pub fn parse_unlock_at(summary: &serde_json::Value) -> Option<i64> {
    summary["data"]["inviter"]["unlock_at"].as_i64()
}

/// 解析已邀请人数(累计)
pub fn parse_invited_count(summary: &serde_json::Value) -> Option<i64> {
    summary["data"]["inviter"]["invited"]["total"].as_i64()
}

/// 本周名额口径(官方实测 2026-10-22):
/// `invited = { total: 2, weekly: 0, reward_cap: 100, capped: false }`
/// - weekly 是本周已邀数,也是分配名额的真实依据(不是 total)
/// - reward_cap 是本期上限(当前 100),不是周末卡门槛 2
/// - capped 由官方直接判定本期是否封顶
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct InviteQuota {
    /// 本周已邀人数(invited.weekly)
    pub weekly: i64,
    /// 本周上限(invited.reward_cap;官方未给时按 100)
    pub cap: i64,
    /// 官方是否已判封顶(invited.capped)
    pub capped: bool,
}

/// 官方未给 reward_cap 时的回退值(实测当前口径)
pub const DEFAULT_INVITE_CAP: i64 = 100;

pub fn parse_invite_quota(summary: &serde_json::Value) -> InviteQuota {
    let invited = &summary["data"]["inviter"]["invited"];
    InviteQuota {
        weekly: invited["weekly"].as_i64().unwrap_or(0),
        cap: invited["reward_cap"]
            .as_i64()
            .filter(|c| *c > 0)
            .unwrap_or(DEFAULT_INVITE_CAP),
        capped: invited["capped"].as_bool().unwrap_or(false),
    }
}

/// 老号侧的邀请奖励卡进度(inviter.rewards 数组按 reward_type 取)
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct InviterRewards {
    /// 夜间蹬蹬卡已发放天数
    pub night_days: i64,
    /// 夜间卡状态(in_progress/achieved/issued)
    pub night_state: Option<&'static str>,
    /// 周末蹬蹬卡本周进度
    pub weekend_progress: i64,
    /// 周末卡目标(官方给 2)
    pub weekend_target: i64,
    /// 周重置卡已用次数(需 Coding Plan 订阅)
    pub weekly_reset_used: i64,
}

/// 解析老号侧的三种邀请奖励卡进度。
/// rewards 数组按 reward_type 匹配;缺失的卡按 0 处理(官方没给就是没有)。
pub fn parse_inviter_rewards(summary: &serde_json::Value) -> InviterRewards {
    let mut out = InviterRewards::default();
    let Some(arr) = summary["data"]["inviter"]["rewards"].as_array() else {
        return out;
    };
    for r in arr {
        let state = r["state"].as_str();
        // issued 是官方已发到账的份数;achieved/in_progress 时用 current 表示进度
        let issued = r["issued"].as_i64().unwrap_or(0);
        let current = r["current"].as_i64().unwrap_or(0);
        match r["reward_type"].as_str() {
            Some("night_card") => {
                out.night_days = issued.max(current);
                out.night_state = match state {
                    Some("in_progress") => Some("in_progress"),
                    Some("achieved") => Some("achieved"),
                    Some("issued") => Some("issued"),
                    _ => None,
                };
            }
            Some("weekend_card") => {
                out.weekend_progress = current;
                out.weekend_target = r["condition"]["min"].as_i64().unwrap_or(2);
            }
            Some("weekly_reset_card") => {
                out.weekly_reset_used = issued;
            }
            _ => {}
        }
    }
    out
}

/// 新号侧的邀请任务完成度(invitee.tasks 三个 key)
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct InviteeTasks {
    /// 登录客户端
    pub login_device: bool,
    /// 绑定邀请码
    pub bind_code: bool,
    /// 发送消息
    pub send_message: bool,
}

impl InviteeTasks {
    /// 三个新手任务是否全部完成(官方据此发夜间蹬蹬卡)
    pub fn all_done(&self) -> bool {
        self.login_device && self.bind_code && self.send_message
    }

    pub fn done_count(&self) -> usize {
        [self.login_device, self.bind_code, self.send_message]
            .iter()
            .filter(|b| **b)
            .count()
    }
}

/// 解析新号侧的任务完成度与整体状态。
/// invitee 节点缺失(未被邀请过)时返回 None。
pub fn parse_invitee_tasks(summary: &serde_json::Value) -> Option<(InviteeTasks, Option<&'static str>)> {
    let invitee = &summary["data"]["invitee"];
    let arr = invitee["tasks"].as_array()?;
    let mut t = InviteeTasks::default();
    for item in arr {
        let done = item["completed"].as_bool().unwrap_or(false);
        match item["key"].as_str() {
            Some("login_device") => t.login_device = done,
            Some("referral_bind_code") => t.bind_code = done,
            Some("send_message") => t.send_message = done,
            _ => {}
        }
    }
    let state = match invitee["state"].as_str() {
        Some("done") => Some("done"),
        Some(s) if !s.is_empty() => Some("pending"),
        _ => None,
    };
    Some((t, state))
}

/// 把 unlock_at 渲染成中文相对时间文案(尚未获得资格时给用户一个预期)。
/// now_sec 传入当前时间戳以便测试;返回 None 表示无解锁时间。
pub fn format_unlock_at(unlock_at: Option<i64>, now_sec: i64) -> Option<String> {
    let ts = unlock_at?;
    if ts <= 0 {
        return None;
    }
    let delta = ts - now_sec;
    if delta <= 0 {
        return Some("即将开放".to_string());
    }
    let mins = delta / 60;
    if mins < 60 {
        return Some(format!("约 {mins} 分钟后"));
    }
    let hours = mins / 60;
    if hours < 24 {
        return Some(format!("约 {hours} 小时后"));
    }
    let days = hours / 24;
    Some(format!("约 {days} 天后"))
}

/// bind-code 的错误码 → 中文消息(逆向自官方奖励中心页面的 i18n 表)。
/// 传 None 表示接口未给出 code(HTTP 层已失败),用兜底文案。
pub fn bind_error_message(code: Option<i64>) -> String {
    match code {
        Some(0) => "捆绑成功".to_string(),
        Some(3007) => "安全验证未通过,请重试(验证码过期?)".to_string(),
        Some(401) | Some(1006) => "登录已失效,请重新导入该账号".to_string(),
        Some(3501) => "邀请活动暂未开放".to_string(),
        Some(3502) => "当前账号不符合参与条件".to_string(),
        Some(3503) => "邀请链接无效,请向主号获取新的邀请码".to_string(),
        Some(3504) => "不能接受自己的邀请".to_string(),
        Some(3505) => "该账号已绑定过邀请,无法重复接受".to_string(),
        Some(3506) => "主号暂未获得邀请资格,当前无法接受此邀请".to_string(),
        Some(3507) => "已超过新用户参与时限,无法接受此邀请".to_string(),
        Some(c) => format!("捆绑失败(官方错误码 {c})"),
        None => "捆绑失败:接口无响应,请稍后重试".to_string(),
    }
}

/// 判定 bind-code 响应是否真的捆绑成功。
/// 官方判定口径:code==0 且 data.bound===true(只有 code==0 但 bound 缺失/为 false
/// 时说明服务端接了请求但没落绑定,仍算失败)。
pub fn is_bind_success(resp: &serde_json::Value) -> bool {
    resp["code"].as_i64() == Some(0) && resp["data"]["bound"].as_bool() == Some(true)
}

/// 一次邀请资格同步的结果摘要
#[derive(serde::Serialize)]
pub struct InviteQualificationResult {
    /// 成功查询到资格的账号数
    pub synced: usize,
    /// 其中已获得资格的(active)
    pub active: usize,
    /// 其中尚未获得资格的(locked)
    pub locked: usize,
    /// 查询失败的账号(凭证失效等)
    pub errors: Vec<String>,
}

/// 逐账号查询邀请资格并落库(官方 referral/summary 的 inviter 节点)。
/// 单账号失败不中断;接口未给字段时保留库内旧值(见 repo::set_invite_qualification)。
pub async fn sync_invite_qualification(
    db: &SqlitePool,
    http: &ZaiClient,
) -> AppResult<InviteQualificationResult> {
    let accounts = repo::list(db).await?;
    let mut result = InviteQualificationResult { synced: 0, active: 0, locked: 0, errors: vec![] };

    for a in &accounts {
        let summary = match http.get_referral_summary(&a.jwt, &a.device_mid).await {
            Ok(v) => v,
            Err(e) => {
                result.errors.push(format!("{}: {e}", a.label));
                continue;
            }
        };
        if summary["code"].as_i64() != Some(0) {
            let msg = summary["msg"].as_str().unwrap_or("接口返回异常");
            result.errors.push(format!("{}: {msg}", a.label));
            continue;
        }

        let code = parse_inviter_code(&summary);
        let state = parse_invite_state(&summary);
        let unlock_at = parse_unlock_at(&summary);
        let invited_count = parse_invited_count(&summary);
        let quota = parse_invite_quota(&summary);
        // active 时清掉解锁时间,避免资格开放后仍显示旧的预计时间
        let unlock_at = if state == Some(InviteState::Active) { None } else { unlock_at };

        repo::set_invite_qualification(
            db,
            a.id,
            code.as_deref(),
            state.map(|s| s.as_str()),
            unlock_at,
            invited_count,
            Some(quota.weekly),
            Some(quota.cap),
            Some(i64::from(quota.capped)),
        )
        .await?;

        // 邀请奖励卡进度(老号侧的三种卡)+ 新号侧任务完成度
        let rw = parse_inviter_rewards(&summary);
        let (task_login, task_bind, task_send, invitee_state) =
            match parse_invitee_tasks(&summary) {
                Some((t, st)) => (
                    Some(i64::from(t.login_device)),
                    Some(i64::from(t.bind_code)),
                    Some(i64::from(t.send_message)),
                    st,
                ),
                None => (None, None, None, None),
            };
        repo::set_invite_rewards(
            db,
            a.id,
            Some(rw.night_days),
            rw.night_state,
            Some(rw.weekend_progress),
            Some(rw.weekend_target),
            Some(rw.weekly_reset_used),
            task_login,
            task_bind,
            task_send,
            invitee_state,
        )
        .await?;
        result.synced += 1;
        match state {
            Some(InviteState::Active) => result.active += 1,
            Some(InviteState::Locked) => result.locked += 1,
            None => {}
        }
    }
    Ok(result)
}

/// 捆绑前的双方状态预检(不发起任何网络请求)。
/// 返回 Ok(()) 表示可以发起捆绑;Err 携带可直接展示给用户的中文原因。
pub fn check_bind_precondition(
    account: &models::Account,
    inviter: &models::Account,
) -> AppResult<()> {
    if account.id == inviter.id {
        return Err(AppError::Other("不能把账号绑定到它自己".into()));
    }
    if account.invite_role.as_deref() == Some("invited") {
        return Err(AppError::Other(
            "该账号已被标记为被邀请者;如需更换主号,请先清除邀请标记".into(),
        ));
    }
    if inviter.invite_state.as_deref() == Some("locked") {
        let hint = format_unlock_at(inviter.invite_unlock_at, chrono::Utc::now().timestamp())
            .map(|t| format!("(预计 {t})"))
            .unwrap_or_default();
        return Err(AppError::Other(format!(
            "主号「{}」尚未获得邀请资格,暂不能邀请{hint}",
            inviter.label
        )));
    }
    let code = match inviter.invite_code.as_deref().and_then(normalize_code) {
        Some(c) => c,
        None => {
            return Err(AppError::Other(format!(
                "主号「{}」没有可用邀请码,请先点「同步资格」刷新",
                inviter.label
            )))
        }
    };
    if let Some(a) = account.invite_code.as_deref().and_then(normalize_code) {
        if a == code {
            return Err(AppError::Other("不能接受自己的邀请".into()));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary_with(state: &str, code: &str) -> serde_json::Value {
        serde_json::json!({
            "code": 0,
            "data": { "inviter": { "state": state, "invite_code": code } }
        })
    }

    #[test]
    fn normalize_code_accepts_official_shape() {
        assert_eq!(normalize_code("URK7W973").as_deref(), Some("URK7W973"));
        // 大小写归一 + 去空白
        assert_eq!(normalize_code(" urk7w973 ").as_deref(), Some("URK7W973"));
    }

    #[test]
    fn normalize_code_rejects_bad_shapes() {
        // 长度不足
        assert!(normalize_code("URK7W97").is_none());
        assert!(normalize_code("URK7W9733").is_none());
        // 空/纯空白
        assert!(normalize_code("").is_none());
        assert!(normalize_code("   ").is_none());
        // 易混淆字符不在字符集内
        assert!(normalize_code("0RK7W973").is_none());
        assert!(normalize_code("IRK7W973").is_none());
        assert!(normalize_code("ORK7W973").is_none());
        assert!(normalize_code("1RK7W973").is_none());
    }

    #[test]
    fn extract_code_handles_links_and_bare() {
        assert_eq!(extract_code("URK7W973").as_deref(), Some("URK7W973"));
        assert_eq!(
            extract_code("https://zcode.z.ai/invite/URK7W973").as_deref(),
            Some("URK7W973")
        );
        // 跟随 307 落地后的 /cn/invite/ 形态
        assert_eq!(
            extract_code("https://zcode.z.ai/cn/invite/URK7W973").as_deref(),
            Some("URK7W973")
        );
        // 带 query / hash
        assert_eq!(
            extract_code("https://zcode.z.ai/invite/URK7W973?from=share").as_deref(),
            Some("URK7W973")
        );
        assert_eq!(
            extract_code("https://zcode.z.ai/invite/URK7W973#top").as_deref(),
            Some("URK7W973")
        );
        // 小写链接
        assert_eq!(
            extract_code("https://zcode.z.ai/invite/urk7w973").as_deref(),
            Some("URK7W973")
        );
        // 无邀请码
        assert!(extract_code("").is_none());
        assert!(extract_code("https://zcode.z.ai/cn").is_none());
        assert!(extract_code("https://zcode.z.ai/invite/ZZZZ").is_none());
    }

    #[test]
    fn invite_link_roundtrip() {
        let link = invite_link("urk7w973").expect("合法码应生成链接");
        assert_eq!(link, "https://zcode.z.ai/invite/URK7W973");
        // 生成 → 提取回环
        assert_eq!(extract_code(&link).as_deref(), Some("URK7W973"));
        assert!(invite_link("bad").is_none());
    }

    #[test]
    fn parse_summary_fields() {        let v = summary_with("active", "URK7W973");
        assert_eq!(parse_inviter_code(&v).as_deref(), Some("URK7W973"));
        assert_eq!(parse_invite_state(&v), Some(InviteState::Active));

        let locked = serde_json::json!({
            "code": 0,
            "data": { "inviter": { "state": "locked", "invite_code": "FVYE3498", "unlock_at": 1792723200,
                                  "invited": { "total": 3, "weekly": 0 } } }
        });
        assert_eq!(parse_invite_state(&locked), Some(InviteState::Locked));
        assert_eq!(parse_unlock_at(&locked), Some(1792723200));
        assert_eq!(parse_invited_count(&locked), Some(3));

        // 未知状态与缺失字段都不猜测
        let weird = serde_json::json!({"code":0,"data":{"inviter":{"state":"whatever"}}});
        assert_eq!(parse_invite_state(&weird), None);
        assert_eq!(parse_inviter_code(&weird), None);
    }

    #[test]
    fn format_unlock_at_text() {
        let now = 1_000_000;
        // 未来:分钟/小时/天三档
        assert_eq!(format_unlock_at(Some(now + 30 * 60), now).as_deref(), Some("约 30 分钟后"));
        assert_eq!(format_unlock_at(Some(now + 3 * 3600), now).as_deref(), Some("约 3 小时后"));
        assert_eq!(format_unlock_at(Some(now + 2 * 86400), now).as_deref(), Some("约 2 天后"));
        // 已过期
        assert_eq!(format_unlock_at(Some(now - 1), now).as_deref(), Some("即将开放"));
        // 缺失/非法值
        assert_eq!(format_unlock_at(None, now), None);
        assert_eq!(format_unlock_at(Some(0), now), None);
    }

    #[test]
    fn bind_error_message_covers_all_codes() {
        assert_eq!(bind_error_message(Some(0)), "捆绑成功");
        assert!(bind_error_message(Some(3007)).contains("安全验证"));
        assert!(bind_error_message(Some(3505)).contains("已绑定过"));
        assert!(bind_error_message(Some(3506)).contains("暂未获得邀请资格"));
        assert!(bind_error_message(Some(3507)).contains("新用户参与时限"));
        // 未知码给出码值便于排查
        assert!(bind_error_message(Some(9999)).contains("9999"));
        // HTTP 层失败(无 code)有兜底
        assert!(bind_error_message(None).contains("接口无响应"));
    }

    #[test]
    fn is_bind_success_requires_code_and_bound() {
        // 官方成功形态
        assert!(is_bind_success(&serde_json::json!({"code":0,"data":{"bound":true}})));
        // code==0 但 bound 缺失/为 false:服务端未落绑定,算失败
        assert!(!is_bind_success(&serde_json::json!({"code":0,"data":{}})));
        assert!(!is_bind_success(&serde_json::json!({"code":0,"data":{"bound":false}})));
        // 错误码
        assert!(!is_bind_success(&serde_json::json!({"code":3505,"msg":"已绑定过"})));
        // 空响应
        assert!(!is_bind_success(&serde_json::json!({})));
    }

    /// 名额口径(形态取自 2026-10-22 真实响应):
    /// weekly 是本周已邀数(不是 total),reward_cap 是本期上限(不是周末卡门槛 2)。
    /// 旧实现把 total=2 当本周已用、cap 写死 2,会误判累计邀过 2 人的主号为"没名额"。
    #[test]
    fn parse_invite_quota_uses_weekly_and_reward_cap() {
        let s = serde_json::json!({
            "code": 0,
            "data": { "inviter": {
                "state": "active", "invite_code": "URK7W973",
                "invited": { "total": 2, "weekly": 0, "reward_cap": 100, "capped": false }
            }}
        });
        let q = parse_invite_quota(&s);
        assert_eq!(q.weekly, 0, "本周已邀应为 weekly,不是 total");
        assert_eq!(q.cap, 100, "上限应为 reward_cap,不是周末卡门槛 2");
        assert!(!q.capped);
        // 剩余名额必须 > 0:累计邀满 2 人不代表本周满了
        assert_eq!(q.cap - q.weekly, 100);

        // 本周真的邀了 3 个:remaining 随 weekly 递减
        let s2 = serde_json::json!({
            "data": { "inviter": { "invited": { "total": 9, "weekly": 3, "reward_cap": 10 } } }
        });
        let q2 = parse_invite_quota(&s2);
        assert_eq!(q2.weekly, 3);
        assert_eq!(q2.cap, 10);
        assert_eq!(q2.cap - q2.weekly, 7);

        // 官方判封顶
        let s3 = serde_json::json!({
            "data": { "inviter": { "invited": { "weekly": 100, "reward_cap": 100, "capped": true } } }
        });
        assert!(parse_invite_quota(&s3).capped);

        // reward_cap 缺失时回退默认上限(不把上限当 0 导致全员没名额)
        let s4 = serde_json::json!({ "data": { "inviter": { "invited": { "total": 5 } } } });
        let q4 = parse_invite_quota(&s4);
        assert_eq!(q4.weekly, 0);
        assert_eq!(q4.cap, DEFAULT_INVITE_CAP);
    }

    /// 老号侧三种邀请奖励卡的解析(形态取自真实接口响应)
    #[test]
    fn parse_inviter_rewards_from_real_shape() {
        let s = serde_json::json!({
            "code": 0,
            "data": { "inviter": {
                "state": "active", "invite_code": "U8W92H4J",
                "invited": { "total": 0, "weekly": 0 },
                "rewards": [
                    { "reward_type": "night_card", "state": "in_progress", "issued": 0,
                      "current": 0, "target": 100 },
                    { "reward_type": "weekend_card", "state": "in_progress", "issued": 0,
                      "current": 0, "target": 2, "condition": { "min": 2 } },
                    { "reward_type": "weekly_reset_card", "state": "in_progress", "issued": 0,
                      "current": 0, "target": 10 }
                ]
            }}
        });
        let r = parse_inviter_rewards(&s);
        assert_eq!(r.night_days, 0);
        assert_eq!(r.night_state, Some("in_progress"));
        assert_eq!(r.weekend_progress, 0);
        assert_eq!(r.weekend_target, 2);
        assert_eq!(r.weekly_reset_used, 0);

        // 已发卡的账号:issued 计入天数,周末卡按 current 计进度
        let s2 = serde_json::json!({
            "code": 0,
            "data": { "inviter": { "rewards": [
                { "reward_type": "night_card", "state": "issued", "issued": 7, "current": 7 },
                { "reward_type": "weekend_card", "state": "achieved", "issued": 1,
                  "current": 2, "condition": { "min": 2 } },
                { "reward_type": "weekly_reset_card", "state": "issued", "issued": 1 }
            ]}}
        });
        let r2 = parse_inviter_rewards(&s2);
        assert_eq!(r2.night_days, 7);
        assert_eq!(r2.night_state, Some("issued"));
        assert_eq!(r2.weekend_progress, 2);
        assert_eq!(r2.weekend_target, 2);
        assert_eq!(r2.weekly_reset_used, 1);

        // rewards 缺失时不猜测,全 0
        let empty = serde_json::json!({"code":0,"data":{"inviter":{"state":"active"}}});
        assert_eq!(parse_inviter_rewards(&empty), InviterRewards::default());
    }

    /// 新号侧三个新手任务的解析
    #[test]
    fn parse_invitee_tasks_from_real_shape() {
        let done = serde_json::json!({
            "code": 0,
            "data": { "invitee": {
                "state": "done",
                "tasks": [
                    { "key": "login_device", "completed": true },
                    { "key": "referral_bind_code", "completed": true },
                    { "key": "send_message", "completed": true }
                ]
            }}
        });
        let (t, st) = parse_invitee_tasks(&done).expect("应解析出任务");
        assert_eq!(st, Some("done"));
        assert!(t.all_done());
        assert_eq!(t.done_count(), 3);

        // 部分完成:还差发消息
        let partial = serde_json::json!({
            "code": 0,
            "data": { "invitee": { "state": "pending", "tasks": [
                { "key": "login_device", "completed": true },
                { "key": "referral_bind_code", "completed": true },
                { "key": "send_message", "completed": false }
            ]}}
        });
        let (t2, st2) = parse_invitee_tasks(&partial).expect("应解析出任务");
        assert_eq!(st2, Some("pending"));
        assert!(!t2.all_done());
        assert_eq!(t2.done_count(), 2);
        assert!(t2.login_device && t2.bind_code && !t2.send_message);

        // 未被邀请过(无 invitee 节点)
        let none = serde_json::json!({"code":0,"data":{"inviter":{"state":"active"}}});
        assert!(parse_invitee_tasks(&none).is_none());
    }

    #[test]
    fn bind_precondition_blocks_bad_pairs() {
        let now = chrono::Utc::now().timestamp();
        // 每个构造出的账号分配唯一 id(默认都取 1 会误触"自己绑自己")
        let mut next_id = 1i64;
        let mut base = |role: Option<&str>, state: Option<&str>, code: Option<&str>| {
            let id = next_id;
            next_id += 1;
            models::Account {
            id,
            label: "x@mozmail.com".into(),
            user_id: None,
            jwt: "j".into(),
            refresh_token: String::new(),
            device_mid: "m".into(),
            status: "stored".into(),
            is_current: false,
            user_info: None,
            zai_access_token: None,
            zai_refresh_token: None,
            invite_role: role.map(str::to_string),
            invited_by: None,
            invite_code: code.map(str::to_string),
            invite_state: state.map(str::to_string),
            invite_unlock_at: None,
            invited_count: None,
            invited_weekly: None,
            invite_cap: None,
            invite_capped: None,
            invite_synced_at: None,
            night_card_days: None,
            night_card_state: None,
            weekend_progress: None,
            weekend_target: None,
            weekly_reset_used: None,
            task_login_device: None,
            task_bind_code: None,
            task_send_message: None,
            invitee_state: None,
            plan_kind: None,
            plan_servable_models: None,
            plan_grant_units: None,
            plan_used_units: None,
            plan_remaining_units: None,
            plan_period: None,
            plan_night_only: None,
            plan_ends_at: None,
            plan_profile_json: None,
            profile_synced_at: None,
            created_at: now,
            updated_at: now,
            }
        };

        let account = base(None, None, None);
        let inviter = base(None, Some("active"), Some("URK7W973"));
        assert!(check_bind_precondition(&account, &inviter).is_ok());

        // 自己绑自己(同一个对象)
        let self_pair = base(None, Some("active"), Some("URK7W973"));
        assert!(check_bind_precondition(&self_pair, &self_pair).is_err());

        // 已是被邀请者
        let already = base(Some("invited"), None, None);
        assert!(check_bind_precondition(&already, &inviter).is_err());

        // 主号尚未获得资格(带预计时间)
        let mut cooling = base(None, Some("locked"), Some("FVYE3498"));
        cooling.invite_unlock_at = Some(now + 7200);
        let err = check_bind_precondition(&account, &cooling).unwrap_err().to_string();
        assert!(err.contains("邀请资格"), "应提示未获得资格: {err}");
        assert!(err.contains("约 2 小时后"), "应给出预计开放时间: {err}");

        // 主号无邀请码
        let nocode = base(None, Some("active"), None);
        assert!(check_bind_precondition(&account, &nocode).is_err());

        // 自己接受自己的码
        let mine = base(None, None, Some("URK7W973"));
        let err = check_bind_precondition(&mine, &inviter).unwrap_err().to_string();
        assert!(err.contains("自己的邀请"), "应拦截自邀请: {err}");
    }
}
