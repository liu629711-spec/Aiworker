//! 马甲邮箱验证码/验证链接收取(注册链路用)。
//!
//! 背景:ZCode 注册走 chat.z.ai 的邮箱验证——`/api/v1/auths/signup` 发出验证邮件,
//! 邮件里的 token 用于 `/api/v1/auths/verify_email`(配合 username/email)。
//! 工具用 Gmail IMAP 自动收这封邮件,把 token 交给页面完成注册。
//!
//! 连接参数(App Password)存 app_kv(个人工具,明文落库符合现有约定):
//! - `imap_host`(默认 imap.gmail.com)
//! - `imap_port`(默认 993)
//! - `imap_user`(Gmail 地址)
//! - `imap_pass`(Gmail App Password,16 位,需账号开 2FA 后生成)
//!
//! 设计:协议交互(async-imap)与"从邮件里提取 token"的纯逻辑分离——
//! 纯函数可离线单测,不依赖真实邮箱。

use crate::zcode::error::{AppError, AppResult};
use crate::zcode::util;

/// IMAP 连接参数(从 app_kv 读出后构造)
#[derive(Debug, Clone)]
pub struct ImapConfig {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub pass: String,
}

impl ImapConfig {
    /// 从 app_kv 读配置;缺 user/pass 返回 Err(引导用户去配)
    pub async fn load(db: &sqlx::SqlitePool) -> AppResult<Self> {
        let host = kv_get(db, "imap_host")
            .await
            .unwrap_or_else(|| "imap.gmail.com".to_string());
        let port = kv_get(db, "imap_port")
            .await
            .and_then(|s| s.parse().ok())
            .unwrap_or(993);
        let user = kv_get(db, "imap_user")
            .await
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| {
                AppError::Other("未配置马甲邮箱:请在「自动注册」面板填写 Gmail 地址与 App Password".into())
            })?;
        let pass = kv_get(db, "imap_pass")
            .await
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| {
                AppError::Other("未配置马甲邮箱密码:请填写 Gmail App Password(需开启两步验证)".into())
            })?;
        Ok(Self { host, port, user, pass })
    }

    /// 写回 app_kv(连接测试成功后由命令层调用)
    pub async fn save(&self, db: &sqlx::SqlitePool) {
        kv_set(db, "imap_host", &self.host).await;
        kv_set(db, "imap_port", &self.port.to_string()).await;
        kv_set(db, "imap_user", &self.user).await;
        kv_set(db, "imap_pass", &self.pass).await;
    }
}

/// app_kv 读取(委托 zcode::util,失败静默返回 None)
async fn kv_get(db: &sqlx::SqlitePool, key: &str) -> Option<String> {
    util::kv_get(db, key).await
}

async fn kv_set(db: &sqlx::SqlitePool, key: &str, value: &str) {
    util::kv_set(db, key, value).await;
}

/// 从邮件正文里提取完整的验证链接(纯函数,可离线单测)。
///
/// 页面实测(2026-10-23)是「请点击邮件中的链接完成验证」——没有 6 位验证码输入框。
/// 所以正确做法是把窗口**导航到这个链接**,而不是往页面里填 code:
/// 链接本身带 token + email + username,服务端验完直接进登录态。
///
/// 邮箱里的链接是 quoted-printable 编码态(`token=3Dverify-...&amp;email=3D...`),
/// 调用方应先 `decode_mime_body` 再交给本函数;这里额外容忍 `&amp;` 与 `=3D` 残留。
pub fn extract_verify_url(body: &str) -> Option<String> {
    // HTML 里可能写成 href="..." 或纯文本;从 scheme 起截到引号/空白/尖括号
    let idx = body.find("chat.z.ai/auth/verify").or_else(|| body.find("/auth/verify_email"))?;
    // 往前补 scheme(正文里通常已有 https://,缺失时不算链接)
    let start = if body[..idx].ends_with("https://") {
        idx - "https://".len()
    } else if body[..idx].ends_with("http://") {
        idx - "http://".len()
    } else {
        idx
    };
    let tail = &body[start..];
    let url: String = tail
        .chars()
        .take_while(|c| {
            !c.is_whitespace()
                && *c != '"'
                && *c != '\''
                && *c != '<'
                && *c != '>'
                && *c != '\\'
        })
        .collect();
    if !url.contains("token=") {
        return None;
    }
    // QP 残留还原:&amp; → &(顺序先长后短,避免 &amp; 被拆成 & + amp;)
    let url = url
        .replace("&amp;", "&")
        .replace("%26", "&")
        .replace("=3D", "=");
    Some(url)
}

/// 从邮件正文里提取验证 token(纯函数,可离线单测)。
/// 两种形态都支持(未真机确认前不做单一假设):
/// ① 链接形:`https://chat.z.ai/auth/verify_email?token=XXXX`(也可能带 & 后续参数)
/// ② 数字形:邮件里的 6~8 位验证码
/// 返回 None 表示这封邮件里没有可用 token(调用方继续等下一封)。
pub fn extract_verify_token(body: &str) -> Option<String> {
    // ① 优先链接形:token= 后面到 & # 或空白为止
    if let Some(idx) = body.find("token=") {
        let tail = &body[idx + "token=".len()..];
        let token: String = tail
            .chars()
            .take_while(|c| !c.is_whitespace() && *c != '&' && *c != '"' && *c != '\'' && *c != '<' && *c != '>' && *c != '\\')
            .collect();
        if !token.is_empty() {
            return Some(token);
        }
    }
    // ② 数字形:按非字母数字切段,找长度 6~8 的纯数字段。
    // 中文标点(，。！等)不是 ASCII 字母数字,天然成为分隔符。
    for candidate in body.split(|c: char| !c.is_ascii_alphanumeric()) {
        if candidate.len() >= 6 && candidate.len() <= 8 && candidate.chars().all(|c| c.is_ascii_digit()) {
            return Some(candidate.to_string());
        }
    }
    None
}

/// 判断一封邮件是否是目标注册验证邮件(主题含验证/verify,或正文含 verify_email 链接)。
/// 用于在收件箱里过滤掉订阅/广告等噪声邮件。
pub fn is_verification_mail(subject: &str, body: &str) -> bool {
    let s = subject.to_lowercase();
    if s.contains("verify") || s.contains("验证") || s.contains("确认") || s.contains("confirm") {
        return true;
    }
    // 主题可能整体是 MIME 编码词(=?utf-8?b?...?=),解码后仍有中文"验证"
    let s_decoded = decode_mime_header(subject).to_lowercase();
    if s_decoded.contains("verify") || s_decoded.contains("验证") || s_decoded.contains("确认") {
        return true;
    }
    if body.contains("verify_email") || body.contains("/auth/verify") {
        return true;
    }
    // 正文常是 base64 / quoted-printable:原样看不出 verify_email,解码后再判一次
    let b = decode_mime_body(body).to_lowercase();
    b.contains("verify_email") || b.contains("/auth/verify")
}

/// 解码 MIME 邮件正文(纯函数,可离线单测)。
///
/// `async-imap` 的 `Fetch::text()` 返回的是**未经解码**的 `BODY[TEXT]`:
/// base64 块是字母数字密文、quoted-printable 块里是 `token=3Dverify-...`、
/// 折行处还有 `=\r\n` 软换行。不解码直接找 `token=` 会拿到 `3Dverify-xxx`
/// 这种脏值,交给页面必然验证失败(2026-10-23 真机注册实测)。
///
/// 处理顺序:
///   ① 按 Content-Transfer-Encoding 逐块解码(base64 忽略空白;QP 处理 =XX 与软换行)
///   ② 去掉 base64 / QP 解码后残留的 HTML 噪声对 token 无影响的字符
/// 非 MIME 或无法识别编码时原样返回(纯文本邮件照常工作)。
pub fn decode_mime_body(raw: &str) -> String {
    // 没有 MIME 头特征:按纯文本处理
    let lower = raw.to_ascii_lowercase();
    if !lower.contains("content-transfer-encoding")
        && !lower.contains("multipart")
        && !lower.contains("content-type")
    {
        return raw.to_string();
    }

    let mut out = String::new();
    // 按 MIME 边界切块;每块自带自己的 CTE 头
    for part in raw.split("--").collect::<Vec<_>>() {
        let header_end = part.find("\r\n\r\n").or_else(|| part.find("\n\n"));
        let (headers, payload) = match header_end {
            Some(i) => {
                let nl = if part[i..].starts_with("\r\n\r\n") { 4 } else { 2 };
                (&part[..i], &part[i + nl..])
            }
            None => ("", part),
        };
        let h = headers.to_ascii_lowercase();
        if h.contains("content-transfer-encoding: base64") {
            out.push_str(&b64_decode_loose(payload));
        } else if h.contains("content-transfer-encoding: quoted-printable") {
            out.push_str(&qp_decode(payload));
        } else if h.contains("content-type: text/") || h.contains("content-type: multipart") {
            out.push_str(payload);
        } else {
            // 头块本身(部分邮件首块无正文)或未知块:原样保留,避免丢内容
            out.push_str(part);
        }
        out.push('\n');
    }
    if out.trim().is_empty() {
        // 整封只有一个编码块、没有边界的情况:整体尝试
        if lower.contains("base64") {
            return b64_decode_loose(raw);
        }
        if lower.contains("quoted-printable") {
            return qp_decode(raw);
        }
        return raw.to_string();
    }
    out
}

/// 宽松 base64 解码:去掉所有非表内字符(空白/换行/边界残留),失败则返回原文。
fn b64_decode_loose(s: &str) -> String {
    use base64::Engine as _;
    let cleaned: String = s
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '+' || *c == '/' || *c == '=')
        .collect();
    match base64::engine::general_purpose::STANDARD.decode(cleaned.as_bytes()) {
        Ok(bytes) => String::from_utf8_lossy(&bytes).to_string(),
        Err(_) => s.to_string(),
    }
}

/// quoted-printable 解码:处理 `=XX` 与行尾软换行 `=\r\n`。
pub fn qp_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'=' {
            // 软换行:=\r\n 或 =\n
            if i + 1 < bytes.len() && bytes[i + 1] == b'\r' {
                i += 2;
                if i < bytes.len() && bytes[i] == b'\n' {
                    i += 1;
                }
                continue;
            }
            if i + 1 < bytes.len() && bytes[i + 1] == b'\n' {
                i += 2;
                continue;
            }
            // =XX 十六进制
            if i + 2 < bytes.len() {
                let hex = &s[i + 1..i + 3];
                if let Ok(b) = u8::from_str_radix(hex, 16) {
                    out.push(b);
                    i += 3;
                    continue;
                }
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

/// 解码 MIME 编码词 `=?utf-8?b?xxxx?=` / `=?utf-8?q?xxxx?=`
/// (邮件主题常整体编码,不解码就看不出"验证")。
pub fn decode_mime_header(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(start) = rest.find("=?") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        // 格式:charset?encoding?text?=
        let Some(enc_sep) = after.find('?') else { break };
        let enc = &after[enc_sep + 1..];
        if enc.len() < 2 {
            out.push_str(&rest[start..]);
            return out;
        }
        let kind = enc.chars().next().unwrap_or('b').to_ascii_lowercase();
        let after_kind = &enc[1..];
        if !after_kind.starts_with('?') {
            out.push_str(&rest[start..]);
            return out;
        }
        let text_and_rest = &after_kind[1..];
        let Some(end) = text_and_rest.find("?=") else {
            out.push_str(&rest[start..]);
            return out;
        };
        let text = &text_and_rest[..end];
        let decoded = if kind == 'b' {
            b64_decode_loose(text)
        } else {
            // Q 编码:下划线是空格
            qp_decode(&text.replace('_', " "))
        };
        out.push_str(&decoded);
        rest = &text_and_rest[end + 2..];
    }
    out.push_str(rest);
    // 折行的编码词之间常有空格,解码后按需保留
    out
}

/// 这封邮件是不是发给 `target_email` 的。
///
/// 必须做这个判断:同一个 Gmail 收所有马甲的验证邮件,上一次注册的邮件只要还在
/// INBOX 前几封里就会被优先返回,把旧 token 交给页面 → 服务端拒绝 →
/// 用户看到"验证超时"却完全看不出是 token 拿错了。
///
/// `to_header` 是原始 `To:` 头值(可能含显示名、多个地址)。宽松匹配:
/// 只要整个头里出现目标地址(忽略大小写)就算,兼容 `Name <addr>` 与多余空白。
pub fn mail_is_for_target(to_header: &str, target_email: &str) -> bool {
    let target = target_email.trim().to_ascii_lowercase();
    if target.is_empty() {
        return false;
    }
    to_header.to_ascii_lowercase().contains(&target)
}

/// 判定这封邮件是否由本次注册的马甲地址触发。
///
/// 只看 `To:` 头不够——Firefox Relay 转投会**改写收件人**为真实 Gmail
/// (`To: liu629711@gmail.com`),马甲地址只留在两处:
///   · `From: "noreply@email.z.ai [via Relay]" <alias@mozmail.com>`
///   · 正文首行 `This email was sent to your alias <alias@mozmail.com>`
/// 实测(2026-10-23 首次真机注册):只查 To 头会把新号的验证信全部当"别人的邮件"
/// 丢掉,流水线一直等到超时。
pub fn mail_is_for_alias(to: &str, from: &str, body: &str, target_email: &str) -> bool {
    let target = target_email.trim().to_ascii_lowercase();
    if target.is_empty() {
        return false;
    }
    mail_is_for_target(to, &target)
        || mail_is_for_target(from, &target)
        || body.to_ascii_lowercase().contains(&target)
}

/// 解析 IMAP `Date:` 头为 Unix 时间戳(秒)。
/// 支持 RFC 3501 基本格式 `dd Mon yyyy HH:MM:SS +ZZZZ`;
/// 解析不了返回 None(调用方按"未知时间"放行,宁放过不误杀——时效是兜底不是主判)。
pub fn parse_mail_date(header: &str) -> Option<i64> {
    // 形如 "Tue, 21 Oct 2026 09:15:33 +0800"
    let s = header.trim();
    // 去掉前导 "Tue, " 这种星期前缀
    let s = s.splitn(2, ", ").nth(1).unwrap_or(s);
    let mut parts = s.split_whitespace();
    let _day: i64 = parts.next()?.parse().ok()?;
    let mon = parts.next()?;
    let year: i64 = parts.next()?.parse().ok()?;
    let hms = parts.next()?;
    let tz = parts.next().unwrap_or("+0000");

    let month = match mon.to_ascii_lowercase().as_str() {
        "jan" => 1, "feb" => 2, "mar" => 3, "apr" => 4, "may" => 5, "jun" => 6,
        "jul" => 7, "aug" => 8, "sep" => 9, "oct" => 10, "nov" => 11, "dec" => 12,
        _ => return None,
    };
    let mut hms_parts = hms.split(':');
    let hour: i64 = hms_parts.next()?.parse().ok()?;
    let min: i64 = hms_parts.next()?.parse().ok()?;
    let sec: i64 = hms_parts.next().unwrap_or("0").parse().ok()?;

    // 时区偏移(分钟)。-0500 → -300
    let (sign, rest) = tz.split_at(1);
    let off: i64 = if rest.len() == 4 {
        let h: i64 = rest[..2].parse().ok()?;
        let m: i64 = rest[2..].parse().ok()?;
        (h * 60 + m) * if sign == "-" { -1 } else { 1 }
    } else {
        0
    };

    Some(
        epoch_days(year, month, _day) * 86400 + hour * 3600 + min * 60 + sec - off * 60,
    )
}

/// 民用历 → 1970-01-01 起的天数(Howard Hinnant 算法,免依赖 chrono)
fn epoch_days(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// 从原始邮件头里取一个字段的值(如 "To" / "Date" / "Subject")。
/// 邮件头是 `Name: value\r\n` 行序列;支持 RFC 5322 折行(续行以空白开头)。
/// 找不到返回空串。
fn header_field(raw_headers: &str, name: &str) -> String {
    let prefix = format!("{name}:");
    let prefix_lower = prefix.to_ascii_lowercase();
    let mut out = String::new();
    let mut collecting = false;
    for line in raw_headers.split('\n') {
        let line = line.trim_end_matches('\r');
        let ll = line.trim_start().to_ascii_lowercase();
        if collecting {
            // 折行续接:以空格/TAB 开头
            if line.starts_with(' ') || line.starts_with('\t') {
                out.push(' ');
                out.push_str(line.trim());
                continue;
            }
            break;
        }
        if ll.starts_with(&prefix_lower) {
            collecting = true;
            out.push_str(line[prefix.len()..].trim());
        }
    }
    out
}

/// 日志用:把验证链接里的 token 打码(只留前缀),避免凭证进日志文件。
pub fn mask_url(url: &str) -> String {
    match url.find("token=") {
        Some(i) => {
            let head = &url[..i + "token=".len()];
            let tail = &url[i + "token=".len()..];
            let token_end = tail.find('&').unwrap_or(tail.len());
            let token = &tail[..token_end];
            let shown: String = token.chars().take(12).collect();
            format!("{head}{shown}…{}", &tail[token_end..])
        }
        None => url.to_string(),
    }
}

/// 一封验证邮件里能拿到的两种凭证。
/// 页面实测是「点击链接」形态(`url` 为主),`token` 保留给少数发验证码的形态。
#[derive(Debug, Clone, PartialEq)]
pub struct VerifyMail {
    /// 邮件里的完整验证链接(带 token/email/username,直接导航即可完成验证)
    pub url: Option<String>,
    /// 从链接或正文里抽出的 token(链接缺失时的兜底)
    pub token: String,
}

/// 连接 Gmail 并在 INBOX 里轮询等待注册验证邮件,拿到凭证即返回。
/// `deadline_sec` 为最长等待秒数;`target_email` 用于过滤掉发给别的马甲的旧邮件。
///
/// 实现要点:IMAP 会话跨多个 await 持有,且 async-imap 的 Session 不是 Send,
/// 所以整条协议流程放在一个 async 块里顺序执行(不跨线程传递 Session),
/// 由外层 deadline 控制超时。
///
/// `cancel` 每轮检查一次:用户点「取消」时不用干等满 deadline(IMAP 等待最长可到 30 分钟)。
pub async fn wait_verify_token<F>(
    cfg: &ImapConfig,
    target_email: &str,
    deadline_sec: u64,
    mut cancel: F,
) -> AppResult<VerifyMail>
where
    F: FnMut() -> bool,
{
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(deadline_sec);
    // 流水线启动时刻:只认这之后到达的邮件,天然排掉历史遗留的验证邮件
    let started_at = chrono::Utc::now().timestamp();
    loop {
        if cancel() {
            return Err(AppError::Other("用户已取消".into()));
        }
        match fetch_token_once(cfg, target_email, started_at).await {
            Ok(Some(mail)) => return Ok(mail),
            Ok(None) => {}
            Err(e) => {
                // 连接/登录类错误直接失败(重试也没用)
                let msg = e.to_string();
                if msg.contains("登录") || msg.contains("握手") || msg.contains("连接") {
                    return Err(e);
                }
            }
        }
        if std::time::Instant::now() >= deadline {
            return Err(AppError::Other(format!(
                "等待 {target_email} 的验证邮件超时({deadline_sec}s):\
                 请确认该马甲邮箱能收到 z.ai 的验证邮件(必要时检查垃圾箱)"
            )));
        }
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    }
}

/// 单次尝试:连上 → 登录 → 选 INBOX → 从最新往旧找第一封匹配的验证邮件 → 提取 token。
/// 匹配条件(三个都要过,任一不过就跳过这封继续看下一封):
///   ① `is_verification_mail` 主题/正文像验证邮件
///   ② `mail_is_for_target` To 头含本次注册的马甲地址(排掉别的马甲的旧邮件)
///   ③ 邮件时间不早于 `not_before`(排掉本次启动前就躺在收件箱里的旧邮件)
/// 返回 Ok(None) 表示这轮没找到(继续等)。
async fn fetch_token_once(
    cfg: &ImapConfig,
    target_email: &str,
    not_before: i64,
) -> AppResult<Option<VerifyMail>> {
    use async_imap::Client;
    use tokio_rustls::rustls::{ClientConfig, RootCertStore};
    use tokio_rustls::TlsConnector;

    let mut roots = RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let tls = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();

    let tcp = tokio::net::TcpStream::connect((cfg.host.as_str(), cfg.port))
        .await
        .map_err(|e| AppError::Other(format!("连接 {}:{} 失败: {e}", cfg.host, cfg.port)))?;
    let server_name = cfg
        .host
        .clone()
        .try_into()
        .map_err(|_| AppError::Other(format!("非法 IMAP 主机名: {}", cfg.host)))?;
    let tls_stream = TlsConnector::from(std::sync::Arc::new(tls))
        .connect(server_name, tcp)
        .await
        .map_err(|e| AppError::Other(format!("TLS 握手失败: {e}")))?;

    let client = Client::new(tls_stream);
    let mut session = client
        .login(&cfg.user, &cfg.pass)
        .await
        .map_err(|(e, _client)| AppError::Other(format!("IMAP 登录失败(检查地址与 App Password): {e}")))?;

    session
        .select("INBOX")
        .await
        .map_err(|e| AppError::Other(format!("选择 INBOX 失败: {e}")))?;

    // 从最新 10 封里找(UID SEARCH ALL 后按序号倒序取)
    let uids = session
        .uid_search("ALL")
        .await
        .map_err(|e| AppError::Other(format!("搜索邮件失败: {e}")))?;
    let mut sorted: Vec<u32> = uids.into_iter().collect();
    sorted.sort_unstable_by(|a, b| b.cmp(a));
    sorted.truncate(10);

    let mut found: Option<VerifyMail> = None;
    'outer: for uid in sorted {
        // 拉完整 HEADER(不是 HEADER.FIELDS:async-imap 的 Fetch::header() 只认
        // SectionPath::Full(Header),部分字段请求会让 header() 返回 None)
        let mut stream = session
            .uid_fetch(
                uid.to_string(),
                "(UID BODY.PEEK[HEADER] BODY.PEEK[TEXT])",
            )
            .await
            .map_err(|e| AppError::Other(format!("拉取邮件 {uid} 失败: {e}")))?;
        use futures_util::StreamExt;
        while let Some(fetch) = stream.next().await {
            let f = fetch.map_err(|e| AppError::Other(format!("解析邮件 {uid} 失败: {e}")))?;
            let raw_headers = f
                .header()
                .map(|h| String::from_utf8_lossy(h).to_string())
                .unwrap_or_default();
            let raw_body = f
                .text()
                .map(|t| String::from_utf8_lossy(t).to_string())
                .unwrap_or_default();
            // BODY[TEXT] 是未解码的 MIME 内容:base64/QP 必须先解码,
            // 否则既认不出 verify_email,也提不出 token(会拿到 "3Dverify-...")
            let body = decode_mime_body(&raw_body);
            let subject = decode_mime_header(&header_field(&raw_headers, "Subject"));
            let to = header_field(&raw_headers, "To");
            let from = decode_mime_header(&header_field(&raw_headers, "From"));
            let date_hdr = header_field(&raw_headers, "Date");
            if !is_verification_mail(&subject, &raw_body) {
                continue;
            }
            // 排掉发给别的马甲的旧邮件(同一 Gmail 收所有马甲的验证信)。
            // Relay 会把 To 改写成真实 Gmail,所以还要看 From 与正文里的别名。
            if !mail_is_for_alias(&to, &from, &body, target_email) {
                continue;
            }
            // 排掉本次启动前就躺在收件箱里的旧邮件;日期解析失败则放行(宁放过不误杀)
            if let Some(ts) = parse_mail_date(&date_hdr) {
                if ts < not_before - 300 {
                    continue;
                }
            }
            // 优先取完整验证链接(页面是"点击链接"形态);链接缺失才用 token 兜底
            let url = extract_verify_url(&body);
            if let Some(token) = extract_verify_token(&body) {
                // 找到即止:先记住凭证,等 stream drop 后再 logout
                // (logout 要再借 session,而 stream 正持有它)
                found = Some(VerifyMail { url, token });
                break 'outer;
            }
        }
    }
    let _ = session.logout().await;
    Ok(found)
}

/// 连接测试:只验证能否登录并选 INBOX(供前端「测试连接」按钮)
pub async fn test_connection(cfg: &ImapConfig) -> AppResult<String> {
    use async_imap::Client;
    use tokio_rustls::rustls::{ClientConfig, RootCertStore};
    use tokio_rustls::TlsConnector;

    let mut roots = RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let tls = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();

    let tcp = tokio::net::TcpStream::connect((cfg.host.as_str(), cfg.port))
        .await
        .map_err(|e| AppError::Other(format!("连接 {}:{} 失败: {e}", cfg.host, cfg.port)))?;
    let server_name = cfg
        .host
        .clone()
        .try_into()
        .map_err(|_| AppError::Other(format!("非法 IMAP 主机名: {}", cfg.host)))?;
    let tls_stream = TlsConnector::from(std::sync::Arc::new(tls))
        .connect(server_name, tcp)
        .await
        .map_err(|e| AppError::Other(format!("TLS 握手失败: {e}")))?;

    let mut session = Client::new(tls_stream)
        .login(&cfg.user, &cfg.pass)
        .await
        .map_err(|(e, _client)| AppError::Other(format!("IMAP 登录失败(检查地址与 App Password): {e}")))?;
    let mailbox = session
        .select("INBOX")
        .await
        .map_err(|e| AppError::Other(format!("选择 INBOX 失败: {e}")))?;
    let _ = session.logout().await;
    Ok(format!("连接成功:INBOX 共 {} 封邮件", mailbox.exists))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_token_from_link() {
        // 链接形:token= 后面截到 & 为止
        let body = r#"<a href="https://chat.z.ai/auth/verify_email?token=abc123XYZ&email=a@b.com">验证</a>"#;
        assert_eq!(extract_verify_token(body).as_deref(), Some("abc123XYZ"));
        // token 后跟引号/尖括号
        let body2 = r#"click https://chat.z.ai/auth/verify_email?token=TK9988" now"#;
        assert_eq!(extract_verify_token(body2).as_deref(), Some("TK9988"));
        let body3 = "url: https://chat.z.ai/auth/verify_email?token=XYZ789< br";
        assert_eq!(extract_verify_token(body3).as_deref(), Some("XYZ789"));
    }

    #[test]
    fn extract_token_from_digits() {
        // 数字形:6~8 位独立数字
        assert_eq!(extract_verify_token("您的验证码是 123456，5 分钟内有效。").as_deref(), Some("123456"));
        assert_eq!(extract_verify_token("code: 12345678").as_deref(), Some("12345678"));
        // HTML 里的 6 位数字
        assert_eq!(extract_verify_token("<p>654321</p>").as_deref(), Some("654321"));
    }

    #[test]
    fn extract_token_rejects_noise() {
        // 无 token=,也没有 6~8 位独立数字
        assert!(extract_verify_token("hello world, no code here").is_none());
        // 5 位太短
        assert!(extract_verify_token("code 12345").is_none());
        // 9 位太长(不算验证码)
        assert!(extract_verify_token("id 123456789 end").is_none());
        // 空
        assert!(extract_verify_token("").is_none());
        // token= 后面为空
        assert!(extract_verify_token("token=&other=1").is_none());
    }

    #[test]
    fn is_verification_mail_filters() {
        assert!(is_verification_mail("Verify your email", ""));
        assert!(is_verification_mail("验证您的邮箱", ""));
        assert!(is_verification_mail("Confirm your account", ""));
        // 正文带验证链接也算(主题是外语/乱码时的兜底)
        assert!(is_verification_mail("Action required", "goto https://chat.z.ai/auth/verify_email?token=x"));
        // 噪声邮件
        assert!(!is_verification_mail("Weekly newsletter", "<p>Hello there friend</p>"));
        assert!(!is_verification_mail("促销活动", "全场五折"));
    }

    // ── 新增:收件人匹配 + 日期时效(防止拿到别人的旧验证邮件) ──────────

    #[test]
    fn mail_is_for_target_matches_address() {
        // 标准形式
        assert!(mail_is_for_target("mv7j0fwzg@mozmail.com", "mv7j0fwzg@mozmail.com"));
        // 带显示名
        assert!(mail_is_for_target("Someone <mv7j0fwzg@mozmail.com>", "mv7j0fwzg@mozmail.com"));
        // 大小写不敏感
        assert!(mail_is_for_target("MV7J0FWZG@MOZMAIL.COM", "mv7j0fwzg@mozmail.com"));
        // 多个地址(转发/抄送)
        assert!(mail_is_for_target(
            "a@x.com, mv7j0fwzg@mozmail.com, b@y.com",
            "mv7j0fwzg@mozmail.com"
        ));
    }

    #[test]
    fn mail_is_for_target_rejects_other_address() {
        // 这是核心修复点:另一个马甲的旧邮件必须被排除
        assert!(!mail_is_for_target("other7j64s55tn@mozmail.com", "mv7j0fwzg@mozmail.com"));
        // 空目标不该匹配任何东西
        assert!(!mail_is_for_target("someone@mozmail.com", ""));
        assert!(!mail_is_for_target("someone@mozmail.com", "   "));
    }

    #[test]
    fn parse_mail_date_parses_rfc3501() {
        // 2026-10-21 09:15:33 +0800 → UTC 01:15:33
        let ts = parse_mail_date("Tue, 21 Oct 2026 09:15:33 +0800").expect("应能解析");
        // 与 chrono 无关的手算校验:2026-10-21T01:15:33Z
        assert_eq!(ts, 1792545333, "时间戳应等于 2026-10-21T01:15:33Z");
        // 无星期前缀
        assert!(parse_mail_date("21 Oct 2026 09:15:33 +0800").is_some());
        // 缺时区按 UTC
        assert!(parse_mail_date("21 Oct 2026 09:15:33").is_some());
    }

    #[test]
    fn parse_mail_date_rejects_garbage() {
        assert!(parse_mail_date("").is_none());
        assert!(parse_mail_date("not a date").is_none());
        assert!(parse_mail_date("21 Xyz 2026 09:15:33 +0800").is_none(), "月份非法");
    }

    // ── 真机注册回归(2026-10-23 首轮):两个必失败的过滤条件 ──────────────

    /// Firefox Relay 转投会改写 To 头:马甲地址只在 From 与正文里。
    /// 旧实现只查 To → 新号的第一封验证信被判成"别人的邮件",流水线等满超时。
    #[test]
    fn mail_is_for_alias_sees_relay_rewritten_recipient() {
        let alias = "tr19zp42m@mozmail.com";
        // 实测头部形态
        let to = "liu629711@gmail.com";
        let from = "\"noreply@email.z.ai [via Relay]\" <tr19zp42m@mozmail.com>";
        let body = "This email was sent to your alias tr19zp42m@mozmail.com. To stop receiving...";
        assert!(
            mail_is_for_alias(to, from, body, alias),
            "Relay 改写 To 后,必须靠 From/正文里的别名认出来"
        );
        // 只有正文提到(From 被换成别的形态)也要认
        assert!(mail_is_for_alias(to, "noreply@email.z.ai", body, alias));
        // 别的马甲的邮件仍要排除(核心不回归)
        assert!(!mail_is_for_alias(
            to,
            "<other55tn@mozmail.com>",
            "This email was sent to your alias other55tn@mozmail.com.",
            alias
        ));
        // 空目标不匹配任何东西
        assert!(!mail_is_for_alias(to, from, body, ""));
    }

    /// 真实邮件是 multipart/alternative:text/plain 用 base64、text/html 用
    /// quoted-printable,token 在编码态里是 `token=3Dverify-...`。
    /// 不解码直接提取会拿到 "3Dverify-xxx" 这种脏值 → 页面验证失败。
    #[test]
    fn decode_mime_body_handles_real_relay_shape() {
        // 实测正文关键行(base64 段 + QP 段的等价形态)
        let raw = concat!(
            "Content-Type: multipart/alternative; boundary=\"==abc==\"\r\n",
            "\r\n",
            "--==abc==\r\n",
            "Content-Type: text/plain; charset=\"utf-8\"\r\n",
            "Content-Transfer-Encoding: base64\r\n",
            "\r\n",
            // "https://chat.z.ai/auth/verify_email?token=verify-abc123&email=x"
            "aHR0cHM6Ly9jaGF0LnouYWkvYXV0aC92ZXJpZnlfZW1haWw/dG9rZW49dmVyaWZ5LWFiYzEyMyZlbWFpbD14\r\n",
            "--==abc==\r\n",
            "Content-Type: text/html; charset=\"utf-8\"\r\n",
            "Content-Transfer-Encoding: quoted-printable\r\n",
            "\r\n",
            "<a href=3D\"https://chat.z.ai/auth/verify_email?token=3Dverify-abc123&amp;email=3Dx\">\r\n",
            "--==abc==--\r\n"
        );
        let decoded = decode_mime_body(raw);
        assert!(
            decoded.contains("token=verify-abc123"),
            "解码后应出现干净的 token,实际: {decoded}"
        );
        assert!(
            !decoded.contains("3Dverify"),
            "QP 的 =3D 必须被还原成 '=',不能把脏值交给页面"
        );
        // 端到端:解码后能提出正确 token
        assert_eq!(extract_verify_token(&decoded).as_deref(), Some("verify-abc123"));
    }

    /// 主题整体是 MIME 编码词(base64 的"验证您的邮箱"),原样看没有"验证"二字。
    #[test]
    fn decode_mime_header_reads_encoded_subject() {
        // "=?utf-8?b?6aqM6K+B5oKo55qE55S15a2Q6YKu566x?=" = 验证您的电子邮箱 的实测形态
        let s = "=?utf-8?b?6aqM6K+B5oKo55qE55S1?=";
        let decoded = decode_mime_header(s);
        assert!(decoded.contains('验') || decoded.contains('证'), "实际: {decoded}");
        // 明文主题原样返回
        assert_eq!(decode_mime_header("Verify your email"), "Verify your email");
        // 编码词与明文混排
        let mixed = "=?utf-8?b?6aqM6K+B?= please";
        assert!(decode_mime_header(&mixed).contains('验'));
    }

    /// 纯文本(无 MIME 头)邮件不能被解码函数破坏。
    #[test]
    fn decode_mime_body_passes_plain_text_through() {
        let plain = "code: 123456\nhttps://chat.z.ai/auth/verify_email?token=abc";
        assert_eq!(decode_mime_body(plain), plain);
    }

    #[test]
    fn qp_decode_basic() {
        assert_eq!(qp_decode("<a href=3D\"x\">"), "<a href=\"x\">");
        // 软换行被吃掉
        assert_eq!(qp_decode("abc=\r\ndef"), "abcdef");
        assert_eq!(qp_decode("abc=\ndef"), "abcdef");
    }

    /// 页面实测是「点击邮件中的链接完成验证」,链接必须整条取出来(含 email/username),
    /// 不能只取 token——服务端要 email+username 一起校验。
    #[test]
    fn extract_verify_url_from_real_mail() {
        // 实测正文(QP 解码后仍可能残留 =3D / &amp;)
        let body = "https://chat.z.ai/auth/verify_email?token=verify-536948ba7b8959eb3f48a300ef257229&email=tr19zp42m%40mozmail.com&username=123123123&language=zh";
        let url = extract_verify_url(body).expect("应提取到链接");
        assert!(url.starts_with("https://chat.z.ai/auth/verify_email?"));
        assert!(url.contains("token=verify-536948ba7b8959eb3f48a300ef257229"));
        assert!(url.contains("email=tr19zp42m%40mozmail.com"));
        assert!(url.contains("username=123123123"));

        // QP 未解码形态:&amp; 与 =3D 都要还原
        let qp = "href=3D\"https://chat.z.ai/auth/verify_email?token=3Dverify-abc&amp;email=3Dx%40y.com\"";
        let u2 = extract_verify_url(qp).expect("QP 形态也应提取到");
        assert!(u2.contains("token=verify-abc"), "实际: {u2}");
        assert!(u2.contains("&email=x%40y.com"), "实际: {u2}");
        assert!(!u2.contains("=3D"), "不应残留 =3D");

        // HTML 属性里的链接不能被引号截进 URL
        let html = "<a href=\"https://chat.z.ai/auth/verify_email?token=tk1&amp;email=a\">点击</a>";
        let u3 = extract_verify_url(html).expect("HTML 链接应提取到");
        assert_eq!(u3, "https://chat.z.ai/auth/verify_email?token=tk1&email=a");

        // 无 token 的链接不算验证链接
        assert!(extract_verify_url("https://chat.z.ai/auth/login").is_none());
        assert!(extract_verify_url("no link here").is_none());
    }

    /// 日志脱敏:token 只能留前缀。
    #[test]
    fn mask_url_hides_token() {
        let u = "https://chat.z.ai/auth/verify_email?token=verify-SECRETSECRET&email=a@b.com";
        let m = mask_url(u);
        assert!(!m.contains("SECRETSECRET"), "实际: {m}");
        assert!(m.contains("verify-SECR…") || m.contains("…"), "实际: {m}");
        // 无 token 原样返回
        assert_eq!(mask_url("https://x/y"), "https://x/y");
    }

    #[test]
    fn header_field_reads_to_and_date() {
        let raw = "Return-Path: <x@y.com>
Subject: Verify
To: \"Me\" <mv7j0fwzg@mozmail.com>
Date: Tue, 21 Oct 2026 09:15:33 +0800

";
        assert_eq!(header_field(raw, "To"), "\"Me\" <mv7j0fwzg@mozmail.com>");
        assert_eq!(header_field(raw, "Date"), "Tue, 21 Oct 2026 09:15:33 +0800");
        assert_eq!(header_field(raw, "Subject"), "Verify");
        // 找不到返回空
        assert_eq!(header_field(raw, "From"), "");
        // 大小写不敏感
        assert_eq!(header_field(raw, "to"), "\"Me\" <mv7j0fwzg@mozmail.com>");
    }

    #[test]
    fn header_field_handles_folding() {
        // RFC 5322 折行:续行以空格开头
        let raw = "To: someone@x.com,
 mv7j0fwzg@mozmail.com
Date: x
";
        let to = header_field(raw, "To");
        assert!(to.contains("mv7j0fwzg@mozmail.com"), "折行要拼起来: {to}");
    }
}
