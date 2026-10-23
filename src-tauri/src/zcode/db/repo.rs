use sqlx::SqlitePool;

use super::models::{Account, NewAccount, SecretItem};

pub async fn list(db: &SqlitePool) -> sqlx::Result<Vec<Account>> {
    sqlx::query_as::<_, Account>("SELECT * FROM accounts ORDER BY id")
        .fetch_all(db)
        .await
}

pub async fn fetch_by_id(db: &SqlitePool, id: i64) -> sqlx::Result<Option<Account>> {
    sqlx::query_as::<_, Account>("SELECT * FROM accounts WHERE id = ?1")
        .bind(id)
        .fetch_optional(db)
        .await
}

pub async fn insert(db: &SqlitePool, new: &NewAccount) -> sqlx::Result<Account> {
    let now = chrono::Utc::now().timestamp();
    let result = sqlx::query(
        "INSERT INTO accounts (label, device_mid, user_id, jwt, refresh_token, user_info, zai_access_token, zai_refresh_token, invite_role, invited_by, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
    )
    .bind(&new.label)
    .bind(&new.device_mid)
    .bind(&new.user_id)
    .bind(&new.jwt)
    .bind(&new.refresh_token)
    .bind(&new.user_info)
    .bind(&new.zai_access_token)
    .bind(&new.zai_refresh_token)
    .bind(&new.invite_role)
    .bind(&new.invited_by)
    .bind(now)
    .bind(now)
    .execute(db)
    .await?;
    fetch_by_id(db, result.last_insert_rowid())
        .await
        .map(|a| a.expect("刚插入的行必定存在"))
}

/// 重复导入同一账号时更新凭证(JWT 可能已刷新);身份字段仅在拿到新值时覆盖
pub async fn update_credentials(
    db: &SqlitePool,
    id: i64,
    jwt: &str,
    refresh_token: &str,
    user_id: Option<&str>,
    user_info: Option<&str>,
    zai_access_token: Option<&str>,
    zai_refresh_token: Option<&str>,
) -> sqlx::Result<Account> {
    let now = chrono::Utc::now().timestamp();
    sqlx::query(
        "UPDATE accounts SET jwt = ?1, refresh_token = ?2, user_id = COALESCE(?3, user_id),
         user_info = COALESCE(?4, user_info), zai_access_token = COALESCE(?5, zai_access_token),
         zai_refresh_token = COALESCE(?6, zai_refresh_token), updated_at = ?7
         WHERE id = ?8",
    )
    .bind(jwt)
    .bind(refresh_token)
    .bind(user_id)
    .bind(user_info)
    .bind(zai_access_token)
    .bind(zai_refresh_token)
    .bind(now)
    .bind(id)
    .execute(db)
    .await?;
    fetch_by_id(db, id)
        .await
        .map(|a| a.expect("待更新的行必定存在"))
}

pub async fn delete(db: &SqlitePool, id: i64) -> sqlx::Result<u64> {
    let result = sqlx::query("DELETE FROM accounts WHERE id = ?1")
        .bind(id)
        .execute(db)
        .await?;
    Ok(result.rows_affected())
}

/// 设置账号的邀请关系标识(inviter/invited/None=清除);invited_by 仅对被邀请者有意义
pub async fn set_invite_role(
    db: &SqlitePool,
    id: i64,
    role: Option<&str>,
    invited_by: Option<i64>,
) -> sqlx::Result<u64> {
    let result =
        sqlx::query("UPDATE accounts SET invite_role = ?1, invited_by = ?2, updated_at = ?3 WHERE id = ?4")
            .bind(role)
            .bind(invited_by)
            .bind(chrono::Utc::now().timestamp())
            .bind(id)
            .execute(db)
            .await?;
    Ok(result.rows_affected())
}

/// 写入一次邀请资格查询结果(官方 referral/summary 的 inviter 节点)。
/// code/state 为 None 表示接口未给出该字段(保留库内旧值,避免把有效码擦掉);
/// unlock_at 显式传入以支持"资格到期后从 active 回落 locked"的刷新。
/// 名额三件套(weekly/cap/capped)为 None 时保留旧值。
pub async fn set_invite_qualification(
    db: &SqlitePool,
    id: i64,
    code: Option<&str>,
    state: Option<&str>,
    unlock_at: Option<i64>,
    invited_count: Option<i64>,
    invited_weekly: Option<i64>,
    invite_cap: Option<i64>,
    invite_capped: Option<i64>,
) -> sqlx::Result<u64> {
    let result = sqlx::query(
        "UPDATE accounts SET invite_code = COALESCE(?1, invite_code),
         invite_state = COALESCE(?2, invite_state), invite_unlock_at = ?3,
         invited_count = COALESCE(?4, invited_count),
         invited_weekly = COALESCE(?5, invited_weekly),
         invite_cap = COALESCE(?6, invite_cap),
         invite_capped = COALESCE(?7, invite_capped),
         invite_synced_at = ?8,
         updated_at = ?8 WHERE id = ?9",
    )
    .bind(code)
    .bind(state)
    .bind(unlock_at)
    .bind(invited_count)
    .bind(invited_weekly)
    .bind(invite_cap)
    .bind(invite_capped)
    .bind(chrono::Utc::now().timestamp())
    .bind(id)
    .execute(db)
    .await?;
    Ok(result.rows_affected())
}

/// 写入邀请奖励卡进度(官方 inviter.rewards 与 invitee.tasks/invitee.state)。
/// 与 set_invite_qualification 同批调用,一次同步把邀请档案刷全。
/// 任务类字段传 None 表示接口未给出(保留旧值);0/1 是明确的完成态。
pub async fn set_invite_rewards(
    db: &SqlitePool,
    id: i64,
    night_days: Option<i64>,
    night_state: Option<&str>,
    weekend_progress: Option<i64>,
    weekend_target: Option<i64>,
    weekly_reset_used: Option<i64>,
    task_login_device: Option<i64>,
    task_bind_code: Option<i64>,
    task_send_message: Option<i64>,
    invitee_state: Option<&str>,
) -> sqlx::Result<u64> {
    let result = sqlx::query(
        "UPDATE accounts SET night_card_days = COALESCE(?1, night_card_days),
         night_card_state = COALESCE(?2, night_card_state),
         weekend_progress = COALESCE(?3, weekend_progress),
         weekend_target = COALESCE(?4, weekend_target),
         weekly_reset_used = COALESCE(?5, weekly_reset_used),
         task_login_device = COALESCE(?6, task_login_device),
         task_bind_code = COALESCE(?7, task_bind_code),
         task_send_message = COALESCE(?8, task_send_message),
         invitee_state = COALESCE(?9, invitee_state),
         updated_at = ?10 WHERE id = ?11",
    )
    .bind(night_days)
    .bind(night_state)
    .bind(weekend_progress)
    .bind(weekend_target)
    .bind(weekly_reset_used)
    .bind(task_login_device)
    .bind(task_bind_code)
    .bind(task_send_message)
    .bind(invitee_state)
    .bind(chrono::Utc::now().timestamp())
    .bind(id)
    .execute(db)
    .await?;
    Ok(result.rows_affected())
}

/// 写入一次套餐画像(由 `zcode::plan_profile::PlanProfile` 归一化后落库)。
///
/// 语义:`profile` 为 None 表示本次拉取失败——**保留旧画像**,只在图片确实为空时
/// 才说明"这账号真没套餐"。这样接口抖动不会把可用账号从池子里踢掉。
pub async fn set_plan_profile(
    db: &SqlitePool,
    id: i64,
    profile: Option<&crate::zcode::plan_profile::PlanProfile>,
) -> sqlx::Result<u64> {
    let Some(p) = profile else {
        return Ok(0);
    };
    let now = chrono::Utc::now().timestamp();
    let json = serde_json::to_string(p).unwrap_or_default();
    let result = sqlx::query(
        "UPDATE accounts SET plan_kind = ?1,
         plan_servable_models = ?2,
         plan_grant_units = ?3,
         plan_used_units = ?4,
         plan_remaining_units = ?5,
         plan_period = ?6,
         plan_night_only = ?7,
         plan_ends_at = ?8,
         plan_profile_json = ?9,
         profile_synced_at = ?10,
         updated_at = ?10 WHERE id = ?11",
    )
    .bind(p.kind_tag())
    .bind(p.servable_models_str())
    .bind(p.grant_units)
    .bind(p.used_units)
    .bind(p.remaining_units)
    .bind(&p.quota_period)
    .bind(if p.night_only { 1i64 } else { 0i64 })
    .bind(p.ends_at)
    .bind(json)
    .bind(now)
    .bind(id)
    .execute(db)
    .await?;
    Ok(result.rows_affected())
}

/// 账密管理:全部条目(先给每个已导入账号补一行空记录,保证账号必在列)。
/// title 对关联账号取账号 label,独立条目取自录入时的名称。
pub async fn secrets_list(db: &SqlitePool) -> sqlx::Result<Vec<SecretItem>> {
    sqlx::query("INSERT OR IGNORE INTO secret_entries (account_id, title, login_name, password, updated_at) SELECT id, '', '', '', 0 FROM accounts")
        .execute(db)
        .await?;
    sqlx::query_as(
        "SELECT e.id, e.account_id, COALESCE(a.label, e.title) AS title,
                e.login_name, e.password, e.updated_at
         FROM secret_entries e LEFT JOIN accounts a ON a.id = e.account_id
         ORDER BY (e.account_id IS NULL), e.id",
    )
    .fetch_all(db)
    .await
}

/// 保存某条账密(登录名/密码;title 供独立条目改名,关联账号的展示名固定为账号 label)
pub async fn secrets_save(
    db: &SqlitePool,
    id: i64,
    title: &str,
    login_name: &str,
    password: &str,
) -> sqlx::Result<()> {
    sqlx::query(
        "UPDATE secret_entries
         SET title = CASE WHEN account_id IS NULL THEN ?2 ELSE title END,
             login_name = ?3, password = ?4, updated_at = ?5
         WHERE id = ?1",
    )
    .bind(id)
    .bind(title)
    .bind(login_name)
    .bind(password)
    .bind(chrono::Utc::now().timestamp())
    .execute(db)
    .await?;
    Ok(())
}

/// 新增一条独立账密(不关联已导入账号),返回新行 id
pub async fn secrets_add(
    db: &SqlitePool,
    title: &str,
    login_name: &str,
    password: &str,
) -> sqlx::Result<i64> {
    let result = sqlx::query(
        "INSERT INTO secret_entries (account_id, title, login_name, password, updated_at)
         VALUES (NULL, ?1, ?2, ?3, ?4)",
    )
    .bind(title)
    .bind(login_name)
    .bind(password)
    .bind(chrono::Utc::now().timestamp())
    .execute(db)
    .await?;
    Ok(result.last_insert_rowid())
}

/// 删除独立账密条目(关联账号的行随账号存亡,不在此删)
pub async fn secrets_delete(db: &SqlitePool, id: i64) -> sqlx::Result<u64> {
    let result = sqlx::query("DELETE FROM secret_entries WHERE id = ?1 AND account_id IS NULL")
        .bind(id)
        .execute(db)
        .await?;
    Ok(result.rows_affected())
}

/// 列出"尚未注册(未入池)"的马甲邮箱:CSV/手工录入的独立条目,其 login_name
/// 既不是任何账号的 label,也没出现在账号 user_info 的 email 里。
/// 供自动注册面板优先复用现成的马甲邮箱(不必手输)。
pub async fn unused_mask_emails(db: &SqlitePool) -> sqlx::Result<Vec<(i64, String, String)>> {
    let rows: Vec<(i64, String, String)> = sqlx::query_as(
        "SELECT e.id, e.login_name, e.password
         FROM secret_entries e
         WHERE e.account_id IS NULL
           AND e.login_name <> ''
           AND e.login_name NOT IN (SELECT label FROM accounts WHERE label <> '')
           AND NOT EXISTS (
               SELECT 1 FROM accounts a
               WHERE a.user_info LIKE '%\"email\":\"' || e.login_name || '\"%'
                  OR a.user_info LIKE '%\"email\": \"' || e.login_name || '\"%'
           )
         ORDER BY e.id",
    )
    .fetch_all(db)
    .await?;
    Ok(rows)
}

/// 读取某条账密的登录名与密码(批量登录用)
pub async fn secrets_get_credentials(
    db: &SqlitePool,
    id: i64,
) -> sqlx::Result<Option<(String, String)>> {
    let row: Option<(String, String)> =
        sqlx::query_as("SELECT login_name, password FROM secret_entries WHERE id = ?1")
            .bind(id)
            .fetch_optional(db)
            .await?;
    Ok(row)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::SqliteConnectOptions;
    use std::str::FromStr;

    /// 临时诊断:打印账号表的切换与邀请字段(只读)。
    /// 读当前应用的 zcode.sqlite(不是旧版 com.abellee.zaiclaim 库——
    /// 那个库 schema 停留在早期,没有后续迁移加的列)。
    #[tokio::test]
    async fn dump_accounts_switch_fields() {
        let dir = std::env::var("APPDATA").unwrap();
        let db_path = format!(
            "{}/com.aiwork.studio/zcode.sqlite",
            dir.replace(std::path::MAIN_SEPARATOR, "/")
        );
        let opts = SqliteConnectOptions::from_str(&db_path)
            .unwrap()
            .read_only(true);
        let pool = SqlitePool::connect_with(opts).await.unwrap();
        // 迁移只在应用启动时跑;若当前进程没重启过,库可能还缺后续加的列。
        // read_only 连接不能 ALTER,所以按 PRAGMA 实际存在的列动态拼 SELECT——
        // 缺的列用 NULL 占位(与 Account 结构对齐),以后加新列也不用改这里。
        let existing: Vec<String> =
            sqlx::query_scalar("SELECT name FROM pragma_table_info('accounts')")
                .fetch_all(&pool)
                .await
                .unwrap_or_default();
        let pick = |name: &str| {
            if existing.iter().any(|c| c == name) {
                name.to_string()
            } else {
                format!("NULL AS {name}")
            }
        };
        let cols = [
            "id", "label", "user_id", "jwt", "refresh_token", "device_mid", "status", "is_current",
            "user_info", "zai_access_token", "zai_refresh_token", "invite_role", "invited_by",
            "invite_code", "invite_state", "invite_unlock_at", "invited_count", "invite_synced_at",
            "night_card_days", "night_card_state", "weekend_progress", "weekend_target",
            "weekly_reset_used", "task_login_device", "task_bind_code", "task_send_message",
            "invitee_state", "invited_weekly", "invite_cap", "invite_capped",
            "plan_kind", "plan_servable_models", "plan_grant_units", "plan_used_units",
            "plan_remaining_units", "plan_period", "plan_night_only", "plan_ends_at",
            "plan_profile_json", "profile_synced_at",
            "created_at", "updated_at",
        ]
        .iter()
        .map(|c| pick(c))
        .collect::<Vec<_>>()
        .join(", ");
        let rows = sqlx::query_as::<_, Account>(&format!("SELECT {cols} FROM accounts ORDER BY id"))
            .fetch_all(&pool)
            .await
            .unwrap();
        for a in rows {
            println!(
                "#{} {} | user_id={:?} | jwt_uid={:?} | user_info={} | access={} | refresh={} | role={:?} | code={:?} | state={:?}",
                a.id,
                a.label,
                a.user_id.as_deref().unwrap_or("NULL"),
                a.jwt.split('.').nth(1).map(|p| {
                    use base64::Engine;
                    base64::engine::general_purpose::URL_SAFE_NO_PAD
                        .decode(p.trim_end_matches('='))
                        .ok()
                        .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
                        .and_then(|v| v["user_id"].as_str().map(String::from))
                        .unwrap_or("?".into())
                }).unwrap_or("?".into()),
                a.user_info.as_ref().map_or("NULL", |s| if s.is_empty() { "空" } else { "有" }),
                a.zai_access_token.as_ref().map_or("NULL", |s| if s.is_empty() { "空" } else { "有" }),
                a.zai_refresh_token.as_ref().map_or("NULL", |s| if s.is_empty() { "空" } else { "有" }),
                a.invite_role,
                a.invite_code,
                a.invite_state,
            );
        }
    }
}
