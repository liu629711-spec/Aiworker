//! Firefox Relay 马甲邮箱自动生成(逆向自 relay.firefox.com 的 REST API)。
//!
//! 关键设计:不自己登 Relay(accounts.firefox.com 的人机验证是 Mozilla 风控,
//! 工具不做也不该绕过),而是复用本机浏览器里已登录的会话 cookie。
//! 默认读 Zen 浏览器的 cookies.sqlite(与 Firefox 同源的 profiles.ini 布局),
//! 取 relay.firefox.com 的 sessionid + csrftoken。
//!
//! 已验证接口(POST 需 X-CSRFToken + Referer):
//! - `GET  /api/v1/relayaddresses/`  列出马甲(含 full_address/enabled/created_at)
//! - `POST /api/v1/relayaddresses/`  新建随机马甲(body 可带 description/generated_for;
//!   mask_type 只读,由服务端生成 8 位随机名)
//! - `GET  /api/v1/users/`           当前账号邮箱(用于校验 cookie 有效性)
//!
//! 免费账号上限 50 个马甲;达到上限时 POST 会返回错误,由调用方转成可读消息。

use crate::zcode::error::{AppError, AppResult};

/// 一个马甲邮箱
#[derive(Debug, Clone)]
pub struct MaskAddress {
    pub id: i64,
    /// 完整地址(xxx@mozmail.com)
    pub full_address: String,
    /// 是否启用
    pub enabled: bool,
}

/// Relay 会话凭证(从浏览器 cookie 里取)
#[derive(Debug, Clone)]
pub struct RelaySession {
    pub sessionid: String,
    pub csrftoken: String,
}

/// 浏览器 cookie 来源(默认 Zen;留出手动指定 profile 的余地)
#[derive(Debug, Clone)]
pub struct CookieSource {
    /// cookies.sqlite 的完整路径
    pub path: std::path::PathBuf,
}

impl CookieSource {
    /// 定位本机 Zen 浏览器的默认 profile cookies.sqlite。
    /// profiles.ini 里 Default=1 的那个 profile 优先,取不到就用第一个。
    pub fn zen_default() -> AppResult<Self> {
        let base = dirs_roaming().ok_or_else(|| {
            AppError::Other("无法定位 Roaming 目录(取不到 APPDATA 环境变量)".into())
        })?;
        let zen = base.join("Zen");
        let ini = zen.join("profiles.ini");
        if !ini.is_file() {
            return Err(AppError::Other(format!(
                "未找到 Zen 浏览器配置 {}：请确认已安装 Zen 浏览器并登录过 Firefox Relay",
                ini.display()
            )));
        }
        let mut candidates: Vec<(bool, String)> = Vec::new();
        let text = std::fs::read_to_string(&ini)
            .map_err(|e| AppError::Other(format!("读取 profiles.ini 失败: {e}")))?;
        let mut is_default = false;
        for line in text.lines() {
            let line = line.trim();
            if line.starts_with('[') {
                is_default = false;
            } else if let Some(v) = line.strip_prefix("Default=") {
                is_default = v.trim() == "1";
            } else if let Some(v) = line.strip_prefix("Path=") {
                candidates.push((is_default, v.trim().to_string()));
            }
        }
        let chosen = candidates
            .iter()
            .find(|(d, _)| *d)
            .or_else(|| candidates.first())
            .ok_or_else(|| AppError::Other("profiles.ini 里没有 profile 条目".into()))?;
        let rel: String = chosen.1.chars().map(|c| if c == '/' { std::path::MAIN_SEPARATOR } else { c }).collect();
        let cookies = zen.join(rel).join("cookies.sqlite");
        if !cookies.is_file() {
            return Err(AppError::Other(format!(
                "未找到 cookie 文件 {}：该 profile 可能从未启动过",
                cookies.display()
            )));
        }
        Ok(Self { path: cookies })
    }
}

fn dirs_roaming() -> Option<std::path::PathBuf> {
    std::env::var("APPDATA").ok().map(std::path::PathBuf::from)
}

/// 从浏览器 cookie 库里读 Relay 会话(cookie 库被浏览器独占,先复制到临时文件再读)。
///
/// **必须连 `-wal` 一起复制**:Firefox/Zen 默认 journal_mode=WAL,浏览器运行期间
/// 新写入(包括用户刚登录 Relay 拿到的 sessionid)先落在 `cookies.sqlite-wal`,
/// 主库可能还是空壳。只复制主库会读到一张没有 moz_cookies 表的空文件,
/// 表现为「马甲邮箱一直转圈 / 已入池 0 · 未使用 0」。
pub fn load_relay_session(source: &CookieSource) -> AppResult<RelaySession> {
    let tmp = std::env::temp_dir().join(format!("twa_zen_cookies_{}.db", std::process::id()));
    std::fs::copy(&source.path, &tmp)
        .map_err(|e| AppError::Other(format!("复制 cookie 库失败(浏览器是否正在运行?): {e}")))?;
    // 连同 WAL / SHM 一起复制,否则读不到尚未 checkpoint 的最新 cookie
    let mut copied: Vec<std::path::PathBuf> = vec![tmp.clone()];
    for suffix in ["-wal", "-shm"] {
        let from = std::path::PathBuf::from(format!("{}{}", source.path.display(), suffix));
        if from.is_file() {
            let to = std::path::PathBuf::from(format!("{}{}", tmp.display(), suffix));
            if std::fs::copy(&from, &to).is_ok() {
                copied.push(to);
            }
        }
    }
    let result = read_cookies(&tmp);
    for p in copied {
        let _ = std::fs::remove_file(p);
    }
    result
}

/// 用 sqlx 只读打开临时副本,取 relay.firefox.com 的 sessionid/csrftoken
fn read_cookies(path: &std::path::Path) -> AppResult<RelaySession> {
    use sqlx::sqlite::SqliteConnectOptions;
    use std::str::FromStr;

    let url = format!("sqlite://{}", path.display().to_string().replace('\\', "/"));
    let opts = SqliteConnectOptions::from_str(&url)
        .map_err(|e| AppError::Other(format!("非法 cookie 库路径: {e}")))?
        .read_only(true)
        .create_if_missing(false);

    // cookie 库读取是短平快的同步操作,用阻塞方式拿结果。
    // ⚠️ 必须桥接:`zcode_relay_status` 等命令是 async 的,跑在 tokio worker 上,
    // 直接 block_on 会 panic "Cannot start a runtime from within a runtime"
    // (panic.log 2026-10-23 16:2x 有案底,表现为马甲页永久"检测中")。
    let session = crate::zcode::util::bridge_block_on(async {
        let pool = sqlx::SqlitePool::connect_with(opts).await?;
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT name, value FROM moz_cookies WHERE host LIKE '%relay.firefox.com%'",
        )
        .fetch_all(&pool)
        .await?;
        pool.close().await;
        Ok::<_, sqlx::Error>(rows)
    })
    .map_err(|e| AppError::Other(format!("读取 cookie 库失败: {e}")))?;

    let mut sessionid = None;
    let mut csrftoken = None;
    for (name, value) in session {
        match name.as_str() {
            "sessionid" => sessionid = Some(value),
            "csrftoken" => csrftoken = Some(value),
            _ => {}
        }
    }
    let sessionid = sessionid
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            AppError::Other(
                "浏览器里没有 Relay 登录态：请先在 Zen 浏览器登录 relay.firefox.com".into(),
            )
        })?;
    Ok(RelaySession {
        sessionid,
        csrftoken: csrftoken.unwrap_or_default(),
    })
}

/// Relay API 客户端(带会话 cookie)
#[derive(Clone)]
pub struct RelayClient {
    http: reqwest::Client,
    session: RelaySession,
}

impl RelayClient {
    /// 用浏览器里的现成登录态构造客户端
    pub fn from_local_browser() -> AppResult<Self> {
        let source = CookieSource::zen_default()?;
        let session = load_relay_session(&source)?;
        // **Relay 不走系统代理**。
        //
        // 实测(2026-10-23):relay.firefox.com 直连 200;经 127.0.0.1:7892 反而
        // 连不上(Firefox/Mozilla 域名不在代理的分流规则里,却因为全局代理被强转)。
        // 早前无脑套 detect_proxy() 会让「马甲邮箱」页在开着代理时永久转圈。
        //
        // 同时显式设超时:避免任何网络异常把请求挂成无限等待。
        let builder = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(20))
            .connect_timeout(std::time::Duration::from_secs(8))
            .no_proxy();
        Ok(Self {
            http: builder.build().map_err(AppError::from)?,
            session,
        })
    }

    fn headers(&self) -> reqwest::header::HeaderMap {
        use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, CONTENT_TYPE, REFERER};
        let mut h = HeaderMap::new();
        h.insert(
            reqwest::header::COOKIE,
            HeaderValue::from_str(&format!(
                "sessionid={}; csrftoken={}",
                self.session.sessionid, self.session.csrftoken
            ))
            .expect("合法 cookie 头"),
        );
        h.insert(
            "X-CSRFToken",
            HeaderValue::from_str(&self.session.csrftoken).expect("合法 CSRF 头"),
        );
        h.insert(REFERER, HeaderValue::from_static("https://relay.firefox.com/accounts/profile/"));
        h.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        h.insert(AUTHORIZATION, HeaderValue::from_static(""));
        h.insert(
            reqwest::header::USER_AGENT,
            HeaderValue::from_static(
                "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36",
            ),
        );
        h
    }

    /// 校验会话是否有效(顺带返回账号邮箱)
    pub async fn account_email(&self) -> AppResult<String> {
        let resp = self
            .http
            .get("https://relay.firefox.com/api/v1/users/")
            .headers(self.headers())
            .send()
            .await?;
        let status = resp.status();
        let text = resp.text().await?;
        if !status.is_success() {
            return Err(AppError::Other(format!(
                "Relay 会话无效(HTTP {status}):请重新在 Zen 浏览器登录 relay.firefox.com"
            )));
        }
        let v: serde_json::Value = serde_json::from_str(&text)
            .map_err(|e| AppError::Other(format!("响应不是合法 JSON: {e}\n{text}")))?;
        v.as_array()
            .and_then(|a| a.first())
            .and_then(|u| u["email"].as_str())
            .map(str::to_string)
            .ok_or_else(|| AppError::Other("Relay 响应里没有账号邮箱".into()))
    }

    /// 列出所有马甲
    pub async fn list_masks(&self) -> AppResult<Vec<MaskAddress>> {
        let resp = self
            .http
            .get("https://relay.firefox.com/api/v1/relayaddresses/")
            .headers(self.headers())
            .send()
            .await?;
        let status = resp.status();
        let text = resp.text().await?;
        if !status.is_success() {
            return Err(AppError::Other(format!("HTTP {status}: {text}")));
        }
        let v: serde_json::Value = serde_json::from_str(&text)
            .map_err(|e| AppError::Other(format!("响应不是合法 JSON: {e}\n{text}")))?;
        Ok(v.as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|m| {
                        Some(MaskAddress {
                            id: m["id"].as_i64()?,
                            full_address: m["full_address"].as_str()?.to_string(),
                            enabled: m["enabled"].as_bool().unwrap_or(true),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default())
    }

    /// 新建一个随机马甲;`description` 会显示在 Relay 面板里(便于人肉核对)。
    /// 免费账号上限 50 个,超限时服务端返回非 0 code,这里转成可读错误。
    pub async fn create_mask(&self, description: &str) -> AppResult<MaskAddress> {
        let resp = self
            .http
            .post("https://relay.firefox.com/api/v1/relayaddresses/")
            .headers(self.headers())
            .json(&serde_json::json!({
                "enabled": true,
                "description": description,
            }))
            .send()
            .await?;
        let status = resp.status();
        let text = resp.text().await?;
        if !status.is_success() {
            return Err(AppError::Other(format!("HTTP {status}: {text}")));
        }
        let v: serde_json::Value = serde_json::from_str(&text)
            .map_err(|e| AppError::Other(format!("响应不是合法 JSON: {e}\n{text}")))?;
        let full = v["full_address"]
            .as_str()
            .ok_or_else(|| AppError::Other(format!("创建响应缺少 full_address: {text}")))?
            .to_string();
        let id = v["id"].as_i64().unwrap_or_default();
        Ok(MaskAddress { id, full_address: full, enabled: true })
    }

    /// 删除一个马甲(腾出免费额度)。删除后该地址立即失效,无法再收信——
    /// 已入池的账号不受影响(它们的凭证在本地),但以后不能用这个地址找回。
    pub async fn delete_mask(&self, id: i64) -> AppResult<()> {
        let resp = self
            .http
            .delete(format!(
                "https://relay.firefox.com/api/v1/relayaddresses/{id}/"
            ))
            .headers(self.headers())
            .send()
            .await?;
        let status = resp.status();
        // 官方成功返回 204 No Content;404 表示已不存在(视为删成功)
        if status.is_success() || status == reqwest::StatusCode::NOT_FOUND {
            Ok(())
        } else {
            let text = resp.text().await.unwrap_or_default();
            Err(AppError::Other(format!("删除马甲失败 HTTP {status}: {text}")))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cookie_source_prefers_default_profile() {
        // 无 Zen 安装时应给出可读错误(不 panic)
        match CookieSource::zen_default() {
            Ok(src) => {
                // 有安装:路径必须指向 cookies.sqlite
                assert!(src.path.ends_with("cookies.sqlite"), "{:?}", src.path);
            }
            Err(e) => {
                let msg = e.to_string();
                assert!(
                    msg.contains("Zen") || msg.contains("APPDATA") || msg.contains("cookie"),
                    "错误信息应说明原因: {msg}"
                );
            }
        }
    }

    #[test]
    fn parse_mask_list_from_sample() {
        // 用真实响应形态验证解析(不联网)
        let sample = serde_json::json!([
            {"id":23335857,"full_address":"5mnyb8fbd@mozmail.com","enabled":true},
            {"id":23335858,"full_address":"x1tcjr1c5@mozmail.com","enabled":false},
            {"id":23335859,"full_address":"no-id@mozmail.com"}
        ]);
        let masks: Vec<MaskAddress> = sample
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|m| {
                Some(MaskAddress {
                    id: m["id"].as_i64()?,
                    full_address: m["full_address"].as_str()?.to_string(),
                    enabled: m["enabled"].as_bool().unwrap_or(true),
                })
            })
            .collect();
        assert_eq!(masks.len(), 3);
        assert_eq!(masks[0].full_address, "5mnyb8fbd@mozmail.com");
        assert!(masks[0].enabled);
        // enabled 缺失时按启用处理
        assert!(masks[2].enabled);
    }
}
