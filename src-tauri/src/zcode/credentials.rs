//! 本机 ZCode 客户端凭证读取与解密(算法与客户端一致,已由 PoC check-claim.mjs 验证)。
//! 文件:`~/.zcode/v2/credentials.json`,值为 `enc:v1:` 前缀密文。
//! 格式:`enc:v1:` + base64url(iv) + "." + base64url(tag) + "." + base64url(ciphertext),
//! AES-256-GCM,密钥 = sha256(密钥串);密钥串默认
//! `zcode-credential-fallback:win32:{homedir}:{username}`,环境变量 ZCODE_CREDENTIAL_SECRET 可覆盖。

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::path::PathBuf;

use crate::zcode::error::{AppError, AppResult};

const ENC_PREFIX: &str = "enc:v1:";

#[derive(Debug, Clone)]
pub struct LocalCredentials {
    /// 查询/领取用的 JWT(credentials.json 的 `zcodejwttoken` 键)
    pub jwt: String,
    pub refresh_token: String,
    /// 切换账号写回时需要的完整身份字段
    pub user_info: Option<String>,
    pub zai_access_token: Option<String>,
    pub zai_refresh_token: Option<String>,
    pub user_id: Option<String>,
    /// email/phone/nickname/name 依次取第一个非空
    pub display_name: Option<String>,
}

pub fn credentials_path() -> PathBuf {
    home_dir().join(".zcode").join("v2").join("credentials.json")
}

fn home_dir() -> PathBuf {
    std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .map(PathBuf::from)
        .expect("无法定位用户主目录")
}

fn derive_key() -> [u8; 32] {
    let secret = match std::env::var("ZCODE_CREDENTIAL_SECRET") {
        Ok(s) => s,
        Err(_) => {
            let username = std::env::var("USERNAME").unwrap_or_default();
            format!("zcode-credential-fallback:win32:{}:{}", home_dir().display(), username)
        }
    };
    Sha256::digest(secret.as_bytes()).into()
}

fn decrypt_value(key: &[u8; 32], value: &str) -> AppResult<String> {    if !value.starts_with(ENC_PREFIX) {
        return Ok(value.to_string()); // 明文直接返回
    }
    let body = &value[ENC_PREFIX.len()..];
    let parts: Vec<&str> = body.split('.').collect();
    if parts.len() != 3 {
        return Err(AppError::Other("凭证密文格式非法".into()));
    }
    let decode = |s: &str| -> AppResult<Vec<u8>> {
        URL_SAFE_NO_PAD
            .decode(s.trim_end_matches('='))
            .map_err(|e| AppError::Other(format!("base64 解码失败: {e}")))
    };
    let iv = decode(parts[0])?;
    let tag = decode(parts[1])?;
    let data = decode(parts[2])?;

    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key));
    let mut ciphertext = data;
    ciphertext.extend_from_slice(&tag);
    let plain = cipher
        .decrypt(
            Nonce::from_slice(&iv),
            Payload {
                msg: &ciphertext,
                aad: b"",
            },
        )
        .map_err(|_| AppError::Other("凭证解密失败(密钥不匹配?)".into()))?;
    String::from_utf8(plain).map_err(|_| AppError::Other("凭证解密后非 UTF-8".into()))
}

fn plain_field(
    map: &serde_json::Map<String, Value>,
    key: &[u8; 32],
    name: &str,
) -> AppResult<Option<String>> {
    match map.get(name).and_then(|v| v.as_str()) {
        Some(s) => Ok(Some(decrypt_value(key, s)?)),
        None => Ok(None),
    }
}

/// 加密为客户端同款格式:`enc:v1:` + b64url(iv) + "." + b64url(tag) + "." + b64url(ciphertext)
pub fn encrypt_value(key: &[u8; 32], plain: &str) -> String {
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key));
    let uuid = uuid::Uuid::new_v4();
    let iv = &uuid.as_bytes()[..12]; // GCM 标准 96 位 nonce,取自 OS 随机
    let sealed = cipher
        .encrypt(
            Nonce::from_slice(iv),
            Payload {
                msg: plain.as_bytes(),
                aad: b"",
            },
        )
        .expect("AES-GCM 加密不会失败");
    let (data, tag) = sealed.split_at(sealed.len() - 16);
    format!(
        "{ENC_PREFIX}{}.{}.{}",
        URL_SAFE_NO_PAD.encode(iv),
        URL_SAFE_NO_PAD.encode(tag),
        URL_SAFE_NO_PAD.encode(data)
    )
}

/// 切换账号时写回 credentials.json 的内容;None 的键会从文件中移除。
/// `zai_token_valid` = 目标账号的 zai access_token 是否还有效:
/// - true:完整切换(active_provider + access_token + jwt)
/// - false:只写 jwt,不碰 zai OAuth 相关键——ZCode 3.15 实测「jwt 有效 +
///   无 zai token」可正常启动(写失效 token 反而会导致启动 OAuth 校验失败后静默退出)
pub struct SwitchPayload {
    pub jwt: String,
    pub user_info: Option<String>,
    pub zai_access_token: Option<String>,
    pub zai_refresh_token: Option<String>,
    pub zai_token_valid: bool,
}

/// 客户端恢复会话第一步就读它(逆向:restoreCachedSession → getActiveProvider),
/// 缺失 = 直接判"未登录"要求重新登录,与 JWT 是否有效无关。本工具只管理 zai 账号。
const OAUTH_ACTIVE_PROVIDER: &str = "zai";

fn set_or_remove(
    map: &mut serde_json::Map<String, Value>,
    key: &[u8; 32],
    name: &str,
    value: Option<&str>,
) {
    // None 时移除而非保留旧值:残留上一个账号的 token 会造成双账号身份串号,
    // 客户端启动校验失败会主动登出并清空整份会话(oauth-logout),比缺键更糟
    match value {
        Some(v) => {
            map.insert(name.to_string(), Value::String(encrypt_value(key, v)));
        }
        None => {
            map.remove(name);
        }
    }
}

/// 将目标账号写回 credentials.json(先备份为 credentials.json.bak)
pub fn write_account_switch(payload: &SwitchPayload) -> AppResult<PathBuf> {
    let path = credentials_path();
    let raw = std::fs::read_to_string(&path)
        .map_err(|e| AppError::Other(format!("读取凭证失败: {e}")))?;
    let mut map: serde_json::Map<String, Value> = serde_json::from_str(&raw)
        .map_err(|e| AppError::Other(format!("凭证文件解析失败: {e}")))?;
    let backup = path.with_extension("json.bak");
    std::fs::copy(&path, &backup)
        .map_err(|e| AppError::Other(format!("备份凭证失败: {e}")))?;

    let key = derive_key();
    map.insert(
        "zcodejwttoken".to_string(),
        Value::String(encrypt_value(&key, &payload.jwt)),
    );
    if payload.zai_token_valid {
        // 必须显式写回:客户端(或其自身登出逻辑)清掉后,不补这个键客户端永远显示未登录
        map.insert(
            "oauth:active_provider".to_string(),
            Value::String(encrypt_value(&key, OAUTH_ACTIVE_PROVIDER)),
        );
        set_or_remove(&mut map, &key, "oauth:zai:access_token", payload.zai_access_token.as_deref());
        set_or_remove(&mut map, &key, "oauth:zai:refresh_token", payload.zai_refresh_token.as_deref());
    } else {
        // zai token 失效:不动 zai OAuth 三件套(移除而非写空,与客户端自身登出后的形态一致),
        // 客户端凭 jwt 恢复会话即可用;写失效 token 会导致启动校验失败静默退出
        map.remove("oauth:active_provider");
        map.remove("oauth:zai:access_token");
        map.remove("oauth:zai:refresh_token");
    }
    set_or_remove(&mut map, &key, "oauth:zai:user_info", payload.user_info.as_deref());

    std::fs::write(&path, serde_json::to_string_pretty(&map).map_err(|e| AppError::Other(format!("序列化凭证失败: {e}")))?)
        .map_err(|e| AppError::Other(format!("写入凭证失败: {e}")))?;
    Ok(backup)
}

pub fn load_local() -> AppResult<LocalCredentials> {
    let path = credentials_path();
    let raw = std::fs::read_to_string(&path).map_err(|_| {
        AppError::Other(format!(
            "未找到本机凭证 {}(请先在 ZCode 客户端登录)",
            path.display()
        ))
    })?;
    let map: serde_json::Map<String, Value> =
        serde_json::from_str(&raw).map_err(|e| AppError::Other(format!("凭证文件解析失败: {e}")))?;
    let key = derive_key();

    let jwt = plain_field(&map, &key, "zcodejwttoken")?
        .ok_or_else(|| AppError::Other("凭证中缺少 zcodejwttoken".into()))?;
    let refresh_token =
        plain_field(&map, &key, "oauth:zai:refresh_token")?.unwrap_or_default();
    let zai_access_token = plain_field(&map, &key, "oauth:zai:access_token")?;
    let user_info_raw = plain_field(&map, &key, "oauth:zai:user_info")?;

    let user_info: Value = user_info_raw
        .as_deref()
        .and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or(Value::Null);

    let str_field = |name: &str| user_info[name].as_str().map(str::to_string);
    let user_id = str_field("id")
        .or_else(|| str_field("userId"))
        .or_else(|| str_field("sub"));
    let display_name = str_field("email")
        .or_else(|| str_field("phone"))
        .or_else(|| str_field("nickname"))
        .or_else(|| str_field("name"));

    let zai_refresh = Some(refresh_token.clone()).filter(|s| !s.is_empty());
    Ok(LocalCredentials {
        jwt,
        refresh_token,
        user_info: user_info_raw,
        zai_access_token,
        zai_refresh_token: zai_refresh,
        user_id,
        display_name,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 验证与 ZCode 客户端一致的解密链路(依赖本机已登录)
    #[test]
    fn decrypt_local_credentials() {
        let c = load_local().expect("加载本机凭证失败(本机需已登录 ZCode)");
        assert!(c.jwt.len() > 100, "JWT 长度异常: {}", c.jwt.len());
        println!("display_name = {:?}", c.display_name);
        println!("user_id = {:?}", c.user_id);
    }

    /// 加密→解密回环:切换账号写回 credentials.json 的正确性保证
    #[test]
    fn encrypt_roundtrip() {
        let key = derive_key();
        let plain = "test-jwt-abc.123/加解密回环~";
        let sealed = encrypt_value(&key, plain);
        assert!(sealed.starts_with(ENC_PREFIX));
        assert_eq!(decrypt_value(&key, &sealed).unwrap(), plain);
    }
}
