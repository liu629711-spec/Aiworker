use crate::zcode::error::{AppError, AppResult};
use reqwest::header::{HeaderMap, HeaderValue, InvalidHeaderValue};
use reqwest::{Client, Method, Url};
use serde_json::Value;

use super::{APP_VERSION, BASE_URL, PLATFORM};

/// URL 路径段编码
fn urlencode_component(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// z.ai API 客户端。
/// 预置 ZCode 客户端同款请求头(缺失会被服务端 400 code=3001 拒绝,见 HANDOFF §3.2);
/// 账号相关的 Authorization 与 X-Device-Mid 按每次请求传入。
///
/// 所有请求均为 async,由 tokio 调度,不占用 UI 线程。
#[derive(Clone)]
pub struct ZaiClient {
    http: Client,
    base: Url,
}

impl ZaiClient {
    pub fn new() -> AppResult<Self> {
        fn header(v: &str) -> Result<HeaderValue, AppError> {
            v.parse()
                .map_err(|e: InvalidHeaderValue| AppError::Other(format!("非法请求头值: {e}")))
        }

        let mut headers = HeaderMap::new();
        headers.insert("user-agent", header(&format!("ZCode/{APP_VERSION}"))?);
        headers.insert("x-zcode-app-version", header(APP_VERSION)?);
        headers.insert("x-platform", header(PLATFORM)?);
        headers.insert("x-os-category", header("windows")?);
        headers.insert("x-client-language", header("zh-CN")?);
        headers.insert("x-client-timezone", header("Asia/Shanghai")?);
        headers.insert("http-referer", header("https://zcode.z.ai")?);
        headers.insert("x-title", header("Z Code@electron")?);

        let http = Client::builder().default_headers(headers).build()?;
        let base = BASE_URL
            .parse()
            .map_err(|e| AppError::Other(format!("非法 base url: {e}")))?;
        Ok(Self { http, base })
    }

    /// 统一请求入口:带账号凭证与可选扩展头,非 2xx 时把响应体并入错误信息便于排查。
    #[allow(clippy::too_many_arguments)]
    async fn request_with(
        &self,
        method: Method,
        path_and_query: &str,
        jwt: &str,
        device_mid: &str,
        extra_headers: &[(&str, String)],
        body: Option<Value>,
    ) -> AppResult<Value> {
        let url = self
            .base
            .join(path_and_query)
            .map_err(|e| AppError::Other(format!("非法请求路径: {e}")))?;
        let mut req = self
            .http
            .request(method, url)
            .bearer_auth(jwt)
            .header("x-device-mid", device_mid);
        for (name, value) in extra_headers {
            req = req.header(*name, value);
        }
        if let Some(b) = body {
            req = req.json(&b);
        }
        let resp = req.send().await?;

        let status = resp.status();
        let text = resp.text().await?;
        if !status.is_success() {
            return Err(AppError::Other(format!("HTTP {status}: {text}")));
        }
        serde_json::from_str(&text)
            .map_err(|e| AppError::Other(format!("响应不是合法 JSON: {e}\n{text}")))
    }

    async fn request(&self, method: Method, path_and_query: &str, jwt: &str, device_mid: &str) -> AppResult<Value> {
        self.request_with(method, path_and_query, jwt, device_mid, &[], None)
            .await
    }

    /// 每日额度与用量(HANDOFF §3.2 balance)
    pub async fn get_balance(&self, jwt: &str, device_mid: &str) -> AppResult<Value> {
        self.request(
            Method::GET,
            &format!("/api/v1/zcode-plan/billing/balance?app_version={APP_VERSION}"),
            jwt,
            device_mid,
        )
        .await
    }

    /// 当前 claimable 活动计划(HANDOFF §3.2 preview;data.plans 非空即有活动)
    pub async fn get_preview(&self, jwt: &str, device_mid: &str) -> AppResult<Value> {
        self.request(
            Method::GET,
            &format!(
                "/api/v1/zcode-plan/billing/preview?app_version={APP_VERSION}&platform={PLATFORM}"
            ),
            jwt,
            device_mid,
        )
        .await
    }

    /// 邀请活动总览(incentive/referral/summary;inviter 节点含邀请码与邀请总数)
    pub async fn get_referral_summary(&self, jwt: &str, device_mid: &str) -> AppResult<Value> {
        self.request(Method::GET, "/api/v1/incentive/referral/summary", jwt, device_mid)
            .await
    }

    /// 邀请记录列表(被邀请者;user_id 打码为前2+***+后2;cursor 翻页)
    pub async fn get_referral_invitees(
        &self,
        jwt: &str,
        device_mid: &str,
        cursor: Option<&str>,
    ) -> AppResult<Value> {
        let cursor_q = cursor
            .map(|c| format!("&cursor={c}"))
            .unwrap_or_default();
        self.request(
            Method::GET,
            &format!("/api/v1/incentive/referral/invitees?limit=50{cursor_q}"),
            jwt,
            device_mid,
        )
        .await
    }

    /// 客户端配置(含阿里云验证码参数 prefix/region/sceneId,HANDOFF §3.2)
    pub async fn get_client_configs(&self, jwt: &str, device_mid: &str) -> AppResult<Value> {
        self.request(
            Method::GET,
            &format!("/api/v1/client/configs?app_version={APP_VERSION}"),
            jwt,
            device_mid,
        )
        .await
    }

    /// 领取活动。必须带 X-Aliyun-Captcha-Verify-Param(浏览器完成验证码后产出)。
    pub async fn post_claim(
        &self,
        jwt: &str,
        device_mid: &str,
        plan_id: &str,
        captcha_verify_param: &str,
        region: Option<&str>,
    ) -> AppResult<Value> {
        let mut headers = vec![(
            "x-aliyun-captcha-verify-param",
            captcha_verify_param.to_string(),
        )];
        if let Some(r) = region {
            headers.push(("x-aliyun-captcha-verify-region", r.to_string()));
        }
        self.request_with(
            Method::POST,
            "/api/v1/zcode-plan/billing/claim",
            jwt,
            device_mid,
            &headers,
            Some(serde_json::json!({ "plan_id": plan_id })),
        )
        .await
    }

    /// OAuth 轮询登录:初始化会话(逆向自客户端 startOAuthWithPolling)。
    /// poll_token 为本地自生成的 64 位 hex,同时充当后续轮询的 Bearer 凭证。
    /// 响应 data: {flow_id, authorize_url, expires_at, poll_interval_sec}
    pub async fn init_oauth_polling(&self, poll_token: &str) -> AppResult<Value> {
        let url = self
            .base
            .join("/api/v1/oauth/cli/init")
            .map_err(|e| AppError::Other(format!("非法请求路径: {e}")))?;
        let resp = self
            .http
            .post(url)
            .bearer_auth(poll_token)
            .json(&serde_json::json!({ "provider": "zai" }))
            .send()
            .await?;
        Self::text_to_value(resp).await
    }

    /// OAuth 轮询登录:查询会话状态。data.status = pending | ready | failed;
    /// ready 时 data 含 token(zcodejwttoken)/ zai.access_token / user。
    pub async fn poll_oauth(&self, flow_id: &str, poll_token: &str) -> AppResult<Value> {
        let path = format!("/api/v1/oauth/cli/poll/{}", urlencode_component(flow_id));
        let url = self
            .base
            .join(&path)
            .map_err(|e| AppError::Other(format!("非法请求路径: {e}")))?;
        let resp = self.http.get(url).bearer_auth(poll_token).send().await?;
        Self::text_to_value(resp).await
    }

    async fn text_to_value(resp: reqwest::Response) -> AppResult<Value> {
        let status = resp.status();
        let text = resp.text().await?;
        if !status.is_success() {
            return Err(AppError::Other(format!("HTTP {status}: {text}")));
        }
        serde_json::from_str(&text)
            .map_err(|e| AppError::Other(format!("响应不是合法 JSON: {e}\n{text}")))
    }

    /// 用 z.ai OAuth 生 token 换业务 token(客户端登录后存的就是业务 token;
    /// 直接把生 token 写进 oauth:zai:access_token 会导致客户端启动校验失败并主动登出)。
    /// 返回 (业务 token, expires_in 秒)。生 token 已失效或已是业务 token 时会报错,由调用方决定回退。
    pub async fn exchange_zai_business_token(&self, raw_token: &str) -> AppResult<(String, Option<i64>)> {
        let resp = self
            .http
            .post(crate::zcode::api::ZAI_LOGIN_URL)
            .timeout(std::time::Duration::from_secs(10))
            .json(&serde_json::json!({ "token": raw_token.trim() }))
            .send()
            .await?;
        let status = resp.status();
        let text = resp.text().await?;
        if !status.is_success() {
            return Err(AppError::Other(format!("HTTP {status}: {text}")));
        }
        let v: Value = serde_json::from_str(&text)
            .map_err(|e| AppError::Other(format!("响应不是合法 JSON: {e}\n{text}")))?;
        // 客户端判定:code 缺失或 0/200 视为成功,success=false 视为失败
        let code_ok = match &v["code"] {
            Value::Null => true,
            Value::Number(n) => matches!(n.as_i64(), Some(0) | Some(200)),
            Value::String(s) => s == "0" || s == "200",
            _ => false,
        };
        if v["success"].as_bool() == Some(false) || !code_ok {
            return Err(AppError::Other(format!("z/login 换业务 token 失败: {v}")));
        }
        let token = v["data"]["access_token"]
            .as_str()
            .or_else(|| v["data"]["accessToken"].as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| AppError::Other("z/login 响应缺少 data.access_token".into()))?
            .to_string();
        Ok((token, v["data"]["expires_in"].as_i64()))
    }

    /// OAuth 授权码换 token(逆向自客户端:POST JSON,无 PKCE)。回调流已弃用,保留备用
    #[allow(dead_code)]
    pub async fn exchange_oauth_token(
        &self,
        code: &str,
        redirect_uri: &str,
        state: &str,
    ) -> AppResult<Value> {
        let url = self
            .base
            .join("/api/v1/oauth/token")
            .map_err(|e| AppError::Other(format!("非法请求路径: {e}")))?;
        let resp = self
            .http
            .post(url)
            .json(&serde_json::json!({
                "provider": "zai",
                "code": code,
                "redirect_uri": redirect_uri,
                "state": state,
            }))
            .send()
            .await?;
        let status = resp.status();
        let text = resp.text().await?;
        if !status.is_success() {
            return Err(AppError::Other(format!("HTTP {status}: {text}")));
        }
        serde_json::from_str(&text)
            .map_err(|e| AppError::Other(format!("响应不是合法 JSON: {e}\n{text}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 直连 balance 接口,验证请求头组合与解密后的 JWT 均可用(依赖本机已登录)
    #[tokio::test]
    async fn fetch_balance_live() {
        let creds = crate::zcode::credentials::load_local().expect("本机凭证");
        let client = ZaiClient::new().unwrap();
        // 与 PoC 相同的固定 device-mid,仅测试用
        let v = client
            .get_balance(&creds.jwt, "cb090112-c5ce-450d-bf17-ebe55d2349d8")
            .await
            .unwrap();
        assert_eq!(v["code"], 0, "接口返回异常: {v}");
        let count = v["data"]["balances"].as_array().map(|a| a.len()).unwrap_or(0);
        println!("balances 条目数 = {count}");
    }

    /// 诊断:打印 balance/preview 接口返回的套餐原始名称(排查"生命周期"文案来源)
    #[tokio::test]
    async fn dump_plan_names() {
        let creds = crate::zcode::credentials::load_local().expect("本机凭证");
        let client = ZaiClient::new().unwrap();
        let mid = "cb090112-c5ce-450d-bf17-ebe55d2349d8";
        let bal = client.get_balance(&creds.jwt, mid).await.unwrap();
        println!("=== balance plans ===");
        if let Some(plans) = bal["data"]["plans"].as_array() {
            for p in plans {
                println!("plan.name = {:?}", p["name"].as_str());
            }
        }
        let prev = client.get_preview(&creds.jwt, mid).await.unwrap();
        println!("=== preview plans ===");
        println!("{}", serde_json::to_string_pretty(&prev["data"]).unwrap_or_default());
    }

    /// 诊断:OAuth 轮询 init 返回的 authorize_url 原始形态
    #[tokio::test]
    async fn dump_oauth_init() {
        let client = ZaiClient::new().unwrap();
        let token: String = uuid::Uuid::new_v4().simple().to_string()
            + &uuid::Uuid::new_v4().simple().to_string();
        let v = client.init_oauth_polling(&token).await.unwrap();
        println!("init code = {}", v["code"]);
        println!("authorize_url = {}", v["data"]["authorize_url"]);
        println!(
            "expires_at = {}, poll_interval_sec = {}, flow_id = {}",
            v["data"]["expires_at"],
            v["data"]["poll_interval_sec"],
            v["data"]["flow_id"]
        );
    }
}
