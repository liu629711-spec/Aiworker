//! ZCode 夜间/周末批处理（2026-10-23）
//!
//! ## 为什么是「批处理」而不是「API 网关」
//!
//! 实测结论（详见 docs/pending/ZCode套餐池接入方案.md 第七、八节）：
//! - ZCode 免费套餐（Start Plan 日额度 / Global Build 夜间档）**没有对外 API**：
//!   `zcode.z.ai/api/v1/zcode-plan/anthropic/v1/messages` 对非客户端请求返回
//!   405 + `code:3012`（应用层拒绝，不是 TLS 指纹 —— 同域 GET billing/balance
//!   用同样 curl 能 200）。
//! - 客户端（desktop）能用，靠的是 "provider runtime headers" 动态头；CLI 没有这个
//!   机制，所以 **CLI 恒回退到 providerOrder[0]**（也就是用户自有的 aiwork-* 中转池）。
//! - 一次 agent 回合的 inputTokens ≈ 263K（系统提示 + skills + mcp 全量），
//!   所以 3M/日 ≈ 11 次；夜间 1e8 ≈ 380 次/夜。
//!
//! 结论：日额度撑不起在线网关，但**夜间档 / 周末档的额度足够跑批处理**——这正是
//! 本模块的定位：把任务排在队列里，只在窗口内消费额度。
//!
//! ## 执行器与「归因」
//!
//! 执行走 `zcode.cjs -p <prompt> --json`（headless 单回合）。由于 CLI 会静默回退
//! provider，**不能靠配置推断到底烧了谁的额度**——所以每次执行后都去 CLI 日志里
//! 读真实 `providerId` / `modelId` 落库（`provider_used` / `model_used`）。
//!
//! 这是本模块最重要的一条：它让「今晚到底有没有烧到免费套餐」变成可观测事实，
//! 而不是又一轮猜测。
//!
//! ## 串行与工作目录
//!
//! - **串行执行**：CLI 对 Built-in Endpoint 有跨进程租约锁（`nextEligibleAt` 退避），
//!   并发只会互相顶掉、把退避推得更远。
//! - **每个任务一个干净 workspace**：CLI 按 workspace 记忆上次模型，复用目录会
//!   继承上一个任务的模型选择，污染归因。

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

/// 调度节拍：窗口内每分钟尝试取一个任务
const TICK: Duration = Duration::from_secs(60);
/// 单任务超时（实测 36–112s，留足余量到 5 分钟）
const TASK_TIMEOUT: Duration = Duration::from_secs(300);

/// 队列任务（前端直读）
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct BatchTask {
    pub id: i64,
    pub batch_id: Option<String>,
    pub prompt: String,
    pub model: String,
    pub status: String,
    pub result: Option<String>,
    pub error: Option<String>,
    /// 归因：真实 providerId（如 `account:zai-start-plan` / `aiwork-trae`）
    pub provider_used: Option<String>,
    /// 归因：真实 modelId
    pub model_used: Option<String>,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub duration_ms: Option<i64>,
    /// night | forced | off
    pub window_tag: Option<String>,
    pub attempts: i64,
    pub created_at: i64,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
}

/// 调度器对外状态（前端展示「窗口是否打开 / 下一个窗口何时开」）
#[derive(Debug, Clone, Serialize, Default)]
pub struct BatchStatus {
    pub running: bool,
    /// 是否处于夜间档窗口（23:00→09:00 本地时间）
    pub window_open: bool,
    /// 窗口关闭时，下一次开启的时间戳（unix 秒）
    pub next_window_at: Option<i64>,
    pub pending: i64,
    pub done_today: i64,
    pub failed_today: i64,
    /// 最近一次执行用到的 providerId（洞察面板直接用）
    pub last_provider: Option<String>,
    /// CLI 入口是否探测到；None = 没找到，批处理不可用
    pub cli_entry: Option<String>,
    /// 运行中/请求取消的任务（交互 #4：前端显示已运行时长 + 「完成后取消」提示）
    pub running_task: Option<BatchRunningTask>,
}

/// 当前正在执行（或等待收尾取消）的任务
#[derive(Debug, Clone, Serialize)]
pub struct BatchRunningTask {
    pub id: i64,
    pub started_at: i64,
    /// true = 用户已请求取消，CLI 无法安全中断，执行完成后收尾兑现
    pub cancel_requested: bool,
}

/// 批处理运行时（挂在 ZcodeState 上）
pub struct BatchRuntime {
    running: AtomicBool,
    /// 唤醒标志：不等 60s tick，立刻检查队列
    wake: AtomicBool,
    /// 强制标志：本次唤醒忽略时间窗（前端「立即跑一条」）
    force: AtomicBool,
}

impl Default for BatchRuntime {
    fn default() -> Self {
        Self {
            running: AtomicBool::new(false),
            wake: AtomicBool::new(false),
            force: AtomicBool::new(false),
        }
    }
}

impl BatchRuntime {
    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }
    pub fn stop(&self) {
        self.running.store(false, Ordering::SeqCst);
    }
    /// 唤醒调度器立刻看队列。`force=true` 表示这次忽略时间窗（手动插队）。
    pub fn wake(&self, force: bool) {
        if force {
            self.force.store(true, Ordering::SeqCst);
        }
        self.wake.store(true, Ordering::SeqCst);
    }
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

// ---------------------------------------------------------------------------
// CLI 入口探测
// ---------------------------------------------------------------------------

/// 探测 `zcode.cjs` 的绝对路径。
///
/// ZCode 的安装位置随发行方式变化（`D:\ZCode`、`%LOCALAPPDATA%\Programs\ZCode`、
/// `C:\Program Files\ZCode`），这里按常见位置逐个探，找到第一个存在的。
/// 找不到时返回 None，调用方给用户明确错误（而不是静默失败）。
pub fn find_cli_entry() -> Option<PathBuf> {
    let mut cands: Vec<PathBuf> = Vec::new();
    if let Ok(v) = std::env::var("ZCODE_CLI_ENTRY") {
        let p = PathBuf::from(v.trim());
        if p.is_file() {
            return Some(p);
        }
    }
    if let Ok(local) = std::env::var("LOCALAPPDATA") {
        cands.push(Path::new(&local).join("Programs/ZCode/resources/glm/zcode.cjs"));
        cands.push(Path::new(&local).join("ZCode/resources/glm/zcode.cjs"));
    }
    if let Ok(pf) = std::env::var("ProgramFiles") {
        cands.push(Path::new(&pf).join("ZCode/resources/glm/zcode.cjs"));
    }
    // 本机实测路径（发行版直接解压到磁盘根目录的情况）
    cands.push(PathBuf::from("D:/ZCode/resources/glm/zcode.cjs"));
    cands.push(PathBuf::from("C:/ZCode/resources/glm/zcode.cjs"));
    cands.into_iter().find(|p| p.is_file())
}

/// `zcode.cjs` 依赖同目录下的 `provider/zcode-builtin.json`；发行包里放在
/// `resources/config/provider/`。缺失时 CLI 会直接退出（实测报「无法定位 CLI
/// ZCode Built-in Provider Config」），这里在首次执行前补齐（幂等，已存在则跳过）。
fn ensure_provider_dir(entry: &Path) -> Result<(), String> {
    let Some(root) = entry.parent() else {
        return Err("CLI 入口无父目录".into());
    };
    let dst = root.join("provider");
    if dst.join("zcode-builtin.json").is_file() {
        return Ok(());
    }
    let src = root.parent().map(|p| p.join("config/provider"));
    let Some(src) = src.filter(|p| p.is_dir()) else {
        return Err(format!(
            "缺少 provider 配置目录：{}（请确认 ZCode 安装完整）",
            dst.display()
        ));
    };
    std::fs::create_dir_all(&dst).map_err(|e| format!("创建 {} 失败: {e}", dst.display()))?;
    for ent in std::fs::read_dir(&src).map_err(|e| e.to_string())?.flatten() {
        let to = dst.join(ent.file_name());
        if !to.exists() {
            let _ = std::fs::copy(ent.path(), &to);
        }
    }
    if dst.join("zcode-builtin.json").is_file() {
        Ok(())
    } else {
        Err("复制 provider 配置后仍缺 zcode-builtin.json".into())
    }
}

// ---------------------------------------------------------------------------
// 窗口判定
// ---------------------------------------------------------------------------

/// 夜间档窗口：23:00 → 次日 09:00（本地时间）。
///
/// 口径与 `zcode::plan_profile::NIGHT_START_HOUR/NIGHT_END_HOUR` 完全一致
/// （实测 id=17 的 `effective_at`=23:00、`ends_at`=次日 09:00），
/// 不另立常量，避免两处漂移。
pub fn window_open() -> bool {
    crate::zcode::plan_profile::in_night_window(crate::zcode::plan_profile::local_hour())
}

/// 下一次窗口开启的 unix 时间戳。
pub fn next_window_at() -> i64 {
    use chrono::{Datelike, Local, TimeZone, Timelike};
    let now = Local::now();
    let start_h = crate::zcode::plan_profile::NIGHT_START_HOUR as u32;
    let today_start = Local
        .with_ymd_and_hms(now.year(), now.month(), now.day(), start_h, 0, 0)
        .single();
    match today_start {
        Some(t) if now.hour() < start_h => t.timestamp(),
        Some(_) => (now + chrono::Duration::days(1))
            .date_naive()
            .and_hms_opt(start_h, 0, 0)
            .and_then(|n| Local.from_local_datetime(&n).single())
            .map(|d| d.timestamp())
            .unwrap_or_else(|| now.timestamp() + 3600),
        None => now.timestamp() + 3600,
    }
}

// ---------------------------------------------------------------------------
// 执行
// ---------------------------------------------------------------------------

/// 单次执行结果
struct RunOutcome {
    response: String,
    input_tokens: i64,
    output_tokens: i64,
    provider: Option<String>,
    model: Option<String>,
}

/// CLI 日志路径（`~/.zcode/cli/log/zcode-YYYY-MM-DD.jsonl`）
fn cli_log_path() -> Option<PathBuf> {
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .ok()?;
    let day = chrono::Local::now().format("%Y-%m-%d").to_string();
    let p = Path::new(&home)
        .join(".zcode/cli/log")
        .join(format!("zcode-{day}.jsonl"));
    p.is_file().then_some(p)
}

/// 归因：从 CLI 日志里读本次执行窗口内出现的 `providerId` / `modelId`。
///
/// 为什么必须这么做：CLI 的模型选择不是"配置里写了什么就用什么" —— 实测改
/// `enabled`、换 workspace、调 `providerOrder` 三种办法都会静默回退到
/// `aiwork-*`。唯一可信的证据是它自己日志里记的 providerId。
///
/// 只读文件尾部 2MB（日志单日可达上百 MB），按时间戳过滤本次执行窗口。
fn attribute_from_log(since_ts: i64) -> (Option<String>, Option<String>) {
    let Some(path) = cli_log_path() else { return (None, None) };
    let Ok(meta) = std::fs::metadata(&path) else { return (None, None) };
    let size = meta.len();
    let tail = 2 * 1024 * 1024u64;
    let start = size.saturating_sub(tail);
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut f) = std::fs::File::open(&path) else { return (None, None) };
    if f.seek(SeekFrom::Start(start)).is_err() {
        return (None, None);
    }
    let mut buf = Vec::new();
    if f.read_to_end(&mut buf).is_err() {
        return (None, None);
    }
    let text = String::from_utf8_lossy(&buf);
    let since_rfc = chrono::DateTime::from_timestamp(since_ts, 0)
        .map(|d| d.format("%Y-%m-%dT%H:%M").to_string())
        .unwrap_or_default();
    let mut provider = None;
    let mut model = None;
    for line in text.lines() {
        // 只认本次执行之后的行；日志时间戳形如 2026-10-23T07:33:20.649Z
        if !since_rfc.is_empty() {
            if let Some(pos) = line.find("\"timestamp\":\"") {
                let ts = &line[pos + 13..];
                if ts.len() >= 16 && &ts[..16] < since_rfc.as_str() {
                    continue;
                }
            }
        }
        if let Some(v) = json_str_field(line, "providerId") {
            provider = Some(v);
        }
        if let Some(v) = json_str_field(line, "modelId") {
            model = Some(v);
        }
    }
    (provider, model)
}

/// 从一行里抠 `"key":"value"`（不引入完整 JSON 解析，日志行可能被截断）
fn json_str_field(line: &str, key: &str) -> Option<String> {
    let pat = format!("\"{key}\":\"");
    let i = line.find(&pat)?;
    let rest = &line[i + pat.len()..];
    let end = rest.find('"')?;
    let v = &rest[..end];
    (!v.is_empty()).then(|| v.to_string())
}

/// 执行一个任务（阻塞，调用方负责放到 blocking 线程里跑）
fn run_task_blocking(entry: &Path, workspace: &Path, prompt: &str, model: &str) -> Result<RunOutcome, String> {
    ensure_provider_dir(entry)?;
    std::fs::create_dir_all(workspace).map_err(|e| format!("创建 workspace 失败: {e}"))?;

    let started = now();
    let mut cmd = std::process::Command::new("node");
    cmd.arg(entry)
        .arg("-p")
        .arg(prompt)
        .arg("--json")
        // 只当模型用：plan 模式不落盘、不改文件
        .arg("--mode")
        .arg("plan")
        .arg("--cwd")
        .arg(workspace)
        .current_dir(entry.parent().unwrap_or(Path::new(".")))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // 注意：CLI 没有 --model 参数，模型由客户端配置决定。这里**不去改用户配置**
    // 来"凑"模型（那会改变用户客户端的默认模型），只把期望值记在任务里，
    // 真实用到的模型一律以 `attribute_from_log` 的归因为准。
    let _ = model;

    let mut child = cmd.spawn().map_err(|e| {
        format!("启动 CLI 失败（需要 node 在 PATH 里）: {e}")
    })?;

    // 管道并发排空（审计 P1-2，2026-10-24）：stdout/stderr 若只在退出后读取，
    // 输出超过管道缓冲（~64KB）时 child 阻塞在写、本线程阻塞在 try_wait，
    // 双方互等到 TASK_TIMEOUT 误杀——大 response 必触发。两个 reader 线程
    // 全程排空，主线程只管超时窗口；单流上限 8MB 防异常输出撑爆内存
    fn drain_pipe<R: std::io::Read>(p: R) -> String {
        use std::io::Read;
        let mut buf = Vec::new();
        let _ = p.take(8 * 1024 * 1024).read_to_end(&mut buf);
        String::from_utf8_lossy(&buf).to_string()
    }
    let stdout_pipe = child
        .stdout
        .take()
        .ok_or_else(|| "stdout 管道缺失".to_string())?;
    let stderr_pipe = child
        .stderr
        .take()
        .ok_or_else(|| "stderr 管道缺失".to_string())?;
    let out_reader = std::thread::spawn(move || drain_pipe(stdout_pipe));
    let err_reader = std::thread::spawn(move || drain_pipe(stderr_pipe));

    // 带超时的等待：不用 wait_timeout crate，自旋 + try_wait
    let deadline = Instant::now() + TASK_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break st,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    // kill 后管道随之关闭；reader 有 8MB 读取上限，必然收尾
                    let _ = out_reader.join();
                    let _ = err_reader.join();
                    return Err(format!("CLI 执行超时（{}s）", TASK_TIMEOUT.as_secs()));
                }
                std::thread::sleep(Duration::from_millis(300));
            }
            Err(e) => return Err(format!("等待 CLI 失败: {e}")),
        }
    };
    let stdout = out_reader.join().unwrap_or_default();
    let stderr = err_reader.join().unwrap_or_default();

    // CLI 的 `--json` 输出是"若干诊断行 + 一个完整 JSON 对象"（诊断行在前，
    // 例如 `ZCode Built-in missing`）。所以逐个 `{` 位置尝试整段解析，
    // 取第一个能解析成功且带 `response`/`sessionId` 的对象。
    // 早先按"从最后一个 `{` 往前配平"实现是错的：那样会取到内层 `projection` 子树。
    let v = parse_cli_json(&stdout).ok_or_else(|| {
        format!(
            "CLI 未返回可解析的 JSON（exit={:?}）
stdout: {}
stderr: {}",
            status.code(),
            head(&stdout, 400),
            head(&stderr, 300)
        )
    })?;
    let response = v["response"].as_str().unwrap_or("").to_string();
    if response.is_empty() && v["eventCount"].as_i64().is_none() {
        return Err(format!(
            "CLI 无 response 字段\nstdout: {}\nstderr: {}",
            head(&stdout, 400),
            head(&stderr, 300)
        ));
    }
    let input_tokens = v["usage"]["inputTokens"].as_i64().unwrap_or(0);
    let output_tokens = v["usage"]["outputTokens"].as_i64().unwrap_or(0);

    // 归因（给日志一点落盘时间）
    std::thread::sleep(Duration::from_millis(600));
    let (provider, model_used) = attribute_from_log(started);

    Ok(RunOutcome { response, input_tokens, output_tokens, provider, model: model_used })
}

/// 从 CLI 混合输出里抠出顶层 JSON 对象。
///
/// 逐 `{` 尝试 `from_str`（要求从该位置到末尾整体是合法 JSON），取第一个
/// 带 `response` 或 `sessionId` 的对象 —— 这两个字段只在顶层结果里出现，
/// 能天然排除内层 `projection` / `usage` 子树。
fn parse_cli_json(stdout: &str) -> Option<serde_json::Value> {
    for (i, _) in stdout.match_indices('{') {
        let tail = &stdout[i..];
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(tail) {
            if v.get("response").is_some() || v.get("sessionId").is_some() {
                return Some(v);
            }
        }
    }
    None
}

fn head(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

// ---------------------------------------------------------------------------
// 队列操作
// ---------------------------------------------------------------------------

/// 入队（一次可入多条；batch_id 相同视为同一批）
pub async fn enqueue(
    db: &SqlitePool,
    prompts: &[String],
    model: &str,
    batch_id: Option<&str>,
) -> Result<usize, String> {
    let ts = now();
    let mut n = 0usize;
    for p in prompts {
        let p = p.trim();
        if p.is_empty() {
            continue;
        }
        sqlx::query(
            "INSERT INTO batch_tasks (batch_id, prompt, model, status, created_at) VALUES (?1, ?2, ?3, 'pending', ?4)",
        )
        .bind(batch_id)
        .bind(p)
        .bind(model)
        .bind(ts)
        .execute(db)
        .await
        .map_err(|e| e.to_string())?;
        n += 1;
    }
    Ok(n)
}

pub async fn list(db: &SqlitePool, limit: i64) -> Result<Vec<BatchTask>, String> {
    sqlx::query_as::<_, BatchTask>(
        "SELECT * FROM batch_tasks ORDER BY id DESC LIMIT ?1",
    )
    .bind(limit)
    .fetch_all(db)
    .await
    .map_err(|e| e.to_string())
}

pub async fn cancel(db: &SqlitePool, id: i64) -> Result<(), String> {
    // pending → 直接取消；running → 请求取消（CLI 无法安全中断，收尾时兑现，
    // 交互 #4）：running 中改打 cancel_requested，run_one 收尾分支兑现
    sqlx::query("UPDATE batch_tasks SET status='canceled', finished_at=?1 WHERE id=?2 AND status='pending'")
        .bind(now())
        .bind(id)
        .execute(db)
        .await
        .map_err(|e| e.to_string())?;
    sqlx::query("UPDATE batch_tasks SET status='cancel_requested' WHERE id=?1 AND status='running'")
        .bind(id)
        .execute(db)
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

pub async fn clear_finished(db: &SqlitePool) -> Result<u64, String> {
    let r = sqlx::query("DELETE FROM batch_tasks WHERE status IN ('done','failed','canceled')")
        .execute(db)
        .await
        .map_err(|e| e.to_string())?;
    Ok(r.rows_affected())
}

/// 重跑失败/取消的任务（把状态打回 pending，attempts 保留以便观察重试次数）
pub async fn requeue(db: &SqlitePool, id: i64) -> Result<(), String> {
    sqlx::query(
        "UPDATE batch_tasks SET status='pending', error=NULL, result=NULL, started_at=NULL, finished_at=NULL
         WHERE id=?1 AND status IN ('failed','canceled')",
    )
    .bind(id)
    .execute(db)
    .await
    .map_err(|e| e.to_string())?;
    Ok(())
}

fn batch_workspace_root() -> PathBuf {
    let base = std::env::var("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir());
    base.join("com.aiwork.studio").join("batch-ws")
}

/// 取一个 pending 任务并执行（返回是否有任务被处理）。
///
/// `force=true` 忽略窗口（前端「立即跑」），`window_tag` 记录命中原因。
pub async fn run_one(db: &SqlitePool, force: bool) -> Result<bool, String> {
    let tag = if force {
        "forced"
    } else if window_open() {
        "night"
    } else {
        "off"
    };
    if tag == "off" {
        return Ok(false);
    }
    let Some(entry) = find_cli_entry() else {
        return Err("未找到 zcode.cjs（可用环境变量 ZCODE_CLI_ENTRY 指定）".into());
    };

    let row: Option<(i64, String, String)> = sqlx::query_as(
        "SELECT id, prompt, model FROM batch_tasks WHERE status='pending' ORDER BY id LIMIT 1",
    )
    .fetch_optional(db)
    .await
    .map_err(|e| e.to_string())?;
    let Some((id, prompt, model)) = row else { return Ok(false) };

    sqlx::query(
        "UPDATE batch_tasks SET status='running', started_at=?1, window_tag=?2, attempts=attempts+1 WHERE id=?3",
    )
    .bind(now())
    .bind(tag)
    .bind(id)
    .execute(db)
    .await
    .map_err(|e| e.to_string())?;

    // 每任务独立 workspace：CLI 按 workspace 记忆模型，复用会污染归因
    let ws = batch_workspace_root().join(format!("task-{id}"));
    let entry2 = entry.clone();
    let prompt2 = prompt.clone();
    let model2 = model.clone();
    let started = Instant::now();
    let res = tokio::task::spawn_blocking(move || {
        run_task_blocking(&entry2, &ws, &prompt2, &model2)
    })
    .await
    .map_err(|e| format!("执行线程 panic: {e}"))?;
    let dur = started.elapsed().as_millis() as i64;

    // 收尾前兑现取消请求（交互 #4）：执行期间用户可请求取消（CLI 无法安全
    // 中断，单任务最长 5 分钟）——cancel_requested → canceled 并丢弃结果。
    // 先读后写，调度单线程，竞态窗口仅毫秒级
    let cancel_requested = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM batch_tasks WHERE id=?1 AND status='cancel_requested'",
    )
    .bind(id)
    .fetch_one(db)
    .await
    .unwrap_or(0)
        > 0;

    match res {
        Ok(o) => {
            if cancel_requested {
                sqlx::query(
                    "UPDATE batch_tasks SET status='canceled', error='用户取消（执行完成后生效）', duration_ms=?1, finished_at=?2 WHERE id=?3",
                )
                .bind(dur)
                .bind(now())
                .bind(id)
                .execute(db)
                .await
                .map_err(|e| e.to_string())?;
                return Ok(true);
            }
            sqlx::query(
                "UPDATE batch_tasks SET status='done', result=?1, provider_used=?2, model_used=?3,
                 input_tokens=?4, output_tokens=?5, duration_ms=?6, finished_at=?7 WHERE id=?8",
            )
            .bind(&o.response)
            .bind(&o.provider)
            .bind(&o.model)
            .bind(o.input_tokens)
            .bind(o.output_tokens)
            .bind(dur)
            .bind(now())
            .bind(id)
            .execute(db)
            .await
            .map_err(|e| e.to_string())?;
        }
        Err(e) => {
            // 错误分类前缀（交互 #4）：原始日志文本对用户不可判读，前缀一眼
            // 分清「超时/输出异常/CLI异常/其他」→ 决定重跑有没有用
            let tagged = format!("{}{}", classify_err(&e), e);
            if cancel_requested {
                sqlx::query(
                    "UPDATE batch_tasks SET status='canceled', error=?1, duration_ms=?2, finished_at=?3 WHERE id=?4",
                )
                .bind("用户取消（执行完成后生效）")
                .bind(dur)
                .bind(now())
                .bind(id)
                .execute(db)
                .await
                .map_err(|e| e.to_string())?;
                return Ok(true);
            }
            sqlx::query(
                "UPDATE batch_tasks SET status='failed', error=?1, duration_ms=?2, finished_at=?3 WHERE id=?4",
            )
            .bind(&tagged)
            .bind(dur)
            .bind(now())
            .bind(id)
            .execute(db)
            .await
            .map_err(|e| e.to_string())?;
        }
    }
    Ok(true)
}

/// 错误分类前缀（交互 #4）：任务失败原因肉眼可分
fn classify_err(e: &str) -> &'static str {
    if e.contains("执行超时") {
        "[超时] "
    } else if e.contains("未返回可解析的 JSON") || e.contains("无 response 字段") {
        "[输出异常] "
    } else if e.contains("启动 CLI 失败") || e.contains("未找到 zcode.cjs") || e.contains("缺少 provider") {
        "[CLI异常] "
    } else {
        "[其他] "
    }
}

/// 汇总状态
pub async fn status(db: &SqlitePool, running: bool) -> BatchStatus {
    let pending: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM batch_tasks WHERE status='pending'")
        .fetch_one(db)
        .await
        .unwrap_or(0);
    let today_start = {
        use chrono::TimeZone;
        let now = chrono::Local::now();
        now.date_naive()
            .and_hms_opt(0, 0, 0)
            .and_then(|n| chrono::Local.from_local_datetime(&n).single())
            .map(|d| d.timestamp())
            .unwrap_or(0)
    };
    let done_today: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM batch_tasks WHERE status='done' AND finished_at >= ?1",
    )
    .bind(today_start)
    .fetch_one(db)
    .await
    .unwrap_or(0);
    let failed_today: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM batch_tasks WHERE status='failed' AND finished_at >= ?1",
    )
    .bind(today_start)
    .fetch_one(db)
    .await
    .unwrap_or(0);
    let last_provider: Option<String> = sqlx::query_scalar(
        "SELECT provider_used FROM batch_tasks WHERE provider_used IS NOT NULL ORDER BY id DESC LIMIT 1",
    )
    .fetch_optional(db)
    .await
    .ok()
    .flatten();

    let running_task: Option<BatchRunningTask> = sqlx::query_as::<_, (i64, i64, String)>(
        "SELECT id, started_at, status FROM batch_tasks WHERE status IN ('running','cancel_requested') ORDER BY id DESC LIMIT 1",
    )
    .fetch_optional(db)
    .await
    .ok()
    .flatten()
    .map(|(id, started_at, st)| BatchRunningTask {
        id,
        started_at,
        cancel_requested: st == "cancel_requested",
    });

    let open = window_open();
    BatchStatus {
        running,
        window_open: open,
        next_window_at: (!open).then(next_window_at),
        pending,
        done_today,
        failed_today,
        last_provider,
        cli_entry: find_cli_entry().map(|p| p.display().to_string()),
        running_task,
    }
}

    /// 启动调度协程（幂等）。只在窗口内（或手动唤醒）取任务，串行执行。
    pub fn start_loop(db: SqlitePool, rt: Arc<BatchRuntime>) {
    if rt.running.swap(true, Ordering::SeqCst) {
        return;
    }
    tauri::async_runtime::spawn(async move {
        // 孤儿回收（审计 P2-4，2026-10-24）：调度严格串行，进入循环前不可能
        // 存在本进程的 running——任何 running 行都是上次进程强退的残骸。
        // cancel 只认 pending、requeue 只认 failed/canceled、clear 不含 running，
        // 不回收则该行永远卡在「运行中」且三条恢复路都够不着它
        match sqlx::query(
            "UPDATE batch_tasks SET status='failed', error=?1, finished_at=?2 WHERE status IN ('running','cancel_requested')",
        )
        .bind("应用中断，任务未完成")
        .bind(now())
        .execute(&db)
        .await
        {
            Ok(r) if r.rows_affected() > 0 => {
                crate::fs_utils::app_log_default(&format!(
                    "[zcode/batch] 孤儿回收: {} 个中断任务标记为 failed（可重跑）",
                    r.rows_affected()
                ));
            }
            Ok(_) => {}
            Err(e) => {
                crate::fs_utils::app_log_default(&format!(
                    "[zcode/batch] 孤儿回收失败: {e}"
                ));
            }
        }
        loop {
            if !rt.running.load(Ordering::SeqCst) {
                break;
            }
            let woken = rt.wake.swap(false, Ordering::SeqCst);
            // force 只在"被唤醒"这一次有效，避免残留标志让窗口外一直跑
            let forced = woken && rt.force.swap(false, Ordering::SeqCst);
            // 窗口外且非手动唤醒 → 只睡不取任务
            if woken || window_open() {
                // 队列空时不必反复起进程
                let pending: i64 =
                    sqlx::query_scalar("SELECT COUNT(*) FROM batch_tasks WHERE status='pending'")
                        .fetch_one(&db)
                        .await
                        .unwrap_or(0);
                if pending > 0 {
                    if let Err(e) = run_one(&db, forced).await {
                        crate::fs_utils::app_log_default(&format!("[zcode/batch] 执行失败: {e}"));
                    }
                    // 刚跑完一个，立刻看还有没有（窗口内连续消费）
                    continue;
                }
            }
            tokio::time::sleep(TICK).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_field_extracts_provider() {
        let line = r#"{"ctx":{"providerId":"account:zai-start-plan","modelId":"GLM-5.3"}}"#;
        assert_eq!(json_str_field(line, "providerId").as_deref(), Some("account:zai-start-plan"));
        assert_eq!(json_str_field(line, "modelId").as_deref(), Some("GLM-5.3"));
        assert_eq!(json_str_field(line, "nope"), None);
    }

    #[test]
    fn next_window_is_within_a_day() {
        let t = next_window_at();
        let now = now();
        assert!(t > now, "下次窗口必须在未来");
        assert!(t - now <= 24 * 3600 + 60, "且不超过一天");
    }

    /// 真实跑一次 CLI，验证「执行 → 解析 → 归因」整条链路。
    /// 默认 ignore（要 1–2 分钟且会消耗上游额度），手动验证时：
    /// `cargo test e2e_real_cli_once -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn e2e_real_cli_once() {
        let entry = find_cli_entry().expect("本机需安装 ZCode（或设 ZCODE_CLI_ENTRY）");
        let ws = std::env::temp_dir().join("zcode-batch-e2e");
        let out = run_task_blocking(&entry, &ws, "Reply with exactly one word: ok", "GLM-5.3")
            .expect("CLI 执行失败");
        println!("--- e2e 结果 ---");
        println!("response     = {}", head(&out.response, 120));
        println!("input_tokens = {}", out.input_tokens);
        println!("output_tokens= {}", out.output_tokens);
        println!("provider     = {:?}", out.provider);
        println!("model        = {:?}", out.model);
        assert!(!out.response.is_empty(), "必须拿到 response");
        assert!(out.provider.is_some(), "归因必须能读到 providerId（否则无法判断烧了谁的额度）");
    }

    #[test]
    fn window_matches_plan_profile_constants() {
        // 两处口径必须一致，避免"调度窗口"和"套餐画像窗口"漂移
        assert_eq!(
            window_open(),
            crate::zcode::plan_profile::in_night_window(crate::zcode::plan_profile::local_hour())
        );
    }
}
