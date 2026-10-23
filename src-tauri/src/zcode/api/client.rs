use crate::zcode::error::{AppError, AppResult};
use reqwest::header::{HeaderMap, HeaderValue, InvalidHeaderValue};
use reqwest::{Client, Method, Url};
use serde_json::Value;

use super::{APP_VERSION, BASE_URL, PLATFORM};

/// 探测可用的 HTTP 代理地址(返回 reqwest 可用的完整 URL)。
/// 优先级:环境变量 HTTPS_PROXY/HTTP_PROXY → Windows 注册表 ProxyServer。
/// 返回 None 表示不走代理(国内直连 z.ai 也可用)。
pub fn detect_proxy() -> Option<String> {
    for var in ["HTTPS_PROXY", "https_proxy", "HTTP_PROXY", "http_proxy"] {
        if let Ok(v) = std::env::var(var) {
            let v = v.trim();
            if !v.is_empty() {
                return Some(normalize_proxy_url(v));
            }
        }
    }
    #[cfg(windows)]
    {
        if let Some(v) = read_windows_proxy() {
            return Some(v);
        }
    }
    None
}

/// 把用户可能填的 `127.0.0.1:7892` / `socks5://x:1080` 归一成 reqwest 需要的完整 URL
fn normalize_proxy_url(raw: &str) -> String {
    let s = raw.trim();
    if s.contains("://") {
        s.to_string()
    } else {
        // 缺 scheme 时按 http 算(Clash/Mihomo 混合端口最常见)
        format!("http://{s}")
    }
}

/// 读 Windows 注册表里的 IE/用户代理设置(ProxyEnable=1 才生效)。
/// Flybird、Clash for Windows、v2rayN 等都会写这里,WebView2 也读同一处。
#[cfg(windows)]
fn read_windows_proxy() -> Option<String> {
    use std::process::Command;
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let script = r#"
$k='HKCU:\Software\Microsoft\Windows\CurrentVersion\Internet Settings'
$p=Get-ItemProperty -Path $k -ErrorAction SilentlyContinue
if ($p -and $p.ProxyEnable -eq 1 -and $p.ProxyServer) { $p.ProxyServer }
"#;
    let out = Command::new("powershell")
        .args(["-NoProfile", "-Command", script])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() {
        return None;
    }
    // 注册表里可能是 `127.0.0.1:7892`,也可能是 `http=127.0.0.1:1;https=127.0.0.1:2` 的分协议形式
    let picked = s
        .split(';')
        .find_map(|kv| {
            let (k, v) = kv.split_once('=')?;
            if k.eq_ignore_ascii_case("http") || k.eq_ignore_ascii_case("https") {
                Some(v.to_string())
            } else {
                None
            }
        })
        .unwrap_or(s);
    Some(normalize_proxy_url(&picked))
}

/// URL 路径段编码
fn urlencode_component(s: &str) -> String {    let mut out = String::new();
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

        let http = Self::build_http(headers)?;
        let base = BASE_URL
            .parse()
            .map_err(|e| AppError::Other(format!("非法 base url: {e}")))?;
        Ok(Self { http, base })
    }

    /// 构造 HTTP 客户端(带上传入的默认请求头)。
    /// 代理策略:reqwest 默认不读 Windows 系统代理,而 z.ai 全球版注册/接口
    /// 在部分地区需要代理(Flybird 等 VPN 走 127.0.0.1:7892 这类本地端口),
    /// 所以显式探测:优先环境变量 HTTP_PROXY/HTTPS_PROXY,其次 Windows 注册表
    /// 的 ProxyServer(仅 ProxyEnable=1 时)。探测不到就不走代理(国内可直连)。
    ///
    /// **区域判定警告(实测 2026-10-23)**:z.ai 服务端按**出口 IP** 决定
    /// `client/configs` 返回的 `captcha.region`(国际=s gp / 国内=cn)。
    /// 本进程 detect_proxy 找到的是自家 8899 中转(出口在国内)→ 永远拿到 cn
    /// → 国际账号被配 cn 验证码场景 → TRACELESS 静默卡死,捆绑/领取本地页不可用。
    /// 浏览器(Zen)走真实代理出口是 sgp,能正常完成验证。
    /// 结论:捆绑与领额度走**官方网页流程**(invite 页接受邀请),不要走本地验证页。
    fn build_http(headers: HeaderMap) -> AppResult<Client> {
        let mut builder = Client::builder().default_headers(headers);
        if let Some(proxy) = detect_proxy() {
            log::info!("[zcode] HTTP 走代理: {proxy}");
            builder = builder.proxy(
                reqwest::Proxy::all(&proxy)
                    .map_err(|e| AppError::Other(format!("代理地址非法({proxy}): {e}")))?,
            );
        }
        builder.build().map_err(AppError::from)
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

    /// 当前套餐画像(官方 billing/current;比 balance 多 capabilities/period/priority/status)
    ///
    /// 实测 2026-10-23:HTTP 200,`data` 为**单个套餐对象**(id=30 会返回当前生效的那个);
    /// 多套餐账号要拿全量得配合 `get_balance` 的 `data.plans[]`。
    /// `capabilities` 里的 `model:glm-5.3-flash` 是"该权益覆盖哪些模型"的唯一权威口径,
    /// 网关的模型-账号匹配就靠它(见 `zcode::plan_profile`)。
    pub async fn get_billing_current(&self, jwt: &str, device_mid: &str) -> AppResult<Value> {
        self.request(
            Method::GET,
            &format!("/api/v1/zcode-plan/billing/current?app_version={APP_VERSION}"),
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

    /// 捆绑邀请码(被邀请者持自己的 token 调用,把账号绑到主号名下)。
    /// 逆向自官方奖励中心页面:POST `/zcode-api/incentive/referral/bind-code`
    /// (注意前缀是 /zcode-api,与 /api/v1 不同),body `{invite_code}`,
    /// 必须带阿里云验证码两件套。返回原始响应,由调用方按 code 判成败。
    pub async fn bind_invite_code(
        &self,
        jwt: &str,
        device_mid: &str,
        invite_code: &str,
        captcha_verify_param: &str,
        region: Option<&str>,
    ) -> AppResult<Value> {
        let mut headers = vec![
            ("x-client-language", "zh-CN".to_string()),
            ("x-aliyun-captcha-verify-param", captcha_verify_param.to_string()),
        ];
        if let Some(r) = region {
            headers.push(("x-aliyun-captcha-verify-region", r.to_string()));
        }
        self.request_with(
            Method::POST,
            "/zcode-api/incentive/referral/bind-code",
            jwt,
            device_mid,
            &headers,
            Some(serde_json::json!({ "invite_code": invite_code })),
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

    /// 发一条消息(完成官方新手任务 send_message)。
    ///
    /// 逆向依据:官方 `agent/configs` 的 `proxyEndpoint.mapping` 显示,
    /// 桌面端客户端内部调 `https://api.z.ai/api/anthropic/v1/messages`,
    /// 由服务端转发到 `https://zcode.z.ai/api/v1/ultra-zai/anthropic/v1/messages`。
    /// 也就是说桌面端与网页端最终打到同一个服务端端点,官方判定该任务时
    /// 只看这个端点有没有被此账号调用过——脚本调用与客户端调用等价。
    ///
    /// 凭证用账号的 zai_access_token(OAuth 生 token 即可,不需要 coding-plan key)。
    /// 返回 Ok(true) 表示服务端已受理这条消息。
    pub async fn send_message(
        &self,
        zai_access_token: &str,
        device_mid: &str,
        content: &str,
    ) -> AppResult<bool> {
        let url = self
            .base
            .join("/api/v1/ultra-zai/anthropic/v1/messages")
            .map_err(|e| AppError::Other(format!("非法请求路径: {e}")))?;
        let resp = self
            .http
            .post(url)
            .header("x-api-key", zai_access_token)
            .header("x-device-mid", device_mid)
            .header("anthropic-version", "2023-06-01")
            .json(&serde_json::json!({
                "model": "GLM-5.3-Flash",
                "max_tokens": 32,
                "messages": [{ "role": "user", "content": content }],
            }))
            .send()
            .await?;
        let status = resp.status();
        let text = resp.text().await?;
        // 429 = 认证通过但额度不足(官方错误码 1113)。此时消息已被服务端记录,
        // 任务通常已算完成,所以也算成功——由调用方按需区分。
        if status.is_success() {
            return Ok(true);
        }
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            log::info!("[zcode] send_message 额度不足(消息已受理): {text}");
            return Ok(true);
        }
        if status == reqwest::StatusCode::UNAUTHORIZED {
            return Err(AppError::Other(format!(
                "zai token 失效,无法发消息(HTTP 401): {text}"
            )));
        }
        Err(AppError::Other(format!("发消息失败 HTTP {status}: {text}")))
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

    #[test]
    fn normalize_proxy_url_handles_common_forms() {
        // 缺 scheme 的本地端口(Clash/Mihomo/Flybird 最常见)
        assert_eq!(normalize_proxy_url("127.0.0.1:7892"), "http://127.0.0.1:7892");
        assert_eq!(normalize_proxy_url(" 127.0.0.1:7890 "), "http://127.0.0.1:7890");
        // 已有 scheme 的原样保留
        assert_eq!(normalize_proxy_url("http://127.0.0.1:7892"), "http://127.0.0.1:7892");
        assert_eq!(
            normalize_proxy_url("socks5://127.0.0.1:1080"),
            "socks5://127.0.0.1:1080"
        );
    }


    /// 注册表可能返回分协议形式 `http=x;https=y`,取 https 优先(纯函数逻辑验证)
    #[test]
    fn pick_proxy_from_registry_value() {        let pick = |s: &str| -> Option<String> {
            s.split(';')
                .find_map(|kv| {
                    let (k, v) = kv.split_once('=')?;
                    if k.eq_ignore_ascii_case("http") || k.eq_ignore_ascii_case("https") {
                        Some(v.to_string())
                    } else {
                        None
                    }
                })
                .or_else(|| Some(s.to_string()))
        };
        assert_eq!(
            pick("http=127.0.0.1:1;https=127.0.0.1:2").as_deref(),
            Some("127.0.0.1:1")
        );
        assert_eq!(pick("127.0.0.1:7892").as_deref(), Some("127.0.0.1:7892"));
    }
}
