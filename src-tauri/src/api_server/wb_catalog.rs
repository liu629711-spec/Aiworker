//! WorkBuddy 模型目录（T2.1/F-37 静态兜底层）
//!
//! §5.6 原则：
//! - 静态兜底目录落 kv（缺失时以本文件内置表写入）；
//! - 能力声明永远读上游字段（inputModalities/supportedEfforts），勿硬编码——
//!   本表仅作上游目录接口不可用时的兜底。上游源按区分叉（2026-10-17 实测）：
//!   CN = `GET {chatBase}/console/enterprises/personal/models`（批次 4 F-37）；
//!   Global = `GET {chatBase}/v3/config` 的 `data.models`（海外桌面客户端
//!   产品配置同源，credits 计费倍率与客户端选择器一致）。注意 `/v2/enterprises/
//!   personal/models` 是 **platform=CLI** 视图，倍率与桌面端不同（hy4-preview
//!   在该视图 x0.00、桌面端 x0.29），不可作 Global 目录源——曾致账号页免费档错标。
//! - `effort_override` 为实测修正层（hy3 系列仅 high 真正生效，v1.2），
//!   优先级：修正层 > 客户端请求 > 上游默认。

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::Path;

/// 单个模型的能力声明
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WbModel {
    /// 模型 id（客户端请求的 model 字段，小写）
    pub id: String,
    /// 展示名
    #[serde(default)]
    pub display: String,
    /// 上下文长度（token）
    #[serde(default)]
    pub context_length: u64,
    /// 单次最大输出
    #[serde(default)]
    pub max_tokens: u64,
    /// 是否支持图片模态（读上游 inputModalities，勿拍脑袋）
    #[serde(default)]
    pub supports_image: bool,
    /// 支持的 reasoning_effort 档位（升序）
    #[serde(default)]
    pub supported_efforts: Vec<String>,
    /// 实测修正层：按模型族强制映射（如 "high"）
    #[serde(default)]
    pub effort_override: Option<String>,
    /// 积分倍率（展示用，成本模型按无缓存估算 §5.5 #10）
    #[serde(default)]
    pub rate: f64,
    /// 所属区（"cn" / "global"；取号层据此只出该区账号——国内/海外号池
    /// 逻辑隔离，国内模型不出海外号、反之亦然）。内置表 = "cn"（国内实测快照）；
    /// 上游分区目录刷新时按区写入。
    #[serde(default = "default_cn")]
    pub region: String,
}

fn default_cn() -> String {
    "cn".to_string()
}

impl WbModel {
    /// reasoning_effort 降级：请求档位不受支持时按「≤ 请求值的最大支持档位，
    /// 否则最低档位」降级（§5.5 #3）
    pub fn resolve_effort(&self, requested: Option<&str>) -> Option<String> {
        // 修正层最优先（v1.2 §5.6）
        if let Some(o) = &self.effort_override {
            if !o.is_empty() {
                return Some(o.clone());
            }
        }
        if self.supported_efforts.is_empty() {
            return requested.map(str::to_string);
        }
        let req = match requested {
            Some(r) if !r.trim().is_empty() => r.trim().to_lowercase(),
            _ => return None, // 未请求 → 不下发，走上游默认
        };
        if self.supported_efforts.iter().any(|e| e == &req) {
            return Some(req);
        }
        let order = ["minimal", "low", "medium", "high", "xhigh", "max"];
        let rank = |e: &str| order.iter().position(|o| *o == e).unwrap_or(usize::MAX);
        let req_rank = rank(&req);
        // ≤ 请求档位的最大支持档位；都没有则最低档
        self.supported_efforts
            .iter()
            .filter(|e| rank(e) <= req_rank)
            .max_by_key(|e| rank(e))
            .or_else(|| {
                self.supported_efforts
                    .iter()
                    .min_by_key(|e| rank(e))
            })
            .cloned()
    }
}

/// 内置静态兜底目录（15 模型，§5.6；倍率为 2026-09 WorkBuddy 客户端截图快照，
/// 多界面并列时取最低：hy4-preview / hy3 限时免费 0.00x，deepseek-v4-flash 0.03x，
/// glm-5.3 / glm-5.2 0.78x，deepseek-v4-pro 0.51x，kimi-k3 / kimi-k3-1 1.62x，
/// minimax-m3 0.25x，qwen3.8-max 1.50x）
pub fn builtin() -> Vec<WbModel> {
    let m = |id: &str,
             display: &str,
             ctx: u64,
             mt: u64,
             img: bool,
             efforts: &[&str],
             ov: Option<&str>,
             rate: f64| {
        WbModel {
            id: id.to_string(),
            display: display.to_string(),
            context_length: ctx,
            max_tokens: mt,
            supports_image: img,
            supported_efforts: efforts.iter().map(|s| s.to_string()).collect(),
            effort_override: ov.map(str::to_string),
            rate,
            region: default_cn(),
        }
    };
    vec![
        m("hy4-preview", "Hy4 Preview", 1_000_000, 128_000, true, &["low", "medium", "high"], None, 0.00),
        m("hy4", "Hy4", 1_000_000, 128_000, true, &["low", "medium", "high"], None, 0.20),
        m("hy3-x", "Hy3-X", 200_000, 64_000, false, &["high"], Some("high"), 0.05),
        m("hy3", "Hy3", 200_000, 64_000, false, &["high"], Some("high"), 0.00),
        m("glm-5.3", "GLM-5.3", 200_000, 96_000, true, &["low", "medium", "high"], None, 0.78),
        m("glm-5.3-flash", "GLM-5.3 Flash", 128_000, 64_000, false, &["low", "medium", "high"], None, 0.06),
        m("glm-5.2", "GLM-5.2", 128_000, 64_000, false, &["low", "medium", "high"], None, 0.78),
        m("glm-5", "GLM-5", 128_000, 64_000, false, &["low", "medium", "high"], None, 0.20),
        m("kimi-k3-1", "Kimi K3.1", 256_000, 64_000, true, &["medium", "high"], None, 1.62),
        m("kimi-k3", "Kimi K3", 256_000, 64_000, false, &["medium", "high"], None, 1.62),
        m("deepseek-v4-flash", "DeepSeek V4 Flash", 168_000, 32_000, false, &["low", "medium"], None, 0.03),
        m("deepseek-v4-pro", "DeepSeek V4 Pro", 168_000, 64_000, false, &["medium", "high"], None, 0.51),
        m("minimax-m3", "MiniMax M3", 200_000, 64_000, false, &["low", "medium", "high"], None, 0.25),
        m("qwen3.8-max", "Qwen3.8 Max", 262_000, 64_000, true, &["low", "medium", "high"], None, 1.50),
        m("qwen-3.7-plus", "Qwen 3.7 Plus", 131_000, 32_000, false, &["low", "medium", "high"], None, 0.25),
    ]
}

/// 海外区内置兜底目录（2026-10-17 从海外客户端 5.8.4 安装目录
/// `resources/app.asar.unpacked/cli/product.json` 固化——客户端本地基础配置，
/// 与远程 `/v3/config` 并集合并（远程同 id 优先）。仅收录带 credits 计费的
/// 文本模型；`default-model`（Auto 智能路由，上游不标价）与图像/视频模型
/// 不入库；gpt-6-astra 为 Pro 套餐锁定模型（试用账号不可用），按用户裁决
/// 不收录展示。免费档：hy3 / step-5-preview / deepseek-v4.1-flash 0.00x。
pub fn builtin_global() -> Vec<WbModel> {
    let m = |id: &str,
             display: &str,
             ctx: u64,
             mt: u64,
             img: bool,
             efforts: &[&str],
             ov: Option<&str>,
             rate: f64| {
        WbModel {
            id: id.to_string(),
            display: display.to_string(),
            context_length: ctx,
            max_tokens: mt,
            supports_image: img,
            supported_efforts: efforts.iter().map(|s| s.to_string()).collect(),
            effort_override: ov.map(str::to_string),
            rate,
            region: "global".to_string(),
        }
    };
    vec![
        m("fast-model", "Fast", 200_000, 32_000, true, &["medium"], None, 0.34),
        m("balanced-model", "Balanced", 256_000, 32_000, true, &["medium"], None, 0.59),
        m("primary-model", "Primary", 272_000, 72_000, true, &["high"], None, 1.70),
        m("deep-model", "Deep", 176_000, 24_000, true, &[], None, 2.20),
        m("deepseek-v4.1-flash", "Deepseek-V4.1-Flash", 1_000_000, 128_000, true, &["high"], None, 0.00),
        m("deepseek-v4.1-flash-sg", "Deepseek-V4.1-Flash", 1_000_000, 128_000, true, &["high"], None, 0.03),
        m("hy4-preview", "Hy4 preview", 1_000_000, 64_000, true, &["high"], None, 0.29),
        m("hy3", "Hy3", 192_000, 64_000, true, &["low", "high"], None, 0.00),
        // 海外免费档第三个（2026-10-17 用户实测客户端选择器：Hy3/Step-5-Preview/
        // Deepseek-V4.1-Flash 均标 Free now；各海外配置源未下发，按实测收录）
        m("step-5-preview", "Step-5-Preview", 1_000_000, 64_000, true, &["low", "medium", "high"], None, 0.00),
        m("kimi-k2.8-preview", "Kimi-K2.8-Preview", 1_000_000, 32_000, true, &["low", "high", "max"], None, 0.77),
        m("gpt-5.6-sol", "GPT-5.6-Sol", 1_000_000, 128_000, true, &["low", "medium", "high", "xhigh", "max"], None, 3.47),
        m("gpt-5.6-terra", "GPT-5.6-Terra", 1_000_000, 128_000, true, &["low", "medium", "high", "xhigh", "max"], None, 1.39),
        m("gpt-5.6-luna", "GPT-5.6-Luna", 1_000_000, 128_000, true, &["low", "medium", "high", "xhigh", "max"], None, 0.14),
        m("gpt-5.5", "GPT-5.5", 1_000_000, 128_000, true, &["low", "medium", "high", "xhigh"], None, 3.31),
        m("gpt-5.4", "GPT-5.4", 272_000, 72_000, true, &["low", "medium", "high", "xhigh"], None, 1.65),
        m("gemini-3.5-flash", "Gemini-3.5-Flash", 1_000_000, 65_536, true, &["medium"], None, 0.99),
        m("glm-5.3-flash", "GLM-5.3-Flash", 1_000_000, 32_000, true, &["low", "high", "max"], None, 0.06),
        m("glm-5.3", "GLM-5.3", 1_000_000, 48_000, true, &["low", "high", "max"], None, 0.79),
        m("glm-5.2", "GLM-5.2", 1_000_000, 48_000, true, &["high", "xhigh"], None, 0.79),
        m("kimi-k3", "Kimi-K3", 1_000_000, 32_000, true, &["medium"], None, 1.62),
        m("kimi-k2.6", "Kimi-K2.6", 256_000, 32_000, true, &["medium"], None, 0.52),
    ]
}

/// 目录文件路径（data/wb_model_catalog.json，§3.8）。
/// 数据文件统一 data/ 子目录（与 api_models/api_keys/api_usage 同层）；
/// Buddy 侧不保历史（§9.4 #7）：旧根目录位置不迁移，缺失即内置兜底重建
/// 内置表版本：倍率等快照更新时 +1。由旧版本内置表自动落盘的目录（fetched_at 为空且
/// builtin_rev 低于当前值）会按新内置表重建，使倍率修正对存量安装生效；
/// 上游同步写入的目录（fetched_at 非空）不受影响。人工维护的目录请把 builtin_rev
/// 手工改为当前值以退出重建。
pub const BUILTIN_REV: u32 = 4;

/// 目录 kv 存储键按区域拆分（修复「海外账号模型目录」）：CN 沿用存量键
/// `wb_model_catalog`（兼容既有安装），Global 落新键 `wb_model_catalog_global`——
/// 两区模型清单由上游各自下发（id/倍率/免费档均可不同），单份存储会让两区
/// 互相覆盖（海外账号刷一次目录就把国内模型冲掉）。读取侧 [`load`] 合并两区。
const KV_KEY_CN: &str = "wb_model_catalog";
pub const KV_KEY_GLOBAL: &str = "wb_model_catalog_global";
/// 合并结果热路径缓存键（dispatch 每请求读取；fetch_and_replace 写路径显式失效）
const CACHE_KEY_MERGED: &str = "wb_model_catalog_merged";

/// 单区目录读取：上游同步结果优先保留；旧版本内置表落盘的目录按新内置表重建
/// （fetched_at 为空且 builtin_rev 低于当前值）。Global 键缺失/为空时落
/// [`builtin_global`]（海外客户端 product.json 固化快照，非国内表冒充）。
fn load_region(data_dir: &Path, global: bool) -> Vec<WbModel> {
    let key = if global { KV_KEY_GLOBAL } else { KV_KEY_CN };
    let read = || -> Option<Vec<WbModel>> {
        let file = crate::store::db(data_dir)
            .kv_get_raw(key)
            .and_then(|t| serde_json::from_str::<WbCatalogFile>(&t).ok())?;
        if file.models.is_empty() {
            return None;
        }
        // 上游同步结果优先保留；否则旧版本内置表落盘的目录按新内置表重建
        (file.fetched_at.is_some() || file.builtin_rev >= BUILTIN_REV).then_some(file.models)
    };
    if let Some(models) = read() {
        return tag_region(models, global);
    }
    // 缺失时落盘各区内置表并返回（Global 表见 builtin_global 文档）
    let builtin = if global { builtin_global() } else { builtin() };
    let _ = crate::store::db(data_dir).kv_set(
        key,
        &WbCatalogFile {
            models: builtin.clone(),
            fetched_at: None,
            builtin_rev: BUILTIN_REV,
        },
    );
    builtin
}

/// 给目录条目打区标记（存量无 region 字段的记录统一按所在区补上；
/// 上游分区目录若已带 region 则尊重原值——两侧恒为该区值）
fn tag_region(models: Vec<WbModel>, global: bool) -> Vec<WbModel> {
    let r = if global { "global" } else { "cn" };
    models
        .into_iter()
        .map(|mut m| {
            if m.region.is_empty() {
                m.region = r.to_string();
            }
            m
        })
        .collect()
}

/// 两区目录合并：CN 在前（同 id CN 优先，保持存量行为），Global 独有条目追加
/// ——dispatch 的 Buddy 源判定、统一模型目录、免费模型清单由此覆盖双区模型。
fn merge_regions(cn: Vec<WbModel>, global: Vec<WbModel>) -> Vec<WbModel> {
    let mut out = cn;
    let seen: std::collections::HashSet<String> =
        out.iter().map(|m| m.id.to_lowercase()).collect();
    for m in global {
        if !seen.contains(&m.id.to_lowercase()) {
            out.push(m);
        }
    }
    out
}

/// 目录文件结构（支持上游动态替换后的全量覆盖）
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct WbCatalogFile {
    #[serde(default)]
    pub models: Vec<WbModel>,
    #[serde(default)]
    pub fetched_at: Option<i64>,
    #[serde(default)]
    pub builtin_rev: u32,
}

/// 加载目录：CN 键 + Global 键合并（两区模型并集；同 id CN 优先），
/// 任一区缺失/为空时按区兜底（CN 回内置表、Global 回空）。合并结果走
/// 热路径缓存（`wb_model_catalog_merged`），fetch_and_replace 写路径显式失效。
/// SQLite 化（P2）：原 data/wb_model_catalog.json → kv 键（热路径单行读取替代解析缓存）。
pub fn load(data_dir: &Path) -> Vec<WbModel> {
    super::config_cache::get_or_load(data_dir, CACHE_KEY_MERGED, || {
        merge_regions(load_region(data_dir, false), load_region(data_dir, true))
    })
}

/// 海外区目录只读视图（ZCode 页「海外号池」面板用）：load_region(true) 的
/// pub 包装——合并读取（load）对同名模型 CN 优先，海外专属条目才可见；
/// 本函数直接取海外区原始清单（含两区同名模型）
pub fn list_global(data_dir: &Path) -> Vec<WbModel> {
    list_region(data_dir, true)
}

/// 指定区域目录只读视图（合并读取对同名模型 CN 优先，区域口径须直读分区）
pub fn list_region(data_dir: &Path, global: bool) -> Vec<WbModel> {
    load_region(data_dir, global)
}

/// 大小写不敏感查找
pub fn find<'a>(catalog: &'a [WbModel], model: &str) -> Option<&'a WbModel> {
    let lower = model.trim().to_lowercase();
    catalog.iter().find(|m| m.id == lower)
}

/// 模型 → 所属区（取号层号池隔离的依据）：Some(true)=海外、Some(false)=国内、
/// None=目录未收录（别名/透传语义）→ 取号不限区（保持既有行为）。
/// 双区同名模型合并时 CN 优先（merge_regions 语义），故命中即 CN。
pub fn region_of(data_dir: &Path, model: &str) -> Option<bool> {
    find(&load(data_dir), model).map(|m| m.region == "global")
}

/// 目录中免费模型清单（免费额度可见性，Buddy 域）。
///
/// 判定依据：目录 rate == 0.0（不消耗积分的限时免费档，实测 hy3 / step-5-preview）。
/// 返回 [{id,name,rate}]——与 Qoder 域 free_tier_models 同形（便于前端统一渲染），
/// rate 恒为 0.0。目录加载失败回退内置表（load 自身语义），因此本函数恒有结果。
pub fn free_models(data_dir: &Path) -> Vec<Value> {
    free_models_impl(&load(data_dir))
}

/// 指定区域的免费模型清单（账号页回写用）：海外账号只显示 Global 目录的
/// 免费档，不再混入国内目录（两区免费模型/倍率由上游各自下发）。
/// 实测口径（2026-10-17，海外桌面客户端 + `/v3/config`）：海外免费档 =
/// hy3 / deepseek-v4.1-flash（hy4-preview 0.29x 不免费）；国内 = hy3 /
/// step-5-preview。
pub fn free_models_for(data_dir: &Path, global: bool) -> Vec<Value> {
    free_models_impl(&load_region(data_dir, global))
}

fn free_models_impl(catalog: &[WbModel]) -> Vec<Value> {
    catalog
        .iter()
        .filter(|m| m.rate == 0.0)
        .map(|m| {
            serde_json::json!({
                "id": m.id,
                "name": if m.display.is_empty() { m.id.clone() } else { m.display.clone() },
                "rate": 0.0,
                // 条目归属区（国内/海外目录不同，前端按区分组展示模型名）
                "region": m.region,
            })
        })
        .collect()
}

// ==================== T5.1/F-37 动态目录替换 ====================

/// 条目是否具备「模型能力特征」——至少含一个能力/定价字段。
/// 2026-09 实测：agents 容器里无 models 子数组的 agent 条目（仅 name/description/
/// instructions/tools/commands，如 general-purpose/compact/plan）会被定向收集兜进来，
/// 此处按特征剔除，防 agent/命令名污染目录（agent 不是模型人格，models 挂字符串数组）。
fn looks_like_model(item: &Value) -> bool {
    const KEYS: &[&str] = &[
        "maxOutputTokens",
        "maxInputTokens",
        "maxTokens",
        "max_tokens",
        "credits",
        "rate",
        "ratio",
        "price",
        "creditRate",
        "contextLength",
        "context_length",
        "inputTokenLimit",
        "inputModalities",
        "input_modalities",
        "modalities",
        "supportsImages",
        "reasoning",
        "supportedEfforts",
        "supported_efforts",
    ];
    KEYS.iter().any(|k| item.get(*k).is_some())
}

/// 噪音模型 id 黑名单：路由别名 / 内部条目，不是可调度的真实模型。
/// 2026-09-19 用户确认 + 抓包实证（30 条目录 vs 客户端 14 条可用模型）：
/// - "default"：通用路由别名（vendor v，兜底倍率 x2.20）；
/// - "auto"：AutoMode 元模型（客户端由 productFeatures.AutoMode 开关单独渲染，非菜单项）；
/// - "hunyuan-chat"：无定价内部条目（仅 supportsToolCall + 尺寸字段）。
/// "hunyuan-image-v3.0"（画图条目，零能力字段）已由 looks_like_model 剔除，无需入名单。
fn is_noise_model_id(id: &str) -> bool {
    matches!(id, "auto" | "default" | "hunyuan-chat")
}

/// 官方客户端过滤逻辑逆向（2026-09-19 抓包实证）：客户端模型菜单 =
/// `cli` agent（tags 含 cli+default）的 models 白名单 ∩ data.models 元数据，
/// 菜单顺序也与该白名单一致；auto 由 AutoMode 开关单独渲染不入菜单。
/// 同名双档（hy3/hy3-x 同名 Hy3、hy4-preview/hy4-preview-x 同名 Hy4 preview）
/// 客户端按展示名去重只显示首个，但二者是真实不同价档位，网关侧保留双档。
/// 返回 None = 响应无 cli 主 agent（旧形态/端点变更），不做白名单过滤。
fn cli_agent_whitelist(body: &Value) -> Option<Vec<String>> {
    let agents = body
        .get("data")
        .and_then(|d| d.get("agents"))
        .or_else(|| body.get("agents"))?
        .as_array()?;
    for a in agents {
        let tags: Vec<&str> = a
            .get("tags")
            .and_then(|t| t.as_array())
            .map(|arr| arr.iter().filter_map(|t| t.as_str()).collect())
            .unwrap_or_default();
        if !tags.contains(&"cli") || !tags.contains(&"default") {
            continue;
        }
        let ids: Vec<String> = a
            .get("models")?
            .as_array()?
            .iter()
            .filter_map(|m| m.as_str().map(str::to_string))
            .collect();
        if !ids.is_empty() {
            return Some(ids);
        }
    }
    None
}

/// 从单条候选对象解析模型（字段链宽容探测，能力字段读上游勿硬编码）。
/// 2026-09 实测字段链（copilot.tencent.com console/personal/models data.models[]）：
/// - 倍率 `credits`："x0.05" / "x0.00 credits" 字符串；
/// - 档位嵌套 `reasoning.supportedEfforts`（数组）或 `reasoning.effort`（固定单档）；
/// - 上下文 `maxInputTokens`（兜底 `maxAllowedSize` / `contextWindow.defaultLength`）；
/// - 图片 `supportsImages` 布尔（`disabledMultimodal: true` 时禁用多模态）；
/// - 最大输出 `maxOutputTokens`（此前唯一命中的键）。
fn model_from_item(item: &Value) -> Option<WbModel> {
    if !item.is_object() {
        return None;
    }
    let dig_str = |v: &Value, keys: &[&str]| -> Option<String> {
        keys.iter().find_map(|k| {
            v.get(*k).and_then(|x| x.as_str()).map(str::to_string).or_else(|| {
                v.get(*k).and_then(|x| x.as_i64()).map(|n| n.to_string())
            })
        })
    };
    let dig_u64 = |v: &Value, keys: &[&str]| -> Option<u64> {
        keys.iter().find_map(|k| {
            v.get(*k)
                .and_then(|x| x.as_u64())
                .or_else(|| v.get(*k).and_then(|x| x.as_str()).and_then(|s| s.parse().ok()))
        })
    };
    let dig_f64 = |v: &Value, keys: &[&str]| -> Option<f64> {
        keys.iter().find_map(|k| {
            v.get(*k)
                .and_then(|x| x.as_f64())
                .or_else(|| v.get(*k).and_then(|x| x.as_str()).and_then(|s| s.parse().ok()))
        })
    };
    let dig_strs = |v: &Value, keys: &[&str]| -> Vec<String> {
        keys.iter()
            .find_map(|k| {
                v.get(*k)
                    .and_then(|x| x.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|s| s.as_str().map(str::to_string))
                            .collect::<Vec<_>>()
                    })
                    .filter(|a| !a.is_empty())
            })
            .unwrap_or_default()
    };
    let Some(id) = dig_str(item, &["id", "model", "modelId", "name"])
        .map(|s| s.trim().to_lowercase())
        .filter(|s| !s.is_empty())
    else {
        return None;
    };
    // 倍率：credits 字符串形如 "x0.05" / "x0.00 credits"（前缀倍数标记 + 可能的单位尾巴）；
    // 数字形态或旧键（rate/ratio/price/creditRate）兜底。
    // credits 键存在但为空串（/v3/config 的 default-model/Auto 智能路由不标价）→
    // 无计价信息，条目跳过：解析成 0.0 会混入免费档（rate==0 判定）。
    //
    // 2026-10-22：`priceFactor` 与 `price_factor` 一并识别（Qoder 侧上游已确认
    // 会下发「限时免费」= priceFactor:0 + originalPriceFactor:原价；Buddy 目录
    // 来自另一端点，字段形态可能跟进同一命名）。**只读当前生效价，不读
    // originalPriceFactor**——后者是划线原价，当兜底会把限免模型误判成付费。
    let rate = match dig_f64(item, &["rate", "ratio", "price", "creditRate"]) {
        Some(r) => r,
        None => match dig_f64(item, &["priceFactor", "price_factor"]) {
            Some(r) => r,
            None => match item.get("credits") {
                Some(Value::String(s)) if s.trim().is_empty() => return None,
                other => other
                    .and_then(|x| match x {
                        Value::Number(n) => n.as_f64(),
                        Value::String(s) => s
                            .trim()
                            .trim_start_matches(['x', 'X'])
                            .trim()
                            .split_whitespace()
                            .next()
                            .and_then(|n| n.parse().ok()),
                        _ => None,
                    })
                    .unwrap_or(0.0),
            },
        },
    };
    // 档位：顶层旧键 → reasoning.supportedEfforts 数组 → reasoning.effort 固定单档（统一小写）
    let reasoning = item.get("reasoning");
    let mut supported_efforts = dig_strs(item, &["supportedEfforts", "supported_efforts", "efforts"]);
    if supported_efforts.is_empty() {
        if let Some(r) = reasoning {
            supported_efforts = dig_strs(r, &["supportedEfforts", "supported_efforts", "efforts"]);
            if supported_efforts.is_empty() {
                if let Some(e) = r.get("effort").and_then(|x| x.as_str()) {
                    if !e.trim().is_empty() {
                        supported_efforts.push(e.trim().to_string());
                    }
                }
            }
        }
    }
    let supported_efforts: Vec<String> =
        supported_efforts.into_iter().map(|e| e.to_lowercase()).collect();
    // 图片：supportsImages 布尔（disabledMultimodal 显式禁用多模态时置否）；
    // 旧模态数组形态（inputModalities/modalities）兜底
    let disabled_multimodal = item.get("disabledMultimodal").and_then(Value::as_bool).unwrap_or(false);
    let modalities = dig_strs(item, &["inputModalities", "input_modalities", "modalities"]);
    let supports_image = match item.get("supportsImages").and_then(Value::as_bool) {
        Some(b) => b && !disabled_multimodal,
        None => modalities
            .iter()
            .any(|m| m.eq_ignore_ascii_case("image") || m.eq_ignore_ascii_case("image_url")),
    };
    Some(WbModel {
        display: dig_str(item, &["displayName", "display", "name"]).unwrap_or_else(|| id.clone()),
        context_length: dig_u64(
            item,
            &["contextLength", "context_length", "inputTokenLimit", "maxInputTokens", "maxAllowedSize"],
        )
        .unwrap_or_else(|| {
            item.get("contextWindow")
                .and_then(|c| c.get("defaultLength"))
                .and_then(Value::as_u64)
                .unwrap_or(0)
        }),
        max_tokens: dig_u64(item, &["maxTokens", "max_tokens", "maxOutputTokens"]).unwrap_or(0),
        supports_image,
        supported_efforts,
        effort_override: None, // 修正层仅人工/实测维护，不从上游读
        rate,
        id,
        // 占位默认（cn）：实际区由调用方按本次拉取的分区盖章
        // （fetch_and_replace 依 global_region 覆写为 "cn" / "global"）
        region: default_cn(),
    })
}

/// 定向容器收集：从响应中找出「模型条目」候选对象列表。
/// 兼容形态（按上游演进逐版累积）：
/// 1. 根数组 / `{data: []}` / `{models: []}` / `{list: []}`（条目直接是模型）；
/// 2. **`{code:0, data:{agents:[…]}}`**（2026-09 实测新形态）——`data` 为对象，
///    模型/agent 列表挂在对象的数组字段下（agents/models/list/items/entries）；
/// 3. agent 条目内嵌 `models`/`list`/`items` 子数组（每 agent 挂自己的模型表）→ 展开子条目，
///    无内嵌子数组的 agent 条目本身作为候选（agent 即模型人格的形态）。
fn collect_candidates(v: &Value, out: &mut Vec<Value>, depth: usize) {
    if depth > 6 {
        return;
    }
    match v {
        Value::Array(arr) => {
            for el in arr {
                let mut nested = false;
                for key in ["models", "list", "items"] {
                    if let Some(sub) = el.get(key).and_then(Value::as_array) {
                        collect_candidates(&Value::Array(sub.clone()), out, depth + 1);
                        nested = true;
                    }
                }
                if !nested {
                    out.push(el.clone());
                }
            }
        }
        Value::Object(obj) => {
            for key in ["data", "models", "list", "items", "agents", "entries", "result"] {
                if let Some(sub) = obj.get(key) {
                    if sub.is_array() || sub.is_object() {
                        collect_candidates(sub, out, depth + 1);
                    }
                }
            }
        }
        _ => {}
    }
}

/// 深度扫描兜底（定向解析产出 0 条时）：全树递归收集「像模型条目」的对象
/// ——必须有**非空字符串**的显式 id 键（id/model/modelId/model_id）才算候选，
/// 防止把分页数字等噪音当模型。上游结构再漂移时的最后防线。
fn deep_scan_models(v: &Value, out: &mut Vec<Value>, depth: usize) {
    if depth > 8 {
        return;
    }
    match v {
        Value::Array(arr) => {
            for x in arr {
                deep_scan_models(x, out, depth + 1);
            }
        }
        Value::Object(_) => {
            let id_like = ["id", "model", "modelId", "model_id"].iter().any(|k| {
                matches!(v.get(*k), Some(Value::String(s)) if !s.trim().is_empty())
            });
            if id_like {
                out.push(v.clone());
            }
            if let Some(obj) = v.as_object() {
                for (_k, val) in obj {
                    deep_scan_models(val, out, depth + 1);
                }
            }
        }
        _ => {}
    }
}

/// 上游目录响应 → 模型列表（宽容解析：定向容器探测 → 深扫兜底 → 按 id 去重）。
/// 候选须通过 `looks_like_model` 能力特征过滤（剔 agent/命令噪音条目）；
/// 解析产出 0 条视为失败（不产脏目录），由调用方决定回退行为。
pub fn parse_upstream_catalog(body: &Value) -> Vec<WbModel> {
    let mut candidates: Vec<Value> = Vec::new();
    collect_candidates(body, &mut candidates, 0);
    // 官方客户端口径（抓包逆向）：cli agent 白名单 ∩ data.models，再剔路由别名黑名单；
    // 白名单缺失（旧形态）时仅黑名单过滤
    let whitelist = cli_agent_whitelist(body);
    let admitted = |m: &WbModel| -> bool {
        !is_noise_model_id(&m.id)
            && whitelist
                .as_ref()
                .map_or(true, |w| w.iter().any(|x| x.eq_ignore_ascii_case(&m.id)))
    };
    let mut out: Vec<WbModel> = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for item in &candidates {
        if !looks_like_model(item) {
            continue;
        }
        if let Some(m) = model_from_item(item) {
            if admitted(&m) && seen.insert(m.id.clone()) {
                out.push(m);
            }
        }
    }
    if out.is_empty() {
        let mut scanned: Vec<Value> = Vec::new();
        deep_scan_models(body, &mut scanned, 0);
        for item in &scanned {
            if !looks_like_model(item) {
                continue;
            }
            if let Some(m) = model_from_item(item) {
                if admitted(&m) && seen.insert(m.id.clone()) {
                    out.push(m);
                }
            }
        }
    }
    out
}

/// 从上游模型目录接口拉取并全量替换本地目录（T5.1/F-37 启动动态替换）。
/// 失败返回 Err（本地目录保持不动——静态兜底永远不因网络抖动被清掉）。
pub fn fetch_and_replace(
    data_dir: &Path,
    uid: &str,
    token: &str,
    domain: &str,
    enterprise_id: &str,
    global_region: bool,
) -> Result<usize, String> {
    use super::wb_upstream::{build_chat_headers, wb_agent, WbCreds};
    let creds = WbCreds {
        id: String::new(),
        uid: uid.to_string(),
        name: String::new(),
        token: token.to_string(),
        domain: domain.to_string(),
        enterprise_id: enterprise_id.to_string(),
        global_region,
    };
    // 目录端点按区域分叉（2026-10-17 海外实测）：CN = `console/enterprises/
    // personal/models`；Global = `/v3/config` 的 `data.models`（海外桌面客户端
    // 产品配置同源，credits 计费倍率与客户端选择器一致；UA 需 CLI 形态，
    // 浏览器 UA 被 12403 拒）。注意 `/v2/enterprises/personal/models` 是
    // platform=CLI 视图，倍率与桌面端不同（hy4-preview x0.00 vs x0.29），
    // 曾致账号页海外免费档错标，不可用。
    let path = if global_region {
        "/v3/config"
    } else {
        "/console/enterprises/personal/models"
    };
    let url = format!("{}{}", creds.chat_base(), path);
    let mut req = wb_agent().get(&url).timeout(std::time::Duration::from_secs(20));
    for (k, v) in build_chat_headers(&creds) {
        // 目录为 GET JSON：accept 覆盖 build_chat_headers 的 text/event-stream 默认
        let v = if k.eq_ignore_ascii_case("accept") { "application/json".into() } else { v };
        req = req.set(k, &v);
    }
    let body: Value = match req.call() {
        Ok(r) => r.into_json().map_err(|e| format!("目录响应解析失败: {e}"))?,
        Err(ureq::Error::Status(code, r)) => {
            // 诊断增强：附响应体摘要，区分 401/403 鉴权拒绝与端点变更
            let text = r.into_string().unwrap_or_default();
            let excerpt: String = text.chars().take(200).collect();
            return Err(format!("目录接口 HTTP {code}: body 摘要: {excerpt}"));
        }
        Err(e) => return Err(format!("目录请求失败: {e}")),
    };
    let mut models = parse_upstream_catalog(&body);
    // 按本次拉取的分区盖章：两区目录合并读取时依此判定条目归属区
    // （模型只出本区账号，国内/海外号池逻辑隔离）
    let region = if global_region { "global" } else { "cn" };
    for m in &mut models {
        m.region = region.to_string();
    }
    if models.is_empty() {
        // 诊断增强：附响应体摘要（截 200 字符），定位 200 状态下「结构不符」的
        // 真实形态（200 错误信封 / 多层嵌套 / 端点变更），避免只有 0 模型无从下手
        let text = serde_json::to_string(&body).unwrap_or_else(|_| format!("{body:?}"));
        let excerpt: String = text.chars().take(200).collect();
        return Err(format!(
            "上游目录解析产出 0 个模型（响应结构与预期不符，body 摘要: {excerpt}），本地目录保持不变"
        ));
    }
    // 质量守门：全部条目的能力字段全零（倍率/档位/上下文皆空）= 字段链漂移信号
    // （2026-09 实测教训：agents 新形态下仅 maxOutputTokens 命中，产出 39 条全零目录）。
    // 有 id 无能力 = 噪音目录，拒绝落盘保持旧目录。
    if models
        .iter()
        .all(|m| m.rate == 0.0 && m.supported_efforts.is_empty() && m.context_length == 0)
    {
        return Err(format!(
            "上游目录 {} 个条目能力字段全空（倍率/档位/上下文均未解析到，字段链疑似漂移），本地目录保持不变",
            models.len()
        ));
    }
    // 海外区并集（客户端同语义：本地 product.json + 远程配置，远程同 id 优先）：
    // 内置海外表补齐远程未下发的条目（如 gpt-6-astra），倍率以远程为准
    if global_region {
        let fetched: std::collections::HashSet<String> =
            models.iter().map(|m| m.id.to_lowercase()).collect();
        for b in builtin_global() {
            if !fetched.contains(&b.id.to_lowercase()) {
                models.push(b);
            }
        }
    }
    let count = models.len();
    // 分区落盘（修复「海外账号模型目录」）：两区各自一份，读取侧合并
    let key = if global_region { KV_KEY_GLOBAL } else { KV_KEY_CN };
    crate::store::db(data_dir)
        .kv_set(
            key,
            &WbCatalogFile {
                models,
                fetched_at: Some(
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs() as i64)
                        .unwrap_or(0),
                ),
                ..Default::default()
            },
        )
        .map_err(|e| format!("写目录失败: {e}"))?;
    // 写路径显式失效（批次 A）：目录替换即时生效（两区源键 + 合并缓存键）
    super::config_cache::invalidate(data_dir, KV_KEY_CN);
    super::config_cache::invalidate(data_dir, KV_KEY_GLOBAL);
    super::config_cache::invalidate(data_dir, CACHE_KEY_MERGED);
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_has_15_models_and_unique_ids() {
        let c = builtin();
        assert_eq!(c.len(), 15);
        let mut ids: Vec<&str> = c.iter().map(|m| m.id.as_str()).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), 15);
    }

    /// 免费模型提取（免费额度可见性）：恰为目录中 rate==0 的条目，
    /// name 回退 id、rate 恒 0.0。合并目录 = CN 内置（hy3 / step-5-preview 0 倍率档）
    /// ∪ Global 内置（hy3 / deepseek-v4.1-flash 0 倍率档，海外实测口径）。
    #[test]
    fn free_models_match_zero_rate_entries() {
        let dir = std::env::temp_dir().join(format!("twa_wb_free_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let free = free_models(&dir);
        let mut want: Vec<String> = builtin()
            .iter()
            .chain(builtin_global().iter())
            .filter(|m| m.rate == 0.0)
            .map(|m| m.id.clone())
            .collect();
        want.sort();
        want.dedup();
        let mut got: Vec<String> = free
            .iter()
            .filter_map(|m| m.get("id").and_then(Value::as_str))
            .map(str::to_string)
            .collect();
        got.sort();
        assert_eq!(got, want, "免费清单应恰为目录 rate=0 的条目");
        assert!(!want.is_empty(), "内置表应至少有一个 0 倍率档（hy3/step-5-preview）");
        for m in &free {
            assert_eq!(m.get("rate").and_then(Value::as_f64), Some(0.0));
            assert!(!m.get("id").and_then(Value::as_str).unwrap_or("").is_empty());
            assert!(!m.get("name").and_then(Value::as_str).unwrap_or("").is_empty());
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn hy3_effort_override_forces_high() {
        let c = builtin();
        let hy3 = find(&c, "hy3-x").unwrap();
        // 客户端请求 medium/xhigh 均被修正层强制为 high
        assert_eq!(hy3.resolve_effort(Some("medium")).as_deref(), Some("high"));
        assert_eq!(hy3.resolve_effort(Some("xhigh")).as_deref(), Some("high"));
        assert_eq!(hy3.resolve_effort(None).as_deref(), Some("high"));
    }

    #[test]
    fn effort_downgrade_picks_closest_supported() {
        let c = builtin();
        // deepseek-v4-flash 仅支持 low/medium：请求 high → 降级 medium
        let ds = find(&c, "deepseek-v4-flash").unwrap();
        assert_eq!(ds.resolve_effort(Some("high")).as_deref(), Some("medium"));
        assert_eq!(ds.resolve_effort(Some("low")).as_deref(), Some("low"));
        // 请求不支持的最小档以下 → 最低档
        assert_eq!(ds.resolve_effort(Some("minimal")).as_deref(), Some("low"));
        // 未请求 → 不下发
        assert_eq!(ds.resolve_effort(None), None);
    }

    #[test]
    fn find_is_case_insensitive_and_unknown_returns_none() {
        let c = builtin();
        assert!(find(&c, "GLM-5.3").is_some());
        assert!(find(&c, "glm-5.3").is_some());
        assert!(find(&c, "not-a-model").is_none());
    }

    /// 分区目录（修复「海外账号模型目录」）：Global 键独立存储（CN 键不受影响），
    /// load 合并两区（Global 独有条目追加、同 id CN 优先）；
    /// Global 目录未刷新（键缺失）时 load_region(true) 落 [`builtin_global`]
    /// （海外客户端 product.json 固化快照），不再返回空表
    #[test]
    fn regional_catalog_merge_and_isolation() {
        let dir = std::env::temp_dir().join(format!("twa_wb_catalog_region_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let global_model = WbModel {
            id: "global-only-model".into(),
            display: "Global Only".into(),
            context_length: 128_000,
            max_tokens: 32_000,
            supports_image: false,
            supported_efforts: vec!["high".into()],
            effort_override: None,
            rate: 0.0,
            region: "global".into(),
        };
        // 直接写 Global 键（模拟 fetch_and_replace(global=true) 落盘结果）
        crate::store::db(&dir)
            .kv_set(
                KV_KEY_GLOBAL,
                &WbCatalogFile {
                    models: vec![global_model],
                    fetched_at: Some(1_700_000_000),
                    builtin_rev: BUILTIN_REV,
                },
            )
            .unwrap();
        super::super::config_cache::invalidate(&dir, CACHE_KEY_MERGED);
        // 合并结果 = CN 内置表 + Global 独有条目
        let merged = load(&dir);
        assert!(find(&merged, "hy3").is_some(), "CN 目录条目保留");
        assert!(find(&merged, "global-only-model").is_some(), "Global 独有模型并入");
        assert_eq!(find(&merged, "global-only-model").unwrap().rate, 0.0);
        // Global 键内容不回写 CN 键（两区隔离）
        let cn_raw = crate::store::db(&dir).kv_get_raw(KV_KEY_CN).unwrap();
        assert!(!cn_raw.contains("global-only-model"), "CN 键不得被 Global 内容污染");
        // 区域过滤的免费模型：Global 区恰含该免费模型，CN 区不含
        let g_free = free_models_for(&dir, true);
        assert_eq!(g_free.len(), 1);
        assert_eq!(g_free[0].get("id").and_then(Value::as_str), Some("global-only-model"));
        let cn_free = free_models_for(&dir, false);
        assert!(!cn_free.iter().any(|m| m.get("id").and_then(Value::as_str) == Some("global-only-model")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ==================== T5.1/F-37 动态解析 ====================

    #[test]
    fn parse_upstream_catalog_three_container_shapes() {
        let entry = serde_json::json!({
            "id": "GLM-5.5",
            "displayName": "GLM 5.5",
            "contextLength": 200000,
            "maxTokens": 96000,
            "inputModalities": ["text", "image"],
            "supportedEfforts": ["low", "medium", "high"],
            "rate": 0.99,
        });
        let root = serde_json::json!([entry.clone()]);
        let data = serde_json::json!({"data": [entry.clone()]});
        let models = serde_json::json!({"models": [entry.clone()]});
        for body in [root, data, models] {
            let out = parse_upstream_catalog(&body);
            assert_eq!(out.len(), 1, "container shape");
            let m = &out[0];
            assert_eq!(m.id, "glm-5.5");
            assert_eq!(m.display, "GLM 5.5");
            assert_eq!(m.context_length, 200000);
            assert_eq!(m.max_tokens, 96000);
            assert!(m.supports_image, "inputModalities 含 image");
            assert_eq!(m.supported_efforts, vec!["low", "medium", "high"]);
            assert!((m.rate - 0.99).abs() < 1e-9);
            assert!(m.effort_override.is_none(), "修正层不从上游读");
        }
    }

    #[test]
    fn parse_upstream_catalog_tolerates_missing_fields_and_garbage() {
        // 有能力字段但缺其他字段 → 零值默认，不报错
        let out = parse_upstream_catalog(&serde_json::json!({"data": [
            {"id": "M1", "credits": "x0.10"}, {"model": "M2", "maxOutputTokens": 4096},
        ]}));
        assert_eq!(out.len(), 2);
        assert!(out[0].id == "m1" && out[1].id == "m2");
        assert!(!out[0].supports_image);
        assert!(out[0].supported_efforts.is_empty());
        assert!((out[0].rate - 0.10).abs() < 1e-9);
        assert_eq!(out[1].max_tokens, 4096);
        // 裸 id 条目（无任何能力字段）→ 当噪音过滤
        let out = parse_upstream_catalog(&serde_json::json!({"data": ["junk", {"displayName": "无id"}, 42, {"id": "bare"}]}));
        assert!(out.is_empty());
        // 完全无关结构 → 空列表（调用方保持本地目录不变）
        assert!(parse_upstream_catalog(&serde_json::json!({"foo": 1})).is_empty());
        assert!(parse_upstream_catalog(&serde_json::json!("text")).is_empty());
    }

    #[test]
    fn parse_upstream_catalog_numeric_and_string_fields() {
        let body = serde_json::json!({"data": [
            {"id": "hy5", "contextLength": "1000000", "rate": "0.35", "modalities": ["image_url"]},
        ]});
        let out = parse_upstream_catalog(&body);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].context_length, 1_000_000, "字符串数值宽容解析");
        assert!((out[0].rate - 0.35).abs() < 1e-9);
        assert!(out[0].supports_image, "modalities 别名键");
    }

    #[test]
    fn parse_upstream_catalog_data_object_with_agents() {
        // 防御形态：data 为对象，agents[] 挂在对象字段下（每 agent 内嵌完整模型对象——
        // 带能力字段才认；2026-09 抓包实测上游真实形态为字符串 id 数组，见
        // parse_upstream_catalog_2026_09_copilot_real_shape）
        let entry = serde_json::json!({
            "id": "glm-5.3",
            "displayName": "GLM 5.3",
            "contextLength": 200000,
            "inputModalities": ["text", "image"],
        });
        let body = serde_json::json!({
            "code": 0,
            "data": {
                "agents": [
                    {"commands": ["init", "compact", "statusline", "insights"], "models": [entry.clone()]},
                    {"commands": ["init"], "models": [{"id": "hy4-preview", "displayName": "Hy4 Preview", "maxOutputTokens": 8192}]},
                ]
            }
        });
        let out = parse_upstream_catalog(&body);
        assert_eq!(out.len(), 2, "agents 内嵌 models 展开");
        assert_eq!(out[0].id, "glm-5.3");
        assert!(out[0].supports_image);
        assert_eq!(out[1].id, "hy4-preview");
    }

    #[test]
    fn parse_upstream_catalog_agents_without_capability_are_noise() {
        // 2026-09 实测修正：agent 条目（无 models 子数组）不是模型人格——
        // 仅 id/name/commands 的 agent（general-purpose/compact 等）一律剔除
        let body = serde_json::json!({
            "code": 0,
            "data": {
                "agents": [
                    {"id": "kimi-k3", "name": "Kimi K3", "commands": ["init", "compact"]},
                    {"name": "general-purpose", "description": "general-purpose agent", "instructions": "x", "tools": []},
                ]
            }
        });
        assert!(parse_upstream_catalog(&body).is_empty(), "agent 条目不再进目录");
    }

    #[test]
    fn parse_upstream_catalog_2026_09_copilot_real_shape() {
        // 2026-09-19 copilot.tencent.com/console/enterprises/personal/models 抓包固化：
        // data.agents[].models 是字符串 id 数组（cli 主 agent = 客户端模型菜单白名单）；
        // 真实模型元数据在 data.models[]，倍率 credits="x0.05 credits"、档位嵌套
        // reasoning.supportedEfforts / reasoning.effort 单档、上下文 maxInputTokens、
        // 图片 supportsImages±disabledMultimodal；
        // 过滤：cli 白名单之外剔除（default/glm-4.6 等），黑名单剔除 auto/hunyuan-chat
        let body = serde_json::json!({
            "code": 0, "msg": "OK",
            "data": {
                "endpoint": "https://copilot.tencent.com",
                "mergeStrategy": "merge",
                "agents": [
                    {"commands": ["init", "compact"], "description": "cli agent",
                     "models": ["auto", "hy3", "hy3-x", "glm-5.3", "glm-5.0"], "name": "cli", "tags": ["cli", "default"]},
                    {"description": "general-purpose agent", "instructions": "x",
                     "name": "general-purpose", "tags": ["cli", "general-purpose"], "tools": []},
                    {"description": "compact agent", "instructions": "x", "name": "compact", "tools": []},
                ],
                "models": [
                    {"credits": "x2.20 credits", "id": "auto", "name": "Auto",
                     "maxInputTokens": 256000, "maxOutputTokens": 32000,
                     "onlyReasoning": true, "reasoning": {"effort": "high"}, "supportsImages": true},
                    {"credits": "x0.00 credits", "id": "hy3", "name": "Hy3",
                     "maxInputTokens": 192000, "maxOutputTokens": 64000,
                     "onlyReasoning": true, "reasoning": {"effort": "high", "summary": "auto"},
                     "supportsImages": true, "supportsReasoning": true,
                     "tags": ["craft", "badge:限时免费:#FF0000"], "vendor": "j"},
                    {"credits": "x0.05", "id": "hy3-x", "name": "Hy3",
                     "maxInputTokens": 192000, "maxOutputTokens": 64000,
                     "reasoning": {"canDisableThinking": false, "defaultEffort": "high",
                                   "summary": "auto", "supportedEfforts": ["low", "high"]},
                     "supportsImages": true, "vendor": "j"},
                    {"credits": "x0.79", "disabledMultimodal": true, "id": "glm-5.0",
                     "maxInputTokens": 200000, "maxOutputTokens": 48000, "name": "GLM-5.0",
                     "reasoning": {"effort": "medium"}, "supportsImages": true, "vendor": "e"},
                    {"credits": "x0.23 credits", "id": "default", "name": "Default",
                     "maxInputTokens": 200000, "maxOutputTokens": 24000, "supportsToolCall": true},
                    {"id": "hunyuan-chat", "maxInputTokens": 200000, "maxOutputTokens": 8192,
                     "name": "Hunyuan-Turbos", "supportsImages": false},
                ],
            }
        });
        let out = parse_upstream_catalog(&body);
        assert_eq!(out.len(), 3, "cli 白名单内真模型：hy3/hy3-x/glm-5.0；auto/default/hunyuan-chat 剔除");
        assert!(out.iter().all(|m| m.id != "auto"), "auto 属黑名单（白名单命中也剔除）");
        assert!(out.iter().all(|m| m.id != "default"), "default 不在 cli 白名单");
        assert!(out.iter().all(|m| m.id != "hunyuan-chat"), "hunyuan-chat 黑名单 + 不在白名单");
        let hy3 = out.iter().find(|m| m.id == "hy3").unwrap();
        assert!((hy3.rate - 0.0).abs() < 1e-9, "credits 带单位尾巴解析为数值");
        assert_eq!(hy3.supported_efforts, vec!["high"], "reasoning.effort 固定单档");
        assert_eq!(hy3.context_length, 192_000);
        assert!(hy3.supports_image);
        let hy3x = out.iter().find(|m| m.id == "hy3-x").unwrap();
        assert!((hy3x.rate - 0.05).abs() < 1e-9);
        assert_eq!(hy3x.supported_efforts, vec!["low", "high"], "reasoning.supportedEfforts 嵌套数组");
        let glm50 = out.iter().find(|m| m.id == "glm-5.0").unwrap();
        assert!((glm50.rate - 0.79).abs() < 1e-9);
        assert!(!glm50.supports_image, "disabledMultimodal=true 压制 supportsImages");
        assert_eq!(glm50.supported_efforts, vec!["medium"]);
    }

    #[test]
    fn parse_upstream_catalog_without_cli_agent_keeps_denylist_only() {
        // 无 cli 主 agent（旧形态/端点变更）→ 不做白名单过滤，仅黑名单兜底
        let body = serde_json::json!({
            "code": 0,
            "data": {
                "models": [
                    {"credits": "x0.10", "id": "m-1", "name": "M1", "maxInputTokens": 100000},
                    {"credits": "x2.20", "id": "default", "name": "Default", "maxInputTokens": 200000},
                    {"id": "hunyuan-chat", "maxInputTokens": 200000, "maxOutputTokens": 8192},
                ],
            }
        });
        let out = parse_upstream_catalog(&body);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, "m-1");
    }

    #[test]
    fn parse_upstream_catalog_deep_scan_fallback_and_dedupe() {
        // 定向容器全不命中 → 全树深扫兜底（要求非空字符串显式 id + 能力特征）
        let body = serde_json::json!({"x": {"y": [{"meta": 1}, {"model": "unknown-model", "credits": "x1.0"}]}});
        let out = parse_upstream_catalog(&body);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, "unknown-model");
        // 同 id 去重（保留首个）
        let body = serde_json::json!({"data": [{"id": "M1", "credits": "x0.1"}, {"id": "M1", "credits": "x0.2"}, {"id": "m1", "credits": "x0.3"}]});
        let out = parse_upstream_catalog(&body);
        assert_eq!(out.len(), 1, "大小写不敏感去重");
        // 数字 id / 空 id / 纯噪音不进深扫结果
        let body = serde_json::json!({"a": {"id": 3}, "b": {"id": ""}, "c": {"id": "ok-model", "maxOutputTokens": 1024}});
        let out = parse_upstream_catalog(&body);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, "ok-model");
    }

    #[test]
    fn builtin_global_free_set_and_region() {
        let g = builtin_global();
        assert!(g.len() >= 20, "海外内置表应含客户端 product.json 全量文本模型");
        let mut ids: Vec<&str> = g.iter().map(|m| m.id.as_str()).collect();
        let n = ids.len();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), n, "id 不得重复");
        assert!(g.iter().all(|m| m.region == "global"), "内置海外表全部盖 global 章");
        // 实测口径（2026-10-17 海外客户端 + /v3/config）：免费档恰为 hy3 / deepseek-v4.1-flash；
        // hy4-preview 0.29x 不免费（/v2 CLI 视图才误标 0.00）
        let mut free: Vec<String> = g
            .iter()
            .filter(|m| m.rate == 0.0)
            .map(|m| m.id.clone())
            .collect();
        free.sort();
        assert_eq!(
            free,
            vec!["deepseek-v4.1-flash".to_string(), "hy3".to_string(), "step-5-preview".to_string()]
        );
        let hy4 = find(&g, "hy4-preview").unwrap();
        assert!((hy4.rate - 0.29).abs() < 1e-9, "hy4-preview 内置倍率 = 客户端 0.29x");
        assert!(find(&g, "gpt-6-astra").is_none(), "Pro 锁定模型不收录（用户裁决）");
    }

    #[test]
    fn load_region_global_falls_back_to_builtin() {
        let dir = std::env::temp_dir().join(format!("twa_wb_gfallback_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::create_dir_all(&dir);
        let g = load_region(&dir, true);
        assert_eq!(g.len(), builtin_global().len(), "Global 键缺失 → 内置海外表");
        assert!(g.iter().all(|m| m.region == "global"));
        // 已落盘（builtin_rev 满足当前版本），二次读取一致
        assert_eq!(load_region(&dir, true).len(), g.len());
        let raw = crate::store::db(&dir).kv_get_raw(KV_KEY_GLOBAL).unwrap();
        assert!(raw.contains("deepseek-v4.1-flash"), "内置海外表落盘供热路径缓存命中");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parse_upstream_catalog_2026_10_global_v3config_shape() {
        // 2026-10-17 www.workbuddy.ai/v3/config 实测固化：data.models[]（credits
        // 字符串倍率 + maxInputTokens/maxOutputTokens + reasoning.supportedEfforts），
        // data.agents[].name=="cli" 的 models 白名单生效；default-model（Auto
        // 智能路由）credits 为空串 → 无计价信息，跳过（防混入免费档）；
        // deep-model / deepseek-v4.1-flash-sg 不在 cli 白名单 → 剔除（客户端同语义）
        let body = serde_json::json!({
            "code": 0, "msg": "OK", "requestId": "r1",
            "data": {
                "endpoint": "https://www.codebuddy.ai",
                "platform": "CLI",
                "deploymentType": "SaaS",
                "agents": [
                    {"commands": ["init"], "description": "cli agent", "name": "cli", "tags": ["cli", "default"],
                     "models": ["default-model", "fast-model", "hy4-preview", "hy3", "deepseek-v4.1-flash", "kimi-k3"]},
                    {"description": "general-purpose agent", "name": "general-purpose", "tools": []},
                ],
                "models": [
                    {"credits": "", "id": "default-model", "isDefault": true, "name": "Auto",
                     "maxInputTokens": 176000, "maxOutputTokens": 24000, "supportsImages": true},
                    {"credits": "x0.34 credits", "id": "fast-model", "name": "Fast",
                     "maxInputTokens": 200000, "maxOutputTokens": 32000, "supportsImages": true},
                    {"credits": "x0.29", "id": "hy4-preview", "name": "Hy4 preview",
                     "maxInputTokens": 1000000, "maxOutputTokens": 64000,
                     "reasoning": {"supportedEfforts": ["high"]}, "supportsImages": true},
                    {"credits": "x0.00", "id": "hy3", "name": "Hy3",
                     "maxInputTokens": 192000, "maxOutputTokens": 64000,
                     "reasoning": {"supportedEfforts": ["low", "high"]}, "supportsImages": true},
                    {"credits": "x0.00", "id": "deepseek-v4.1-flash", "name": "Deepseek-V4.1-Flash",
                     "maxInputTokens": 1000000, "maxOutputTokens": 128000, "supportsImages": true},
                    {"credits": "x1.62", "id": "kimi-k3", "name": "Kimi-K3", "supportsImages": true},
                    // 白名单外：不出现在结果
                    {"credits": "x0.03", "id": "deepseek-v4.1-flash-sg", "name": "Deepseek-V4.1-Flash", "supportsImages": true},
                ],
            }
        });
        let out = parse_upstream_catalog(&body);
        assert_eq!(out.len(), 5, "default-model（空 credits）跳过；sg 变体被白名单剔除");
        assert!(out.iter().all(|m| m.id != "default-model"), "Auto 智能路由无标价，不入目录");
        let hy4 = out.iter().find(|m| m.id == "hy4-preview").unwrap();
        assert!((hy4.rate - 0.29).abs() < 1e-9, "SaaS 视图 hy4-preview = 0.29（非 /v2 CLI 视图的 0.00）");
        assert_eq!(hy4.context_length, 1_000_000, "maxInputTokens → 上下文");
        let hy3 = out.iter().find(|m| m.id == "hy3").unwrap();
        assert!((hy3.rate - 0.0).abs() < 1e-9);
        assert_eq!(hy3.supported_efforts, vec!["low", "high"]);
        assert!(out.iter().all(|m| m.id != "deepseek-v4.1-flash-sg"), "cli 白名单外剔除");
        let ds = out.iter().find(|m| m.id == "deepseek-v4.1-flash").unwrap();
        assert!((ds.rate - 0.0).abs() < 1e-9, "海外免费档含 deepseek-v4.1-flash");
    }
}
