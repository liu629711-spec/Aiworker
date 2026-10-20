//! ZCode 上游请求路径（z.ai GLM OAuth 账号池）
//!
//! 请求流程（对齐 wb/qoder 两池的执行路径语义，去掉本池不需要的分支）：
//! 1. 取号（zcode_pool 调度，Key 白名单/专一约束同构适用，无区域维度）；
//! 2. identity 回调解析该账号最新 zai token + device_mid；
//! 3. 构造上游请求（OpenAI 内部格式 → anthropic messages 信封，强制 stream）；
//! 4. 分级重试（RetrySame 同号 / SwitchKey 换号 / Fatal 透传）；
//! 5. SSE keep-alive 15s + 首字超时 60s 故障转移（不计错不冷却）；
//! 6. 用量记账 + 请求级日志（含 TTFB）。
//!
//! 上游错误语义（实测）：
//! - 429 code=1113「Insufficient balance or no resource package」= 账号无资源包
//!   （zcode 侧 plans 为空）→ 按 SoftRate 整号短冷却，池内换号重试；
//! - 401 = token 被吊销 → SessionDead 语义（zai token 无 exp，长期有效，
//!   被吊销即需用户在 ZCode 域重新登录导入，本路径不做刷新）；
//! - 4xx 其余 = 请求级错误，Fatal 透传。

use std::collections::HashSet;
use std::io::Read;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::StatusCode;
use axum::response::Response;
use bytes::Bytes;
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

use super::pool::PickedAccount;
use super::retry::{retry_plan, RetryAction};
use super::routes::{anthropic_error, openai_error, stream_msg_or_fallback, Protocol};
use super::wb_sse;
use super::zcode_catalog::{self, ZCodeCreds};
use super::{classify_error, ApiSharedState, ErrKind, InflightGuard};

/// 上游 agent（连接 10s / 读 300s，同 wb_upstream 口径；GLM 长输出不设短读超时）
fn zcode_agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_read(Duration::from_secs(300))
        .timeout_write(Duration::from_secs(30))
        .timeout_connect(Duration::from_secs(10))
        .max_idle_connections(10)
        .max_idle_connections_per_host(10)
        .build()
}

/// 上游错误（HTTP 层）：(状态码, 响应体, Retry-After 秒)
type UpstreamErr = (u16, String, Option<u64>);

/// 无资源包错误码（实测 429 body：code=1113「Insufficient balance or no
/// resource package. Please recharge.」）——账号级套餐问题，按 SoftRate
/// 整号短冷却后池内换号
const NO_RESOURCE_PACK_CODE: &str = "1113";

/// ZCode 客户端同款请求头（ZaiClient::new 同款，缺失会被服务端 400 code=3001 拒绝）
fn client_headers() -> [(&'static str, &'static str); 5] {
    [
        ("user-agent", "ZCode/3.14.5"),
        ("x-zcode-app-version", "3.14.5"),
        ("x-platform", "win32-x64"),
        ("x-os-category", "windows"),
        ("http-referer", "https://zcode.z.ai"),
    ]
}

/// OpenAI 内部格式 → anthropic messages 信封。
/// system 消息提取为顶层 system 字段（ anthropic 协议形态）；messages 的
/// system 角色剔除；user/assistant 原样保留（content 字符串或块数组两种
/// 形态 anthropic 均接受）。max_tokens 键名兼容 max_completion_tokens。
/// stream 恒 true（上游只回 SSE，聚合与非流式统一处理）。
fn to_anthropic_body(peek: &Value, model: &str) -> Vec<u8> {
    let mut system_parts: Vec<String> = Vec::new();
    let mut msgs: Vec<Value> = Vec::new();
    if let Some(arr) = peek.get("messages").and_then(Value::as_array) {
        for m in arr {
            let role = m.get("role").and_then(Value::as_str).unwrap_or("");
            match role {
                "system" => {
                    // system 内容提顶层（字符串或块数组取文本段）
                    match m.get("content") {
                        Some(Value::String(s)) => system_parts.push(s.clone()),
                        Some(Value::Array(blocks)) => {
                            for b in blocks.iter().filter_map(Value::as_object) {
                                if b.get("type").and_then(Value::as_str) == Some("text") {
                                    if let Some(t) = b.get("text").and_then(Value::as_str) {
                                        system_parts.push(t.to_string());
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                }
                "user" | "assistant" => msgs.push(m.clone()),
                _ => {}
            }
        }
    }
    let mut body = json!({
        "model": model,
        "stream": true,
        "messages": msgs,
    });
    if !system_parts.is_empty() {
        body["system"] = json!(system_parts.join("\n\n"));
    }
    // max_tokens：anthropic 必填（新键优先兼容）
    let max_tokens = peek
        .get("max_tokens")
        .or_else(|| peek.get("max_completion_tokens"))
        .and_then(Value::as_i64)
        .filter(|v| *v > 0)
        .unwrap_or(8192);
    body["max_tokens"] = json!(max_tokens);
    // temperature / top_p 原样透传（anthropic 同名字段）
    for key in ["temperature", "top_p"] {
        if let Some(v) = peek.get(key) {
            body[key] = v.clone();
        }
    }
    // GLM thinking：客户端经 reasoning_effort 传档位时映射 anthropic thinking 字段
    // （low/max/high → enabled + budget_tokens 分档；其余值不注入）
    if let Some(effort) = peek.get("reasoning_effort").and_then(Value::as_str) {
        if let Some(budget) = thinking_budget(effort, max_tokens) {
            body["thinking"] = json!({"type": "enabled", "budget_tokens": budget});
        }
    }
    body.to_string().into_bytes()
}

/// GLM-5.3 思考档位 → thinking budget_tokens（分档口径 = ZCode 客户端
/// reasoning.variants [low, max, high]；budget 按输出上限比例取整）
fn thinking_budget(effort: &str, max_tokens: i64) -> Option<i64> {
    let ratio = match effort.trim().to_lowercase().as_str() {
        "low" => 0.2,
        "max" => 0.6,
        "high" => 0.8,
        _ => return None,
    };
    Some(((max_tokens as f64) * ratio) as i64 / 1024 * 1024)
}

/// 发起上游请求（anthropic messages，强制 SSE）
fn make_zcode_request(
    creds: &ZCodeCreds,
    body: &[u8],
) -> Result<Box<dyn Read + Send>, UpstreamErr> {
    let url = format!("{}{}", zcode_catalog::BASE_URL, zcode_catalog::MESSAGES_PATH);
    let mut req = zcode_agent()
        .post(&url)
        .set("content-type", "application/json")
        .set("accept", "text/event-stream")
        .set("anthropic-version", "2023-06-01")
        .set("authorization", &format!("Bearer {}", creds.access_token));
    if !creds.device_mid.is_empty() {
        req = req.set("x-device-mid", &creds.device_mid);
    }
    for (k, v) in client_headers() {
        req = req.set(k, v);
    }
    match req.send_bytes(body) {
        Ok(r) => Ok(Box::new(r.into_reader())),
        Err(ureq::Error::Status(code, resp)) => {
            let text = resp.into_string().unwrap_or_default();
            Err((code, text, None))
        }
        Err(e) => Err((502, format!("ZCode 上游传输错误: {e}"), None)),
    }
}

/// 上游错误体里的业务码提取（anthropic 错误格式：error.code）
fn upstream_code(body: &str) -> Option<String> {
    serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| v.pointer("/error/code").and_then(Value::as_str).map(str::to_string))
}

/// 1113（无资源包）判定：账号级套餐问题 → SoftRate 语义
fn is_no_resource_pack(status: u16, body: &str) -> bool {
    status == 429 && upstream_code(body).as_deref() == Some(NO_RESOURCE_PACK_CODE)
}

fn now_ts() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn safe_slice(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

fn internal_error_response() -> Response {
    Response::builder()
        .status(StatusCode::INTERNAL_SERVER_ERROR)
        .body(Body::from("{\"error\":{\"message\":\"internal error\"}}"))
        .unwrap_or_else(|_| Response::new(Body::empty()))
}

// ==================== 执行入口 ====================

/// 流式执行入口（三协议入口的 ZCode 臂共用）
pub fn zcode_stream_chat(
    state: Arc<ApiSharedState>,
    body_vec: Vec<u8>,
    model: String,
    start_ts: Instant,
    proto: Protocol,
    key_id: String,
    guard: InflightGuard,
) -> Response {
    let (tx, rx) = mpsc::channel::<Result<Bytes, std::io::Error>>(64);
    let state2 = Arc::clone(&state);
    // 上游请求 + 轮换重试整体放阻塞池（同 qoder/wb 流式路径）
    std::thread::spawn(move || {
        let _ = run_stream(&state2, &body_vec, &model, &key_id, guard, &tx, proto, start_ts);
    });
    let stream = ReceiverStream::new(rx);
    Response::builder()
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache")
        .header("connection", "keep-alive")
        .body(Body::from_stream(stream))
        .unwrap_or_else(|_| internal_error_response())
}

/// 非流式执行入口（聚合 + 分级重试，迁入 stream_runtime 阻塞池）
pub async fn zcode_aggregate_chat(
    state: Arc<ApiSharedState>,
    body_vec: Vec<u8>,
    model: String,
    stream: bool,
    start_ts: Instant,
    proto: Protocol,
    key_id: String,
    guard: InflightGuard,
) -> Response {
    let model_out = model.clone();
    let result = super::stream_runtime().spawn_blocking(move || {
        let mut guard = guard;
        let key_name = super::api_keys::key_name_for(&state.data_dir, &key_id);
        let peek: Value = serde_json::from_slice(&body_vec).unwrap_or(json!({}));

        let mut tried: HashSet<String> = HashSet::new();
        let mut same_attempt: u32 = 0;

        'pick: loop {
            // ── 取号：Key 约束 + 调度策略（无区域维度）──────────────
            let (allowed_set, dedicated) = super::api_keys::constraints_for(&state.data_dir, &key_id)
                .and_then(|k| k.pool_constraints("zcode"))
                .map_or((None, None), |c| (c.allowed, c.dedicated));
            let picked = match state.zcode_pool.pick_excluding_constrained_ev_region(
                &tried,
                allowed_set.as_ref(),
                dedicated.as_deref(),
                None,
            ) {
                Some((p, ev)) => {
                    if let Some(ev) = ev {
                        state.logger.log_sched_event(&ev);
                    }
                    p
                }
                None => {
                    state.record_usage(false, &model, "none", &key_id, false, stream,
                        start_ts.elapsed().as_millis() as u64, 0, 0);
                    let (healthy_total, _) = state.zcode_pool.selectable_stats_in(None);
                    return Err(format!(
                        "no healthy account available (pool=zcode healthy={healthy_total})"
                    ));
                }
            };
            tried.insert(picked.uid.clone());
            // F-77 账号级在途计数（CAS 复核并发上限；over_limit 候选无条件绑定）
            if picked.over_limit {
                guard = guard.bind_account(state.zcode_pool.inflight_handle(&picked.uid));
            } else if !guard.bind_account_cas(
                state.zcode_pool.inflight_handle(&picked.uid),
                state.zcode_pool.concurrency_limit(),
            ) {
                // 在途已满：不发起请求，换下一候选（不计错误不冷却）
                continue;
            }
            *safe_lock(&state.active_uid) = Some(picked.uid.clone());

            // ── identity 回调解析最新凭证 ──────────────────────────
            let creds: ZCodeCreds = match state.zcode_identity.as_ref() {
                Some(cb) => match cb(&picked.uid) {
                    Ok(c) if !c.access_token.is_empty() => c,
                    Ok(_) => {
                        state.zcode_pool.note_error(&picked.uid, ErrKind::SessionDead);
                        continue;
                    }
                    Err(e) => {
                        // 暂态/永久统一走换号（zcode 无刷新链路，错误由下轮
                        // identity 回调重试；永久失效需用户在 zcode 域重新登录）
                        *safe_lock(&state.last_error) =
                            Some(format!("zcode identity uid={} err={}", picked.uid, e));
                        state.zcode_pool.note_error(&picked.uid, ErrKind::Server);
                        continue;
                    }
                },
                None => {
                    return Err("zcode identity 回调未注入（ZCode 池未装配）".to_string());
                }
            };

            let anthropic_body = to_anthropic_body(&peek, &model);
            let ttfb_start = Instant::now();
            match make_zcode_request(&creds, &anthropic_body) {
                Ok(reader) => {
                    // 聚合路径：首字窗口 60s 超时 → Server 换号（不计 model 冷却）
                    let ilines = match super::wb_upstream::InterruptibleLines::from_reader_timeout(
                        reader,
                        Duration::from_secs(60),
                    ) {
                        Ok(l) => l,
                        Err(()) => {
                            state.zcode_pool.note_error(&picked.uid, ErrKind::Server);
                            continue 'pick;
                        }
                    };
                    let (resp, error_info) = wb_sse::aggregate(ilines, &format!("chatcmpl-{}", now_ts()));
                    let duration_ms = start_ts.elapsed().as_millis() as u64;
                    let ttfb_ms = ttfb_start.elapsed().as_millis() as u64;
                    match (resp, error_info) {
                        (Some(mut r), None) => {
                            r["model"] = json!(model);
                            let (pt, ct) = r.get("usage").map(wb_sse::u64_pair).unwrap_or((0, 0));
                            state.record_usage(false, &model, &picked.uid, &key_id, true, stream,
                                duration_ms, pt, ct);
                            state.zcode_pool.note_success(&picked.uid);
                            state.logger.log_request(
                                "zcode", "POST", "/v1/chat/completions", &model, stream, 200, &picked.uid,
                                duration_ms, &key_name, &state.zcode_pool.name_of(&picked.uid), None,
                            );
                            let _ = ttfb_ms;
                            return Ok(r);
                        }
                        (None, Some((code, msg))) => {
                            let kind = classify_error(code as u16, &msg);
                            if kind != ErrKind::None {
                                // 1113 无资源包按 SoftRate（整号短冷却，池内换号）；
                                // 其余按 classify 口径
                                state.zcode_pool.note_error(&picked.uid, kind);
                            }
                            *safe_lock(&state.last_error) =
                                Some(format!("zcode uid={} code={} msg={}", picked.uid, code, msg));
                            state.logger.log_request(
                                "zcode", "POST", "/v1/chat/completions", &model, stream, 200, &picked.uid,
                                duration_ms, &key_name, &state.zcode_pool.name_of(&picked.uid), Some(&msg),
                            );
                            continue 'pick;
                        }
                        _ => {
                            state.zcode_pool.note_error(&picked.uid, ErrKind::Server);
                            state.logger.log_request(
                                "zcode", "POST", "/v1/chat/completions", &model, stream, 502, &picked.uid,
                                duration_ms, &key_name, &state.zcode_pool.name_of(&picked.uid), Some("empty response"),
                            );
                            continue 'pick;
                        }
                    }
                }
                Err((status, resp_body, retry_after)) => {
                    // 1113 无资源包：账号级问题，直接换号（同号重试无意义）
                    let action = if is_no_resource_pack(status, &resp_body) {
                        state.zcode_pool.note_error(&picked.uid, ErrKind::SoftRate);
                        RetryAction::SwitchKey
                    } else {
                        retry_plan(status, &resp_body, same_attempt, retry_after)
                    };
                    match action {
                        RetryAction::RetrySame { delay_ms } => {
                            same_attempt += 1;
                            std::thread::sleep(Duration::from_millis(delay_ms.min(60_000)));
                            continue;
                        }
                        RetryAction::SwitchKey => {
                            let kind = classify_error(status, &resp_body);
                            if kind != ErrKind::None {
                                state.zcode_pool.note_error(&picked.uid, kind);
                            }
                            state.logger.log_request(
                                "zcode", "POST", "/v1/chat/completions", &model, stream, status, &picked.uid,
                                start_ts.elapsed().as_millis() as u64,
                                &key_name, &state.zcode_pool.name_of(&picked.uid),
                                Some(&safe_slice(&resp_body, 300)),
                            );
                            continue 'pick;
                        }
                        RetryAction::Fatal => {
                            state.record_usage(false, &model, &picked.uid, &key_id, false, stream,
                                start_ts.elapsed().as_millis() as u64, 0, 0);
                            state.logger.log_request(
                                "zcode", "POST", "/v1/chat/completions", &model, stream, status, &picked.uid,
                                start_ts.elapsed().as_millis() as u64, &key_name,
                                &state.zcode_pool.name_of(&picked.uid), Some(&safe_slice(&resp_body, 300)),
                            );
                            return Err(format!(
                                "upstream {} error: {}",
                                status,
                                safe_slice(&resp_body, 300)
                            ));
                        }
                    }
                }
            }
        }
    })
    .await;

    match result {
        Ok(Ok(resp)) => {
            let body = match proto {
                Protocol::Anthropic => wb_sse::completion_to_anthropic(
                    &resp,
                    &format!("msg_{}", now_ts()),
                    &model_out,
                ),
                Protocol::OpenAiText => wb_sse::completion_to_text(&resp, &model_out),
                Protocol::Responses => super::wb_responses::completion_to_responses(
                    &resp,
                    &format!("resp_{}", now_ts()),
                    &model_out,
                    &HashSet::new(),
                ),
                Protocol::OpenAi => resp,
            };
            Response::builder()
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap_or_else(|_| internal_error_response())
        }
        Ok(Err(msg)) => {
            let upstream_status = msg
                .strip_prefix("upstream ")
                .and_then(|rest| rest.split(' ').next())
                .and_then(|s| s.parse::<u16>().ok());
            let status = match upstream_status {
                Some(s) if (400..500).contains(&s) && !matches!(s, 401 | 403) => {
                    StatusCode::from_u16(s).unwrap_or(StatusCode::SERVICE_UNAVAILABLE)
                }
                _ => StatusCode::SERVICE_UNAVAILABLE,
            };
            match proto {
                Protocol::Anthropic => anthropic_error(status, "api_error", &msg),
                _ => openai_error(status, "upstream_error", &msg),
            }
        }
        Err(e) => openai_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            &format!("task join error: {}", e),
        ),
    }
}

// ==================== 流式内部 ====================

/// 流式轮换主循环（首次成功即转发至 EOF；失败换号重试）。
/// 简化口径（vs wb/qoder 流式）：无对冲、无排队退避、无 per-model 冷却——
/// zcode 池错误面窄（1113 换号 / 401 禁用 / 4xx 透传），重试语义
/// 由换号循环 + retry_plan 同号退避覆盖
fn run_stream(
    state: &Arc<ApiSharedState>,
    body_vec: &[u8],
    model: &str,
    key_id: &str,
    mut guard: InflightGuard,
    tx: &mpsc::Sender<Result<Bytes, std::io::Error>>,
    proto: Protocol,
    start_ts: Instant,
) -> Result<(), String> {
    let key_name = super::api_keys::key_name_for(&state.data_dir, key_id);
    let peek: Value = serde_json::from_slice(body_vec).unwrap_or(json!({}));
    let mut tried: HashSet<String> = HashSet::new();
    let mut same_attempt: u32 = 0;
    let chat_id = format!("chatcmpl-{}", now_ts());

    'pick: loop {
        let (allowed_set, dedicated) = super::api_keys::constraints_for(&state.data_dir, key_id)
            .and_then(|k| k.pool_constraints("zcode"))
            .map_or((None, None), |c| (c.allowed, c.dedicated));
        let picked: PickedAccount = match state.zcode_pool.pick_excluding_constrained_ev_region(
            &tried,
            allowed_set.as_ref(),
            dedicated.as_deref(),
            None,
        ) {
            Some((p, ev)) => {
                if let Some(ev) = ev {
                    state.logger.log_sched_event(&ev);
                }
                p
            }
            None => {
                let (healthy_total, _) = state.zcode_pool.selectable_stats_in(None);
                let msg = format!(
                    "no healthy account available (pool=zcode healthy={healthy_total})"
                );
                state.record_usage(false, model, "none", key_id, false, true,
                    start_ts.elapsed().as_millis() as u64, 0, 0);
                state.logger.log_request(
                    "zcode", "POST", "/v1/chat/completions", model, true, 503, "-",
                    start_ts.elapsed().as_millis() as u64, &key_name, "", Some(&msg),
                );
                send_stream_error(tx, proto, 503, &msg);
                return Ok(());
            }
        };
        tried.insert(picked.uid.clone());
        let bound = if picked.over_limit {
            guard = guard.bind_account(state.zcode_pool.inflight_handle(&picked.uid));
            true
        } else {
            guard.bind_account_cas(
                state.zcode_pool.inflight_handle(&picked.uid),
                state.zcode_pool.concurrency_limit(),
            )
        };
        if !bound {
            continue;
        }
        *safe_lock(&state.active_uid) = Some(picked.uid.clone());

        let creds: ZCodeCreds = match state.zcode_identity.as_ref() {
            Some(cb) => match cb(&picked.uid) {
                Ok(c) if !c.access_token.is_empty() => c,
                Ok(_) => {
                    state.zcode_pool.note_error(&picked.uid, ErrKind::SessionDead);
                    continue;
                }
                Err(e) => {
                    *safe_lock(&state.last_error) =
                        Some(format!("zcode identity uid={} err={}", picked.uid, e));
                    state.zcode_pool.note_error(&picked.uid, ErrKind::Server);
                    continue;
                }
            },
            None => {
                let msg = "zcode identity 回调未注入（ZCode 池未装配）".to_string();
                send_stream_error(tx, proto, 503, &msg);
                return Ok(());
            }
        };

        let anthropic_body = to_anthropic_body(&peek, model);
        match make_zcode_request(&creds, &anthropic_body) {
            Ok(reader) => {
                // 首字窗口 60s（z.ai GLM 长思考模型首字可慢；超时不计错不冷却）
                let ilines = match super::wb_upstream::InterruptibleLines::from_reader_timeout(
                    reader,
                    Duration::from_secs(60),
                ) {
                    Ok(l) => l,
                    Err(()) => {
                        // 首字超时：Server 语义换号重试（不计 model 冷却）
                        state.zcode_pool.note_error(&picked.uid, ErrKind::Server);
                        state.logger.log_request(
                            "zcode", "POST", "/v1/chat/completions", model, true, 504, &picked.uid,
                            start_ts.elapsed().as_millis() as u64, &key_name,
                            &state.zcode_pool.name_of(&picked.uid), Some("first byte timeout"),
                        );
                        break;
                    }
                };
                // 上游 SSE（OpenAI 兼容 chunk）→ 客户端协议转发（含 keep-alive 15s
                // + 客户端断连三层检测 + Anthropic/Responses 协议转换）
                let (error_info, _sent_any, _failed_inline, usage) = wb_sse::stream_forward_ex(
                    ilines, tx, proto, &chat_id, model, &HashSet::new(),
                );
                let duration_ms = start_ts.elapsed().as_millis() as u64;
                match error_info {
                    None => {
                        let (pt, ct) = usage.as_ref().map(wb_sse::u64_pair).unwrap_or((0, 0));
                        state.record_usage(false, model, &picked.uid, key_id, true, true,
                            duration_ms, pt, ct);
                        state.zcode_pool.note_success(&picked.uid);
                        state.logger.log_request(
                            "zcode", "POST", "/v1/chat/completions", model, true, 200, &picked.uid,
                            duration_ms, &key_name, &state.zcode_pool.name_of(&picked.uid), None,
                        );
                        return Ok(());
                    }
                    Some((code, msg)) => {
                        // 流内错误（上游 SSE 带回 error 帧）：按错误码分类换号
                        let kind = classify_error(code as u16, &msg);
                        if kind != ErrKind::None {
                            state.zcode_pool.note_error(&picked.uid, kind);
                        }
                        *safe_lock(&state.last_error) =
                            Some(format!("zcode uid={} code={} msg={}", picked.uid, code, msg));
                        state.logger.log_request(
                            "zcode", "POST", "/v1/chat/completions", model, true, 200, &picked.uid,
                            duration_ms, &key_name, &state.zcode_pool.name_of(&picked.uid), Some(&msg),
                        );
                        continue 'pick;
                    }
                }
            }
            Err((status, resp_body, retry_after)) => {
                let action = if is_no_resource_pack(status, &resp_body) {
                    state.zcode_pool.note_error(&picked.uid, ErrKind::SoftRate);
                    RetryAction::SwitchKey
                } else {
                    retry_plan(status, &resp_body, same_attempt, retry_after)
                };
                match action {
                    RetryAction::RetrySame { delay_ms } => {
                        same_attempt += 1;
                        std::thread::sleep(Duration::from_millis(delay_ms.min(60_000)));
                        continue;
                    }
                    RetryAction::SwitchKey => {
                        let kind = classify_error(status, &resp_body);
                        if kind != ErrKind::None {
                            state.zcode_pool.note_error(&picked.uid, kind);
                        }
                        state.logger.log_request(
                            "zcode", "POST", "/v1/chat/completions", model, true, status, &picked.uid,
                            start_ts.elapsed().as_millis() as u64, &key_name,
                            &state.zcode_pool.name_of(&picked.uid), Some(&safe_slice(&resp_body, 300)),
                        );
                        break;
                    }
                    RetryAction::Fatal => {
                        state.record_usage(false, model, &picked.uid, key_id, false, true,
                            start_ts.elapsed().as_millis() as u64, 0, 0);
                        state.logger.log_request(
                            "zcode", "POST", "/v1/chat/completions", model, true, status, &picked.uid,
                            start_ts.elapsed().as_millis() as u64, &key_name,
                            &state.zcode_pool.name_of(&picked.uid), Some(&safe_slice(&resp_body, 300)),
                        );
                        let msg = format!(
                            "upstream {} error: {}",
                            status,
                            safe_slice(&resp_body, 300)
                        );
                        let code = status as i64;
                        send_stream_error(tx, proto, code, &msg);
                        return Ok(());
                    }
                }
            }
        }
    }
    // 换号后重进 loop（tried 已含失败号）；外层 loop 继续
    run_stream(state, body_vec, model, key_id, guard, tx, proto, start_ts)
}

/// 流内错误帧下发（同 qoder_route::send_stream_error 形态）
fn send_stream_error(
    tx: &mpsc::Sender<Result<Bytes, std::io::Error>>,
    proto: Protocol,
    code: i64,
    msg: &str,
) {
    let msg = stream_msg_or_fallback(msg, code);
    match proto {
        Protocol::Anthropic => {
            let err = json!({"type":"error","error":{"type":"api_error","message":msg}});
            let _ = tx.blocking_send(Ok(Bytes::from(format!(
                "event: error\ndata: {}\n\n",
                err
            ))));
        }
        Protocol::Responses => {
            let resp = json!({
                "id": format!("resp_{}", now_ts()),
                "object": "response",
                "status": "failed",
                "output": [],
                "error": {"code": code.to_string(), "message": msg},
            });
            let body = json!({"type": "response.failed", "response": resp});
            let _ = tx.blocking_send(Ok(Bytes::from(format!(
                "event: response.failed\ndata: {}\n\n",
                body
            ))));
        }
        _ => {
            let body = json!({"error": { "message": msg, "type": "api_error", "code": code }});
            let _ = tx.blocking_send(Ok(Bytes::from(format!("data: {}\n\n", body))));
            let _ = tx.blocking_send(Ok(Bytes::from("data: [DONE]\n\n")));
        }
    }
}

/// 锁中毒安全读取（同 wb_route safe_lock）
fn safe_lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anthropic_body_extracts_system_and_maps_effort() {
        let peek = json!({
            "messages": [
                {"role": "system", "content": "你是助手"},
                {"role": "user", "content": "hi"}
            ],
            "max_tokens": 100_000,
            "reasoning_effort": "high",
            "temperature": 0.5
        });
        let body: Value = serde_json::from_slice(&to_anthropic_body(&peek, "GLM-5.3")).unwrap();
        assert_eq!(body["model"], "GLM-5.3");
        assert_eq!(body["stream"], true);
        assert_eq!(body["system"], "你是助手");
        assert_eq!(body["max_tokens"], 100_000);
        assert_eq!(body["temperature"], 0.5);
        // thinking budget = 100000 * 0.8 取整到 KiB
        assert_eq!(body["thinking"]["type"], "enabled");
        let budget = body["thinking"]["budget_tokens"].as_i64().unwrap();
        assert_eq!(budget, 80_000);
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["role"], "user");
    }

    #[test]
    fn anthropic_body_defaults_max_tokens() {
        let peek = json!({"messages": [{"role": "user", "content": "hi"}]});
        let body: Value = serde_json::from_slice(&to_anthropic_body(&peek, "GLM-5.3-Flash")).unwrap();
        assert_eq!(body["max_tokens"], 8192);
        assert!(body.get("system").is_none());
        assert!(body.get("thinking").is_none());
    }

    #[test]
    fn max_completion_tokens_key_compat() {
        let peek = json!({
            "messages": [{"role": "user", "content": "hi"}],
            "max_completion_tokens": 4096
        });
        let body: Value = serde_json::from_slice(&to_anthropic_body(&peek, "GLM-5.3")).unwrap();
        assert_eq!(body["max_tokens"], 4096);
    }

    #[test]
    fn no_resource_pack_detection() {
        assert!(is_no_resource_pack(429, r#"{"type":"error","error":{"code":"1113","message":"Insufficient balance"}}"#));
        assert!(!is_no_resource_pack(429, r#"{"type":"error","error":{"code":"1114","message":"rate"}}"#));
        assert!(!is_no_resource_pack(401, r#"{"type":"error","error":{"code":"1113"}}"#));
    }

    #[test]
    fn thinking_budget_tiers() {
        assert_eq!(thinking_budget("low", 10_000), Some(2_000));
        assert_eq!(thinking_budget("max", 10_000), Some(6_000));
        assert_eq!(thinking_budget("high", 10_000), Some(8_000));
        assert_eq!(thinking_budget("off", 10_000), None);
        assert_eq!(thinking_budget("weird", 10_000), None);
    }
}
