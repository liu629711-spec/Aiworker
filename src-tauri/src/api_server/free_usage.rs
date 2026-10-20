//! 免费模型用量计数（2026-10-17 额度中心数据源）。
//!
//! 上游不开放免费模型剩余额度查询（2026-10-17 实测：billing/free-packages
//! 海外侧 400、包接口仅积分包、促销活动无查询接口）——网关侧自行记账：
//! - 成功调用 rate==0 模型 → (family, account, model) 计数 +1；
//! - 耗尽错误码记录（wb/qoder 的 note_free_*_exhausted）→ 计数归零 + 记
//!   耗尽时刻/恢复时间 → 「本周期已用 N 次」语义成立；
//! - 展示：额度中心（跨家族聚合）。
//!
//! 存储：kv `free_model_usage`；热路径每成功一次一读一写（个人应用 QPS 低，
//! SQLite 单行 WAL，可接受）。自身 Mutex 防并发覆盖。

use serde_json::{json, Value};
use std::path::Path;
use std::sync::Mutex;

static LOCK: Mutex<()> = Mutex::new(());

fn load(data_dir: &Path) -> Value {
    let v: Value = crate::store::db(data_dir).kv_get("free_model_usage");
    if v.is_object() {
        v
    } else {
        json!({})
    }
}

fn save(data_dir: &Path, v: &Value) {
    let _ = crate::store::db(data_dir).kv_set("free_model_usage", v);
}

/// 成功调用免费模型计数（family: buddy | qoder；trae 无免费档概念不接入）
pub fn bump(data_dir: &Path, family: &str, uid: &str, model: &str) {
    let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut root = load(data_dir);
    let ptr = format!("/{family}/{uid}/{}", model.to_lowercase());
    let cnt = root
        .pointer(&ptr)
        .and_then(|m| m.get("count"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    if let Some(obj) = root.as_object_mut() {
        let fam = obj
            .entry(family.to_string())
            .or_insert_with(|| json!({}));
        if let Some(ao) = fam.as_object_mut() {
            let acct = ao
                .entry(uid.to_string())
                .or_insert_with(|| json!({}));
            if let Some(mo) = acct.as_object_mut() {
                let m = mo
                    .entry(model.to_lowercase())
                    .or_insert_with(|| json!({"count": 0}));
                m["count"] = json!(cnt + 1);
                m["last_success"] = json!(now_cn());
            }
        }
    }
    save(data_dir, &root);
}

/// 耗尽记录联动：计数归零（新周期开始）+ 记耗尽时刻与恢复时间
pub fn mark_exhausted(data_dir: &Path, family: &str, uid: &str, model: &str, reset_at: Option<&str>) {
    let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut root = load(data_dir);
    let now = now_cn();
    if let Some(m) = root
        .pointer_mut(&format!("/{family}/{uid}/{}", model.to_lowercase()))
        .and_then(Value::as_object_mut)
    {
        m["count"] = json!(0);
        m["exhausted_at"] = json!(now);
        if let Some(r) = reset_at {
            m["reset_at"] = json!(r);
        }
        save(data_dir, &root);
    }
}

/// 全量快照（额度中心）：{family: {uid: {model: {count, last_success, exhausted_at, reset_at}}}}
pub fn snapshot(data_dir: &Path) -> Value {
    load(data_dir)
}

/// 东八区 "YYYY-MM-DD HH:MM:SS"（与 wb_route/qoder_route 的耗尽时刻同形态）
fn now_cn() -> String {
    use chrono::TimeZone;
    chrono::FixedOffset::east_opt(8 * 3600)
        .and_then(|cn| cn.timestamp_opt(chrono::Utc::now().timestamp(), 0).single())
        .map(|d| d.format("%Y-%m-%d %H:%M:%S").to_string())
        .unwrap_or_default()
}
