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
                // 2026-10-22 P0：insert() 而非 IndexMut（preserve_order 下键缺失
                // 会 panic，见 mark_exhausted 处说明）。`last_success` 在新建
                // 条目里同样不存在，是同一处雷。
                if let Some(obj) = m.as_object_mut() {
                    obj.insert("count".to_string(), json!(cnt + 1));
                    obj.insert("last_success".to_string(), json!(now_cn()));
                }
            }
        }
    }
    save(data_dir, &root);
}

/// 耗尽记录联动：计数归零（新周期开始）+ 记耗尽时刻与恢复时间。
///
/// 必须走 upsert：某模型「一次都没成功过就直接撞限额」时 `/family/uid/model`
/// 路径尚未存在，旧实现 `pointer_mut` 拿不到对象就静默返回，额度中心永远显示
/// 「—」而非「已用完」——正是刚上线就撞免费额度的模型（第一个请求即 114/119）
/// 最需要可见的场景。
pub fn mark_exhausted(data_dir: &Path, family: &str, uid: &str, model: &str, reset_at: Option<&str>) {
    let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut root = load(data_dir);
    let model_key = model.to_lowercase();
    let fam = match root.get_mut(family).and_then(Value::as_object_mut) {
        Some(f) => f,
        None => {
            // 家族缺失：新建（首条记录即耗尽的账号）
            if !root.is_object() {
                root = json!({});
            }
            let obj = root.as_object_mut().expect("root coerced to object");
            obj.entry(family.to_string()).or_insert_with(|| json!({}));
            obj.get_mut(family).and_then(Value::as_object_mut).expect("just inserted")
        }
    };
    let acct = match fam.get_mut(uid).and_then(Value::as_object_mut) {
        Some(a) => a,
        None => {
            fam.entry(uid.to_string()).or_insert_with(|| json!({}));
            fam.get_mut(uid).and_then(Value::as_object_mut).expect("just inserted")
        }
    };
    let m = acct.entry(model_key.clone()).or_insert_with(|| json!({"count": 0}));
    if !m.is_object() {
        *m = json!({"count": 0});
    }
    if let Some(mo) = m.as_object_mut() {
        // 2026-10-22 P0：一律用 insert()，禁用 `mo["k"] = ...`。
        // serde_json 开 preserve_order 时 Map 底层是 IndexMap，其 IndexMut
        // 在键不存在时 panic（"no entry found for key"）——`entry()` 新建的
        // 对象只有 count，写 exhausted_at 必崩，整个请求线程随之死亡
        // （流式 = 零字节 SSE → 客户端 empty_model_response）。
        mo.insert("count".to_string(), json!(0));
        mo.insert("exhausted_at".to_string(), json!(now_cn()));
        if let Some(r) = reset_at {
            mo.insert("reset_at".to_string(), json!(r));
        }
    }
    save(data_dir, &root);
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

#[cfg(test)]
mod tests {
    use super::*;

    /// P0 回归（2026-10-22）：`mark_exhausted` 必须能在**全新路径**上写入。
    ///
    /// 历史 bug：`mo["exhausted_at"] = ...` 在 serde_json 开 preserve_order 时
    /// 走 Map 的 IndexMut → `expect("no entry found for key")` panic。而
    /// `entry().or_insert(json!({"count":0}))` 新建的对象里没有 exhausted_at，
    /// 于是「某模型第一次撞额度」必定 panic（panic.log 记了 25 次），流式请求
    /// 线程当场死亡 → 客户端收到零字节 SSE → empty_model_response。
    #[test]
    fn mark_exhausted_on_fresh_path_does_not_panic() {
        let dir = std::env::temp_dir().join(format!(
            "free-usage-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();

        // 全新 family/uid/model：三层 entry 都要新建，正是历史 panic 的场景
        mark_exhausted(&dir, "qoder", "uid-fresh", "Qwen3.8-Flash", Some("2026-10-23 00:00"));

        let snap = snapshot(&dir);
        let m = snap
            .pointer("/qoder/uid-fresh/qwen3.8-flash")
            .expect("首次耗尽必须落库（历史上这里直接 panic）");
        assert_eq!(m.get("count").and_then(Value::as_u64), Some(0), "耗尽后计数归零");
        assert!(m.get("exhausted_at").and_then(Value::as_str).is_some(), "必须记耗尽时刻");
        assert_eq!(
            m.get("reset_at").and_then(Value::as_str),
            Some("2026-10-23 00:00"),
            "必须记恢复时刻"
        );

        // 再来一次（路径已存在）：仍不得 panic，且能覆盖
        mark_exhausted(&dir, "qoder", "uid-fresh", "qwen3.8-flash", None);
        let snap2 = snapshot(&dir);
        assert!(snap2.pointer("/qoder/uid-fresh/qwen3.8-flash/exhausted_at").is_some());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 同一 P0 的另一半：`bump` 在**首次成功**路径写 `last_success` 时同样
    /// 会命中原生 Map 索引 panic（新建条目只有 count）。
    #[test]
    fn bump_on_fresh_path_does_not_panic() {
        let dir = std::env::temp_dir().join(format!(
            "free-usage-bump-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();

        bump(&dir, "buddy", "wb-fresh", "step-5-preview");
        bump(&dir, "buddy", "wb-fresh", "step-5-preview");

        let snap = snapshot(&dir);
        let m = snap.pointer("/buddy/wb-fresh/step-5-preview").expect("计数必须落库");
        assert_eq!(m.get("count").and_then(Value::as_u64), Some(2));
        assert!(m.get("last_success").and_then(Value::as_str).is_some(), "必须记最后成功时刻");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
