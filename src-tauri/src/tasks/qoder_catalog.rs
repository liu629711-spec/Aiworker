//! Qoder 模型目录每日同步（p3-3 收尾接线）。
//!
//! 网关路由判定依赖 [`qoder_upstream`] 目录（`resolve`/`list`）；静态兜底表是
//! 蓝本快照，倍率/名单会漂移（2026-10-01 实抓实证 GLM-5.3 0.6→0.8 等 7 处）。
//! 本任务每日拉取 `model/list`（真 COSY 签名 GET，凭证走 `ensure_fresh` 全防护）
//! → [`qoder_upstream::adopt_remote`] 原子替换 CN 区缓存。
//! - 空池空转；全部账号无凭证（未登录/需重登）静默跳过不计失败；
//! - 网络/服务端失败返 Err 交调度器 30 分钟冷却重试；
//! - 目录是账号无关的全局数据，无需逐账号拉取：按池序逐可用账号尝试，
//!   任一账号成功即收口；单个账号失败记日志后试下一个，全部失败才 Err（P3）。

use serde_json::{json, Value};

use crate::state::AppState;

use super::{http_agent, qoder_common, qoder_sign, qoder_upstream};

/// model/list 路径（拼在 [`qoder_upstream::QoderRegion::gateway`] 后；
/// GET 空体，COSY 签名覆盖见 qoder_sign——sigpath 自动剥离 /algo 前缀与查询串）
const MODEL_LIST_PATH: &str = "algo/api/v2/model/list";

/// CN 区 model/list 完整 URL（抽纯函数便于单测锚定端点形态）
fn model_list_url() -> String {
    model_list_url_for(false)
}

/// model/list 完整 URL 按区构造（2026-10-17 海外接入）：网关基址随区分叉
/// （CN gateway.qoder.com.cn / Global api3.qoder.sh），路径两区同构。
fn model_list_url_for(global: bool) -> String {
    format!(
        "{}{MODEL_LIST_PATH}",
        if global {
            qoder_upstream::QoderRegion::Global.gateway()
        } else {
            qoder_upstream::QoderRegion::Cn.gateway()
        }
    )
}

/// 免费档模型清单回写账号池（免费额度可见性，2026-10-16）。
///
/// 目录刷新（run_task）采到远程清单后调用——把 `rate=0` 的模型快照写进
/// 每个账号的 `free_models` 字段，供账号页「免费模型」列展示。
/// 与积分链路（qoder_credits 池回写）同一合并语义：**保留既有 exhausted_at**
/// （额度错误码驱动的耗尽标注），只换 id/name/rate/region 部分——避免目录刷新
/// 抹掉耗尽历史。持池锁读-改-写防并发整池覆盖丢写。
fn write_back_free_models(state: &AppState) {
    let free = qoder_upstream::free_tier_models();
    if free.is_empty() {
        return; // 目录未采到免费档（静态兜底表可能无 0 倍率项）：不覆盖既有数据
    }
    let _g = state
        .qoder_pool_lock
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let db = crate::store::db(&state.data_dir);
    let mut pool = crate::store::docs::qoder_pool_load(&db);
    let Some(accounts) = pool.get_mut("accounts").and_then(Value::as_array_mut) else {
        return;
    };
    let now = crate::fs_utils::now_iso();
    let mut changed = false;
    for a in accounts.iter_mut() {
        // 按账号区域过滤免费档（修复「Qoder 免费模型 2/2」）：目录按区采集
        // （同一 Qwen3.8-Flash 两区各一条），账号只能用本区免费额度——国内账号
        // 只回写 cn 条目、海外账号只回写 global 条目（region 缺失 = 存量账号，
        // 恒按国内处理）。Buddy 域 free_models_for(region) 同语义。
        let global = a.get("region").and_then(Value::as_str) == Some("global");
        let region_free: Vec<Value> = free
            .iter()
            .filter(|m| m.get("region").and_then(Value::as_str) == Some(if global { "global" } else { "cn" }))
            .cloned()
            .collect();
        if region_free.is_empty() {
            continue; // 本区目录暂无免费档：不动既有快照（保留 exhausted 历史）
        }
        let prev: Vec<Value> = a
            .get("free_models")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let merged: Vec<Value> = qoder_upstream::merge_free_models(&prev, &region_free);
        a["free_models"] = json!(qoder_upstream::dedupe_free_models_by_id(&merged));
        a["free_models_fetched_at"] = json!(now);
        changed = true;
    }
    if changed {
        if let Err(e) = crate::store::docs::qoder_pool_save(&db, &pool) {
            crate::fs_utils::app_log(
                &state.data_dir,
                &format!("qoder: 免费档清单回写账号池失败（不影响目录同步）: {e}"),
            );
        }
    }
}

/// 调度器/CLI 共用入口：模型目录同步（CN + 海外双区，2026-10-17 海外接入）。
/// 各区独立同步：CN 账号刷 CN 目录（历史语义），海外账号刷 Global 目录
///（api3.qoder.sh）；单区失败不影响另一区，免费档清单按账号区域回写。
pub fn run_task(state: &AppState) -> Result<Value, String> {
    let accounts = crate::commands::qoder::load_pool(state);
    if accounts.is_empty() {
        return Ok(json!({ "ok": true, "skipped": "无 Qoder 账号" }));
    }
    let agent = http_agent(20);
    let mut last_err: Option<String> = None;
    let cn: Vec<_> = accounts.iter().filter(|a| a.region != "global").collect();
    let cn_res = sync_region(state, &agent, &cn, false, &mut last_err);
    let g: Vec<_> = accounts.iter().filter(|a| a.region == "global").collect();
    let global_res = if g.is_empty() {
        Ok(json!({ "skipped": "无海外账号" }))
    } else {
        sync_region(state, &agent, &g, true, &mut last_err)
    };
    // 顶层保持 CN 语义（存量消费方兼容）：CN 成功 → Ok（海外结果随 global_* 键
    // 返回）；CN 失败 → Err（海外成败仅落日志，不掩盖 CN 错误的历史语义）
    match (cn_res, global_res) {
        (Ok(mut cn_v), g_res) => {
            if let Ok(mut g_v) = g_res {
                if let (Some(cn_obj), Some(g_obj)) = (cn_v.as_object_mut(), g_v.as_object_mut()) {
                    for (k, val) in g_obj {
                        cn_obj.insert(format!("global_{k}"), val.clone());
                    }
                }
            } else if let Err(ge) = g_res {
                crate::fs_utils::app_log(
                    &state.data_dir,
                    &format!("[qoder] 海外区目录同步失败: {ge}"),
                );
            }
            Ok(cn_v)
        }
        (Err(e), _) => Err(e),
    }
}

/// 单区目录同步（run_task 的区域循环体）：`global` 决定 URL/采纳区/账号集。
/// P3 语义不变：单账号失败记日志试下一个，全部失败上抛最后错误。
fn sync_region(
    state: &AppState,
    agent: &ureq::Agent,
    accounts: &[&crate::commands::qoder::QoderAccount],
    global: bool,
    last_err: &mut Option<String>,
) -> Result<Value, String> {
    let url = model_list_url_for(global);
    let region = if global { qoder_upstream::QoderRegion::Global } else { qoder_upstream::QoderRegion::Cn };
    for a in accounts {
        let id = a.id.as_str();
        // 24h 惰性窗口（对齐探针/常规任务语义）：临期先刷再用
        let (creds, _refreshed, _note) = qoder_common::ensure_fresh(state, &agent, id, 24);
        // 无凭证/需重登账号跳过，试下一个（needs_relogin 账号的 token 可能已死）
        if creds.access_token.is_empty() || creds.uid.is_empty() {
            continue;
        }
        let identity = qoder_sign::CosyIdentity {
            user_id: &creds.uid,
            auth_token: &creds.access_token,
            name: &creds.nickname,
            email: "",
            machine_id: &creds.machine_id,
        };
        let headers = match qoder_sign::build_cosy_headers(None, &url, &identity) {
            Ok(h) => h,
            Err(e) => {
                *last_err = Some(format!("model/list 签名构造失败: {e}"));
                crate::fs_utils::app_log(
                    &state.data_dir,
                    &format!(
                        "[qoder] 账号 {id} 目录同步失败，尝试下一账号: {}",
                        last_err.as_deref().unwrap_or("")
                    ),
                );
                continue;
            }
        };
        let resp = headers
            .into_iter()
            .fold(agent.get(&url), |r, (k, v)| r.set(&k, &v))
            .call();
        match resp {
            Ok(resp) if resp.status() == 200 => {
                // Q2：响应读取失败 / 非 JSON / 目录采纳失败与签名/HTTP 非 200/网络
                // 失败同款容错——记 last_err 后落循环尾部日志试下一账号，仅在全部
                // 可用账号尝试完毕仍无成功时上抛最后错误（不再因单账号解析失败
                // 提前 Err，与函数头「单个账号失败记日志后试下一个」声明一致）
                match resp
                    .into_string()
                    .map_err(|e| format!("model/list 响应读取失败: {e}"))
                    .and_then(|body| {
                        serde_json::from_str::<Value>(&body)
                            .map_err(|e| format!("model/list 响应非 JSON: {e}"))
                    })
                    .and_then(|payload| {
                        qoder_upstream::adopt_remote_in(region, &payload, Some(&state.data_dir))
                            .map_err(|e| format!("model/list 目录采纳失败: {e}"))
                    })
                {
                    Ok(count) => {
                        // 免费档清单回写（免费额度可见性）：目录刚采到远程清单，
                        // 立即把 rate=0 模型快照写进账号池——账号页「免费模型」列
                        // 依赖它；此前只等积分刷新（≥10 分钟缓存）才更新，目录先到
                        // 会有一段空窗。回写失败不影响目录同步结果（仅少一次展示刷新）。
                        write_back_free_models(state);
                        return Ok(json!({ "ok": true, "models": count, "account": id }));
                    }
                    Err(e) => *last_err = Some(e),
                }
            }
            Ok(resp) => {
                *last_err = Some(format!("model/list HTTP {}", resp.status()));
            }
            Err(e) => {
                *last_err = Some(format!("model/list 网络失败: {e}"));
            }
        }
        crate::fs_utils::app_log(
            &state.data_dir,
            &format!("[qoder] 账号 {id} 目录同步失败，尝试下一账号: {}", last_err.as_deref().unwrap_or("")),
        );
    }
    match last_err.as_ref() {
        Some(e) => Err(e.clone()),
        None => Ok(json!({ "ok": true, "skipped": "无可用 Qoder 凭证" })),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn temp_state(tag: &str) -> AppState {
        let dir = std::env::temp_dir()
            .join(format!("aiwork_qoder_catalog_test_{tag}_{}", std::process::id()));
        let _ = std::fs::create_dir_all(dir.join("data"));
        AppState {
            data_dir: dir,
            jwt_refresh_lock: std::sync::Arc::new(std::sync::Mutex::new(())),
            qoder_pool_lock: std::sync::Arc::new(std::sync::Mutex::new(())),
            wb_pool_lock: std::sync::Arc::new(std::sync::Mutex::new(())),
            doubao_pool_lock: std::sync::Arc::new(std::sync::Mutex::new(())),
            group_defs_lock: std::sync::Arc::new(std::sync::Mutex::new(())),
        }
    }

    #[test]
    fn url_anchored_to_cn_gateway() {
        assert_eq!(
            model_list_url(),
            "https://gateway.qoder.com.cn/algo/api/v2/model/list"
        );
    }

    #[test]
    fn empty_pool_skips() {
        let st = temp_state("empty");
        let out = run_task(&st).expect("空池应 Ok");
        assert_eq!(out["ok"], true);
        assert!(out["skipped"].as_str().unwrap_or_default().contains("无 Qoder 账号"));
    }

    #[test]
    fn accounts_without_creds_skip_silently() {
        let st = temp_state("nocred");
        // 池里放一个账号（qoder_accounts 表），但 token store 为空
        // → ensure_fresh 返回 no_credential → 跳过
        crate::store::docs::qoder_pool_save(
            &crate::store::db(&st.data_dir),
            &json!({ "accounts": [ { "id": "qd-testnocred", "nickname": "t" } ] }),
        )
        .expect("写池失败");
        let out = run_task(&st).expect("无凭证应 Ok 跳过");
        assert_eq!(out["ok"], true);
        assert!(out["skipped"].as_str().unwrap_or_default().contains("无可用 Qoder 凭证"));
    }
}
