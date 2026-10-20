//! WorkBuddy 账号捕获/切换的业务逻辑层。
//!
//! 关键事实(全部来自对 WorkBuddy 5.8.2 app.asar 的逆向与本机实测):
//! 1. 登录会话文件:`%LOCALAPPDATA%\CodeBuddyExtension\Data\Public\auth\workbuddy-desktop.info`
//!    (authentication.id 默认 "workbuddy-desktop",国内/海外共用同一文件名);
//! 2. 会话内 token 为 `$wbEncrypted` 信封密文,protectorKeyId 全机恒定(static-v1),
//!    实测全新数据目录实例可成功恢复并刷新同一会话 → 整文件替换即可完成登录态迁移;
//! 3. `auth.domain` 明文:www.workbuddy.cn → 国内环境;workbuddy.ai/codebuddy.ai → 海外。
//!    官方启动时按 domain 决定 endpoint(resolveWorkbuddyProductEnvironmentFromSessionFile)。
//!
//! 注意:WorkBuddy 单实例锁基于其 userData 目录,且会话文件被 file watcher 监听、
//! 有 `.logged-out` 标记与自动备份机制 —— 切换必须先完全退出 WorkBuddy 再写文件。

use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

use crate::zcode::error::{AppError, AppResult};

#[cfg(windows)]
fn no_window_flags() -> u32 {
    0x0800_0000 // CREATE_NO_WINDOW
}

/// 会话文件的官方存储目录(Windows)
fn shared_auth_dir() -> Option<std::path::PathBuf> {
    #[cfg(windows)]
    {
        std::env::var_os("LOCALAPPDATA").map(|d| {
            std::path::PathBuf::from(d)
                .join("CodeBuddyExtension")
                .join("Data")
                .join("Public")
                .join("auth")
        })
    }
    #[cfg(not(windows))]
    {
        None
    }
}

/// 当前会话文件路径(官方 authentication.id = "workbuddy-desktop")
fn auth_file_path() -> AppResult<std::path::PathBuf> {
    shared_auth_dir()
        .map(|d| d.join("workbuddy-desktop.info"))
        .ok_or_else(|| AppError::Other("无法定位 WorkBuddy 会话目录(非 Windows 暂不支持)".into()))
}

/// 从会话文件明文字段判定的环境:overseas(海外) / domestic(国内)
fn region_of_domain(domain: &str) -> &'static str {
    let d = domain.to_ascii_lowercase();
    if d.contains("workbuddy.ai") || d.contains("codebuddy.ai") {
        "overseas"
    } else {
        "domestic"
    }
}

/// 从会话文件解析出的可展示元数据(只取明文字段,不碰密文)
#[derive(Debug, Clone, Serialize)]
pub struct WbSessionMeta {
    pub uid: String,
    pub uin: Option<String>,
    pub domain: Option<String>,
    pub region: &'static str,
}

/// 解析 .info 文件的明文标识字段(密文整体保留,不在此处理)
fn parse_session_meta(raw: &str) -> AppResult<WbSessionMeta> {
    #[derive(Deserialize)]
    struct AuthBlock {
        #[serde(default)]
        domain: Option<String>,
    }
    #[derive(Deserialize)]
    struct AccountBlock {
        #[serde(default)]
        uid: Option<String>,
        #[serde(default)]
        uin: Option<String>,
    }
    #[derive(Deserialize)]
    struct Info {
        #[serde(default)]
        account: Option<AccountBlock>,
        #[serde(default)]
        auth: Option<AuthBlock>,
    }
    let v: Info = serde_json::from_str(raw)
        .map_err(|e| AppError::Other(format!("会话文件格式无法解析(可能已损坏): {e}")))?;
    let account = v.account.as_ref();
    let uid = account
        .and_then(|a| a.uid.as_deref())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| AppError::Other("会话文件缺少账号 uid,无法捕获".into()))?
        .to_string();
    let domain = v
        .auth
        .as_ref()
        .and_then(|a| a.domain.as_deref())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    Ok(WbSessionMeta {
        region: region_of_domain(&domain.clone().unwrap_or_default()),
        uid,
        uin: account
            .and_then(|a| a.uin.as_deref())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string()),
        domain,
    })
}

/// 供前端展示的账号条目
#[derive(Debug, Clone, Serialize)]
pub struct WbAccount {
    pub id: i64,
    pub uid: String,
    pub label: Option<String>,
    pub uin: Option<String>,
    pub region: String,
    pub domain: Option<String>,
    pub captured_at: i64,
    /// 是否为 WorkBuddy 当前登录中的账号(与会话文件 uid+region 比对)
    pub is_current: bool,
}

/// 当前状态:会话文件存在时的元数据 + 客户端运行状态
#[derive(Debug, Serialize)]
pub struct WbStatus {
    pub logged_in: bool,
    #[serde(flatten)]
    pub meta: Option<WbSessionMeta>,
    pub running: bool,
    pub exe_found: Option<String>,
}

/// 定位 WorkBuddy 客户端 exe:
/// ① app_kv 中的自定义路径;② 运行中进程;③ 注册表卸载表;④ 常见安装目录
async fn locate_exe(db: &SqlitePool) -> Option<std::path::PathBuf> {
    if let Ok(Some((custom,))) = sqlx::query_as::<_, (String,)>(
        "SELECT value FROM app_kv WHERE key = 'wb_exe_path'",
    )
    .fetch_optional(db)
    .await
    {
        let p = std::path::PathBuf::from(&custom);
        if p.is_file() {
            return Some(p);
        }
    }
    if let Some(p) = locate_exe_from_process() {
        return Some(p);
    }
    if let Some(p) = locate_exe_from_registry() {
        return Some(p);
    }
    candidate_exe_paths().into_iter().find(|p| p.is_file())
}

fn candidate_exe_paths() -> Vec<std::path::PathBuf> {
    let mut v = Vec::new();
    // 本机实际安装位置(实测)与常见安装盘。
    // 海外版优先(用户实测路径 D:\WorkBuddyAI\WorkBuddyAI.exe——10-15 自更新把
    // D:\Work\WorkBuddyAI 覆盖成国内版后，海外安装包独立落此目录，自带海外
    // asar 与 WorkBuddyAI.exe 入口)；国内版路径在后兜底。
    for base in ["D", "C", "E"] {
        v.push(std::path::PathBuf::from(format!(
            "{base}:\\WorkBuddyAI\\WorkBuddyAI.exe"
        )));
    }
    for base in ["D", "C", "E"] {
        v.push(std::path::PathBuf::from(format!(
            "{base}:\\Work\\WorkBuddyAI\\WorkBuddy.exe"
        )));
        v.push(std::path::PathBuf::from(format!(
            "{base}:\\Work\\WorkBuddyAI\\WorkBuddyAI.exe"
        )));
    }
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        let local = std::path::PathBuf::from(local);
        v.push(local.join("Programs").join("WorkBuddy").join("WorkBuddy.exe"));
        v.push(local.join("WorkBuddy").join("WorkBuddy.exe"));
    }
    v
}

/// ① 运行中进程的真实路径(与 accounts::locate_zcode_exe 同思路)
fn locate_exe_from_process() -> Option<std::path::PathBuf> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // 海外入口进程名 WorkBuddyAI 与国内入口 WorkBuddy 都查(两者可在
        // 不同安装目录共存；海外版为独立安装包、进程名不同)
        let script = "(Get-Process WorkBuddy,WorkBuddyAI -ErrorAction SilentlyContinue | \
              Where-Object { $_.Path } | Select-Object -First 1).Path";
        if let Ok(out) = std::process::Command::new("powershell")
            .args(["-NoProfile", "-Command", script])
            .creation_flags(no_window_flags())
            .output()
        {
            let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if out.status.success() && !s.is_empty() {
                let p = std::path::PathBuf::from(&s);
                if p.is_file() {
                    return Some(p);
                }
            }
        }
    }
    None
}

/// ② 注册表卸载表:DisplayName 含 WorkBuddy 的项,取 InstallLocation 或 DisplayIcon
fn locate_exe_from_registry() -> Option<std::path::PathBuf> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let script = "\
$k = 'HKCU:\\Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\*',\
'HKLM:\\Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\*',\
'HKLM:\\Software\\WOW6432Node\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\*';\
$e = Get-ItemProperty $k -ErrorAction SilentlyContinue | \
Where-Object { $_.DisplayName -like '*WorkBuddy*' } | Select-Object -First 1; \
if ($e -and $e.InstallLocation) { Join-Path $e.InstallLocation 'WorkBuddy.exe' } \
elseif ($e -and $e.DisplayIcon) { ($e.DisplayIcon -split ',')[0] }";
        if let Ok(out) = std::process::Command::new("powershell")
            .args(["-NoProfile", "-Command", script])
            .creation_flags(no_window_flags())
            .output()
        {
            let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !s.is_empty() {
                let p = std::path::PathBuf::from(&s);
                if p.is_file() {
                    return Some(p);
                }
            }
        }
    }
    None
}

/// 捕获当前登录:读会话文件 → 按 uid+region 覆盖入库
pub async fn capture_current(db: &SqlitePool) -> AppResult<WbAccount> {
    let path = auth_file_path()?;
    let raw = std::fs::read_to_string(&path)
        .map_err(|e| AppError::Other(format!("读取会话文件失败({}): {e}", path.display())))?;
    let meta = parse_session_meta(&raw)?;
    let now = chrono::Utc::now().timestamp();
    sqlx::query(
        "INSERT INTO wb_accounts (uid, label, uin, region, domain, session_blob, captured_at, updated_at)
         VALUES (?1, NULL, ?2, ?3, ?4, ?5, ?6, ?6)
         ON CONFLICT(uid, region) DO UPDATE SET
           uin = excluded.uin,
           domain = excluded.domain,
           session_blob = excluded.session_blob,
           captured_at = excluded.captured_at,
           updated_at = excluded.updated_at",
    )
    .bind(&meta.uid)
    .bind(&meta.uin)
    .bind(meta.region)
    .bind(&meta.domain)
    .bind(raw.as_bytes())
    .bind(now)
    .execute(db)
    .await
    .map_err(|e| AppError::Other(format!("保存账号失败: {e}")))?;

    let id: (i64,) = sqlx::query_as("SELECT id FROM wb_accounts WHERE uid = ?1 AND region = ?2")
        .bind(&meta.uid)
        .bind(meta.region)
        .fetch_one(db)
        .await
        .map_err(|e| AppError::Other(format!("回读账号失败: {e}")))?;
    Ok(WbAccount {
        id: id.0,
        is_current: true,
        uid: meta.uid,
        label: None,
        uin: meta.uin,
        region: meta.region.to_string(),
        domain: meta.domain,
        captured_at: now,
    })
}

/// 列出全部已捕获账号,并标记当前登录中的那个
pub async fn list_accounts(db: &SqlitePool) -> AppResult<Vec<WbAccount>> {
    let current = current_session_meta();
    let rows: Vec<(i64, String, Option<String>, Option<String>, String, Option<String>, i64)> =
        sqlx::query_as(
            "SELECT id, uid, label, uin, region, domain, captured_at
             FROM wb_accounts ORDER BY captured_at DESC",
        )
        .fetch_all(db)
        .await
        .map_err(|e| AppError::Other(format!("读取账号列表失败: {e}")))?;
    Ok(rows
        .into_iter()
        .map(|(id, uid, label, uin, region, domain, captured_at)| WbAccount {
            is_current: current
                .as_ref()
                .map(|c| c.uid == uid && c.region == region)
                .unwrap_or(false),
            id,
            uid,
            label,
            uin,
            region,
            domain,
            captured_at,
        })
        .collect())
}

/// 删除已捕获账号
pub async fn remove_account(db: &SqlitePool, id: i64) -> AppResult<()> {
    sqlx::query("DELETE FROM wb_accounts WHERE id = ?1")
        .bind(id)
        .execute(db)
        .await
        .map_err(|e| AppError::Other(format!("删除账号失败: {e}")))?;
    Ok(())
}

/// 当前会话文件元数据(文件不存在/损坏 → None)
fn current_session_meta() -> Option<WbSessionMeta> {
    let path = auth_file_path().ok()?;
    let raw = std::fs::read_to_string(path).ok()?;
    parse_session_meta(&raw).ok()
}

/// 状态查询:当前登录 + 客户端运行状态
pub async fn status(db: &SqlitePool) -> AppResult<WbStatus> {
    let meta = current_session_meta();
    let running = is_workbuddy_running();
    let exe_found = locate_exe(db)
        .await
        .map(|p| p.to_string_lossy().to_string());
    Ok(WbStatus {
        logged_in: meta.is_some(),
        running,
        exe_found,
        meta,
    })
}

fn is_workbuddy_running() -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let script = "(Get-Process WorkBuddy -ErrorAction SilentlyContinue | \
              Measure-Object).Count";
        if let Ok(out) = std::process::Command::new("powershell")
            .args(["-NoProfile", "-Command", script])
            .creation_flags(no_window_flags())
            .output()
        {
            let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
            return s.parse::<i64>().map(|n| n > 0).unwrap_or(false);
        }
    }
    false
}

/// 切换到指定账号:退出 WorkBuddy → 备份并替换会话文件 → 重新启动
pub async fn switch_account(db: &SqlitePool, id: i64, app_data_dir: &std::path::Path) -> AppResult<()> {
    let row: Option<(String, String, Vec<u8>)> =
        sqlx::query_as("SELECT uid, region, session_blob FROM wb_accounts WHERE id = ?1")
            .bind(id)
            .fetch_optional(db)
            .await
            .map_err(|e| AppError::Other(format!("查询账号失败: {e}")))?;
    let (_, region, blob) = row.ok_or_else(|| AppError::Other("账号不存在或已被删除".into()))?;

    let exe = locate_exe(db).await.ok_or_else(|| {
        AppError::Other("未找到 WorkBuddy 客户端:可在设置里指定 WorkBuddy.exe 的完整路径".into())
    })?;

    // 1. 完全退出 WorkBuddy(watcher/沙箱进程会在退出时收尾,必须等干净)
    kill_workbuddy();

    // 2. 备份当前会话 → 写入目标会话 → 清除登出标记
    let path = auth_file_path()?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| AppError::Other(format!("创建会话目录失败: {e}")))?;
    }
    if path.exists() {
        let stamp = chrono::Utc::now().format("%Y-%m-%dT%H-%M-%S");
        let backup = path.with_file_name(format!("workbuddy-desktop.pre-switch-{stamp}.info"));
        let _ = std::fs::copy(&path, &backup); // 备份失败不阻断(官方自身也会留份)
    }
    std::fs::write(&path, &blob)
        .map_err(|e| AppError::Other(format!("写入会话文件失败: {e}")))?;
    let logged_out = path.with_file_name("workbuddy-desktop.info.logged-out");
    if logged_out.exists() {
        let _ = std::fs::remove_file(&logged_out);
    }

    // 3. 重新启动:海外会话必须注入海外 ACC 配置(否则客户端以默认国内产品域
    // 配置启动,海外会话(domain=workbuddy.ai/codebuddy.ai)登录态判环境不符
    // 连不上服务器——用户实测「切换海外的登录不上去」根因之一);
    // 国内会话不注入(客户端默认配置即国内)
    let acc_config = if region == "overseas" {
        Some(ensure_oversea_acc_config(app_data_dir)?)
    } else {
        None
    };
    launch_client(&exe, acc_config.as_deref())
}

fn kill_workbuddy() {
    // 国内入口 WorkBuddy.exe 与海外入口 WorkBuddyAI.exe 都杀
    // (两版独立安装共存，切换/海外登录前必须都退出，单实例锁会把新启动
    // 请求转给残留实例)
    for im in ["WorkBuddy.exe", "WorkBuddyAI.exe"] {
        let mut kill = std::process::Command::new("taskkill");
        kill.args(["/IM", im, "/T", "/F"]);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            kill.creation_flags(no_window_flags());
        }
        let _ = kill.output();
    }
    // 官方退出时要收尾会话文件/watcher,给足时间
    std::thread::sleep(std::time::Duration::from_secs(3));
}

/// 记录/更新 WorkBuddy.exe 路径(设置里手动指定)
pub async fn set_exe_path(db: &SqlitePool, path: &str) -> AppResult<()> {
    let p = std::path::PathBuf::from(path);
    if !p.is_file() {
        return Err(AppError::Other(format!("路径不存在或不是文件: {path}")));
    }
    sqlx::query(
        "INSERT INTO app_kv (key, value, updated_at) VALUES ('wb_exe_path', ?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = ?1, updated_at = ?2",
    )
    .bind(path)
    .bind(chrono::Utc::now().timestamp())
    .execute(db)
    .await
    .map_err(|e| AppError::Other(format!("保存路径失败: {e}")))?;
    Ok(())
}

/// 海外 ACC 产品配置文件路径（app_data_dir/wb-acc-oversea.json）。
/// 已存在则直接复用（用户机器上可能为完整官方产品配置，不得用最小版覆盖）；
/// 缺失才写最小可用版（endpoint → workbuddy.ai）。
fn ensure_oversea_acc_config(app_data_dir: &std::path::Path) -> AppResult<std::path::PathBuf> {
    let acc_path = app_data_dir.join("wb-acc-oversea.json");
    if acc_path.is_file() {
        return Ok(acc_path);
    }
    let acc_json = r#"{
  "endpoint": "https://www.workbuddy.ai",
  "stagingEndpoint": "https://staging.workbuddy.ai",
  "isOversea": true,
  "networkEnvironment": "external",
  "authentication": {
    "id": "workbuddy-desktop",
    "endpoint": "https://www.workbuddy.ai",
    "attributes": {
      "externalDomain": "*.workbuddy.ai",
      "internalDomain": "www.workbuddy.cn"
    }
  }
}"#;
    std::fs::write(&acc_path, acc_json)
        .map_err(|e| AppError::Other(format!("写入海外配置失败: {e}")))?;
    Ok(acc_path)
}

/// 以毒变量防护启动客户端；`acc_config` 非 None 时注入海外产品配置
/// （ACC_PRODUCT_CONFIG_PATH，登录/接口端点指向 workbuddy.ai）。
fn launch_client(exe: &std::path::Path, acc_config: Option<&std::path::Path>) -> AppResult<()> {
    let mut cmd = std::process::Command::new(exe);
    cmd.env_remove("ELECTRON_RUN_AS_NODE")
        .env_remove("NODE_OPTIONS");
    if let Some(acc) = acc_config {
        cmd.env("ACC_PRODUCT_CONFIG_PATH", acc);
    }
    let poisoned: Vec<String> = std::env::vars()
        .map(|(k, _)| k)
        .filter(|k| k.starts_with("ELECTRON_"))
        .collect();
    for k in poisoned {
        cmd.env_remove(&k);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0000_0008); // DETACHED_PROCESS
    }
    cmd.spawn()
        .map_err(|e| AppError::Other(format!("启动 WorkBuddy 失败({}): {e}", exe.display())))?;
    Ok(())
}

/// 海外登录向导:确认当前账号已捕获 → 退出 WorkBuddy → 写官方登出标记
/// → 注入海外 ACC 产品配置(登录页指向 workbuddy.ai)→ 重新启动。
/// 登录成功后回到工具「捕获」即可,国内账号随时可用 wb_switch 切回。
pub async fn launch_oversea_login(db: &SqlitePool, app_data_dir: &std::path::Path) -> AppResult<()> {
    let exe = locate_exe(db).await.ok_or_else(|| {
        AppError::Other("未找到 WorkBuddy 客户端:可先在设置里指定 WorkBuddy.exe 的完整路径".into())
    })?;

    // 安全阀:当前登录的账号必须已捕获,否则登出后会话就丢了。
    // **只按 uid 匹配**:auth 文件 domain 随客户端产品域漂移(国内账号在海外版
    // 客户端登录 → domain=codebuddy.ai → 判 overseas),旧捕获条目 region=domestic
    // 会让 uid 命中而 region 不命中,误报「尚未捕获」(用户实测卡死点);
    // 命中后把 region 同步为当前判定(会话文件同一份,环境跟随 auth 文件)。
    if let Some(cur) = current_session_meta() {
        let existing: Option<(i64, String)> =
            sqlx::query_as("SELECT id, region FROM wb_accounts WHERE uid = ?1")
                .bind(&cur.uid)
                .fetch_optional(db)
                .await
                .map_err(|e| AppError::Other(format!("查询账号库失败: {e}")))?;
        match existing {
            Some((id, region)) => {
                if region != cur.region {
                    sqlx::query("UPDATE wb_accounts SET region = ?1 WHERE id = ?2")
                        .bind(cur.region)
                        .bind(id)
                        .execute(db)
                        .await
                        .map_err(|e| AppError::Other(format!("同步会话环境失败: {e}")))?;
                }
            }
            None => {
                return Err(AppError::Other(
                    "当前登录的账号尚未捕获,请先点「捕获当前登录」再进行海外登录(否则该账号会话会丢失)".into(),
                ));
            }
        }
    }

    // 退出客户端(登出标记必须在退出后写,避免 watcher 竞争)
    kill_workbuddy();

    // 写官方登出标记(hasLogoutMarker 只做存在性检查)
    let path = auth_file_path()?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| AppError::Other(format!("创建会话目录失败: {e}")))?;
    }
    let logged_out = path.with_file_name("workbuddy-desktop.info.logged-out");
    std::fs::write(&logged_out, "")
        .map_err(|e| AppError::Other(format!("写入登出标记失败: {e}")))?;

    // 注入海外 ACC 产品配置并重启（完整版已存在则复用，见 ensure_oversea_acc_config）
    let acc_path = ensure_oversea_acc_config(app_data_dir)?;
    launch_client(&exe, Some(&acc_path))
}

/// 重命名账号备注
pub async fn rename_account(db: &SqlitePool, id: i64, label: Option<&str>) -> AppResult<()> {
    sqlx::query("UPDATE wb_accounts SET label = ?1, updated_at = ?2 WHERE id = ?3")
        .bind(label)
        .bind(chrono::Utc::now().timestamp())
        .bind(id)
        .execute(db)
        .await
        .map_err(|e| AppError::Other(format!("重命名失败: {e}")))?;
    Ok(())
}
