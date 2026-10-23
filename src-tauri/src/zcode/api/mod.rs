mod client;

pub use client::{detect_proxy, ZaiClient};

/// ZCode 客户端版本(逆向自 3.14.5,客户端升级后需同步更新,见 HANDOFF §6.4)
pub const APP_VERSION: &str = "3.14.5";
pub const PLATFORM: &str = "win32-x64";
pub const BASE_URL: &str = "https://zcode.z.ai";
/// z.ai 业务 token 换取端点(逆向自客户端 ZaiBusinessTokenResolver):
/// POST {token: 生token} → data.access_token 即业务 token,客户端登录后
/// credentials.json 的 oauth:zai:access_token 存的是它,不是生 token。
pub const ZAI_LOGIN_URL: &str = "https://api.z.ai/api/auth/z/login";
