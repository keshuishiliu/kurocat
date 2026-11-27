//! 数据备份：WebDAV 快照上传 + 按时间点恢复。
//!
//! # 与同步（sync.rs）的关系
//!
//! 备份复用同步的 WebDAV 账号与 `Dav` 客户端，但目录独立（`KuroCatBackup/`），
//! 数据互不干扰。**备份只上传**：worker 只做「快照 → 上传 → 清理过期」，
//! 不下载不合并；「恢复到某个时间点」是唯一读取入口，必须由用户在统计窗口
//! 手动触发，且受录入密码门禁（见 lib.rs 的 backup_* 命令）。
//!
//! # 快照内容与跨机密钥
//!
//! 每个时间点快照一个目录，内含 events.db / meta.db / trail.db（SQLCipher
//! 加密原样上传）+ `key.enc`（本机库密钥，用 PBKDF2(WebDAV密码)+AES-GCM 加密，
//! 复用同步包的加密函数）+ `manifest.json`。
//!
//! key.enc 存在的原因：每台机器的库密钥不同（各自 DPAPI 保管），跨机恢复时
//! 必须拿到快照的密钥才能打开库。加密信封与同步包同一信任根——没有 WebDAV
//! 密码的人既解不开库也解不开 key。
//!
//! # 一致性与容错
//!
//! - 快照用 `VACUUM INTO` 在线导出（一致性读，不阻塞录制写入）。
//! - manifest 最后上传，作为「快照完整」的提交标记；列快照时没有 manifest
//!   的目录视为上传中断的残骸，跳过展示、按保留期自然过期清理。
//! - 恢复流程：下载 → 用快照密钥逐一验证 →（跨机时改密到本机密钥）→
//!   recorder 线程内换库（现有数据挪进防呆目录，不删除）。

use crate::config::{BackupSettings, SyncSettings};
use crate::sync::Dav;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

const BACKUP_ROOT: &str = "KuroCatBackup";
/// worker 空转轮询间隔（秒）：未启用备份时的睡眠粒度。
const IDLE_POLL: u64 = 60;
/// 手动操作（立即备份/列表/恢复）的网络等待上限。
const MANUAL_WAIT: Duration = Duration::from_secs(300);

// ---------------------------------------------------------------------------
// 对外类型
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BackupStatus {
    pub enabled: bool,
    pub interval_minutes: u32,
    pub retention_days: u32,
    /// 上次备份完成时刻（ms），0 = 从未备份。
    pub last_backup_ts: i64,
    pub last_ok: bool,
    pub last_message: String,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotMeta {
    /// 快照时间点（ms）。
    pub ts: i64,
    /// 云端目录名（13 位时间戳）。
    pub name: String,
    /// 包含的库文件。
    pub files: Vec<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RestoreReport {
    /// 实际恢复的库文件。
    pub restored: Vec<String>,
    /// 需要用户知道的事项（防呆目录位置、跨机改密等）。
    pub note: String,
}

// ---------------------------------------------------------------------------
// 内部状态
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct BackupCfg {
    enabled: bool,
    interval_minutes: u32,
    retention_days: u32,
}

impl BackupCfg {
    fn from_settings(s: &BackupSettings) -> Self {
        Self {
            enabled: s.enabled,
            interval_minutes: s.interval_minutes,
            retention_days: s.retention_days,
        }
    }
}

enum Msg {
    /// 立即备份一次，回传本次快照信息。
    Run {
        reply: Sender<Result<SnapshotMeta, String>>,
    },
    SettingsChanged,
}

static TX: OnceLock<Sender<Msg>> = OnceLock::new();
static STARTED: AtomicBool = AtomicBool::new(false);
static CFG: OnceLock<Mutex<BackupCfg>> = OnceLock::new();
static SETTINGS: OnceLock<Arc<Mutex<crate::config::AppSettings>>> = OnceLock::new();

static LAST_TS: AtomicI64 = AtomicI64::new(0);
static LAST_OK: AtomicBool = AtomicBool::new(false);
static LAST_MSG: OnceLock<Mutex<String>> = OnceLock::new();

fn current_cfg() -> BackupCfg {
    CFG.get()
        .map(|m| m.lock().unwrap_or_else(|e| e.into_inner()).clone())
        .unwrap_or(BackupCfg {
            enabled: false,
            interval_minutes: 30,
            retention_days: 15,
        })
}

fn settings_arc() -> Option<Arc<Mutex<crate::config::AppSettings>>> {
    SETTINGS.get().cloned()
}

fn set_last(ok: bool, msg: String) {
    LAST_TS.store(now_ms(), Ordering::SeqCst);
    LAST_OK.store(ok, Ordering::SeqCst);
    if let Some(m) = LAST_MSG.get() {
        *m.lock().unwrap_or_else(|e| e.into_inner()) = msg;
    }
}

// ---------------------------------------------------------------------------
// 对外入口
// ---------------------------------------------------------------------------

/// 启动备份 worker。整个进程生命周期只起一次。
pub fn init(settings: Arc<Mutex<crate::config::AppSettings>>) {
    let _ = SETTINGS.set(settings.clone());
    let cfg = {
        let s = settings.lock().unwrap_or_else(|e| e.into_inner());
        BackupCfg::from_settings(&s.backup)
    };
    let _ = CFG.set(Mutex::new(cfg));
    let _ = LAST_MSG.set(Mutex::new("尚未备份".to_string()));
    start_worker();
}

fn start_worker() {
    if STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    let (tx, rx) = mpsc::channel::<Msg>();
    let _ = TX.set(tx);
    let spawned = std::thread::Builder::new()
        .name("kuro-backup".to_string())
        .spawn(move || worker(rx));
    if spawned.is_err() {
        STARTED.store(false, Ordering::SeqCst);
        eprintln!("[backup] 无法创建备份线程");
    }
}

fn worker(rx: Receiver<Msg>) {
    // 启动备份：开关打开才做。失败无所谓，定时周期会再试。
    {
        let cfg = current_cfg();
        if cfg.enabled {
            if let Some(settings) = settings_arc() {
                let _ = run_backup_once(&settings);
            }
        }
    }
    loop {
        let cfg = current_cfg();
        let auto = cfg.enabled && cfg.interval_minutes > 0;
        let wait = if auto {
            Duration::from_secs(cfg.interval_minutes as u64 * 60)
        } else {
            Duration::from_secs(IDLE_POLL)
        };
        match rx.recv_timeout(wait) {
            Ok(Msg::Run { reply }) => {
                let result = match settings_arc() {
                    Some(settings) => run_backup_once(&settings),
                    None => Err("备份未初始化".to_string()),
                };
                let _ = reply.send(result);
            }
            Ok(Msg::SettingsChanged) => continue,
            Err(RecvTimeoutError::Timeout) => {
                if auto {
                    if let Some(settings) = settings_arc() {
                        let _ = run_backup_once(&settings);
                    }
                }
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
}

/// 备份设置变化后调用：刷新 worker 的配置视图；新开启时立刻备份一次。
pub fn apply_settings(settings: &Arc<Mutex<crate::config::AppSettings>>) {
    let cfg = {
        let s = settings.lock().unwrap_or_else(|e| e.into_inner());
        BackupCfg::from_settings(&s.backup)
    };
    if let Some(m) = CFG.get() {
        *m.lock().unwrap_or_else(|e| e.into_inner()) = cfg.clone();
    }
    if cfg.enabled {
        if let Some(tx) = TX.get() {
            let (rtx, _) = mpsc::channel();
            let _ = tx.send(Msg::Run { reply: rtx });
        }
    } else if let Some(tx) = TX.get() {
        let _ = tx.send(Msg::SettingsChanged);
    }
}

pub fn status() -> BackupStatus {
    let cfg = current_cfg();
    BackupStatus {
        enabled: cfg.enabled,
        interval_minutes: cfg.interval_minutes,
        retention_days: cfg.retention_days,
        last_backup_ts: LAST_TS.load(Ordering::SeqCst),
        last_ok: LAST_OK.load(Ordering::SeqCst),
        last_message: LAST_MSG
            .get()
            .map(|m| m.lock().unwrap_or_else(|e| e.into_inner()).clone())
            .unwrap_or_else(|| "尚未备份".to_string()),
    }
}

/// 「立即备份」。走 worker 串行执行，等结果，返回本次快照信息。
pub fn backup_now() -> Result<SnapshotMeta, String> {
    let cfg = current_cfg();
    if !cfg.enabled {
        // 手动备份不允许在未开启自动备份时使用：入口统一，避免
        // 「以为开了备份其实没开」的误会。前端只在开关打开后显示按钮。
        return Err("请先开启数据备份".to_string());
    }
    if STARTED.load(Ordering::SeqCst) {
        if let Some(tx) = TX.get() {
            let (rtx, rrx) = mpsc::channel();
            if tx.send(Msg::Run { reply: rtx }).is_ok() {
                return match rrx.recv_timeout(MANUAL_WAIT + Duration::from_secs(30)) {
                    Ok(result) => result,
                    Err(_) => Err("备份超时，请稍后查看状态".to_string()),
                };
            }
        }
    }
    // worker 未启动时兜底：当前线程直接跑
    let settings = settings_arc().ok_or("备份未初始化")?;
    run_backup_once(&settings)
}

// ---------------------------------------------------------------------------
// 快照上传
// ---------------------------------------------------------------------------

/// 一轮备份：快照 → 上传 → 清理过期。
fn run_backup_once(
    settings: &Arc<Mutex<crate::config::AppSettings>>,
) -> Result<SnapshotMeta, String> {
    let result = run_backup_inner(settings);
    match &result {
        Ok(snap) => set_last(true, format!("备份成功（{}）", format_ts(snap.ts))),
        Err(e) => set_last(false, format!("备份失败：{e}")),
    }
    result
}

fn run_backup_inner(
    settings: &Arc<Mutex<crate::config::AppSettings>>,
) -> Result<SnapshotMeta, String> {
    // 备份依赖同步的 WebDAV 配置
    let (sync_cfg, retention_days) = {
        let s = settings.lock().unwrap_or_else(|e| e.into_inner());
        (s.sync.clone(), s.backup.retention_days)
    };
    if !sync_cfg.configured() {
        return Err("备份复用数据同步的 WebDAV 配置，请先在「数据同步」里填好服务器、账号和应用密码".to_string());
    }
    let dav = Dav::new(&sync_cfg)?;
    let password = sync_cfg.password.clone();
    let Some(machine_id) = crate::sync::current_machine_id()? else {
        return Err("暂无输入数据可备份".to_string());
    };
    let ts = now_ms();
    let snap_name = ts.to_string();
    let local_key = crate::crypto::db_key_hex()?;

    // 1. 就地快照：VACUUM INTO 生成一致性副本（含源库密钥）
    let tmp = temp_dir("snapshot", ts);
    std::fs::create_dir_all(&tmp).map_err(|e| format!("创建临时目录失败: {e}"))?;
    let mut files = Vec::new();
    for name in ["events.db", "meta.db", "trail.db"] {
        let src = crate::recorder::log_dir().join(name);
        if !src.exists() {
            continue; // trail 可能从没开过；events/meta 由 machine_id 检查兜底
        }
        let dst = tmp.join(name);
        vacuum_into(&src, &dst, name)?;
        // 快照副本必须能用本机密钥打开，坏快照绝不上传
        crate::crypto::open_with_explicit_key(&dst, &local_key, name).map(|_| ())?;
        files.push(name.to_string());
    }
    if !files.contains(&"events.db".to_string()) {
        let _ = std::fs::remove_dir_all(&tmp);
        return Err("暂无输入数据可备份".to_string());
    }

    // 2. 库密钥信封（跨机恢复用）与 manifest（最后上传 = 提交标记）
    let key_enc = crate::crypto::encrypt_pack(local_key.as_bytes(), &password)?;
    let manifest = serde_json::json!({
        "ts": ts,
        "machine": machine_id,
        "files": files,
    });
    let manifest_bytes =
        serde_json::to_vec(&manifest).map_err(|e| format!("序列化 manifest 失败: {e}"))?;

    // 3. 上传。manifest 最后传：对端把「有 manifest」当作快照完整的标志。
    let base = format!("{BACKUP_ROOT}/{machine_id}/{snap_name}");
    dav.ensure_dir(BACKUP_ROOT)?;
    dav.ensure_dir(&format!("{BACKUP_ROOT}/{machine_id}"))?;
    dav.ensure_dir(&base)?;
    dav.put(&format!("{base}/key.enc"), key_enc)?;
    for name in &files {
        let data = std::fs::read(tmp.join(name)).map_err(|e| format!("读取快照 {name} 失败: {e}"))?;
        dav.put(&format!("{base}/{name}"), data)?;
    }
    dav.put(&format!("{base}/manifest.json"), manifest_bytes)?;

    // 4. 清理过期快照（只删自己名下）
    cleanup_expired(&dav, &machine_id, retention_days);

    let _ = std::fs::remove_dir_all(&tmp);
    Ok(SnapshotMeta {
        ts,
        name: snap_name,
        files,
    })
}

/// `VACUUM INTO` 在线快照。源库是 SQLCipher 加密库，产物自动继承同一密钥。
fn vacuum_into(src: &Path, dst: &Path, label: &str) -> Result<(), String> {
    if dst.exists() {
        std::fs::remove_file(dst).map_err(|e| format!("清理 {label} 临时文件失败: {e}"))?;
    }
    let conn = crate::crypto::open_log_db(src, label)?;
    // VACUUM INTO 的目标只接受字符串字面量，单引号转义后内插
    let dst_q = dst.to_string_lossy().replace('\'', "''");
    conn.execute_batch(&format!("VACUUM INTO '{dst_q}';"))
        .map_err(|e| format!("快照 {label} 失败: {e}"))?;
    Ok(())
}

/// 列出自己名下的过期快照并删除。目录名 = 13 位毫秒时间戳。
fn cleanup_expired(dav: &Dav, machine_id: &str, retention_days: u32) {
    let cutoff = now_ms() - retention_days as i64 * 86_400_000;
    let own = format!("{BACKUP_ROOT}/{machine_id}");
    let Ok(entries) = dav.list(&own) else {
        return;
    };
    for e in entries {
        if !e.is_dir {
            continue;
        }
        match e.name.parse::<i64>() {
            Ok(ts) if ts < cutoff => {
                let _ = dav.delete(&format!("{own}/{}", e.name));
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// 快照列表与恢复
// ---------------------------------------------------------------------------

/// 列出云端可恢复的快照（按时间倒序）。没有 manifest 的目录是上传中断的
/// 残骸，跳过。
pub fn list_snapshots() -> Result<Vec<SnapshotMeta>, String> {
    let sync_cfg = current_sync_cfg()?;
    let dav = Dav::new(&sync_cfg)?;
    let Some(machine_id) = crate::sync::current_machine_id()? else {
        return Ok(Vec::new());
    };
    let own = format!("{BACKUP_ROOT}/{machine_id}");
    let entries = dav.list(&own)?;
    let mut out = Vec::new();
    for e in entries {
        if !e.is_dir {
            continue;
        }
        let Ok(bytes) = dav.get(&format!("{own}/{}/manifest.json", e.name)) else {
            continue;
        };
        let Ok(v) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
            continue;
        };
        let Some(ts) = v.get("ts").and_then(|x| x.as_i64()) else {
            continue;
        };
        let files = v
            .get("files")
            .and_then(|x| x.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|f| f.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();
        out.push(SnapshotMeta { ts, name: e.name, files });
    }
    out.sort_by(|a, b| b.ts.cmp(&a.ts));
    Ok(out)
}

/// 恢复到某个时间点。长操作（下载整库 + 可能的改密导出），放阻塞线程跑。
///
/// 现有数据恢复前先挪进 `input-log/pre-restore-<ts>/`（不删除），恢复错了
/// 可以手工搬回去。恢复不动 settings.json——分类与各项设置保持现状。
pub fn restore(ts: i64) -> Result<RestoreReport, String> {
    let sync_cfg = current_sync_cfg()?;
    let dav = Dav::new(&sync_cfg)?;
    let password = sync_cfg.password.clone();
    let Some(machine_id) = crate::sync::current_machine_id()? else {
        return Err("本机还没有任何数据".to_string());
    };
    let own = format!("{BACKUP_ROOT}/{machine_id}");
    let snap_name = ts.to_string();
    let base = format!("{own}/{snap_name}");

    // 1. 下载 manifest 与密钥信封
    let manifest_bytes = dav
        .get(&format!("{base}/manifest.json"))
        .map_err(|_| format!("快照 {snap_name} 不存在或 manifest 缺失"))?;
    let manifest: serde_json::Value = serde_json::from_slice(&manifest_bytes)
        .map_err(|e| format!("manifest 解析失败: {e}"))?;
    let files: Vec<String> = manifest
        .get("files")
        .and_then(|x| x.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|f| f.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();
    if files.is_empty() {
        return Err("快照 manifest 里没有任何文件".to_string());
    }
    let key_enc = dav
        .get(&format!("{base}/key.enc"))
        .map_err(|e| format!("下载密钥信封失败: {e}"))?;
    let snap_key = match crate::crypto::try_decrypt_pack(&key_enc, &password)? {
        Some(plain) => String::from_utf8(plain)
            .map_err(|_| "密钥信封内容异常".to_string())?
            .trim()
            .to_string(),
        None => return Err("密钥信封格式异常（不是加密信封）".to_string()),
    };
    if !crate::crypto::is_valid_key_hex(&snap_key) {
        return Err("快照密钥格式异常，快照可能已损坏".to_string());
    }

    // 2. 下载库文件到临时目录
    let tmp = temp_dir("restore", ts);
    std::fs::create_dir_all(&tmp).map_err(|e| format!("创建临时目录失败: {e}"))?;
    for name in &files {
        let data = dav
            .get(&format!("{base}/{name}"))
            .map_err(|e| format!("下载 {name} 失败: {e}"))?;
        std::fs::write(tmp.join(name), &data).map_err(|e| format!("写入临时 {name} 失败: {e}"))?;
    }

    // 3. 用快照密钥逐一验证（坏快照绝不动本地数据）
    for name in &files {
        crate::crypto::open_with_explicit_key(&tmp.join(name), &snap_key, name).map(|_| ())?;
    }

    // 4. 跨机恢复：快照密钥 ≠ 本机密钥 → 改密到本机密钥
    let local_key = crate::crypto::db_key_hex()?;
    let mut cross_machine = false;
    if snap_key != local_key {
        cross_machine = true;
        for name in &files {
            let dst = tmp.join(format!("{name}.rekey"));
            crate::crypto::rekey_db(&tmp.join(name), &snap_key, &dst, &local_key, name)?;
            std::fs::rename(&dst, tmp.join(name))
                .map_err(|e| format!("替换 {name} 失败: {e}"))?;
        }
    }

    // 5. 换库。events/meta 由 recorder 线程处理；trail 单独处理。
    let restored = crate::recorder::restore_from(&tmp, ts)?;
    // trail：先清掉现有连接与文件，再把快照里的 trail.db 放回去。
    // trail 线程的连接是懒打开的，下次采样时会自动从磁盘重新打开。
    crate::trail::set_enabled(false);
    crate::trail::wipe();
    if files.iter().any(|f| f == "trail.db") && restored.iter().any(|f| f == "events.db") {
        // 有 trail 快照就恢复（trail 是独立采样数据，不以 recorder 的恢复结果为准）
        let src = tmp.join("trail.db");
        if src.exists() {
            std::fs::copy(&src, crate::recorder::log_dir().join("trail.db"))
                .map_err(|e| format!("写入 trail.db 失败: {e}"))?;
        }
    }
    crate::trail::set_enabled(current_trail_should_be_enabled());

    let _ = std::fs::remove_dir_all(&tmp);
    let rollback = format!("input-log/pre-restore-{ts}");
    let mut note = format!("恢复前的数据已保留在 {rollback}，确认无误后可自行删除。恢复不影响分类与各项设置。");
    if cross_machine {
        note.push_str("本次为跨机恢复，快照库已改密到本机密钥。");
    }
    Ok(RestoreReport { restored, note })
}

/// 恢复后 trail 是否应该继续工作：照当前设置来（恢复不改变用户的开关偏好）。
fn current_trail_should_be_enabled() -> bool {
    settings_arc()
        .map(|s| {
            let s = s.lock().unwrap_or_else(|e| e.into_inner());
            s.input_log_enabled
                && !s.input_log_paused
                && s.input_log_trail_recording
                && s.input_log_password_hash.is_some()
        })
        .unwrap_or(false)
}

fn current_sync_cfg() -> Result<SyncSettings, String> {
    let settings = settings_arc().ok_or("备份未初始化")?;
    let s = settings.lock().unwrap_or_else(|e| e.into_inner());
    let sync_cfg = s.sync.clone();
    if !sync_cfg.configured() {
        return Err("备份复用数据同步的 WebDAV 配置，请先配置数据同步".to_string());
    }
    Ok(sync_cfg)
}

// ---------------------------------------------------------------------------
// 小工具
// ---------------------------------------------------------------------------

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn temp_dir(tag: &str, ts: i64) -> PathBuf {
    std::env::temp_dir().join(format!("kuro-backup-{tag}-{}-{ts}", std::process::id()))
}

/// 快照时间点的本地可读形式（状态栏展示用）。
fn format_ts(ts: i64) -> String {
    use chrono::TimeZone;
    chrono::Local
        .timestamp_millis_opt(ts)
        .single()
        .map(|d| d.format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_else(|| ts.to_string())
}
