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
        "INSERT INTO accounts (label, device_mid, user_id, jwt, refresh_token, user_info, zai_access_token, zai_refresh_token, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
    )
    .bind(&new.label)
    .bind(&new.device_mid)
    .bind(&new.user_id)
    .bind(&new.jwt)
    .bind(&new.refresh_token)
    .bind(&new.user_info)
    .bind(&new.zai_access_token)
    .bind(&new.zai_refresh_token)
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

    /// 临时诊断:打印账号表的切换字段(只读)
    #[tokio::test]
    async fn dump_accounts_switch_fields() {
        let dir = std::env::var("APPDATA").unwrap();
        let db_path = format!(
            "{}/com.abellee.zaiclaim/zai-claim.db",
            dir.replace(std::path::MAIN_SEPARATOR, "/")
        );
        let opts = SqliteConnectOptions::from_str(&db_path)
            .unwrap()
            .read_only(true);
        let pool = SqlitePool::connect_with(opts).await.unwrap();
        let rows = sqlx::query_as::<_, Account>("SELECT * FROM accounts ORDER BY id")
            .fetch_all(&pool)
            .await
            .unwrap();
        for a in rows {
            println!(
                "#{} {} | user_id={:?} | jwt_uid={:?} | user_info={} | access={} | refresh={}",
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
            );
        }
    }
}
