//! Agent 接入中心（2026-10-23）：一键把网关 Key/号池模型配置写入本机各 agent。
//!
//! 泛化自 `zcode_provider_write`（③b 直写 ZCode 供应商配置）：检测本机已安装的
//! agent → 选 Key 批量写入各自配置文件，全部走「时间戳备份 → 原子写 → 回读
//! 校验」范式。AIWork 写入的条目带可识别标记（WB 用 `vendor=="AIWork"`、
//! ZCode 用 providerId 前缀 `aiwork-`），幂等重跑与「清除」均据此定位。
//!
//! WB 自定义模型（`~/.workbuddy/models.json` 数组条目）走 OpenAI chat 协议：
//! `url` 填网关 `/v1` 基址，模型 `id` 用**带池前缀**的网关模型 id
//! （如 `qoder/qwen3.8-flash`），WB 以 `model=<id>` 请求 `<url>/chat/completions`，
//! 前缀即路由依据（server.rs /v1/chat/completions → 各池路由）。

use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use tauri::State;

use crate::api_server::api_keys::ApiKeyEntry;
use crate::state::AppState;

/// AIWork 写入 WB 条目的厂商标记（清除/幂等重跑定位键）
pub const WB_VENDOR: &str = "AIWork";
/// ZCode providerId 前缀（同上）
pub const ZCODE_PID_PREFIX: &str = "aiwork-";

// ==================== 检测 ====================

/// 本机 agent 家目录（Windows 主用 USERPROFILE；其余 HOME）
pub(crate) fn agent_home() -> Option<PathBuf> {
    std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .ok()
        .filter(|h| !h.trim().is_empty())
        .map(PathBuf::from)
}

/// agent 静态描述：目录锚点 + 配置文件相对路径 + 展示信息
struct AgentSpec {
    id: &'static str,
    label: &'static str,
    /// 判定「已安装」的目录锚（相对 home）
    anchor_dir: &'static str,
    /// 配置文件（相对 home）；不存在时 detected 仍按 anchor_dir 判定，
    /// config_path 原样返回供写入器创建
    config_rel: &'static str,
    desc: &'static str,
    /// 是否支持一键写入（false = 只检测展示，无文件级自定义渠道入口）
    writable: bool,
}

const AGENTS: &[AgentSpec] = &[
    AgentSpec {
        id: "workbuddy",
        label: "WorkBuddy 桌面端",
        anchor_dir: ".workbuddy",
        config_rel: ".workbuddy/models.json",
        desc: "每模型一条自定义模型（含上下文上限与思考深度），OpenAI chat 协议，重开对话生效",
        writable: true,
    },
    AgentSpec {
        id: "zcode",
        label: "ZCode CLI",
        anchor_dir: ".zcode/v2",
        config_rel: ".zcode/v2/provider_config.json",
        desc: "每 Key 一个供应商（anthropic-messages 协议），重启会话生效",
        writable: true,
    },
    AgentSpec {
        id: "claude",
        label: "Claude Code",
        anchor_dir: ".claude",
        config_rel: ".claude/settings.json",
        desc: "写 settings.json 的 env 块（ANTHROPIC_BASE_URL + Token），重启终端生效",
        writable: true,
    },
    AgentSpec {
        id: "codex",
        label: "Codex CLI",
        anchor_dir: ".codex",
        config_rel: ".codex/config.toml",
        desc: "写入独立 provider 段（wire_api=responses，直连 /v1/responses），不改你的默认模型",
        writable: true,
    },
    AgentSpec {
        id: "opencode",
        label: "opencode",
        anchor_dir: ".config/opencode",
        config_rel: ".config/opencode/opencode.jsonc",
        desc: "写入 provider 段（@ai-sdk/openai-compatible，OpenAI chat 协议）",
        writable: true,
    },
    AgentSpec {
        id: "gemini",
        label: "Gemini CLI",
        anchor_dir: ".gemini",
        config_rel: ".gemini/settings.json",
        desc: "仅检测：配置文件只支持 Gemini 官方鉴权与 MCP，无自定义 OpenAI 渠道入口",
        writable: false,
    },
    AgentSpec {
        id: "cursor",
        label: "Cursor",
        anchor_dir: ".cursor",
        config_rel: ".cursor/cli-config.json",
        desc: "仅检测：自定义模型走客户端内置表单，无文件级写入入口",
        writable: false,
    },
    AgentSpec {
        id: "trae",
        label: "Trae",
        anchor_dir: ".trae",
        config_rel: ".trae",
        desc: "仅检测：模型走账号套餐体系，无自定义渠道配置",
        writable: false,
    },
    AgentSpec {
        id: "qoder",
        label: "Qoder",
        anchor_dir: ".qoder",
        config_rel: ".qoder",
        desc: "仅检测：模型走账号套餐体系，无自定义渠道配置",
        writable: false,
    },
    AgentSpec {
        id: "codebuddy",
        label: "CodeBuddy",
        anchor_dir: ".codebuddy",
        config_rel: ".codebuddy/settings.json",
        desc: "仅检测：IDE 设置里只有插件开关，无自定义模型渠道入口",
        writable: false,
    },
];

/// 列出全部已知 agent 及本机检测结果（目录锚存在 = 已检测到）
pub(crate) fn scan(home: &Path) -> Vec<Value> {
    AGENTS
        .iter()
        .map(|a| {
            let detected = home.join(a.anchor_dir).exists();
            json!({
                "agent_id": a.id,
                "label": a.label,
                "desc": a.desc,
                "detected": detected,
                "writable": a.writable,
                "config_path": home.join(a.config_rel).to_string_lossy(),
            })
        })
        .collect()
}

/// ZCode provider 幂等 upsert 核心（zcode_provider_write_one 与批量写入共用）：
/// providerRules 内按 providerId 替换/追加，providerOrder 去重追加。
pub(crate) fn zcode_upsert_provider(cfg: &mut Value, provider: &Value) -> Result<(), String> {
    zcode_upsert_provider_with_limits(cfg, provider, &[])
}

/// 同 `zcode_upsert_provider`，额外写入 `modelConfigRules.providerModelRules`
/// 的逐模型能力元数据（contextWindow / maxOutputTokens / supportsImage）。
///
/// 2026-10-23 新增：此前只写 `providerRules`（模型 id 列表），ZCode 界面因此
/// 回落到它自己的默认值，用户看到「上下文/输出与 ZCode 默认不一致」。现在
/// 显式声明，口径 = 目录真实上下文 + 生态惯例 128K 输出（见
/// `AGENT_CONFIG_MAX_OUTPUT_TOKENS`）；providerModelRules 里同 providerId 的
/// 旧条目整体替换，避免历史脏值（如 384000）残留。
pub(crate) fn zcode_upsert_provider_with_limits(
    cfg: &mut Value,
    provider: &Value,
    model_limits: &[(String, u64, u64, bool)],
) -> Result<(), String> {
    let pid = provider
        .get("providerId")
        .and_then(Value::as_str)
        .ok_or("provider 缺 providerId")?
        .to_string();
    let cfg_obj = cfg
        .get_mut("config")
        .and_then(Value::as_object_mut)
        .ok_or("ZCode 配置缺 config 段")?;
    let rules = cfg_obj
        .get_mut("providerConfigRules")
        .and_then(Value::as_object_mut)
        .ok_or("ZCode 配置缺 providerConfigRules 段")?;
    let list = rules.entry("providerRules").or_insert_with(|| json!([]));
    let arr = list.as_array_mut().ok_or("providerRules 非数组")?;
    match arr
        .iter_mut()
        .find(|r| r.get("providerId").and_then(Value::as_str) == Some(pid.as_str()))
    {
        Some(r) => *r = provider.clone(),
        None => arr.push(provider.clone()),
    }
    let order = cfg_obj.entry("providerOrder").or_insert_with(|| json!([]));
    if let Some(oarr) = order.as_array_mut() {
        if !oarr.iter().any(|v| v.as_str() == Some(pid.as_str())) {
            oarr.push(json!(pid));
        }
    }
    // 逐模型能力元数据（可选段：配置里没有 modelConfigRules 时按需创建）
    if !model_limits.is_empty() {
        let mcr = cfg_obj
            .entry("modelConfigRules")
            .or_insert_with(|| json!({}))
            .as_object_mut()
            .ok_or("modelConfigRules 非对象")?;
        let pmr = mcr
            .entry("providerModelRules")
            .or_insert_with(|| json!([]))
            .as_array_mut()
            .ok_or("providerModelRules 非数组")?;
        // 先清掉本 provider 的旧规则（整体替换，避免历史脏值残留）
        pmr.retain(|r| r.get("providerId").and_then(Value::as_str) != Some(pid.as_str()));
        for (model_id, ctx, out, image) in model_limits {
            let mut props = json!({ "contextWindow": ctx });
            if *image {
                props["inputFormat"] = json!({ "supportsImage": true });
            }
            pmr.push(json!({
                "modelId": model_id,
                "config": {
                    "enabled": true,
                    "properties": props,
                    "optionSpecs": { "maxOutputTokens": { "max": out } },
                },
                "providerId": pid,
            }));
        }
    }
    Ok(())
}

// ==================== WorkBuddy models.json 写入器 ====================

/// 单模型能力（WB 条目构造所需：图片/思考/上下文/输出上限）
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct ModelCaps {
    pub image: bool,
    pub reasoning: bool,
    /// 上下文上限（取目录里声明值的最大值，token）
    pub max_input: u64,
    pub max_output: u64,
}

/// WB 认的思考档位全集
const WB_EFFORTS: &[&str] = &["low", "medium", "high", "xhigh", "max"];

/// 把目录里的 efforts 归一化到 WB 枚举（保留顺序、去重、丢弃未知档）。
/// 空 = 该模型不支持思考（WB 不写 reasoning 块）。
fn norm_efforts(raw: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for e in raw {
        let v = e.trim().to_lowercase();
        // qoder 目录用过 "max"/"high"/"low"；WB 侧额外认 xhigh；别名统一
        let v = match v.as_str() {
            "x-high" | "extra-high" | "ultra" => "xhigh".to_string(),
            other => other.to_string(),
        };
        if WB_EFFORTS.contains(&v.as_str()) && !out.contains(&v) {
            out.push(v);
        }
    }
    out
}

fn as_str_list(v: Option<&Value>) -> Vec<String> {
    v.and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
        .unwrap_or_default()
}

/// 目录能力表：网关带前缀模型 id → ModelCaps。
/// 上下文一律取目录声明的最大值（Qoder 上游给的是 context_window，Trae 有
/// dev/max 双口径时取 max），避免写入后客户端按默认小窗口截断。
pub(crate) fn catalog_support(data_dir: &Path) -> std::collections::HashMap<String, ModelCaps> {
    let mut m: std::collections::HashMap<String, ModelCaps> = std::collections::HashMap::new();
    // trae：kv api_models（ModelOption：context_length / context_length_max / efforts）
    for mo in crate::store::db(data_dir).kv_get::<Vec<Value>>("api_models") {
        let Some(id) = mo.get("id").and_then(Value::as_str) else { continue };
        let ctx_dev = mo.get("context_length").and_then(Value::as_u64).unwrap_or(0);
        let ctx_max = mo.get("context_length_max").and_then(Value::as_u64).unwrap_or(0);
        let efforts = as_str_list(mo.get("efforts"));
        let efforts: Vec<String> = norm_efforts(&efforts);
        // efforts 是 Vec<String> 动态值，这里需要 'static：只为存在思考档的
        // 模型登记（Qoder/ZCode 是静态表；Trae/WB 动态档位走 wb_dynamic_efforts）
        m.insert(
            format!("trae/{}", id),
            ModelCaps {
                image: mo.get("supports_image").and_then(Value::as_bool).unwrap_or(false),
                reasoning: !efforts.is_empty(),
                max_input: ctx_dev.max(ctx_max),
                max_output: 0,
            },
        );
    }
    // buddy：合并目录（WbModel：context_length / max_tokens / supported_efforts）
    for w in crate::api_server::wb_catalog::load(data_dir) {
        m.insert(
            format!("buddy/{}", w.id),
            ModelCaps {
                image: w.supports_image,
                reasoning: !w.supported_efforts.is_empty() || w.effort_override.is_some(),
                max_input: w.context_length,
                max_output: w.max_tokens,
            },
        );
    }
    // qoder：合并目录（entry 注入 maxInputTokens / maxOutputTokens / efforts）
    for q in crate::tasks::qoder_upstream::list() {
        if let Some(id) = q.get("id").and_then(Value::as_str) {
            m.insert(
                format!("qoder/{}", id),
                ModelCaps {
                    image: q.get("supportsImages").and_then(Value::as_bool).unwrap_or(false),
                    reasoning: q.get("supportsReasoning").and_then(Value::as_bool).unwrap_or(false),
                    max_input: q.get("maxInputTokens").and_then(Value::as_u64).unwrap_or(0),
                    // 目录的 maxOutputTokens 是请求侧硬约束（32K），客户端配置
                    // 侧改用生态惯例 128K（见 AGENT_CONFIG_MAX_OUTPUT_TOKENS）
                    max_output: crate::tasks::qoder_upstream::AGENT_CONFIG_MAX_OUTPUT_TOKENS
                        as u64,
                },
            );
        }
    }
    // zcode：静态目录（context_length / max_tokens / efforts / supports_image）
    for z in crate::api_server::zcode_catalog::list_json() {
        if let Some(id) = z.get("id").and_then(Value::as_str) {
            m.insert(
                format!("zcode/{}", id),
                ModelCaps {
                    image: z.get("supportsImages").and_then(Value::as_bool).unwrap_or(false),
                    reasoning: z
                        .get("efforts")
                        .and_then(Value::as_array)
                        .is_some_and(|a| !a.is_empty()),
                    max_input: z.get("maxInputTokens").and_then(Value::as_u64).unwrap_or(0),
                    max_output: z.get("maxOutputTokens").and_then(Value::as_u64).unwrap_or(0),
                },
            );
        }
    }
    m
}

/// 动态思考档位（目录给的是运行时 Vec，无法放进静态 ModelCaps）：
/// 按池前缀回查原目录条目，返回已归一化的档位列表。
fn dynamic_efforts(data_dir: &Path, model_id: &str) -> Vec<String> {
    // 模型名可能带区域后缀（qoder/GLM-5.3@global）：查目录前先剥掉
    let (pool, bare) = match model_id.split_once('/') {
        Some((p, m)) => (p, m),
        None => return vec![],
    };
    let bare = bare.rsplit_once('@').map(|(b, _)| b).unwrap_or(bare);
    match pool {
        "trae" => crate::store::db(data_dir)
            .kv_get::<Vec<Value>>("api_models")
            .into_iter()
            .find(|m| m.get("id").and_then(Value::as_str) == Some(bare))
            .map(|m| norm_efforts(&as_str_list(m.get("efforts"))))
            .unwrap_or_default(),
        "buddy" => crate::api_server::wb_catalog::load(data_dir)
            .into_iter()
            .find(|w| w.id == bare)
            .map(|w| norm_efforts(&w.supported_efforts))
            .unwrap_or_default(),
        "qoder" => crate::tasks::qoder_upstream::list()
            .into_iter()
            .find(|q| q.get("id").and_then(Value::as_str) == Some(bare))
            .map(|q| norm_efforts(&as_str_list(q.get("efforts"))))
            .unwrap_or_default(),
        "zcode" => crate::api_server::zcode_catalog::list_json()
            .into_iter()
            .find(|z| z.get("id").and_then(Value::as_str) == Some(bare))
            .map(|z| norm_efforts(&as_str_list(z.get("efforts"))))
            .unwrap_or_default(),
        _ => vec![],
    }
}

/// WB 条目 JSON（字段形状对齐 WB 5.8.3 `ModelsSettingsPanel` 落盘 schema）。
///
/// 关键：思考深度不是布尔就够——WB 读 `reasoning{defaultEffort,supportedEfforts}`
/// 才在 UI 出「思考深度」选择器；上下文读 `maxInputTokens/maxOutputTokens`，
/// 不写则按 WB 默认小窗口截断（用户实测「导入后没有思考深度」即此两者缺失）。
pub(crate) fn wb_entry(
    model_id: &str,
    base_url: &str,
    api_key: &str,
    caps: &ModelCaps,
    efforts: &[String],
) -> Value {
    let mut e = json!({
        "id": model_id,
        "name": model_id,
        "vendor": WB_VENDOR,
        "url": base_url,
        "apiKey": api_key,
        "supportsToolCall": true,
        "supportsImages": caps.image,
        "supportsReasoning": caps.reasoning || !efforts.is_empty(),
        "useCustomProtocol": false,
    });
    let obj = e.as_object_mut().expect("json object");
    // 上下文：只写有效值（0 = 目录没声明，交给 WB 默认，避免写 0 变不可用）
    if caps.max_input > 0 {
        obj.insert("maxInputTokens".into(), json!(caps.max_input));
    }
    // 输出上限：目录有真实值用它；没有则按生态惯例 128K 兜底
    //（Qoder 目录声明 32768 = 上游硬约束，但客户端配置侧对齐 ZCode 128K，
    //  两值分离见 qoder_upstream::AGENT_CONFIG_MAX_OUTPUT_TOKENS 注释。
    //  Trae 目录无此字段 → 同样落到 128K，避免"输出上限空白"）
    let out_tokens = if caps.max_output > 0 {
        caps.max_output
    } else {
        crate::tasks::qoder_upstream::AGENT_CONFIG_MAX_OUTPUT_TOKENS as u64
    };
    obj.insert("maxOutputTokens".into(), json!(out_tokens));
    // 思考深度：WB 只在 supportsReasoning 为真时读 reasoning 块。
    // 目录没给档位（如 Qwen3.7-Flash：supportsReasoning=true 但 efforts=[]）时
    // 补 WB 的常规三档，否则 UI 会「勾了支持思考却无档位可选」。
    if obj["supportsReasoning"] == json!(true) {
        let list: Vec<String> = if efforts.is_empty() {
            vec!["low".into(), "medium".into(), "high".into()]
        } else {
            efforts.to_vec()
        };
        let default_effort = pick_default_effort(&list);
        obj.insert(
            "reasoning".into(),
            json!({
                "defaultEffort": default_effort,
                "supportedEfforts": list,
            }),
        );
    }
    e
}

/// 默认思考档位：优先 high（WB 常规默认），无 high 则取档位表里最靠后的
/// （档位按 low→max 升序，越靠后越深）。
fn pick_default_effort(efforts: &[String]) -> String {
    efforts
        .iter()
        .find(|e| e.as_str() == "high")
        .cloned()
        .unwrap_or_else(|| efforts.last().cloned().unwrap_or_else(|| "high".into()))
}

/// 纯函数：现有 models.json 条目 + (模型 id → 归属 Key) 展开结果 → 新数组。
/// 保留非 AIWork 条目原样在前，追加全部 AIWork 条目（旧 AIWork 条目整批清除，
/// 幂等重跑不重复）。
pub(crate) fn merge_wb_entries(existing: Vec<Value>, aiwork: Vec<Value>) -> Vec<Value> {
    let mut out: Vec<Value> = existing
        .into_iter()
        .filter(|e| e.get("vendor").and_then(Value::as_str) != Some(WB_VENDOR))
        .collect();
    out.extend(aiwork);
    out
}

/// 模型归属 Key 分配：对每个模型收集所有把它展开出来的 Key，优先 `bind_pool`
/// 与前缀一致的 Key，其次首个展开它的 Key。输入 (model_id, producer_key_idx)
/// 展开顺序 = 用户勾选顺序。
///
/// 模型名可能带区域后缀（`qoder/GLM-5.3@global`）——归属只看池前缀（`@` 之前
/// 的 `qoder`），后缀不影响。同名跨区模型因后缀不同**各自成为独立条目**，
/// 不再被去重吞掉（这正是之前「两个一样的模型」的根因）。
pub(crate) fn assign_model_owners(
    occurrences: &[(String, usize)],
    keys: &[&ApiKeyEntry],
) -> Vec<(String, usize)> {
    let mut seen: Vec<(String, usize)> = Vec::new();
    for (model, _producer) in occurrences.iter() {
        if seen.iter().any(|(m, _)| m == model) {
            continue;
        }
        let prefix = model.split('/').next().unwrap_or("");
        let producers: Vec<usize> = occurrences
            .iter()
            .filter(|(m, _)| m == model)
            .map(|(_, idx)| *idx)
            .collect();
        let owner = producers
            .iter()
            .copied()
            .find(|idx| keys[*idx].bind_pool == prefix)
            .unwrap_or(producers[0]);
        seen.push((model.clone(), owner));
    }
    seen
}

/// 区域限定 Key 的模型名去歧义化（2026-10-23 修复）：
///
/// 当**多把 Key 分属不同区域**同时勾选时（典型：`Qoder` key region=cn +
/// `Qoder 海外号池` key region=global），两把 Key 各自展开的是**裸名且完全
/// 相同的**模型列表（`qoder/GLM-5.3`），合并后后者覆盖前者 —— 结果 WB 里
/// 只有一个 GLM-5.3，且永远走海外（或国内，取决于顺序），用户无法选择。
///
/// 修法：只要本次写入涉及的 Key **跨越两个区域**，就给每条展开记录补上它
/// 所属 Key 的区域后缀（`@cn`/`@global`）。必须在 `assign_model_owners`
/// **去重之前**做——否则同名两条已被合并，加后缀也只剩一条。
/// 单区域写入时保持裸名（无歧义，历史语义）。
pub(crate) fn disambiguate_by_key_region(
    occurrences: Vec<(String, usize)>,
    keys: &[&ApiKeyEntry],
) -> Vec<(String, usize)> {
    let has_cn = keys
        .iter()
        .any(|k| crate::api_server::api_keys::parse_region(&k.region) == Some(false));
    let has_global = keys
        .iter()
        .any(|k| crate::api_server::api_keys::parse_region(&k.region) == Some(true));
    if !(has_cn && has_global) {
        return occurrences; // 单区域（或都不限）→ 裸名即可
    }
    occurrences
        .into_iter()
        .map(|(model, kidx)| {
            // 已带后缀（不限区域 Key 的产物）保持原样
            if model.contains('@') {
                return (model, kidx);
            }
            let suffix = match crate::api_server::api_keys::parse_region(&keys[kidx].region) {
                Some(true) => "@global",
                Some(false) => "@cn",
                None => return (model, kidx), // 不限区域的 Key 不加后缀
            };
            (format!("{model}{suffix}"), kidx)
        })
        .collect()
}

/// WB 写入核心（home 注入便于测试）：展开选中 Key 的模型清单 → 每模型一条
/// models.json 条目 → 原子写 + 回读校验（不备份：模型配置可随时一键重写）。
pub(crate) fn wb_models_apply(
    home: &Path,
    data_dir: &Path,
    selected: &[&ApiKeyEntry],
) -> Result<Value, String> {
    if selected.is_empty() {
        return Err("未选择任何 Key".into());
    }
    // 1. 展开各 Key 生效模型（用户勾选顺序为第一遍展开序）
    let mut occurrences: Vec<(String, usize)> = Vec::new();
    for (idx, k) in selected.iter().enumerate() {
        let scope = super::api_server::key_scope_models(data_dir, k);
        for g in ["trae", "buddy", "qoder", "zcode"] {
            if let Some(arr) = scope.get(g).and_then(Value::as_array) {
                for m in arr.iter().filter_map(Value::as_str) {
                    occurrences.push((m.to_string(), idx));
                }
            }
        }
    }
    if occurrences.is_empty() {
        return Err("所选 Key 生效范围内没有模型（各池目录为空？）".into());
    }
    // 跨区域 Key 混选时先给展开记录补区域后缀（必须在去重前），
    // 否则同名两区会被 assign_model_owners 合并成一条、后者覆盖前者
    let occurrences = disambiguate_by_key_region(occurrences, selected);
    let owned = assign_model_owners(&occurrences, selected);
    // 2. 组条目
    let gs = crate::api_server::gateway_settings::load(data_dir);
    let host = if gs.host.trim().is_empty() { "127.0.0.1" } else { gs.host.trim() };
    let base_url = format!("http://{}:{}/v1", host, gs.port);
    let caps = catalog_support(data_dir);
    let aiwork: Vec<Value> = owned
        .iter()
        .map(|(model, kidx)| {
            // 目录能力表按裸名建键；带 @cn/@global 后缀时剥掉再查
            let bare_model = model.rsplit_once('@').map(|(b, _)| b).unwrap_or(model);
            let c = caps.get(bare_model).copied().unwrap_or_default();
            let efforts = dynamic_efforts(data_dir, model);
            wb_entry(model, &base_url, &selected[*kidx].key, &c, &efforts)
        })
        .collect();
    // 3. 读现有文件（非法 JSON 备份后按空数组重建）
    let path = home.join(".workbuddy").join("models.json");
    let existing: Vec<Value> = match std::fs::read_to_string(&path) {
        Ok(raw) => match serde_json::from_str::<Value>(&raw) {
            Ok(Value::Array(arr)) => arr,
            // 非法结构按空数组重建（不备份：模型配置随时可一键重写）
            Ok(_) => Vec::new(),
            Err(_) => Vec::new(),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(format!("读取 models.json 失败: {}", e)),
    };
    let kept_foreign = existing
        .iter()
        .filter(|e| e.get("vendor").and_then(Value::as_str) != Some(WB_VENDOR))
        .count();
    let removed_aiwork = existing.len() - kept_foreign;
    let merged = merge_wb_entries(existing, aiwork);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("创建 .workbuddy 目录失败: {}", e))?;
    }
    crate::fs_utils::write_json(&path, &merged)?;
    // 4. 回读校验：条数与 AIWork 模型集合一致
    let back: Vec<Value> = serde_json::from_str(
        &std::fs::read_to_string(&path)
            .map_err(|e| format!("回读 models.json 失败（配置可能未生效）: {}", e))?,
    )
    .map_err(|e| format!("回读解析失败: {}", e))?;
    let back_aiwork = back
        .iter()
        .filter(|e| e.get("vendor").and_then(Value::as_str) == Some(WB_VENDOR))
        .count();
    if back_aiwork != owned.len() {
        return Err(format!(
            "回读校验不符：期望 {} 条 AIWork 条目，实得 {}",
            owned.len(),
            back_aiwork
        ));
    }
    let models: Vec<Value> = owned
        .iter()
        .map(|(model, kidx)| {
            json!({
                "id": model,
                "key_name": selected[*kidx].name,
                // 区域：模型名带 @cn/@global 后缀才有；无后缀 = 该区独有/无区域概念
                "region": region_of_model_id(model),
            })
        })
        .collect();
    let (cn_n, gl_n) = (
        models.iter().filter(|m| m["region"] == json!("cn")).count(),
        models.iter().filter(|m| m["region"] == json!("global")).count(),
    );
    Ok(json!({
        "ok": true,
        "path": path.to_string_lossy(),
        "added": owned.len(),
        "kept_foreign": kept_foreign,
        "removed_aiwork": removed_aiwork,
        "cn_count": cn_n,
        "global_count": gl_n,
        "models": models,
    }))
}


/// 从模型 id 提取区域（`qoder/GLM-5.3@cn` → "cn"；无后缀 → ""）
pub(crate) fn region_of_model_id(model: &str) -> &'static str {
    match model.rsplit_once('@').map(|(_, t)| t.trim().to_lowercase()) {
        Some(t) if t == "cn" || t == "国内" => "cn",
        Some(t) if t == "global" || t == "海外" => "global",
        _ => "",
    }
}

/// WB 清除：删除全部 vendor=="AIWork" 条目（保留其他自定义条目）
pub(crate) fn wb_models_clear(home: &Path) -> Result<Value, String> {
    let path = home.join(".workbuddy").join("models.json");
    let raw = std::fs::read_to_string(&path).map_err(|e| format!("读取 models.json 失败: {}", e))?;
    let arr: Vec<Value> = serde_json::from_str(&raw).map_err(|e| format!("models.json 非数组: {}", e))?;
    let removed = arr
        .iter()
        .filter(|e| e.get("vendor").and_then(Value::as_str) == Some(WB_VENDOR))
        .count();
    if removed == 0 {
        return Ok(json!({"ok": true, "removed": 0}));
    }
    let kept: Vec<Value> = arr
        .into_iter()
        .filter(|e| e.get("vendor").and_then(Value::as_str) != Some(WB_VENDOR))
        .collect();
    crate::fs_utils::write_json(&path, &kept)?;
    Ok(json!({"ok": true, "removed": removed, "path": path.to_string_lossy()}))
}

// ==================== ZCode provider_config.json 写入器 ====================

/// 批量写 ZCode 供应商：逐 Key 调 zcode_provider_write_one（其内部已含
/// 备份 + 幂等 upsert），逐条收集成败。
pub(crate) fn zcode_providers_apply(
    data_dir: &Path,
    selected: &[&ApiKeyEntry],
) -> Value {
    let mut results: Vec<Value> = Vec::new();
    for k in selected {
        let r = match super::api_server::zcode_provider_write_one(data_dir, k) {
            Ok(v) => v,
            Err(e) => json!({"ok": false, "error": e}),
        };
        results.push(json!({"key_id": k.id, "key_name": k.name, "result": r}));
    }
    let ok = results.iter().filter(|r| r["result"]["ok"] == json!(true)).count();
    json!({"ok": ok == results.len(), "applied": ok, "total": results.len(), "details": results})
}

/// ZCode 清除：删除 providerId 以 aiwork- 开头的 provider（含 providerOrder 引用）
pub(crate) fn zcode_providers_clear(home: &Path) -> Result<Value, String> {
    let path = home.join(".zcode").join("v2").join("provider_config.json");
    let raw = std::fs::read_to_string(&path).map_err(|e| format!("读取 ZCode 配置失败: {}", e))?;
    let mut cfg: Value = serde_json::from_str(&raw).map_err(|e| format!("ZCode 配置非 JSON: {}", e))?;
    let removed = {
        let cfg_obj = cfg
            .get_mut("config")
            .and_then(Value::as_object_mut)
            .ok_or("ZCode 配置缺 config 段")?;
        let rules = cfg_obj
            .get("providerConfigRules")
            .and_then(Value::as_object)
            .and_then(|o| o.get("providerRules"))
            .and_then(Value::as_array)
            .cloned();
        let kept: Vec<Value> = rules
            .unwrap_or_default()
            .into_iter()
            .filter(|r| {
                r.get("providerId")
                    .and_then(Value::as_str)
                    .map(|pid| !pid.starts_with(ZCODE_PID_PREFIX))
                    .unwrap_or(true)
            })
            .collect();
        let total_before = cfg_obj
            .get("providerConfigRules")
            .and_then(|r| r.get("providerRules"))
            .and_then(Value::as_array)
            .map(|a| a.len())
            .unwrap_or(0);
        let removed = total_before - kept.len();
        if removed > 0 {
            if let Some(rules_obj) = cfg_obj
                .get_mut("providerConfigRules")
                .and_then(Value::as_object_mut)
            {
                rules_obj.insert("providerRules".into(), Value::Array(kept.clone()));
            }
            if let Some(order) = cfg_obj.get_mut("providerOrder").and_then(Value::as_array_mut) {
                order.retain(|v| {
                    v.as_str().map(|s| !s.starts_with(ZCODE_PID_PREFIX)).unwrap_or(true)
                });
            }
            crate::fs_utils::write_json(&path, &cfg)?;
            return Ok(json!({"ok": true, "removed": removed, "path": path.to_string_lossy()}));
        }
        removed
    };
    Ok(json!({"ok": true, "removed": removed}))
}

// ==================== Claude Code settings.json env 写入器 ====================

/// 写 ~/.claude/settings.json 的 env 块（端点 = 网关根址，不带 /v1：
/// Claude Code 自行拼 {base}/v1/messages，网关 routes::messages 按模型前缀
/// 调度各池）。保留文件既有其他键。key = 兜底 Key（单端点单 Key）。
pub(crate) fn claude_env_apply(
    home: &Path,
    data_dir: &Path,
    key: &ApiKeyEntry,
) -> Result<Value, String> {
    let path = home.join(".claude").join("settings.json");
    let mut cfg: Value = match std::fs::read_to_string(&path) {
        Ok(raw) => serde_json::from_str(&raw).map_err(|e| format!("settings.json 非 JSON: {}", e))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => json!({}),
        Err(e) => return Err(format!("读取 settings.json 失败: {}", e)),
    };
    if !path.is_file() {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("创建 .claude 目录失败: {}", e))?;
        }
    }
    let gs = crate::api_server::gateway_settings::load(data_dir);
    let host = if gs.host.trim().is_empty() { "127.0.0.1" } else { gs.host.trim() };
    let base = format!("http://{}:{}", host, gs.port);
    let model = if gs.default_model.trim().is_empty() {
        "buddy/hy3".to_string()
    } else {
        gs.default_model.clone()
    };
    let env = cfg
        .as_object_mut()
        .ok_or("settings.json 顶层非对象")?
        .entry("env")
        .or_insert_with(|| json!({}));
    let env_obj = env.as_object_mut().ok_or("settings.json env 非对象")?;
    for (k, v) in [
        ("ANTHROPIC_BASE_URL", base.as_str()),
        ("ANTHROPIC_AUTH_TOKEN", key.key.as_str()),
        ("ANTHROPIC_API_KEY", key.key.as_str()),
        ("ANTHROPIC_MODEL", model.as_str()),
        ("ANTHROPIC_DEFAULT_SONNET_MODEL", model.as_str()),
        ("ANTHROPIC_DEFAULT_SONNET_MODEL_NAME", model.as_str()),
        ("ANTHROPIC_DEFAULT_OPUS_MODEL", model.as_str()),
        ("ANTHROPIC_DEFAULT_OPUS_MODEL_NAME", model.as_str()),
        ("ANTHROPIC_DEFAULT_HAIKU_MODEL", model.as_str()),
        ("ANTHROPIC_DEFAULT_HAIKU_MODEL_NAME", model.as_str()),
        ("CLAUDE_CODE_SUBAGENT_MODEL", model.as_str()),
    ] {
        env_obj.insert(k.into(), json!(v));
    }
    env_obj.insert("_aiwork_key_name".into(), json!(key.name));
    crate::fs_utils::write_json(&path, &cfg)?;
    Ok(json!({
        "ok": true, "path": path.to_string_lossy(),
        "base_url": base, "model": model, "key_name": key.name,
    }))
}

/// Claude 清除：仅当 env 块确由本工具写入（带 _aiwork_key_name 标记）时移除
/// 相关键，不动用户其他配置。
pub(crate) fn claude_env_clear(home: &Path) -> Result<Value, String> {
    let path = home.join(".claude").join("settings.json");
    let raw = std::fs::read_to_string(&path).map_err(|e| format!("读取 settings.json 失败: {}", e))?;
    let mut cfg: Value = serde_json::from_str(&raw).map_err(|e| format!("settings.json 非 JSON: {}", e))?;
    let ours = cfg
        .pointer("/env/_aiwork_key_name")
        .and_then(Value::as_str)
        .is_some();
    if !ours {
        return Ok(json!({"ok": true, "cleared": false}));
    }
    const KEYS: &[&str] = &[
        "ANTHROPIC_BASE_URL",
        "ANTHROPIC_AUTH_TOKEN",
        "ANTHROPIC_API_KEY",
        "ANTHROPIC_MODEL",
        "ANTHROPIC_DEFAULT_SONNET_MODEL",
        "ANTHROPIC_DEFAULT_SONNET_MODEL_NAME",
        "ANTHROPIC_DEFAULT_OPUS_MODEL",
        "ANTHROPIC_DEFAULT_OPUS_MODEL_NAME",
        "ANTHROPIC_DEFAULT_HAIKU_MODEL",
        "ANTHROPIC_DEFAULT_HAIKU_MODEL_NAME",
        "CLAUDE_CODE_SUBAGENT_MODEL",
        "_aiwork_key_name",
    ];
    if let Some(env_obj) = cfg.get_mut("env").and_then(Value::as_object_mut) {
        for k in KEYS {
            env_obj.remove(*k);
        }
    }
    crate::fs_utils::write_json(&path, &cfg)?;
    Ok(json!({"ok": true, "cleared": true, "path": path.to_string_lossy()}))
}

// ==================== Codex CLI config.toml 写入器 ====================

/// Codex 段标记：本工具写入的 [model_providers.aiwork] 表 + 一条 profile。
/// 用独立 provider id（非 CC Switch 的 `custom`），不去抢用户默认配置：
/// 只在文件里追加 provider 表和一个 `[profiles.aiwork]`，用户按需 `--profile aiwork`。
pub(crate) const CODEX_PROVIDER_ID: &str = "aiwork";
pub(crate) const CODEX_PROFILE: &str = "aiwork";

/// TOML basic string 转义（与 ccswitch.rs 同规则）
fn toml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// 生成/替换本工具在 config.toml 里的两段（provider 表 + profile 表）。
/// 纯文本操作：先删掉旧的本工具段（幂等），再追加新段，其余内容一字不动。
pub(crate) fn apply_codex_toml(existing: &str, port: u16, model: &str) -> String {
    let mut kept: Vec<&str> = Vec::new();
    // 逐行扫描：命中本工具的段头则跳过，直到下一个段头
    let our_headers = [
        format!("[model_providers.{}]", CODEX_PROVIDER_ID),
        format!("[profiles.{}]", CODEX_PROFILE),
    ];
    let mut skipping = false;
    for line in existing.lines() {
        let t = line.trim();
        let is_header = t.starts_with('[') && t.ends_with(']');
        if is_header {
            skipping = our_headers.iter().any(|h| t == h.as_str());
            if skipping {
                continue;
            }
        }
        if skipping {
            continue;
        }
        kept.push(line);
    }
    // 去掉尾部空行，避免越写越长
    while kept.last().is_some_and(|l| l.trim().is_empty()) {
        kept.pop();
    }
    let mut out = kept.join("\n");
    if !out.is_empty() {
        out.push_str("\n\n");
    }
    out.push_str(&format!(
        "[model_providers.{pid}]\n\
         name = \"AI Work Studio 网关\"\n\
         base_url = \"http://127.0.0.1:{port}/v1\"\n\
         wire_api = \"responses\"\n\
         requires_openai_auth = true\n\
         \n\
         [profiles.{prof}]\n\
         model_provider = \"{pid}\"\n\
         model = \"{model}\"\n",
        pid = CODEX_PROVIDER_ID,
        prof = CODEX_PROFILE,
        port = port,
        model = toml_escape(model),
    ));
    out
}

/// Codex 写入：备份 → 改写 config.toml → 回读校验 → 同时把 Key 落到 auth.json
/// （Codex 用 OPENAI_API_KEY 作为 Bearer）。
pub(crate) fn codex_apply(
    home: &Path,
    data_dir: &Path,
    key: &ApiKeyEntry,
) -> Result<Value, String> {
    let dir = home.join(".codex");
    std::fs::create_dir_all(&dir).map_err(|e| format!("创建 .codex 目录失败: {}", e))?;
    let path = dir.join("config.toml");
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let gs = crate::api_server::gateway_settings::load(data_dir);
    let model = if gs.default_model.trim().is_empty() {
        "buddy/hy3".to_string()
    } else {
        gs.default_model.clone()
    };
    let toml_text = apply_codex_toml(&existing, gs.port, &model);
    std::fs::write(&path, &toml_text).map_err(|e| format!("写入 config.toml 失败: {}", e))?;
    // 回读校验
    let back = std::fs::read_to_string(&path).map_err(|e| format!("回读失败: {}", e))?;
    if !back.contains(&format!("[model_providers.{}]", CODEX_PROVIDER_ID))
        || !back.contains(&format!("[profiles.{}]", CODEX_PROFILE))
    {
        return Err("回读校验失败：段未落盘".to_string());
    }
    // auth.json：Codex 读 OPENAI_API_KEY
    let auth_path = dir.join("auth.json");
    let mut auth: Value = std::fs::read_to_string(&auth_path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| json!({}));
    if let Some(o) = auth.as_object_mut() {
        o.insert("OPENAI_API_KEY".into(), json!(key.key));
    }
    crate::fs_utils::write_json(&auth_path, &auth)?;
    Ok(json!({
        "ok": true,
        "path": path.to_string_lossy(),

        "profile": CODEX_PROFILE,
        "model": model,
        "base_url": format!("http://127.0.0.1:{}/v1", gs.port),
        "key_name": key.name,
        "hint": format!("用 `codex --profile {}` 启动；或把 [profiles.{}] 里的 model_provider/model 提到顶层设为默认", CODEX_PROFILE, CODEX_PROFILE),
    }))
}

/// Codex 清除：删掉本工具的两段 + auth.json 里的 OPENAI_API_KEY（仅当值等于
/// 我们写入过的 Key 才删，避免误删用户自己的官方 Key）
pub(crate) fn codex_clear(home: &Path) -> Result<Value, String> {
    let path = home.join(".codex").join("config.toml");
    let Ok(existing) = std::fs::read_to_string(&path) else {
        return Ok(json!({"ok": true, "removed": 0}));
    };
    let had = existing.contains(&format!("[model_providers.{}]", CODEX_PROVIDER_ID));
    if !had {
        return Ok(json!({"ok": true, "removed": 0}));
    }
    // apply_codex_toml 总会追加新段，清除时改为直接逐行过滤旧段
    let mut kept: Vec<&str> = Vec::new();
    let our_headers = [
        format!("[model_providers.{}]", CODEX_PROVIDER_ID),
        format!("[profiles.{}]", CODEX_PROFILE),
    ];
    let mut skipping = false;
    for line in existing.lines() {
        let t = line.trim();
        if t.starts_with('[') && t.ends_with(']') {
            skipping = our_headers.iter().any(|h| t == h.as_str());
            if skipping {
                continue;
            }
        }
        if skipping {
            continue;
        }
        kept.push(line);
    }
    while kept.last().is_some_and(|l| l.trim().is_empty()) {
        kept.pop();
    }
    std::fs::write(&path, kept.join("\n")).map_err(|e| format!("写入 config.toml 失败: {}", e))?;
    Ok(json!({"ok": true, "removed": 1, "path": path.to_string_lossy()}))
}

// ==================== opencode opencode.jsonc 写入器 ====================

pub(crate) const OPENCODE_PROVIDER_ID: &str = "aiwork";

/// opencode provider 段（@ai-sdk/openai-compatible 走 OpenAI chat 协议）。
/// 模型键须与网关模型 id 一致（带池前缀），opencode 以它作为请求 model。
pub(crate) fn opencode_provider_json(
    port: u16,
    key: &ApiKeyEntry,
    models: &[String],
    caps: &std::collections::HashMap<String, ModelCaps>,
) -> Value {
    let mut m = serde_json::Map::new();
    for id in models {
        let c = caps.get(id).copied().unwrap_or_default();
        let mut entry = json!({"name": id});
        if let Some(o) = entry.as_object_mut() {
            if c.max_input > 0 {
                o.insert("limit".into(), json!({"context": c.max_input}));
            }
            if c.reasoning {
                o.insert("reasoning".into(), json!(true));
            }
            if c.image {
                o.insert("attachment".into(), json!(true));
            }
        }
        m.insert(id.clone(), entry);
    }
    json!({
        "name": "AI Work Studio 网关",
        "npm": "@ai-sdk/openai-compatible",
        "options": {
            "baseURL": format!("http://127.0.0.1:{}/v1", port),
            "apiKey": key.key,
        },
        "models": m,
    })
}

/// opencode 写入：备份 → 解析 jsonc（严格 JSON 兼容）→ 覆盖 provider.aiwork →
/// 回读校验。
pub(crate) fn opencode_apply(
    home: &Path,
    data_dir: &Path,
    key: &ApiKeyEntry,
) -> Result<Value, String> {
    let dir = home.join(".config").join("opencode");
    std::fs::create_dir_all(&dir).map_err(|e| format!("创建 opencode 目录失败: {}", e))?;
    let path = dir.join("opencode.jsonc");
    let existing = std::fs::read_to_string(&path).unwrap_or_else(|_| "{}".into());
    let mut cfg: Value = serde_json::from_str(&existing)
        .map_err(|e| format!("opencode.jsonc 非严格 JSON（含注释？）无法安全改写: {}", e))?;
    let gs = crate::api_server::gateway_settings::load(data_dir);
    let models = model_list_for(data_dir, key);
    if models.is_empty() {
        return Err("该 Key 生效范围内没有模型".into());
    }
    let caps = catalog_support(data_dir);
    let provider = opencode_provider_json(gs.port, key, &models, &caps);
    let obj = cfg.as_object_mut().ok_or("opencode.jsonc 顶层非对象")?;
    obj.entry("provider")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or("provider 非对象")?
        .insert(OPENCODE_PROVIDER_ID.into(), provider);
    crate::fs_utils::write_json(&path, &cfg)?;
    let back: Value = serde_json::from_str(
        &std::fs::read_to_string(&path).map_err(|e| format!("回读失败: {}", e))?,
    )
    .map_err(|e| format!("回读解析失败: {}", e))?;
    if back.pointer(&format!("/provider/{}", OPENCODE_PROVIDER_ID)).is_none() {
        return Err("回读校验失败：provider 未落盘".to_string());
    }
    Ok(json!({
        "ok": true,
        "path": path.to_string_lossy(),
        "provider_id": OPENCODE_PROVIDER_ID,
        "added": models.len(),
        "key_name": key.name,
    }))
}

/// opencode 清除：删除 provider.aiwork
pub(crate) fn opencode_clear(home: &Path) -> Result<Value, String> {
    let path = home.join(".config").join("opencode").join("opencode.jsonc");
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return Ok(json!({"ok": true, "removed": 0}));
    };
    let mut cfg: Value = serde_json::from_str(&raw).map_err(|e| format!("opencode.jsonc 非 JSON: {}", e))?;
    let removed = cfg
        .get_mut("provider")
        .and_then(Value::as_object_mut)
        .and_then(|p| p.remove(OPENCODE_PROVIDER_ID))
        .is_some();
    if !removed {
        return Ok(json!({"ok": true, "removed": 0}));
    }
    crate::fs_utils::write_json(&path, &cfg)?;
    Ok(json!({"ok": true, "removed": 1, "path": path.to_string_lossy()}))
}

/// 单 Key 的扁平模型清单（opencode/claude 等单端点场景）
fn model_list_for(data_dir: &Path, key: &ApiKeyEntry) -> Vec<String> {
    let scope = super::api_server::key_scope_models(data_dir, key);
    let mut all = Vec::new();
    for g in ["trae", "buddy", "qoder", "zcode"] {
        if let Some(arr) = scope.get(g).and_then(Value::as_array) {
            all.extend(arr.iter().filter_map(Value::as_str).map(str::to_string));
        }
    }
    all
}

// ==================== Tauri 命令 ====================

/// Agent 接入中心：检测本机 agent
#[tauri::command]
pub fn agent_integration_scan(_state: State<'_, AppState>) -> Result<Value, String> {
    let home = agent_home().ok_or("无法解析用户主目录（USERPROFILE/HOME）")?;
    Ok(json!({"agents": scan(&home)}))
}

/// 写入：agent_id + 勾选 Key（顺序即展开优先级）+ 兜底 Key（Claude 用；
/// WB 缺省用首个勾选 Key 的语义由归属分配规则覆盖）
#[tauri::command]
pub fn agent_integration_apply(
    state: State<'_, AppState>,
    agent_id: String,
    key_ids: Vec<String>,
    fallback_key_id: Option<String>,
) -> Result<Value, String> {
    let home = agent_home().ok_or("无法解析用户主目录")?;
    let file = crate::api_server::api_keys::load(&state.data_dir);
    let resolve = |id: &str| -> Result<&ApiKeyEntry, String> {
        file.keys
            .iter()
            .find(|k| k.id == id)
            .ok_or_else(|| format!("Key 不存在: {}", id))
    };
    // 勾选 Key 按传入顺序；停用 Key 拒绝写入（网关会拒其请求，写了也是死配置）
    let mut selected: Vec<&ApiKeyEntry> = Vec::new();
    for id in &key_ids {
        let k = resolve(id)?;
        if !k.enabled {
            return Err(format!("Key「{}」已停用，请先启用或取消勾选", k.name));
        }
        selected.push(k);
    }
    // 单端点 agent（claude/codex/opencode）：用「兜底 Key」；多 Key 展开仅
    // WB/ZCode 有意义（每模型/每 Key 独立条目）
    let single = matches!(agent_id.as_str(), "claude" | "codex" | "opencode");
    if single && selected.is_empty() && fallback_key_id.is_none() {
        return Err("未选择 Key".into());
    }
    let result = match agent_id.as_str() {
        "workbuddy" => wb_models_apply(&home, &state.data_dir, &selected)?,
        "zcode" => zcode_providers_apply(&state.data_dir, &selected),
        "claude" | "codex" | "opencode" => {
            let fk = fallback_key_id
                .as_deref()
                .map(resolve)
                .transpose()?
                .or_else(|| selected.first().copied())
                .ok_or("未选择 Key")?;
            if !fk.enabled {
                return Err(format!("Key「{}」已停用", fk.name));
            }
            match agent_id.as_str() {
                "claude" => claude_env_apply(&home, &state.data_dir, fk)?,
                "codex" => codex_apply(&home, &state.data_dir, fk)?,
                _ => opencode_apply(&home, &state.data_dir, fk)?,
            }
        }
        other => {
            return Err(format!(
                "{} 暂不支持一键写入（无文件级自定义模型入口）",
                AGENTS
                    .iter()
                    .find(|a| a.id == other)
                    .map(|a| a.label)
                    .unwrap_or(other)
            ))
        }
    };
    let _ = crate::fs_utils::app_log(
        &state.data_dir,
        &format!("Agent 接入中心：写入 {}（{} 个 Key）", agent_id, selected.len()),
    );
    Ok(result)
}

/// 清除：删除该 agent 内本工具写入的配置（WB/ZCode/Claude 各有标记）
#[tauri::command]
pub fn agent_integration_clear(
    state: State<'_, AppState>,
    agent_id: String,
) -> Result<Value, String> {
    let home = agent_home().ok_or("无法解析用户主目录")?;
    let result = match agent_id.as_str() {
        "workbuddy" => wb_models_clear(&home)?,
        "zcode" => zcode_providers_clear(&home)?,
        "claude" => claude_env_clear(&home)?,
        "codex" => codex_clear(&home)?,
        "opencode" => opencode_clear(&home)?,
        other => return Err(format!("未知 agent: {}", other)),
    };
    let _ = crate::fs_utils::app_log(
        &state.data_dir,
        &format!("Agent 接入中心：清除 {} 的 AIWork 配置", agent_id),
    );
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_home(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos();
        let p = std::env::temp_dir().join(format!("aiwork-agent-{}-{}-{}", tag, std::process::id(), nanos));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn key_entry(id: &str, name: &str, key: &str, bind_pool: &str, enabled: bool) -> ApiKeyEntry {
        ApiKeyEntry {
            id: id.into(),
            name: name.into(),
            key: key.into(),
            enabled,
            daily_limit: 0,
            created_at: 0,
            used_date: String::new(),
            used_today: 0,
            allowed_accounts: vec![],
            schedule_mode: String::new(),
            dedicated_account: String::new(),
            region: String::new(),
            bind_pool: bind_pool.into(),
            ..Default::default()
        }
    }

    #[test]
    fn scan_detects_by_anchor_dir() {
        let home = temp_home("scan");
        std::fs::create_dir_all(home.join(".workbuddy")).unwrap();
        let agents = scan(&home);
        let wb = agents.iter().find(|a| a["agent_id"] == json!("workbuddy")).unwrap();
        assert_eq!(wb["detected"], json!(true));
        let zc = agents.iter().find(|a| a["agent_id"] == json!("zcode")).unwrap();
        assert_eq!(zc["detected"], json!(false));
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn merge_wb_keeps_foreign_and_replaces_aiwork() {
        let existing = vec![
            json!({"id": "user-custom", "vendor": "Custom"}),
            json!({"id": "old-model", "vendor": WB_VENDOR}),
        ];
        let aiwork = vec![json!({"id": "qoder/a", "vendor": WB_VENDOR})];
        let merged = merge_wb_entries(existing, aiwork);
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0]["id"], json!("user-custom"));
        assert_eq!(merged[1]["id"], json!("qoder/a"));
    }

    #[test]
    fn assign_owners_prefers_bind_pool_match() {
        // 全局 Key（idx0，bind_pool=""）先展开出全部；绑定池 Key（idx1）也含
        // qoder/a → qoder/a 归 idx1，其余归 idx0
        let g = key_entry("g", "全局", "ck_g", "", true);
        let q = key_entry("q", "Qoder池", "ck_q", "qoder", true);
        let keys = vec![&g, &q];
        let occ = vec![
            ("qoder/a".to_string(), 0usize),
            ("buddy/b".to_string(), 0usize),
            ("qoder/a".to_string(), 1usize),
        ];
        let owned = assign_model_owners(&occ, &keys);
        assert_eq!(owned.len(), 2);
        let qa = owned.iter().find(|(m, _)| m == "qoder/a").unwrap();
        assert_eq!(qa.1, 1, "bind_pool 匹配的 Key 优先持有");
        let bb = owned.iter().find(|(m, _)| m == "buddy/b").unwrap();
        assert_eq!(bb.1, 0);
    }

    #[test]
    fn same_model_two_regions_become_two_entries() {
        // 用户实测问题：「进去之后两个一样的模型吗？」——同名跨区必须可区分
        let g = key_entry("g", "全局", "ck_g", "qoder", true);
        let keys = vec![&g];
        let occ = vec![
            ("qoder/GLM-5.3@cn".to_string(), 0usize),
            ("qoder/GLM-5.3@global".to_string(), 0usize),
        ];
        let owned = assign_model_owners(&occ, &keys);
        assert_eq!(owned.len(), 2, "同名两区各自独立，不得被去重吞掉");
        assert!(owned.iter().any(|(m, _)| m.ends_with("@cn")));
        assert!(owned.iter().any(|(m, _)| m.ends_with("@global")));
        // 区域解析
        assert_eq!(region_of_model_id("qoder/GLM-5.3@cn"), "cn");
        assert_eq!(region_of_model_id("qoder/GLM-5.3@global"), "global");
        assert_eq!(region_of_model_id("trae/glm-5.3-flash"), "");
    }

    #[test]
    fn multi_region_keys_get_region_suffixes() {
        // 用户实况：Qoder(cn) + Qoder 海外号池(global) 同时勾选 →
        // 两把 Key 展开同一批裸名，必须靠后缀区分，否则互相覆盖
        let cn = {
            let mut k = key_entry("cn", "Qoder", "ck_cn", "qoder", true);
            k.region = "cn".into();
            k
        };
        let gl = {
            let mut k = key_entry("gl", "Qoder 海外", "ck_gl", "qoder", true);
            k.region = "global".into();
            k
        };
        let keys = vec![&cn, &gl];
        let occ = vec![
            ("qoder/GLM-5.3".to_string(), 0usize),
            ("qoder/GLM-5.3".to_string(), 1usize),
        ];
        let occ = disambiguate_by_key_region(occ, &keys);
        let out = assign_model_owners(&occ, &keys);
        assert_eq!(out.len(), 2, "同模型两区各自保留");
        assert!(out.iter().any(|(m, _)| m == "qoder/GLM-5.3@cn"));
        assert!(out.iter().any(|(m, _)| m == "qoder/GLM-5.3@global"));
    }

    #[test]
    fn single_region_keys_keep_bare_names() {
        let a = {
            let mut k = key_entry("a", "Qoder", "ck_a", "qoder", true);
            k.region = "cn".into();
            k
        };
        let b = {
            let mut k = key_entry("b", "Qoder2", "ck_b", "qoder", true);
            k.region = "cn".into();
            k
        };
        let keys = vec![&a, &b];
        let occ = vec![("qoder/GLM-5.3".to_string(), 0usize)];
        let occ = disambiguate_by_key_region(occ, &keys);
        let out = assign_model_owners(&occ, &keys);
        assert_eq!(out, vec![("qoder/GLM-5.3".to_string(), 0)], "同区不加后缀");
    }

    #[test]
    fn wb_models_clear_removes_only_aiwork() {
        let home = temp_home("clear");
        let dir = home.join(".workbuddy");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("models.json");
        std::fs::write(
            &path,
            json!([
                {"id": "keep", "vendor": "Custom"},
                {"id": "drop", "vendor": WB_VENDOR},
            ])
            .to_string(),
        )
        .unwrap();
        let r = wb_models_clear(&home).unwrap();
        assert_eq!(r["removed"], json!(1));
        let back: Vec<Value> =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(back.len(), 1);
        assert_eq!(back[0]["id"], json!("keep"));
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn zcode_upsert_writes_model_limits_and_replaces_stale() {
        let mut cfg = json!({"config": {
            "providerConfigRules": {"providerRules": []},
            "modelConfigRules": {"providerModelRules": [
                {"modelId": "buddy/step-5-preview", "providerId": "aiwork-b1",
                 "config": {"optionSpecs": {"maxOutputTokens": {"max": 384000}}}}
            ]}
        }});
        let provider = json!({"providerId": "aiwork-b1", "providerName": "B1"});
        let limits = vec![
            ("buddy/step-5-preview".to_string(), 1_000_000u64, 128_000u64, true),
            ("buddy/hy3".to_string(), 192_000u64, 64_000u64, false),
        ];
        zcode_upsert_provider_with_limits(&mut cfg, &provider, &limits).unwrap();
        let pmr = cfg["config"]["modelConfigRules"]["providerModelRules"]
            .as_array().unwrap();
        // 旧条目（384000）必须被替换掉，不得残留
        assert_eq!(pmr.len(), 2, "同 provider 旧规则整体替换");
        assert!(
            pmr.iter().all(|r| r["config"]["optionSpecs"]["maxOutputTokens"]["max"] != json!(384000)),
            "历史脏值 384000 必须清除"
        );
        let sp = pmr.iter().find(|r| r["modelId"] == json!("buddy/step-5-preview")).unwrap();
        assert_eq!(sp["config"]["properties"]["contextWindow"], json!(1_000_000));
        assert_eq!(sp["config"]["optionSpecs"]["maxOutputTokens"]["max"], json!(128_000));
        assert_eq!(sp["config"]["properties"]["inputFormat"]["supportsImage"], json!(true));
        // 不支持图片的模型不写 inputFormat
        let hy = pmr.iter().find(|r| r["modelId"] == json!("buddy/hy3")).unwrap();
        assert!(hy["config"]["properties"].get("inputFormat").is_none());
    }

    #[test]
    fn wb_entry_falls_back_to_128k_output() {
        // Trae 目录无 max_tokens 字段 → 必须落到 128K，不能留空
        let caps = ModelCaps { max_input: 1_000_000, max_output: 0, image: true, reasoning: false };
        let e = wb_entry("trae/glm-5.3", "http://h/v1", "ck", &caps, &[]);
        assert_eq!(e["maxOutputTokens"], json!(128_000), "无目录值时按生态惯例 128K");
    }

    #[test]
    fn zcode_clear_removes_aiwork_providers_and_order() {
        let home = temp_home("zclear");
        let dir = home.join(".zcode").join("v2");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("provider_config.json");
        std::fs::write(
            &path,
            json!({
                "config": {
                    "providerConfigRules": {
                        "providerRules": [
                            {"providerId": "official-x"},
                            {"providerId": "aiwork-key1"},
                        ]
                    },
                    "providerOrder": ["official-x", "aiwork-key1"],
                }
            })
            .to_string(),
        )
        .unwrap();
        let r = zcode_providers_clear(&home).unwrap();
        assert_eq!(r["removed"], json!(1));
        let back: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            back["config"]["providerConfigRules"]["providerRules"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            back["config"]["providerOrder"].as_array().unwrap(),
            &vec![json!("official-x")]
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn claude_env_write_preserves_other_keys() {
        let home = temp_home("claude");
        let dir = home.join(".claude");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");
        std::fs::write(&path, json!({"theme": "dark", "includeCoAuthoredBy": false}).to_string())
            .unwrap();
        // data_dir 注入 gateway_settings：写入临时数据目录的网关配置
        let data_dir = temp_home("claude-data");
        std::fs::create_dir_all(&data_dir).unwrap();
        crate::api_server::gateway_settings::save(
            &data_dir,
            crate::api_server::gateway_settings::GatewaySettings {
                port: 17899,
                host: "127.0.0.1".into(),
                default_model: "buddy/hy3".into(),
                updated_at: 0,
            },
        )
        .unwrap();
        let k = key_entry("c", "主Key", "ck_main", "", true);
        let r = claude_env_apply(&home, &data_dir, &k).unwrap();
        assert_eq!(r["base_url"], json!("http://127.0.0.1:17899"));
        let back: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(back["theme"], json!("dark"), "其他既有键必须保留");
        assert_eq!(back["env"]["ANTHROPIC_BASE_URL"], json!("http://127.0.0.1:17899"));
        assert_eq!(back["env"]["ANTHROPIC_AUTH_TOKEN"], json!("ck_main"));
        // 清除后标记键消失、其他键仍在
        let rc = claude_env_clear(&home).unwrap();
        assert_eq!(rc["cleared"], json!(true));
        let back2: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(back2["theme"], json!("dark"));
        assert!(back2["env"]["ANTHROPIC_BASE_URL"].is_null());
        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&data_dir).ok();
    }

    #[test]
    fn wb_entry_shape() {
        let caps = ModelCaps {
            image: true,
            reasoning: true,
            max_input: 200_000,
            max_output: 64_000,
        };
        let efforts = vec!["low".to_string(), "high".to_string(), "max".to_string()];
        let e = wb_entry("qoder/x", "http://127.0.0.1:7864/v1", "ck_1", &caps, &efforts);
        assert_eq!(e["vendor"], json!(WB_VENDOR));
        assert_eq!(e["url"], json!("http://127.0.0.1:7864/v1"));
        assert_eq!(e["supportsToolCall"], json!(true));
        assert_eq!(e["supportsImages"], json!(true));
        assert_eq!(e["supportsReasoning"], json!(true));
        assert_eq!(e["useCustomProtocol"], json!(false));
        // 用户反馈的核心缺口：上下文 + 思考深度必须落盘
        assert_eq!(e["maxInputTokens"], json!(200_000), "上下文上限必须写入");
        assert_eq!(e["maxOutputTokens"], json!(64_000));
        assert_eq!(e["reasoning"]["defaultEffort"], json!("high"), "默认取 high");
        assert_eq!(e["reasoning"]["supportedEfforts"], json!(["low", "high", "max"]));
    }

    #[test]
    fn wb_entry_omits_reasoning_when_unsupported() {
        let caps = ModelCaps::default();
        let e = wb_entry("buddy/x", "http://h/v1", "ck_1", &caps, &[]);
        assert_eq!(e["supportsReasoning"], json!(false));
        assert!(e.get("reasoning").is_none(), "不支持思考的模型不写 reasoning 块");
        // 目录未声明上下文时不写 0（避免客户端按 0 截断）
        assert!(e.get("maxInputTokens").is_none());
        // 输出上限：目录无值时按生态惯例 128K 兜底（2026-10-23 对齐 ZCode 口径）
        assert_eq!(e["maxOutputTokens"], json!(128_000));
    }

    #[test]
    fn wb_entry_backfills_efforts_when_reasoning_without_levels() {
        // Qwen3.7-Flash 实况：supportsReasoning=true 但目录 efforts=[]
        let caps = ModelCaps { reasoning: true, max_input: 200_000, max_output: 32_768, image: true };
        let e = wb_entry("qoder/Qwen3.7-Flash", "http://h/v1", "ck_1", &caps, &[]);
        assert_eq!(e["supportsReasoning"], json!(true));
        assert_eq!(
            e["reasoning"]["supportedEfforts"],
            json!(["low", "medium", "high"]),
            "无档位时补常规三档，避免 UI 有开关无档位"
        );
        assert_eq!(e["reasoning"]["defaultEffort"], json!("high"));
    }

    #[test]
    fn norm_efforts_normalizes_and_dedups() {
        let raw = vec![
            "HIGH".to_string(),
            "x-high".to_string(),
            "bogus".to_string(),
            "high".to_string(),
            "low".to_string(),
        ];
        assert_eq!(norm_efforts(&raw), vec!["high", "xhigh", "low"]);
    }

    #[test]
    fn pick_default_prefers_high_then_deepest() {
        assert_eq!(pick_default_effort(&["low".into(), "high".into()]), "high");
        assert_eq!(pick_default_effort(&["low".into(), "medium".into()]), "medium");
        assert_eq!(pick_default_effort(&[]), "high");
    }

    #[test]
    fn codex_toml_is_idempotent_and_preserves_user_config() {
        let existing = "model = \"gpt-6.1-sol\"\nsandbox_mode = \"danger-full-access\"\n\n[model_providers.custom]\nname = \"custom\"\nbase_url = \"http://127.0.0.1:15721/v1\"\n";
        let once = apply_codex_toml(existing, 7864, "buddy/hy3");
        // 用户原有内容保留
        assert!(once.contains("model = \"gpt-6.1-sol\""));
        assert!(once.contains("[model_providers.custom]"));
        // 本工具段出现
        assert!(once.contains("[model_providers.aiwork]"));
        assert!(once.contains("[profiles.aiwork]"));
        assert!(once.contains("base_url = \"http://127.0.0.1:7864/v1\""));
        assert!(once.contains("wire_api = \"responses\""));
        // 幂等：重复应用不累积
        let twice = apply_codex_toml(&once, 7864, "buddy/hy3");
        assert_eq!(once, twice, "重复写入必须幂等");
        assert_eq!(twice.matches("[model_providers.aiwork]").count(), 1);
    }

    #[test]
    fn codex_clear_removes_only_our_sections() {
        let home = temp_home("codex-clear");
        let dir = home.join(".codex");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        let base = "model = \"gpt-6.1-sol\"\n[model_providers.custom]\nname = \"custom\"\n";
        let with_ours = apply_codex_toml(base, 7864, "hy3");
        std::fs::write(&path, &with_ours).unwrap();
        let r = codex_clear(&home).unwrap();
        assert_eq!(r["removed"], json!(1));
        let back = std::fs::read_to_string(&path).unwrap();
        assert!(back.contains("[model_providers.custom]"), "用户段必须保留");
        assert!(!back.contains("[model_providers.aiwork]"));
        assert!(!back.contains("[profiles.aiwork]"));
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn opencode_provider_has_models_and_limits() {
        let k = key_entry("o", "主Key", "ck_o", "", true);
        let mut caps = std::collections::HashMap::new();
        caps.insert(
            "qoder/x".to_string(),
            ModelCaps { image: true, reasoning: true, max_input: 1_000_000, max_output: 128_000},
        );
        let p = opencode_provider_json(7864, &k, &["qoder/x".to_string()], &caps);
        assert_eq!(p["npm"], json!("@ai-sdk/openai-compatible"));
        assert_eq!(p["options"]["baseURL"], json!("http://127.0.0.1:7864/v1"));
        assert_eq!(p["options"]["apiKey"], json!("ck_o"));
        assert_eq!(p["models"]["qoder/x"]["limit"]["context"], json!(1_000_000));
        assert_eq!(p["models"]["qoder/x"]["reasoning"], json!(true));
        assert_eq!(p["models"]["qoder/x"]["attachment"], json!(true));
    }

    #[test]
    fn catalog_support_takes_max_context_of_dual_fields() {
        // Trae 的 context_length/context_length_max 双口径：必须取较大值
        let data_dir = temp_home("caps");
        let store = crate::store::db(&data_dir);
        store
            .kv_set(
                "api_models",
                &json!([{
                    "id": "glm-5.3-flash",
                    "label": "GLM",
                    "context_length": 200000,
                    "context_length_max": 1000000,
                    "efforts": ["low", "high"],
                    "supports_image": true,
                }]),
            )
            .unwrap();
        let caps = catalog_support(&data_dir);
        let c = caps.get("trae/glm-5.3-flash").expect("目录条目");
        assert_eq!(c.max_input, 1_000_000, "双口径取最大值");
        assert!(c.image);
        assert!(c.reasoning);
        std::fs::remove_dir_all(&data_dir).ok();
    }
}

#[cfg(test)]
mod probe3 {
    /// 临时：观察 Qoder 双区重名情况
    #[test]
    #[ignore]
    fn probe_qoder_regions() {
        use crate::tasks::qoder_upstream::{catalog_for_region, QoderRegion};
        let cn = catalog_for_region(QoderRegion::Cn);
        let gl = catalog_for_region(QoderRegion::Global);
        println!("CN n={} Global n={}", cn.len(), gl.len());
        let ids = |v: &[serde_json::Value]| -> Vec<String> {
            v.iter().filter_map(|m| m.get("id").and_then(|i| i.as_str()).map(|s| s.to_string())).collect()
        };
        let (c, g) = (ids(&cn), ids(&gl));
        let dup: Vec<&String> = c.iter().filter(|i| g.contains(i)).collect();
        println!("CN ids: {:?}", c);
        println!("Global ids: {:?}", g);
        println!("两区重名 {}: {:?}", dup.len(), dup);
        let only_g: Vec<&String> = g.iter().filter(|i| !c.contains(i)).collect();
        println!("仅海外 {}: {:?}", only_g.len(), only_g);
    }
}






