//! mint 设备号（Rust 内置版，2026-10-19）：把 scripts/mint_trae_device.mjs 的核心流程
//! 移植进应用，配合「新增 Trae 账号自动配号」使用——池里没有空闲真实设备号时，
//! 应用自动走 prepare → 提示用户重启客户端 → collect → 绑定，全程无黑框无控制台。
//!
//! 与 node 脚本的差异：
//! - 进程内直接读写文件/注册表（cmd /c reg 均带 CREATE_NO_WINDOW，不弹窗）
//! - prepare/collect 状态存 data_dir/mint_state.json（跨应用重启续跑）
//! - 只支持 Trae CN（应用内自动化场景固定客户端）
//!
//! 三层持久层（缺一必回老号，2026-10-19 实测三轮踩坑）：
//! ① HKCU\...\DeveloperTools\deviceid ② 同键 HardwareCache（硬件指纹）
//! ③ %LOCALAPPDATA%\aha_doctor\<app>\event\applog.cache2（did/cdid 真源）

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

const HKCU_KEY: &str = r"HKCU\Software\Microsoft\DeveloperTools";
const HKCU_VALUE: &str = "deviceid";
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
/// 客户端会注册的 aha_doctor 应用目录（按序探测）
const DOCTOR_APPS: &[&str] = &["Trae CN", "Trae", "SOLO CN"];

#[derive(Serialize, Deserialize, Default, Clone)]
pub struct MintState {
    pub backup_dir: String,
    pub prepared_at: String,
    pub old_hkcu_deviceid: String,
    pub old_icube_did: String,
    pub consumed: bool,
    pub collected_did: Option<String>,
    /// prepare 前先关闭客户端；collect 阶段（用户重启过客户端后）再次还原并关一次
    pub phase: String, // "prepared" | "collected"
}

fn trae_dir() -> PathBuf {
    Path::new(&std::env::var_os("APPDATA").unwrap_or_default()).join("Trae CN")
}
fn aha_dir() -> PathBuf {
    trae_dir().join("aha")
}
fn global_storage() -> PathBuf {
    trae_dir().join("User").join("globalStorage")
}
fn doctor_event_dir() -> Option<PathBuf> {
    let local = std::env::var_os("LOCALAPPDATA")?;
    for app in DOCTOR_APPS {
        let p = Path::new(&local).join("aha_doctor").join(app).join("event");
        if p.exists() {
            return Some(p);
        }
    }
    // 目录尚未创建也按第一个候选返回（prepare 要删它，无害）
    Some(Path::new(&local).join("aha_doctor").join(DOCTOR_APPS[0]).join("event"))
}

fn reg_query_value(key: &str, value: &str) -> Option<String> {
    use std::os::windows::process::CommandExt;
    let out = std::process::Command::new("reg")
        .args(["query", key, "/v", value])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    text.lines()
        .find(|l| l.contains(value))
        .and_then(|l| l.split_whitespace().last().map(str::to_string))
}

fn reg_set(key: &str, value: &str, data: &str) {
    use std::os::windows::process::CommandExt;
    let _ = std::process::Command::new("reg")
        .args(["add", key, "/v", value, "/t", "REG_SZ", "/d", data, "/f"])
        .creation_flags(CREATE_NO_WINDOW)
        .output();
}

fn reg_delete(key: &str) {
    use std::os::windows::process::CommandExt;
    let _ = std::process::Command::new("reg")
        .args(["delete", key, "/f"])
        .creation_flags(CREATE_NO_WINDOW)
        .output();
}

fn reg_export(key: &str, file: &Path) -> bool {
    use std::os::windows::process::CommandExt;
    let file = file.to_string_lossy().replace('/', r"\");
    match std::process::Command::new("cmd")
        .args(["/c", "reg", "export", key, &file, "/y"])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
    {
        Ok(o) => o.status.success() && file_exists_len(Path::new(&file)) > 100,
        _ => false,
    }
}

fn reg_import(file: &Path) -> bool {
    use std::os::windows::process::CommandExt;
    std::process::Command::new("cmd")
        .args(["/c", "reg", "import", &file.to_string_lossy().replace('/', r"\")])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn file_exists_len(p: &Path) -> u64 {
    std::fs::metadata(p).map(|m| m.len()).unwrap_or(0)
}

/// 读 ICDRS 日志最新 did（客户端主日志，指纹标识）
fn read_icube_did() -> Option<String> {
    let logs = trae_dir().join("logs");
    let mut best: Option<(String, String)> = None;
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(&logs)
        .ok()?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.len() == 15 && n.starts_with("202"))
                .unwrap_or(false)
        })
        .collect();
    dirs.sort();
    for d in dirs.iter().rev().take(3) {
        let Ok(files) = std::fs::read_dir(d) else { continue };
        for f in files.filter_map(|e| e.ok()).map(|e| e.path()) {
            let Ok(text) = std::fs::read_to_string(&f) else { continue };
            for line in text.lines().rev() {
                // [ICDRS] (init) initialization done, did: 1853105110786875
                if let Some(pos) = line.find("initialization done, did: ") {
                    let did = line[pos + "initialization done, did: ".len()..]
                        .split(',')
                        .next()
                        .unwrap_or("")
                        .trim()
                        .to_string();
                    if !did.is_empty() {
                        let ts = line.chars().take(24).collect();
                        if best.as_ref().map_or(true, |(t, _)| ts > *t) {
                            best = Some((ts, did));
                        }
                        break;
                    }
                }
            }
        }
    }
    best.map(|(_, d)| d)
}

/// 读 aha_doctor event 缓存（applog.cache2）当前 did（二进制 int64 LE，唯一真源）
pub fn read_doctor_cached_did() -> Option<String> {
    let p = doctor_event_dir()?.join("applog.cache2");
    let buf = std::fs::read(&p).ok()?;
    let needle = b"device_id";
    let idx = buf.windows(needle.len()).position(|w| w == needle)?;
    let mut i = idx;
    while i + 8 <= buf.len() {
        let v = u64::from_le_bytes(buf[i..i + 8].try_into().ok()?);
        if (100_000_000_000_000..10_000_000_000_000_000).contains(&v) {
            return Some(v.to_string());
        }
        i += 1;
    }
    None
}

fn copy_if_exists(src: &Path, dst: &Path) -> bool {
    if src.is_file() {
        if let Some(parent) = dst.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        std::fs::copy(src, dst).is_ok()
    } else {
        false
    }
}

fn restore_file(bak: &Path, dst: &Path) -> bool {
    if bak.is_file() {
        if let Some(parent) = dst.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        std::fs::copy(bak, dst).is_ok()
    } else {
        false
    }
}

/// prepare：备份全部身份要素 → 换新指纹 → 清三层缓存。
/// 前置：调用方保证 Trae 客户端已关闭（switcher proc::graceful_kill_images）。
pub fn prepare(data_dir: &Path) -> Result<MintState, String> {
    let ts = chrono::Local::now().format("%Y-%m-%dT%H-%M-%S");
    let backup_root = data_dir.join("mint_backups");
    let dir = backup_root.join(format!("mint-{ts}"));
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;

    let old_hkcu = reg_query_value(HKCU_KEY, HKCU_VALUE)
        .ok_or_else(|| format!("注册表 {HKCU_KEY}\\{HKCU_VALUE} 不存在（客户端从未注册过？）"))?;

    let files: [(&str, PathBuf); 5] = [
        ("tiny_storage", aha_dir().join("TinyStorage")),
        ("remote_state", aha_dir().join("Remote_State")),
        ("state_vscdb", global_storage().join("state.vscdb")),
        ("machineid", trae_dir().join("machineid")),
        ("storage_json", global_storage().join("storage.json")),
    ];
    let mut backed: Vec<&str> = vec![];
    for (k, p) in &files {
        if copy_if_exists(p, &dir.join(k)) {
            backed.push(k);
        }
    }
    let hc_dir = format!("{HKCU_KEY}\\HardwareCache");
    let hc_backup = dir.join("hardware_cache.reg");
    let hc_backed = reg_export(&hc_dir, &hc_backup);
    if hc_backed {
        backed.push("hardware_cache");
    }

    // 换新指纹 + 清三层缓存
    let new_id = uuid_v4();
    reg_set(HKCU_KEY, HKCU_VALUE, &new_id);
    let _ = std::fs::remove_file(&files[0].1); // TinyStorage
    reg_delete(&hc_dir);
    if let Some(ev) = doctor_event_dir() {
        if ev.exists() {
            let ev_backup = dir.join("aha_doctor_event");
            let _ = copy_dir_recursive(&ev, &ev_backup);
            let _ = std::fs::remove_dir_all(&ev);
        }
    }

    let old_icube = read_icube_did().unwrap_or_default();
    let state = MintState {
        backup_dir: dir.to_string_lossy().to_string(),
        prepared_at: chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string(),
        old_hkcu_deviceid: old_hkcu,
        old_icube_did: old_icube,
        consumed: false,
        collected_did: None,
        phase: "prepared".into(),
    };
    let st_path = data_dir.join("mint_state.json");
    std::fs::write(&st_path, serde_json::to_string_pretty(&state).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    Ok(state)
}

/// collect：读重启后注册的新号 → 还原全部身份要素 → 返回新号（调用方负责绑定 device_map）。
pub fn collect(data_dir: &Path) -> Result<(String, MintState), String> {
    let st_path = data_dir.join("mint_state.json");
    let mut state: MintState = serde_json::from_str(
        &std::fs::read_to_string(&st_path).map_err(|e| format!("mint_state.json 读取失败: {e}"))?,
    )
    .map_err(|e| format!("mint_state.json 解析失败: {e}"))?;
    if state.consumed {
        return Err("该 mint 轮次已被消费，请重新 prepare".into());
    }
    let dir = PathBuf::from(&state.backup_dir);

    // 新号：aha_doctor 缓存为准（prepare 清空后新注册写入全新号）；icube 日志兜底
    let fresh = read_doctor_cached_did()
        .filter(|d| d != &state.old_icube_did && !d.is_empty())
        .or_else(|| read_icube_did().filter(|d| d != &state.old_icube_did && !d.is_empty()))
        .ok_or_else(|| "没有读到新设备号——客户端重启了吗？（启动后等 10 秒再试）".to_string())?;
    if fresh == state.old_icube_did {
        // 指纹未变：仍执行还原（无害），报错
        restore_all(&dir, &state);
        state.consumed = true;
        let _ = std::fs::write(&st_path, serde_json::to_string_pretty(&state).unwrap_or_default());
        return Err("指纹未变化（did 与旧号相同）——mint 未生效，已还原现场".into());
    }

    restore_all(&dir, &state);
    state.consumed = true;
    state.collected_did = Some(fresh.clone());
    state.phase = "collected".into();
    let _ = std::fs::write(&st_path, serde_json::to_string_pretty(&state).map_err(|e| e.to_string())?);
    Ok((fresh, state))
}

fn restore_all(dir: &Path, state: &MintState) {
    reg_set(HKCU_KEY, HKCU_VALUE, &state.old_hkcu_deviceid);
    let files: [(&str, PathBuf); 5] = [
        ("tiny_storage", aha_dir().join("TinyStorage")),
        ("remote_state", aha_dir().join("Remote_State")),
        ("state_vscdb", global_storage().join("state.vscdb")),
        ("machineid", trae_dir().join("machineid")),
        ("storage_json", global_storage().join("storage.json")),
    ];
    for (k, p) in &files {
        restore_file(&dir.join(k), p);
    }
    let hc_reg = dir.join("hardware_cache.reg");
    if hc_reg.exists() {
        reg_import(&hc_reg);
    }
    if let Some(ev) = doctor_event_dir() {
        let ev_backup = dir.join("aha_doctor_event");
        if ev_backup.exists() {
            let _ = std::fs::remove_dir_all(&ev);
            let _ = copy_dir_recursive(&ev_backup, &ev);
        }
    }
}

/// 当前是否有未消费的 prepare 轮次（UI 提示「请重启 Trae 客户端」用）
pub fn pending_state(data_dir: &Path) -> Option<MintState> {
    let st_path = data_dir.join("mint_state.json");
    let text = std::fs::read_to_string(&st_path).ok()?;
    let state: MintState = serde_json::from_str(&text).ok()?;
    (!state.consumed).then_some(state)
}

fn uuid_v4() -> String {
    // 无 uuid crate 依赖：随机 16 字节 → RFC4122 v4 格式（与脚本 crypto.randomUUID 等价形态）
    let mut b = [0u8; 16];
    let mut filled = false;
    if let Ok(mut f) = std::fs::File::open(r"\\.\\\Device\\KsecDD") {
        use std::io::Read;
        // 打开成功但读失败同样落兜底——否则 b 保持全零，两次 mint 生成同一 UUID
        filled = f.read_exact(&mut b).is_ok();
    }
    if !filled {
        // 兜底：时间熵（低 12 字节）+ PID（尾 4 字节）。原实现把 u128 的
        // 16 字节 nanos 铺满整个数组后又往 b[16..] 写 PID——必然越界 panic，
        // 签到任务线程整体夭折（2026-10-20 09:19 黑匣子实录 len=16 index=16）
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        let nanos = now.as_nanos().to_le_bytes();
        let pid = std::process::id().to_le_bytes();
        b[..12].copy_from_slice(&nanos[..12]);
        b[12..].copy_from_slice(&pid);
    }
    b[6] = (b[6] & 0x0F) | 0x40;
    b[8] = (b[8] & 0x3F) | 0x80;
    let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for e in std::fs::read_dir(src)? {
        let e = e?;
        let from = e.path();
        let to = dst.join(e.file_name());
        if from.is_dir() {
            copy_dir_recursive(&from, &to)?;
        } else {
            std::fs::copy(&from, &to)?;
        }
    }
    Ok(())
}
