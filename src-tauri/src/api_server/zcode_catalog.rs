//! ZCode 上游模型目录与凭证（z.ai GLM OAuth 账号池，api.z.ai/api/anthropic）
//!
//! 目录为静态内置表——与 ZCode 客户端 `~/.zcode/v2/config.json` 的
//! `builtin:zai` / `builtin:zai-coding-plan` provider models 段一致
//! （GLM-5.3 / GLM-5.3-Flash，1M 上下文 / 128K 输出，思考档位 low·max·high）。
//! 上游 `GET /api/anthropic/v1/models` 实测可用（Bearer zai token，返回 11 个
//! glm-* id），此处只收录 ZCode 客户端声明的两款——网关 GLM 分流只服务这两款。
//!
//! 凭证：账号的 `zai_access_token`（HS512 JWT，payload 含 user_id/customer_id，
//! 无 exp 字段，服务端长期有效直至吊销）+ `device_mid`（请求头 x-device-mid）。

use serde::Serialize;

/// 上游推理端点（anthropic messages 协议；实测 Bearer zai token 可达，
/// 无资源包账号返回 429 code=1113「Insufficient balance or no resource package」）
pub const MESSAGES_PATH: &str = "/api/anthropic/v1/messages";
/// 上游模型清单端点（探测/目录刷新用）
pub const MODELS_PATH: &str = "/api/anthropic/v1/models";
/// z.ai 推理基址（与 ZAI_LOGIN_URL 同域）
pub const BASE_URL: &str = "https://api.z.ai";

/// ZCode 凭证（identity 回调返回值）：池内只存 uid，凭证按次解析
#[derive(Debug, Clone, Default, Serialize)]
pub struct ZCodeCreds {
    /// zai access_token（Bearer 鉴权用）
    pub access_token: String,
    /// 账号 device_mid（x-device-mid 请求头）
    pub device_mid: String,
}

/// 目录条目（静态表唯一形态）
#[derive(Debug, Clone)]
pub struct ZCodeModel {
    /// 上游模型 id（anthropic messages 的 model 字段）
    pub id: &'static str,
    /// 展示名
    pub display: &'static str,
    /// 上下文窗口（token）
    pub context_length: u64,
    /// 输出上限（token）
    pub max_tokens: u64,
    /// 思考档位（GLM-5.3 系全系支持 low/max/high；空 = 不声明）
    pub efforts: &'static [&'static str],
    /// 图片输入支持
    pub supports_image: bool,
}

/// 静态目录：GLM-5.3 / GLM-5.3-Flash（口径 = ZCode 客户端 config.json limit 段：
/// context 1000000 / output 128000，reasoning variants [low, max, high]）
pub static MODELS: &[ZCodeModel] = &[
    ZCodeModel {
        id: "GLM-5.3",
        display: "GLM-5.3",
        context_length: 1_000_000,
        max_tokens: 128_000,
        efforts: &["low", "max", "high"],
        supports_image: true,
    },
    ZCodeModel {
        id: "GLM-5.3-Flash",
        display: "GLM-5.3-Flash",
        context_length: 1_000_000,
        max_tokens: 128_000,
        efforts: &["low", "max", "high"],
        // 客户端 modalities.input 含 video（网关 v1 不区分，按图片同权处理）
        supports_image: true,
    },
];

/// 目录命中解析：请求模型名 → 归一化 id（trim + 大小写不敏感比对）。
/// 命中返回目录条目的 `id` 字面量（执行路径据此构造上游请求）
pub fn resolve(model: &str) -> Option<String> {
    let want = canonical(model);
    MODELS
        .iter()
        .find(|m| canonical(m.id) == want)
        .map(|m| m.id.to_string())
}

/// 按归一化 id 查条目
pub fn find(id: &str) -> Option<&'static ZCodeModel> {
    let want = canonical(id);
    MODELS.iter().find(|m| canonical(m.id) == want)
}

fn canonical(id: &str) -> String {
    id.trim().to_lowercase()
}

/// 目录 JSON 化（unified_catalog 合并用，字段名与 qoder_upstream::list 条目对齐）
pub fn list_json() -> Vec<serde_json::Value> {
    MODELS
        .iter()
        .map(|m| {
            serde_json::json!({
                "id": m.id,
                "name": m.display,
                "maxInputTokens": m.context_length,
                "maxOutputTokens": m.max_tokens,
                "supportsImages": m.supports_image,
                "efforts": m.efforts,
                "vendor": "智谱 AI",
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_matches_case_insensitive() {
        assert_eq!(resolve("GLM-5.3").as_deref(), Some("GLM-5.3"));
        assert_eq!(resolve("glm-5.3").as_deref(), Some("GLM-5.3"));
        assert_eq!(resolve(" glm-5.3-flash ").as_deref(), Some("GLM-5.3-Flash"));
        assert!(resolve("glm-4.5").is_none());
        assert!(resolve("").is_none());
    }

    #[test]
    fn find_returns_entry_with_limits() {
        let m = find("glm-5.3").expect("hit");
        assert_eq!(m.context_length, 1_000_000);
        assert_eq!(m.max_tokens, 128_000);
        assert_eq!(m.efforts, &["low", "max", "high"]);
    }

    #[test]
    fn list_json_shape_matches_qoder_convention() {
        let j = list_json();
        assert_eq!(j.len(), 2);
        assert_eq!(j[0]["id"], "GLM-5.3");
        assert_eq!(j[0]["maxInputTokens"], 1_000_000);
        assert_eq!(j[0]["vendor"], "智谱 AI");
    }
}
