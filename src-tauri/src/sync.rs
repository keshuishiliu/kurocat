//! 数据同步 —— 把本机的输入统计经 WebDAV 网盘与另一台（或 N 台）KuroCat 互相合并。
//!
//! # 总体架构
//!
//! 每台机器继续写自己的本地库（events.db / meta.db / trail.db），同步只做两件事：
//!
//! 1. **导出**：把「上次导出之后的新数据」打成一个 JSON 合并包，PUT 到网盘上
//!    自己名下（`KuroCatSync/<machine_id>/`）；
//! 2. **导入**：列出网盘上其它机器名下的合并包，凡是没导入过的下载回来，
//!    按主键累加合并进本地库，然后登记包名——同一个包绝不二次导入。
//!
//! 两台机器**不需要同时在线**：包放在网盘上等对端来取。也不需要桌面云盘客户端，
//! 应用直接说 WebDAV 协议（坚果云在「安全选项 → 第三方应用管理」生成应用密码）。
//!
//! # 触发时机（除手动外，全部要求「自动同步」开关打开）
//!
//! - 应用启动后立即一次；正常退出前再尝试一次（失败不阻塞退出，下次启动补上）；
//! - 之后按设置里的周期（15/30/60 分钟）定时执行；
//! - 统计窗口「立即同步」按钮随时手动触发（只要求配置完整）。
//!
//! # 幂等与防重复计数（三层）
//!
//! 1. `imported_packs` 登记表：同一个（来源机器，包名）只导入一次；
//! 2. 导入水位线：事件/轨迹按 `ts > 水位线` 过滤，与来源包的 `export_ts` 对齐；
//! 3. `merge_state` 差值合并：汇总表每次只应用「本次值 − 上次值」的差，
//!    就算登记表整个丢了、同一个包被重放，计数也不会翻倍。
//!
//! # 回声抑制（src 列）
//!
//! 导入进来的行如果被本机再导出，就会在两台机器之间无限反弹。所以明细表
//! （events 月表与 trail）带一个 `src` 列：本机记录的行 `src` 为 NULL，
//! 导入的行 `src` = 来源机器 id。导出只带 `src IS NULL` 的行。
//! `src` 列由本模块按需 `ALTER TABLE` 补齐，recorder 的 INSERT 不写它，
//! 默认就是 NULL，两边的写入互不干扰。
//!
//! # 应用身份与分类
//!
//! 数据库里只有进程名（`chrome.exe`），没有路径——路径对另一台机器无意义，
//! 而且后装的软件天然能接上历史数据（按 process 匹配同一行）。合并包带对方的
//! `apps` 表，导入时按进程名映射成本机 `app_id`；本机没有的进程新建一行。
//!
//! 分类随包同步，合并规则（用户钦定）：**本机已有的映射一律不动，本机未分类
//! 的进程采用对方的映射**；映射指向的分类本机没有时自动补进分类列表。
//! 自定义显示名（app_display_names）与忽略名单（app_ignored）走同一套规则。
//! 纯并集合并，天然幂等。
//!
//! # 线程纪律
//!
//! 单个 worker 线程贯穿应用生命周期；所有 SQLite 连接**按次开关**（每次同步
//! 现开现关），绝不长持句柄——「清空全部记录」删目录时不会被卡住。手动同步、
//! 定时同步、退出同步共用一把互斥锁，绝不并发写库。

use crate::config::{normalize_app_categories, normalize_categories, AppSettings, SyncSettings};
use crate::recorder;
use chrono::NaiveDate;
use rusqlite::{params, Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use serde_json::Value as Json;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Emitter};

// ---------------------------------------------------------------------------
// 常量
// ---------------------------------------------------------------------------

/// WebDAV 上的根目录名。所有机器共用，子目录按 machine_id 隔离。
const DAV_ROOT: &str = "KuroCatSync";
/// 合并包格式版本。结构不兼容时递增，旧包导入时按 format 拒绝。
const PACK_FORMAT: u32 = 1;
/// 单包事件明细上限。超出部分留给下一个包（水位线按实际导出的最大 ts 推进）。
const PACK_MAX_EVENTS: usize = 200_000;
/// 单包轨迹采样点上限。
const PACK_MAX_TRAIL: usize = 400_000;
// 云端旧包保留天数不再用常量：取设置 sync.retain_days（默认 15 天）。
/// 退出前最后一次同步最多等这么久，超时不阻塞退出（下次启动自动补传）。
const SHUTDOWN_WAIT: Duration = Duration::from_secs(12);
/// 手动同步的最大等待（命令在阻塞线程里跑，前端要等结果）。
const MANUAL_WAIT: Duration = Duration::from_secs(60);
/// worker 在「未启用 / 仅手动」状态下的巡检间隔：每分钟醒来看一眼配置变没变。
const IDLE_POLL: u64 = 60;
/// 清空日志目录后递增，运行中的同步发现代际变了就立刻收手，避免把刚删掉的库又建出来。
static GENERATION: AtomicU64 = AtomicU64::new(0);

// ---------------------------------------------------------------------------
// 合并包
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
struct PackApp {
    id: i64,
    process: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
struct PackEvent {
    ts: i64,
    kind: i32,
    code: i32,
    flags: i32,
    x: Option<i32>,
    y: Option<i32>,
    app_id: Option<i64>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
struct PackTrail {
    ts: i64,
    x: i32,
    y: i32,
}

/// daily_stats 一行：按键/点击按天计数。
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
struct PackRowStats {
    day: String,
    kind: i32,
    code: i32,
    n: i64,
}

/// daily_hours 一行：按小时分桶。
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
struct PackRowHours {
    day: String,
    hour: i32,
    kd: i64,
    md: i64,
}

/// daily_apps 一行。app 维度直接用进程名——对方库里的 app_id 在本机毫无意义。
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
struct PackRowApps {
    day: String,
    process: String,
    kind: i32,
    n: i64,
    secs: i64,
}

/// monthly_app_keys 一行。
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
struct PackRowKeys {
    month: String,
    process: String,
    code: i32,
    n: i64,
}

/// daily_app_focus 一行：前台占据时长。
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
struct PackRowFocus {
    day: String,
    hour: i32,
    process: String,
    secs: i64,
}

/// 一个合并包 = 导出时刻的全量分类配置 + 水位线之后的增量数据。
///
/// 汇总表带的是**全量值**而不是差值：差值在导入侧按 `merge_state` 现算。
/// 这样丢包可以自愈——任何更新的包都携带完整现状，不依赖「每个包依次到达」。
///
/// `app_display_names` / `app_ignored` 是后加的字段（`serde(default)`）：旧版本
/// 收到新包会忽略未知字段，新版本收到旧包用空列表，双向兼容，不递增 PACK_FORMAT。
#[derive(Serialize, Deserialize, Clone, Debug)]
struct MergePack {
    format: u32,
    machine_id: String,
    exported_at: i64,
    /// 本包承诺：events 里 ts ≤ export_ts 的行已全部送达（含更早的包）。
    export_ts: i64,
    /// 轨迹单独一个水位线（采样独立开关，两库推进不同步）。
    export_trail_ts: i64,
    categories: Vec<Json>,
    app_categories: Vec<(String, String)>,
    /// 进程名 → 自定义显示名（与 app_categories 同一套合并规则：本机已有的一律不动）。
    #[serde(default)]
    app_display_names: Vec<(String, String)>,
    /// 进程名 → 忽略标记（只带 true 的条目）。口径意图必须跨机器一致，
    /// 否则 A 机忽略、B 机照记并把这个程序的数据持续合并回 A。
    #[serde(default)]
    app_ignored: Vec<(String, bool)>,
    apps: Vec<PackApp>,
    events: Vec<PackEvent>,
    trail: Vec<PackTrail>,
    stats: Vec<PackRowStats>,
    hours: Vec<PackRowHours>,
    app_days: Vec<PackRowApps>,
    app_keys: Vec<PackRowKeys>,
    focus: Vec<PackRowFocus>,
}

impl MergePack {
    fn empty(machine_id: &str) -> Self {
        Self {
            format: PACK_FORMAT,
            machine_id: machine_id.to_string(),
            exported_at: 0,
            export_ts: 0,
            export_trail_ts: 0,
            categories: Vec::new(),
            app_categories: Vec::new(),
            app_display_names: Vec::new(),
            app_ignored: Vec::new(),
            apps: Vec::new(),
            events: Vec::new(),
            trail: Vec::new(),
            stats: Vec::new(),
            hours: Vec::new(),
            app_days: Vec::new(),
            app_keys: Vec::new(),
            focus: Vec::new(),
        }
    }
}

/// 合并包文件名：`<导出时刻13位>_<序号6位>_<machine_id>.json`。
/// 前缀时间戳让「清理超期旧包」不用下载内容就能判断。
fn pack_name(exported_at: i64, seq: i64, machine_id: &str) -> String {
    format!("{:013}_{:06}_{}.json", exported_at, seq, machine_id)
}

/// 合并结果，给前端 toast 用。
#[derive(Serialize, Clone, Default, Debug)]
#[serde(rename_all = "camelCase")]
pub struct SyncOutcome {
    pub uploaded: usize,
    pub imported: usize,
    pub skipped: usize,
    pub categories_merged: bool,
    pub warnings: Vec<String>,
}

// ---------------------------------------------------------------------------
// sync_state / merge_state 小助手（全部经由 meta.db）
// ---------------------------------------------------------------------------

fn get_state(conn: &Connection, key: &str) -> Option<String> {
    conn.query_row(
        "SELECT value FROM sync_state WHERE key = ?1",
        params![key],
        |r| r.get(0),
    )
    .ok()
}

fn set_state(conn: &Connection, key: &str, value: &str) -> Result<(), String> {
    conn.execute(
        "INSERT INTO sync_state(key, value) VALUES(?1, ?2) \
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )
    .map(|_| ())
    .map_err(|e| e.to_string())
}

fn get_state_i64(conn: &Connection, key: &str) -> i64 {
    get_state(conn, key)
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}

/// 机器身份：16 位十六进制，存 sync_state，全库只有这一份。
/// 用进程内随机种子（RandomState 每次构造都换新种子）混时间戳生成，
/// 不引随机数依赖；对两三台机器而言碰撞概率可忽略。
fn ensure_machine_id(conn: &Connection) -> String {
    if let Some(id) = get_state(conn, "machine_id") {
        if !id.is_empty() {
            return id;
        }
    }
    use std::hash::{BuildHasher, Hasher};
    let mut out = String::with_capacity(16);
    let mut counter = 0u64;
    while out.len() < 16 {
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u64(now_ms() as u64);
        h.write_u64(std::process::id() as u64);
        h.write_u64(counter);
        counter += 1;
        out.push_str(&format!("{:016x}", h.finish()));
    }
    out.truncate(16);
    let _ = set_state(conn, "machine_id", &out);
    out
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// "YYYY-MM-DD" 的前一天。解析失败原样返回（宁可多导一天，不能漏）。
fn day_before(day: &str) -> String {
    match NaiveDate::parse_from_str(day, "%Y-%m-%d") {
        Ok(d) => (d - chrono::Duration::days(1)).format("%Y-%m-%d").to_string(),
        Err(_) => day.to_string(),
    }
}

// 事件月表月份统一走 recorder::month_of（6 位纯数字 "202611"）。
// recorder 侧的 month_of 格式 bug 已修复，两边的表名天然一致（events_YYYYMM）。

// ---------------------------------------------------------------------------
// 本地连接（按次开关，绝不长持）
// ---------------------------------------------------------------------------

fn open_meta() -> Result<Connection, String> {
    let dir = recorder::log_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("创建日志目录失败: {e}"))?;
    let conn =
        Connection::open(dir.join("meta.db")).map_err(|e| format!("打开 meta.db 失败: {e}"))?;
    conn.busy_timeout(Duration::from_millis(3000))
        .map_err(|e| e.to_string())?;
    conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL;")
        .map_err(|e| format!("配置 meta.db 失败: {e}"))?;
    // 建齐所有表（含 sync 三张表）。记录功能从没开过也能用同步。
    conn.execute_batch(recorder::META_SCHEMA)
        .map_err(|e| format!("建表失败: {e}"))?;
    Ok(conn)
}

/// 只读打开 meta.db（不存在不创建）。状态查询用。
fn open_meta_existing() -> Option<Connection> {
    let path = recorder::log_dir().join("meta.db");
    if !path.exists() {
        return None;
    }
    let conn = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_WRITE).ok()?;
    conn.busy_timeout(Duration::from_millis(2000)).ok()?;
    Some(conn)
}

fn open_events_read() -> Option<Connection> {
    let path = recorder::log_dir().join("events.db");
    if !path.exists() {
        return None;
    }
    let conn = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_WRITE).ok()?;
    conn.busy_timeout(Duration::from_millis(3000)).ok()?;
    Some(conn)
}

fn open_events_write() -> Result<Connection, String> {
    let dir = recorder::log_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("创建日志目录失败: {e}"))?;
    let conn = Connection::open(dir.join("events.db"))
        .map_err(|e| format!("打开 events.db 失败: {e}"))?;
    conn.busy_timeout(Duration::from_millis(3000))
        .map_err(|e| e.to_string())?;
    conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL;")
        .map_err(|e| format!("配置 events.db 失败: {e}"))?;
    Ok(conn)
}

fn open_trail_read() -> Option<Connection> {
    let path = recorder::log_dir().join("trail.db");
    if !path.exists() {
        return None;
    }
    let conn = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_WRITE).ok()?;
    conn.busy_timeout(Duration::from_millis(3000)).ok()?;
    Some(conn)
}

/// 打开（必要时创建）trail.db。只有导入轨迹时才调用。
fn open_trail_write() -> Result<Connection, String> {
    let dir = recorder::log_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("创建日志目录失败: {e}"))?;
    let conn =
        Connection::open(dir.join("trail.db")).map_err(|e| format!("打开 trail.db 失败: {e}"))?;
    conn.busy_timeout(Duration::from_millis(3000))
        .map_err(|e| e.to_string())?;
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA synchronous = NORMAL;
         CREATE TABLE IF NOT EXISTS trail (
             ts INTEGER NOT NULL,
             x  INTEGER NOT NULL,
             y  INTEGER NOT NULL
         );
         CREATE INDEX IF NOT EXISTS idx_trail_ts ON trail(ts);",
    )
    .map_err(|e| format!("初始化 trail.db 失败: {e}"))?;
    Ok(conn)
}

// ---------------------------------------------------------------------------
// src 列：回声抑制的物理基础
// ---------------------------------------------------------------------------

/// 给表补 `src TEXT` 列（已存在则跳过）。老库升级第一条路径。
fn ensure_src_column(conn: &Connection, table: &str) -> Result<(), String> {
    let has_src: bool = conn
        .prepare(&format!("PRAGMA table_info({table})"))
        .and_then(|mut stmt| {
            stmt.query_map([], |r| r.get::<_, String>(1))
                .map(|rows| {
                    rows.flatten().any(|name| name.eq_ignore_ascii_case("src"))
                })
        })
        .map_err(|e| format!("检查 {table} 表结构失败: {e}"))?;
    if !has_src {
        conn.execute_batch(&format!(
            "ALTER TABLE {table} ADD COLUMN src TEXT;"
        ))
        .map_err(|e| format!("{table} 补 src 列失败: {e}"))?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// WebDAV 客户端
// ---------------------------------------------------------------------------

struct DavEntry {
    /// 解码后的名字（目录不带尾斜杠）。
    name: String,
    is_dir: bool,
}

struct Dav {
    client: reqwest::blocking::Client,
    /// 无尾斜杠的 WebDAV 基址，例如 `https://dav.jianguoyun.com/dav`。
    base: String,
    account: String,
    password: String,
}

impl Dav {
    fn new(cfg: &SyncSettings) -> Result<Dav, String> {
        let base = cfg.server.trim().trim_end_matches('/').to_string();
        if base.is_empty() {
            return Err("服务器地址为空".to_string());
        }
        let client = reqwest::blocking::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| format!("创建 HTTP 客户端失败: {e}"))?;
        Ok(Dav {
            client,
            base,
            account: cfg.account.trim().to_string(),
            password: cfg.password.clone(),
        })
    }

    fn url(&self, path: &str) -> String {
        format!("{}/{}", self.base, path.trim_start_matches('/'))
    }

    fn req(&self, method: reqwest::Method, path: &str) -> reqwest::blocking::RequestBuilder {
        self.client
            .request(method, self.url(path))
            .basic_auth(&self.account, Some(&self.password))
    }

    /// MKCOL 建目录。405（已存在）视为成功。
    fn ensure_dir(&self, path: &str) -> Result<(), String> {
        let method =
            reqwest::Method::from_bytes(b"MKCOL").map_err(|e| e.to_string())?;
        let resp = self.req(method, path).send().map_err(|e| e.to_string())?;
        let status = resp.status().as_u16();
        if status == 201 || status == 405 {
            Ok(())
        } else {
            Err(format!("创建目录 {path} 失败（HTTP {status}），请检查应用密码与权限"))
        }
    }

    fn put(&self, path: &str, body: Vec<u8>) -> Result<(), String> {
        let resp = self
            .req(reqwest::Method::PUT, path)
            .body(body)
            .send()
            .map_err(|e| e.to_string())?;
        if resp.status().is_success() {
            Ok(())
        } else {
            Err(format!(
                "上传 {path} 失败（HTTP {}）",
                resp.status().as_u16()
            ))
        }
    }

    fn get(&self, path: &str) -> Result<Vec<u8>, String> {
        let resp = self
            .req(reqwest::Method::GET, path)
            .send()
            .map_err(|e| e.to_string())?;
        let status = resp.status().as_u16();
        if status == 404 {
            return Err(format!("{path} 不存在（404）"));
        }
        if !resp.status().is_success() {
            return Err(format!("下载 {path} 失败（HTTP {status}）"));
        }
        resp.bytes()
            .map(|b| b.to_vec())
            .map_err(|e| format!("读取 {path} 失败: {e}"))
    }

    fn delete(&self, path: &str) -> Result<(), String> {
        let resp = self
            .req(reqwest::Method::DELETE, path)
            .send()
            .map_err(|e| e.to_string())?;
        let status = resp.status().as_u16();
        if status == 404 || resp.status().is_success() {
            Ok(())
        } else {
            Err(format!("删除 {path} 失败（HTTP {status}）"))
        }
    }

    /// PROPFIND Depth:1 列目录。404（还没建）返回空列表。
    fn list(&self, path: &str) -> Result<Vec<DavEntry>, String> {
        let propfind =
            reqwest::Method::from_bytes(b"PROPFIND").map_err(|e| e.to_string())?;
        let resp = self
            .req(propfind, path)
            .header("Depth", "1")
            .header("Content-Type", "application/xml")
            .body(
                r#"<?xml version="1.0" encoding="utf-8"?>
<d:propfind xmlns:d="DAV:"><d:prop><d:resourcetype/></d:prop></d:propfind>"#,
            )
            .send()
            .map_err(|e| e.to_string())?;
        let status = resp.status().as_u16();
        if status == 404 {
            return Ok(Vec::new());
        }
        if !resp.status().is_success() && status != 207 {
            return Err(format!("列出 {path} 失败（HTTP {status}）"));
        }
        let text = resp.text().map_err(|e| e.to_string())?;
        Ok(parse_propfind_hrefs(&text)
            .into_iter()
            .filter_map(|href| {
                let decoded = percent_decode(&href);
                let is_dir = decoded.ends_with('/');
                let name = decoded.trim_end_matches('/');
                let name = name.rsplit('/').next()?.to_string();
                if name.is_empty() {
                    return None; // 目录本身那一行
                }
                Some(DavEntry { name, is_dir })
            })
            .collect())
    }
}

/// 从 multistatus XML 里抠出所有 href。不引 XML 库：命名空间前缀五花八门，
/// 唯一稳定的是 `<任意前缀:href>…</任意前缀:href>` 这个形状。
/// 闭合标签（`</d:href>`）里同样含 ":href"，靠前一个字符是不是 `/` 排除。
fn parse_propfind_hrefs(body: &str) -> Vec<String> {
    let lower = body.to_lowercase();
    let mut out = Vec::new();
    let mut pos = 0usize;
    while let Some(rel) = lower[pos..].find(":href") {
        let start = pos + rel;
        // 排除闭合标签：":href" 前第二个字符是 '/' 即 `</d:href>`
        if start >= 2 && body.as_bytes()[start - 2] == b'/' {
            pos = start + 5;
            continue;
        }
        let Some(gt_rel) = body[start..].find('>') else { break };
        let val_start = start + gt_rel + 1;
        let Some(lt_rel) = body[val_start..].find('<') else { break };
        out.push(body[val_start..val_start + lt_rel].to_string());
        pos = val_start + lt_rel;
    }
    out
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0usize;
    while i < bytes.len() {
        // 至少还剩两个字符才可能是 %XX
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(hi), Some(lo)) = (hi, lo) {
                out.push(((hi << 4) | lo) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

// ---------------------------------------------------------------------------
// 导出
// ---------------------------------------------------------------------------

/// 生成一个合并包。`settings` 用于带走当前的分类配置。
fn build_pack(
    settings: &Arc<Mutex<AppSettings>>,
    machine_id: &str,
    meta: &Connection,
) -> Result<MergePack, String> {
    let mut pack = MergePack::empty(machine_id);
    pack.exported_at = now_ms();

    let ev_wm = get_state_i64(meta, "export_ts");
    let tr_wm = get_state_i64(meta, "export_trail_ts");
    let export_day = get_state(meta, "export_day");
    let export_month = get_state(meta, "export_month");
    // 汇总表带「水位线前一天」的重叠窗口：跨天落盘的延迟写入（23:59 的键
    // 00:01 才提交）必须被下一个包覆盖到，多导一天由导入侧差值消化。
    let overlap_day = export_day
        .as_deref()
        .map(day_before)
        .unwrap_or_else(|| "0000-00-00".to_string());
    let overlap_month = export_month.clone().unwrap_or_else(|| "0000-00".to_string());

    // ---- 分类与应用标注（全量快照，导入侧做并集合并） ----
    {
        let s = settings.lock().unwrap_or_else(|e| e.into_inner());
        pack.categories = serde_json::to_value(&s.categories)
            .map_err(|e| format!("序列化分类失败: {e}"))?
            .as_array()
            .cloned()
            .unwrap_or_default();
        pack.app_categories = s
            .app_categories
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        pack.app_display_names = s
            .app_display_names
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        // 只带 true 的条目：false 与缺失等价
        pack.app_ignored = s
            .app_ignored
            .iter()
            .filter(|(_, v)| **v)
            .map(|(k, _)| (k.clone(), true))
            .collect();
    }

    // ---- 事件明细（只带本机记录的行：src IS NULL；按月表逐个捞，封顶） ----
    if let Some(conn) = open_events_read() {
        let months = recorder::list_event_months(&conn)?;
        let mut remaining = PACK_MAX_EVENTS;
        for month in months {
            if remaining == 0 {
                break;
            }
            ensure_src_column(&conn, &recorder::month_table_name(&month))?;
            let table = recorder::month_table_name(&month);
            let sql = format!(
                "SELECT ts, kind, code, flags, x, y, app_id FROM {table} \
                 WHERE ts > ?1 AND src IS NULL ORDER BY ts LIMIT ?2"
            );
            let mut stmt = conn.prepare(&sql).map_err(|e| e.to_string())?;
            let rows = stmt
                .query_map(params![ev_wm, remaining as i64], |r| {
                    Ok(PackEvent {
                        ts: r.get(0)?,
                        kind: r.get(1)?,
                        code: r.get(2)?,
                        flags: r.get(3)?,
                        x: r.get(4)?,
                        y: r.get(5)?,
                        app_id: r.get(6)?,
                    })
                })
                .map_err(|e| e.to_string())?;
            for row in rows.flatten() {
                remaining -= 1;
                pack.events.push(row);
            }
        }
    }
    pack.export_ts = pack
        .events
        .iter()
        .map(|e| e.ts)
        .fold(ev_wm, i64::max);

    // ---- 轨迹采样（同样只带本机记录的行） ----
    if let Some(conn) = open_trail_read() {
        ensure_src_column(&conn, "trail")?;
        let mut stmt = conn
            .prepare("SELECT ts, x, y FROM trail WHERE ts > ?1 AND src IS NULL ORDER BY ts LIMIT ?2")
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map(params![tr_wm, PACK_MAX_TRAIL as i64], |r| {
                Ok(PackTrail {
                    ts: r.get(0)?,
                    x: r.get(1)?,
                    y: r.get(2)?,
                })
            })
            .map_err(|e| e.to_string())?;
        for row in rows.flatten() {
            pack.trail.push(row);
        }
        pack.export_trail_ts = pack.trail.iter().map(|t| t.ts).fold(tr_wm, i64::max);
    }

    // ---- apps 表全量（导入侧按 process 映射） ----
    {
        let mut stmt = meta
            .prepare("SELECT id, process FROM apps ORDER BY id")
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([], |r| {
                Ok(PackApp {
                    id: r.get(0)?,
                    process: r.get(1)?,
                })
            })
            .map_err(|e| e.to_string())?;
        pack.apps = rows.flatten().collect();
    }

    // ---- 汇总表（重叠窗口内全量值） ----
    {
        let mut stmt = meta
            .prepare("SELECT day, kind, code, n FROM daily_stats WHERE day >= ?1")
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map(params![overlap_day], |r| {
                Ok(PackRowStats {
                    day: r.get(0)?,
                    kind: r.get(1)?,
                    code: r.get(2)?,
                    n: r.get(3)?,
                })
            })
            .map_err(|e| e.to_string())?;
        pack.stats = rows.flatten().collect();
    }
    {
        let mut stmt = meta
            .prepare("SELECT day, hour, keydown, mousedown FROM daily_hours WHERE day >= ?1")
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map(params![overlap_day], |r| {
                Ok(PackRowHours {
                    day: r.get(0)?,
                    hour: r.get(1)?,
                    kd: r.get(2)?,
                    md: r.get(3)?,
                })
            })
            .map_err(|e| e.to_string())?;
        pack.hours = rows.flatten().collect();
    }
    {
        let mut stmt = meta
            .prepare(
                "SELECT d.day, a.process, d.kind, d.n, d.active_seconds \
                 FROM daily_apps d JOIN apps a ON a.id = d.app_id WHERE d.day >= ?1",
            )
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map(params![overlap_day], |r| {
                Ok(PackRowApps {
                    day: r.get(0)?,
                    process: r.get(1)?,
                    kind: r.get(2)?,
                    n: r.get(3)?,
                    secs: r.get(4)?,
                })
            })
            .map_err(|e| e.to_string())?;
        pack.app_days = rows.flatten().collect();
    }
    {
        let mut stmt = meta
            .prepare(
                "SELECT k.month, a.process, k.code, k.n \
                 FROM monthly_app_keys k JOIN apps a ON a.id = k.app_id WHERE k.month >= ?1",
            )
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map(params![overlap_month], |r| {
                Ok(PackRowKeys {
                    month: r.get(0)?,
                    process: r.get(1)?,
                    code: r.get(2)?,
                    n: r.get(3)?,
                })
            })
            .map_err(|e| e.to_string())?;
        pack.app_keys = rows.flatten().collect();
    }
    {
        let mut stmt = meta
            .prepare(
                "SELECT f.day, f.hour, a.process, f.seconds \
                 FROM daily_app_focus f JOIN apps a ON a.id = f.app_id WHERE f.day >= ?1",
            )
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map(params![overlap_day], |r| {
                Ok(PackRowFocus {
                    day: r.get(0)?,
                    hour: r.get(1)?,
                    process: r.get(2)?,
                    secs: r.get(3)?,
                })
            })
            .map_err(|e| e.to_string())?;
        pack.focus = rows.flatten().collect();
    }

    Ok(pack)
}

// ---------------------------------------------------------------------------
// 导入
// ---------------------------------------------------------------------------

/// 导入端：对汇总行做差值计算。返回要累加的增量（≤ 0 一律 0）。
fn agg_delta(last: i64, value: i64) -> i64 {
    (value - last).max(0)
}

fn merge_state_get(conn: &Connection, src: &str, gkey: &str) -> i64 {
    conn.query_row(
        "SELECT last_n FROM merge_state WHERE source_id = ?1 AND gkey = ?2",
        params![src, gkey],
        |r| r.get(0),
    )
    .unwrap_or(0)
}

fn merge_state_set(conn: &Connection, src: &str, gkey: &str, value: i64) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO merge_state(source_id, gkey, last_n) VALUES(?1, ?2, ?3) \
         ON CONFLICT(source_id, gkey) DO UPDATE SET last_n = excluded.last_n",
        params![src, gkey, value],
    )?;
    Ok(())
}

fn local_app_id(conn: &Connection, process: &str) -> Option<i64> {
    conn.query_row(
        "SELECT id FROM apps WHERE process = ?1",
        params![process],
        |r| r.get(0),
    )
    .ok()
}

fn mark_imported(meta: &Connection, src: &str, name: &str) -> Result<(), String> {
    meta.execute(
        "INSERT OR IGNORE INTO imported_packs(source_id, pack_name, imported_at) \
         VALUES(?1, ?2, ?3)",
        params![src, name, now_ms()],
    )
    .map(|_| ())
    .map_err(|e| e.to_string())
}

/// 分类并集合并：本机已有的 id / 映射一律不动；缺的补进来；未映射的进程
/// 且对方分类存在时才填。自定义显示名与忽略名单走同一套规则（本机优先，
/// 纯并集，天然幂等）。返回是否发生了变化（变化时由调用方保存并广播）。
fn merge_categories(
    pack: &MergePack,
    settings: &Arc<Mutex<AppSettings>>,
) -> Result<bool, String> {
    let pack_cats: Vec<crate::config::CategoryDef> = pack
        .categories
        .iter()
        .map(|v| serde_json::from_value(v.clone()).map_err(|e| e.to_string()))
        .collect::<Result<Vec<_>, _>>()?;

    let mut s = settings.lock().unwrap_or_else(|e| e.into_inner());
    let before = serde_json::to_string(&(
        &s.categories,
        &s.app_categories,
        &s.app_display_names,
        &s.app_ignored,
    ))
    .unwrap_or_default();

    for cat in pack_cats {
        if !s.categories.iter().any(|c| c.id == cat.id) {
            s.categories.push(cat);
        }
    }
    for (process, cid) in &pack.app_categories {
        if !s.app_categories.contains_key(process)
            && s.categories.iter().any(|c| &c.id == cid)
        {
            s.app_categories.insert(process.clone(), cid.clone());
        }
    }
    for (process, name) in &pack.app_display_names {
        if !s.app_display_names.contains_key(process) && !name.trim().is_empty() {
            s.app_display_names.insert(process.clone(), name.clone());
        }
    }
    for (process, ignored) in &pack.app_ignored {
        if *ignored && !s.app_ignored.contains_key(process) {
            s.app_ignored.insert(process.clone(), true);
        }
    }
    s.categories = normalize_categories(&s.categories);
    s.app_categories = normalize_app_categories(&s.app_categories, &s.categories);
    s.app_display_names = crate::config::normalize_app_display_names(&s.app_display_names);
    s.app_ignored = crate::config::normalize_app_ignored(&s.app_ignored);

    let after = serde_json::to_string(&(
        &s.categories,
        &s.app_categories,
        &s.app_display_names,
        &s.app_ignored,
    ))
    .unwrap_or_default();
    // 只改内存里的共享设置，落盘由调用方决定——测试用的内存状态不该碰真实 settings.json
    Ok(before != after)
}

/// 导入一个包。events / trail 连接由调用方注入（便于测试用内存库），trail 允许为空。
/// 返回 (导入成功, 是否合并了分类)。
fn import_pack(
    pack: &MergePack,
    src: &str,
    name: &str,
    meta: &Connection,
    events: Option<&Connection>,
    trail: Option<&Connection>,
    settings: &Arc<Mutex<AppSettings>>,
) -> Result<bool, String> {
    if pack.format != PACK_FORMAT {
        // 格式不认识的包：登记成已处理，避免每个周期都重试报错。
        mark_imported(meta, src, name)?;
        return Err(format!("包格式版本不支持（{}）", pack.format));
    }
    if pack.machine_id != src {
        mark_imported(meta, src, name)?;
        return Err("包内 machine_id 与目录名不一致，已跳过".to_string());
    }

    // ---- 1. apps 映射：对方 app_id → 本机 app_id ----
    let mut app_map: HashMap<i64, Option<i64>> = HashMap::new();
    {
        let mut known: HashMap<String, i64> = HashMap::new();
        {
            let mut stmt = meta
                .prepare("SELECT id, process FROM apps")
                .map_err(|e| e.to_string())?;
            let rows = stmt
                .query_map([], |r| Ok((r.get::<_, String>(1)?, r.get::<_, i64>(0)?)))
                .map_err(|e| e.to_string())?;
            for (process, id) in rows.flatten() {
                known.insert(process, id);
            }
        }
        for app in &pack.apps {
            if let Some(&id) = known.get(&app.process) {
                app_map.insert(app.id, Some(id));
            } else {
                meta.execute("INSERT INTO apps(process) VALUES(?1)", params![app.process])
                    .map_err(|e| e.to_string())?;
                let id = meta.last_insert_rowid();
                known.insert(app.process.clone(), id);
                app_map.insert(app.id, Some(id));
            }
        }
    }

    // ---- 2. 事件明细（ts > 水位线，按月表落库，打上 src 标记） ----
    {
        let wm = get_state_i64(meta, &format!("import_ev_ts|{src}"));
        let fresh: Vec<&PackEvent> = pack.events.iter().filter(|e| e.ts > wm).collect();
        if !fresh.is_empty() {
            let events = events.ok_or("事件库未就绪")?;
            let tx = events.unchecked_transaction().map_err(|e| e.to_string())?;
            let mut by_month: HashMap<String, Vec<&PackEvent>> = HashMap::new();
            for e in &fresh {
                by_month.entry(recorder::month_of(e.ts)).or_default().push(e);
            }
            for (month, rows) in by_month {
                recorder::ensure_month_table(&tx, &month).map_err(|e| e.to_string())?;
                ensure_src_column(&tx, &recorder::month_table_name(&month))?;
                let table = recorder::month_table_name(&month);
                let sql = format!(
                    "INSERT INTO {table} (ts, kind, code, flags, x, y, app_id, src) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)"
                );
                let mut stmt = tx.prepare(&sql).map_err(|e| e.to_string())?;
                for e in rows {
                    let local = e
                        .app_id
                        .and_then(|id| app_map.get(&id).copied().flatten());
                    stmt.execute(params![e.ts, e.kind, e.code, e.flags, e.x, e.y, local, src])
                        .map_err(|e| e.to_string())?;
                }
            }
            tx.commit().map_err(|e| e.to_string())?;
            // 连接随作用域关闭，绝不长持
        }
        // 水位线推进到包的 export_ts（哪怕一行都没插——来源方承诺 ≤ 该值的都送过）。
        set_state(meta, &format!("import_ev_ts|{src}"), &pack.export_ts.to_string())?;
    }

    // ---- 3. 轨迹采样 ----
    if !pack.trail.is_empty() {
        let trail = trail.ok_or("轨迹库未就绪")?;
        ensure_src_column(trail, "trail")?;
        let wm = get_state_i64(meta, &format!("import_tr_ts|{src}"));
        let fresh: Vec<&PackTrail> = pack.trail.iter().filter(|t| t.ts > wm).collect();
        if !fresh.is_empty() {
            let tx = trail.unchecked_transaction().map_err(|e| e.to_string())?;
            {
                let mut stmt = tx
                    .prepare("INSERT INTO trail(ts, x, y, src) VALUES(?1, ?2, ?3, ?4)")
                    .map_err(|e| e.to_string())?;
                for t in fresh {
                    stmt.execute(params![t.ts, t.x, t.y, src])
                        .map_err(|e| e.to_string())?;
                }
            }
            tx.commit().map_err(|e| e.to_string())?;
        }
        set_state(meta, &format!("import_tr_ts|{src}"), &pack.export_trail_ts.to_string())?;
    }

    // ---- 4. 汇总表：merge_state 差值合并 ----
    {
        let tx = meta.unchecked_transaction().map_err(|e| e.to_string())?;
        for r in &pack.stats {
            let gkey = format!("ds|{}|{}|{}", r.day, r.kind, r.code);
            let last = merge_state_get(&tx, src, &gkey);
            let delta = agg_delta(last, r.n);
            if delta > 0 {
                tx.execute(
                    "INSERT INTO daily_stats(day, kind, code, n) VALUES(?1, ?2, ?3, ?4) \
                     ON CONFLICT(day, kind, code) DO UPDATE SET n = n + excluded.n",
                    params![r.day, r.kind, r.code, delta],
                )
                .map_err(|e| e.to_string())?;
            }
            merge_state_set(&tx, src, &gkey, r.n).map_err(|e| e.to_string())?;
        }
        for r in &pack.hours {
            for (tag, val, col) in [("kd", r.kd, "keydown"), ("md", r.md, "mousedown")] {
                let gkey = format!("dh|{}|{}|{}", r.day, r.hour, tag);
                let last = merge_state_get(&tx, src, &gkey);
                let delta = agg_delta(last, val);
                if delta > 0 {
                    tx.execute(
                        &format!(
                            "INSERT INTO daily_hours(day, hour, {col}) VALUES(?1, ?2, ?3) \
                             ON CONFLICT(day, hour) DO UPDATE SET {col} = {col} + excluded.{col}"
                        ),
                        params![r.day, r.hour, delta],
                    )
                    .map_err(|e| e.to_string())?;
                }
                merge_state_set(&tx, src, &gkey, val).map_err(|e| e.to_string())?;
            }
        }
        for r in &pack.app_days {
            // n（输入次数）与 active_seconds（活跃时长）分开记账
            for (tag, val, is_n) in [("n", r.n, true), ("as", r.secs, false)] {
                let gkey = format!("da|{}|{}|{}|{}", r.day, r.process, r.kind, tag);
                let last = merge_state_get(&tx, src, &gkey);
                let delta = agg_delta(last, val);
                if delta > 0 {
                    if let Some(local_id) = local_app_id(&tx, &r.process) {
                        if is_n {
                            tx.execute(
                                "INSERT INTO daily_apps(day, app_id, kind, n, active_seconds) \
                                 VALUES(?1, ?2, ?3, ?4, 0) \
                                 ON CONFLICT(day, app_id, kind) DO UPDATE SET n = n + excluded.n",
                                params![r.day, local_id, r.kind, delta],
                            )
                            .map_err(|e| e.to_string())?;
                        } else {
                            tx.execute(
                                "INSERT INTO daily_apps(day, app_id, kind, n, active_seconds) \
                                 VALUES(?1, ?2, ?3, 0, ?4) \
                                 ON CONFLICT(day, app_id, kind) DO UPDATE SET \
                                 active_seconds = active_seconds + excluded.active_seconds",
                                params![r.day, local_id, r.kind, delta],
                            )
                            .map_err(|e| e.to_string())?;
                        }
                    }
                }
                // 即使本机暂时没有这个进程（映射不上），差值进度也要推进，
                // 否则每次包都带着同一笔积压增量，将来一映射就重复入账。
                merge_state_set(&tx, src, &gkey, val).map_err(|e| e.to_string())?;
            }
        }
        for r in &pack.app_keys {
            let gkey = format!("mk|{}|{}|{}", r.month, r.process, r.code);
            let last = merge_state_get(&tx, src, &gkey);
            let delta = agg_delta(last, r.n);
            if delta > 0 {
                if let Some(local_id) = local_app_id(&tx, &r.process) {
                    tx.execute(
                        "INSERT INTO monthly_app_keys(month, app_id, code, n) \
                         VALUES(?1, ?2, ?3, ?4) \
                         ON CONFLICT(month, app_id, code) DO UPDATE SET n = n + excluded.n",
                        params![r.month, local_id, r.code, delta],
                    )
                    .map_err(|e| e.to_string())?;
                }
            }
            merge_state_set(&tx, src, &gkey, r.n).map_err(|e| e.to_string())?;
        }
        for r in &pack.focus {
            let gkey = format!("df|{}|{}|{}", r.day, r.hour, r.process);
            let last = merge_state_get(&tx, src, &gkey);
            let delta = agg_delta(last, r.secs);
            if delta > 0 {
                if let Some(local_id) = local_app_id(&tx, &r.process) {
                    tx.execute(
                        "INSERT INTO daily_app_focus(day, hour, app_id, seconds) \
                         VALUES(?1, ?2, ?3, ?4) \
                         ON CONFLICT(day, hour, app_id) DO UPDATE SET \
                         seconds = seconds + excluded.seconds",
                        params![r.day, r.hour, local_id, delta],
                    )
                    .map_err(|e| e.to_string())?;
                }
            }
            merge_state_set(&tx, src, &gkey, r.secs).map_err(|e| e.to_string())?;
        }
        tx.commit().map_err(|e| e.to_string())?;
    }

    // ---- 5. 分类并集合并 ----
    let cats_merged = merge_categories(pack, settings)?;

    // ---- 6. 登记包名（最后一步：前面任何一步失败都不登记，下次重试） ----
    mark_imported(meta, src, name)?;

    Ok(cats_merged)
}

// ---------------------------------------------------------------------------
// 一次完整同步
// ---------------------------------------------------------------------------

/// 互斥锁：手动 / 定时 / 退出三种同步互斥，绝不并发写库。
static SYNC_BUSY: Mutex<()> = Mutex::new(());

fn check_generation(start_gen: u64) -> Result<(), String> {
    if GENERATION.load(Ordering::SeqCst) != start_gen {
        return Err("数据刚被清空，本次同步中止".to_string());
    }
    Ok(())
}

fn current_cfg() -> SyncSettings {
    CFG.get()
        .map(|m| m.lock().unwrap_or_else(|e| e.into_inner()).clone())
        .unwrap_or_default()
}

fn settings_arc() -> Option<Arc<Mutex<AppSettings>>> {
    SETTINGS.get().cloned()
}

fn deadline_exceeded(deadline: Option<Instant>) -> bool {
    deadline.map(|d| Instant::now() >= d).unwrap_or(false)
}

/// 执行一次完整同步（导出 + 导入）。调用方保证持有 SYNC_BUSY 或已串行化。
fn run_sync_once(
    cfg: &SyncSettings,
    settings: &Arc<Mutex<AppSettings>>,
    deadline: Option<Instant>,
) -> Result<SyncOutcome, String> {
    let gen = GENERATION.load(Ordering::SeqCst);
    let _busy = SYNC_BUSY.lock().unwrap_or_else(|e| e.into_inner());
    check_generation(gen)?;

    let dav = Dav::new(cfg)?;
    let mut meta = open_meta()?;
    let machine_id = ensure_machine_id(&meta);
    let mut out = SyncOutcome::default();
    let mut warnings: Vec<String> = Vec::new();

    // ---------------- 导出 ----------------
    if deadline_exceeded(deadline) {
        warnings.push("时间窗口已到，本次只完成了部分同步".to_string());
    } else {
        match export_once(&dav, settings, &machine_id, &mut meta) {
            Ok(n) => out.uploaded = n,
            Err(e) => warnings.push(format!("导出失败: {e}")),
        }
    }
    check_generation(gen)?;

    // ---------------- 导入 ----------------
    let entries = match dav.list(DAV_ROOT) {
        Ok(v) => v,
        Err(e) => {
            warnings.push(format!("列目录失败: {e}"));
            finish(&mut meta, &mut out, warnings);
            return Ok(out);
        }
    };
    let has_others = entries.iter().any(|d| d.is_dir && d.name != machine_id);
    // 连接按需打开：没有要导入的东西就不动 events.db（避免给纯展示用户造空库）
    let events_conn: Option<Connection> = if has_others {
        Some(open_events_write()?)
    } else {
        None
    };
    let mut trail_conn: Option<Connection> = None;
    for dir in &entries {
        if !dir.is_dir || dir.name == machine_id {
            continue;
        }
        if deadline_exceeded(deadline) {
            warnings.push("时间窗口已到，剩余数据下次同步".to_string());
            break;
        }
        let src = dir.name.clone();
        let sub = format!("{DAV_ROOT}/{src}");
        let files = match dav.list(&sub) {
            Ok(v) => v,
            Err(e) => {
                warnings.push(format!("列出 {sub} 失败: {e}"));
                continue;
            }
        };
        for file in files {
            if file.is_dir || !file.name.ends_with(".json") {
                continue;
            }
            if deadline_exceeded(deadline) {
                warnings.push("时间窗口已到，剩余数据下次同步".to_string());
                break;
            }
            // 已登记过 → 跳过
            let seen: i64 = meta
                .query_row(
                    "SELECT COUNT(*) FROM imported_packs WHERE source_id = ?1 AND pack_name = ?2",
                    params![src, file.name],
                    |r| r.get(0),
                )
                .unwrap_or(1);
            if seen > 0 {
                out.skipped += 1;
                continue;
            }
            let bytes = match dav.get(&format!("{sub}/{}", file.name)) {
                Ok(b) => b,
                Err(e) => {
                    warnings.push(format!("下载 {} 失败: {e}", file.name));
                    continue;
                }
            };
            let pack: MergePack = match serde_json::from_slice(&bytes) {
                Ok(p) => p,
                Err(e) => {
                    // 解析失败的包登记成已处理：网盘传输是原子的，解析失败
                    // 意味着内容本身坏了，重试也不会好。汇总数据可由后续包自愈。
                    let _ = mark_imported(&meta, &src, &file.name);
                    warnings.push(format!("解析 {} 失败: {e}", file.name));
                    continue;
                }
            };
            // 轨迹连接懒打开：只有真的有轨迹要导才碰 trail.db
            if !pack.trail.is_empty() && trail_conn.is_none() {
                trail_conn = Some(open_trail_write()?);
            }
            match import_pack(
                &pack,
                &src,
                &file.name,
                &meta,
                events_conn.as_ref(),
                trail_conn.as_ref(),
                settings,
            ) {
                Ok(cats_merged) => {
                    out.imported += 1;
                    if cats_merged {
                        out.categories_merged = true;
                        // 分类变了立刻落盘，别等整轮导入结束——中途崩溃也不丢
                        let s = settings.lock().unwrap_or_else(|e| e.into_inner());
                        crate::config::save_settings(&s);
                    }
                }
                Err(e) => {
                    warnings.push(format!("导入 {} 失败: {e}", file.name));
                }
            }
            check_generation(gen)?;
        }
    }

    if out.categories_merged {
        if let (Some(app), Some(arc)) = (APP.get(), settings_arc()) {
            let snapshot = arc.lock().unwrap_or_else(|e| e.into_inner()).clone();
            let _ = app.emit("kuro-settings", snapshot);
        }
    }

    finish(&mut meta, &mut out, warnings);
    Ok(out)
}

/// 导出并推进水位线。上传成功才算数——中途任何失败都不动水位线，
/// 下次自动重发（重复上传一个内容相同的包没有副作用）。
fn export_once(
    dav: &Dav,
    settings: &Arc<Mutex<AppSettings>>,
    machine_id: &str,
    meta: &mut Connection,
) -> Result<usize, String> {
    let pack = build_pack(settings, machine_id, meta)?;
    let seq = get_state_i64(meta, "export_seq") + 1;
    let name = pack_name(pack.exported_at, seq, machine_id);
    let json = serde_json::to_vec(&pack).map_err(|e| format!("序列化合并包失败: {e}"))?;

    dav.ensure_dir(DAV_ROOT)?;
    let own = format!("{DAV_ROOT}/{machine_id}");
    dav.ensure_dir(&own)?;
    dav.put(&format!("{own}/{name}"), json)?;

    // 上传成功，推进全部水位线
    let today = recorder::day_hour_local(now_ms()).0;
    let month = today[..7].to_string();
    set_state(meta, "export_ts", &pack.export_ts.to_string())?;
    set_state(meta, "export_trail_ts", &pack.export_trail_ts.to_string())?;
    set_state(meta, "export_seq", &seq.to_string())?;
    set_state(meta, "export_day", &today)?;
    set_state(meta, "export_month", &month)?;

    // 清理自己名下超期的旧包（只删自己的；保留天数在设置里配，默认 15 天。
    // 注意别设太小：对端没来得及导入的包删掉就真没了）
    if let Ok(entries) = dav.list(&own) {
        let retain = settings
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .sync
            .retain_days as i64;
        let cutoff = now_ms() - retain.max(1) * 86_400_000;
        for e in entries {
            if e.is_dir || !e.name.ends_with(".json") {
                continue;
            }
            if let Ok(ts) = e.name[..13].parse::<i64>() {
                if ts < cutoff {
                    let _ = dav.delete(&format!("{own}/{}", e.name));
                }
            }
        }
    }
    Ok(1)
}

/// 写入「上次同步」状态。
fn finish(meta: &mut Connection, out: &mut SyncOutcome, warnings: Vec<String>) {
    let ok = warnings.is_empty();
    let _ = set_state(meta, "last_sync_at", &now_ms().to_string());
    let _ = set_state(meta, "last_sync_ok", if ok { "1" } else { "0" });
    let _ = set_state(meta, "last_sync_err", warnings.first().unwrap_or(&String::new()));
    out.warnings = warnings;
}

// ---------------------------------------------------------------------------
// worker 线程与对外入口
// ---------------------------------------------------------------------------

enum SyncMsg {
    Run {
        reply: Sender<Result<SyncOutcome, String>>,
        deadline: Option<Instant>,
    },
    SettingsChanged,
}

static TX: OnceLock<Sender<SyncMsg>> = OnceLock::new();
static STARTED: AtomicBool = AtomicBool::new(false);
static CFG: OnceLock<Mutex<SyncSettings>> = OnceLock::new();
static SETTINGS: OnceLock<Arc<Mutex<AppSettings>>> = OnceLock::new();
static APP: OnceLock<AppHandle> = OnceLock::new();

/// 应用启动时调用一次。worker 立即尝试一次启动同步（仅当开关打开且配置完整）。
pub fn init(app: AppHandle, settings: Arc<Mutex<AppSettings>>, cfg: SyncSettings) {
    let _ = APP.set(app);
    let _ = SETTINGS.set(settings);
    let _ = CFG.set(Mutex::new(cfg));
    start_worker();
}

fn start_worker() {
    if STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    let (tx, rx) = mpsc::channel::<SyncMsg>();
    let _ = TX.set(tx);
    let spawned = std::thread::Builder::new()
        .name("kuro-sync".to_string())
        .spawn(move || worker(rx));
    if spawned.is_err() {
        STARTED.store(false, Ordering::SeqCst);
        eprintln!("[sync] 无法创建同步线程");
    }
}

fn worker(rx: Receiver<SyncMsg>) {
    // 启动同步：开关打开才做。失败无所谓，定时周期会再试。
    {
        let cfg = current_cfg();
        if cfg.enabled && cfg.configured() {
            if let Some(settings) = settings_arc() {
                let _ = run_sync_once(&cfg, &settings, None);
            }
        }
    }
    loop {
        let cfg = current_cfg();
        let auto = cfg.enabled && cfg.configured() && cfg.interval_minutes > 0;
        let wait = if auto {
            Duration::from_secs(cfg.interval_minutes as u64 * 60)
        } else {
            Duration::from_secs(IDLE_POLL)
        };
        match rx.recv_timeout(wait) {
            Ok(SyncMsg::Run { reply, deadline }) => {
                let result = match settings_arc() {
                    Some(settings) => run_sync_once(&cfg, &settings, deadline),
                    None => Err("同步未初始化".to_string()),
                };
                let _ = reply.send(result);
            }
            Ok(SyncMsg::SettingsChanged) => continue,
            Err(RecvTimeoutError::Timeout) => {
                if auto {
                    if let Some(settings) = settings_arc() {
                        let _ = run_sync_once(&cfg, &settings, None);
                    }
                }
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
}

/// 统计窗口保存同步设置后调用：刷新 worker 的配置视图；新开启时立刻同步一次。
pub fn apply_settings(cfg: SyncSettings) {
    if let Some(m) = CFG.get() {
        *m.lock().unwrap_or_else(|e| e.into_inner()) = cfg.clone();
    }
    if cfg.enabled && cfg.configured() {
        if let Some(tx) = TX.get() {
            let (rtx, _) = mpsc::channel();
            let _ = tx.send(SyncMsg::Run {
                reply: rtx,
                deadline: Some(Instant::now() + MANUAL_WAIT),
            });
        }
    } else if let Some(tx) = TX.get() {
        let _ = tx.send(SyncMsg::SettingsChanged);
    }
}

/// 「立即同步」。配置不完整直接报错；走 worker 串行执行，等结果。
pub fn sync_now() -> Result<SyncOutcome, String> {
    let cfg = current_cfg();
    if !cfg.configured() {
        return Err("请先填写服务器地址、账号和应用密码".to_string());
    }
    if STARTED.load(Ordering::SeqCst) {
        if let Some(tx) = TX.get() {
            let (rtx, rrx) = mpsc::channel();
            if tx
                .send(SyncMsg::Run {
                    reply: rtx,
                    deadline: Some(Instant::now() + MANUAL_WAIT),
                })
                .is_ok()
            {
                return match rrx.recv_timeout(MANUAL_WAIT + Duration::from_secs(5)) {
                    Ok(result) => result,
                    Err(_) => Err("同步超时，请稍后查看状态".to_string()),
                };
            }
        }
    }
    // worker 未启动（init 之前）时兜底：当前线程直接跑
    let settings = settings_arc().ok_or("同步未初始化")?;
    run_sync_once(&cfg, &settings, Some(Instant::now() + MANUAL_WAIT))
}

/// 正常退出前调用（RunEvent::Exit）。最多等 12 秒；失败不阻塞退出，
/// 未推进的水位线下次启动自动补传。
pub fn shutdown_sync() {
    if !STARTED.load(Ordering::SeqCst) {
        return;
    }
    let cfg = current_cfg();
    if !cfg.enabled || !cfg.configured() {
        return;
    }
    if let Some(tx) = TX.get() {
        let (rtx, rrx) = mpsc::channel();
        if tx
            .send(SyncMsg::Run {
                reply: rtx,
                deadline: Some(Instant::now() + SHUTDOWN_WAIT - Duration::from_secs(2)),
            })
            .is_ok()
        {
            let _ = rrx.recv_timeout(SHUTDOWN_WAIT);
        }
    }
}

/// 「清空全部记录」删掉了整个 input-log 目录后调用：作废运行中的同步，
/// 下次同步从全新状态开始（对端的 merge_state 会防止历史数据重复入账）。
pub fn on_log_wiped() {
    GENERATION.fetch_add(1, Ordering::SeqCst);
}

// ---------------------------------------------------------------------------
// 状态查询（统计窗口）
// ---------------------------------------------------------------------------

/// 同步状态快照。machine_id 未生成时为 "—"。
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncStatus {
    pub enabled: bool,
    pub configured: bool,
    pub interval_minutes: u32,
    pub machine_id: String,
    pub last_sync_at: Option<i64>,
    pub last_sync_ok: Option<bool>,
    pub last_sync_error: Option<String>,
}

pub fn status() -> SyncStatus {
    let cfg = current_cfg();
    let mut st = SyncStatus {
        enabled: cfg.enabled,
        configured: cfg.configured(),
        interval_minutes: cfg.interval_minutes,
        machine_id: "—".to_string(),
        last_sync_at: None,
        last_sync_ok: None,
        last_sync_error: None,
    };
    if let Some(conn) = open_meta_existing() {
        if let Some(id) = get_state(&conn, "machine_id") {
            st.machine_id = id;
        }
        st.last_sync_at = get_state(&conn, "last_sync_at").and_then(|v| v.parse().ok());
        st.last_sync_ok = get_state(&conn, "last_sync_ok").map(|v| v == "1");
        st.last_sync_error = get_state(&conn, "last_sync_err").filter(|v| !v.is_empty());
    }
    st
}

/// 「测试连接」：建目录 + 传一个探针文件再删掉。
pub fn test_connection(cfg: &SyncSettings) -> Result<String, String> {
    if !cfg.configured() {
        return Err("请先填写服务器地址、账号和应用密码".to_string());
    }
    let dav = Dav::new(cfg)?;
    dav.ensure_dir(DAV_ROOT)?;
    let probe = format!("{DAV_ROOT}/.kurocat-probe-{}.txt", std::process::id());
    dav.put(&probe, b"kurocat".to_vec())?;
    let _ = dav.delete(&probe);
    Ok(format!("连接成功：{}", cfg.server))
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::CategoryDef;

    fn mem_meta() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(recorder::META_SCHEMA).unwrap();
        conn
    }

    fn sample_pack(machine_id: &str) -> MergePack {
        let mut p = MergePack::empty(machine_id);
        // 生产环境的包由 build_pack 填 export_ts；测试里手动对齐事件的最大 ts
        p.export_ts = 1_700_000_000_000;
        p.apps = vec![PackApp { id: 5, process: "game.exe".to_string() }];
        p.events = vec![PackEvent {
            ts: 1_700_000_000_000,
            kind: recorder::KIND_KEY_DOWN,
            code: 65,
            flags: 0,
            x: None,
            y: None,
            app_id: Some(5),
        }];
        p.stats = vec![PackRowStats {
            day: "2026-11-25".to_string(),
            kind: recorder::KIND_KEY_DOWN,
            code: 65,
            n: 100,
        }];
        p
    }

    #[test]
    fn pack_json_roundtrip() {
        let p = sample_pack("abc123");
        let bytes = serde_json::to_vec(&p).unwrap();
        let back: MergePack = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(back.machine_id, "abc123");
        assert_eq!(back.events.len(), 1);
        assert_eq!(back.apps[0].process, "game.exe");
    }

    #[test]
    fn import_is_idempotent() {
        let meta = mem_meta();
        let events = Connection::open_in_memory().unwrap();
        let settings = Arc::new(Mutex::new(AppSettings::default()));
        let pack = sample_pack("src1");

        let merged1 =
            import_pack(&pack, "src1", "p1.json", &meta, Some(&events), None, &settings).unwrap();
        let merged2 =
            import_pack(&pack, "src1", "p1.json", &meta, Some(&events), None, &settings).unwrap();
        // 示例包不带分类，两次导入都不应产生分类变化
        assert!(!merged1 && !merged2);

        let n: i64 = meta
            .query_row("SELECT n FROM daily_stats", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 100, "同一个包导两次，计数不能翻倍");

        let table = recorder::month_table_name(&recorder::month_of(1_700_000_000_000));
        let cnt: i64 = events
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
            .unwrap();
        assert_eq!(cnt, 1, "事件明细只应插入一次");
    }

    #[test]
    fn app_mapping_and_later_local_record_join() {
        let meta = mem_meta();
        let events = Connection::open_in_memory().unwrap();
        let settings = Arc::new(Mutex::new(AppSettings::default()));
        let pack = sample_pack("src1");
        import_pack(&pack, "src1", "p1.json", &meta, Some(&events), None, &settings).unwrap();

        // 本机 apps 里应出现按进程名新建的行
        let local_id = local_app_id(&meta, "game.exe").expect("应已建 game.exe");
        // 事件明细挂到了本机 app_id 上，且带 src 标记
        let table = recorder::month_table_name(&recorder::month_of(1_700_000_000_000));
        let (row_app, row_src): (Option<i64>, Option<String>) = events
            .query_row(
                &format!("SELECT app_id, src FROM {table}"),
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(row_app, Some(local_id));
        assert_eq!(row_src.as_deref(), Some("src1"));

        // 模拟「后来装了这个软件」：recorder 按 process upsert，必然命中同一行
        let again = local_app_id(&meta, "game.exe").unwrap();
        assert_eq!(again, local_id, "后装软件后按进程名应命中同一行");
    }

    #[test]
    fn grow_delta_applies_only_difference() {
        let meta = mem_meta();
        let events = Connection::open_in_memory().unwrap();
        let settings = Arc::new(Mutex::new(AppSettings::default()));
        let mut pack = sample_pack("src1");
        import_pack(&pack, "src1", "p1.json", &meta, Some(&events), None, &settings).unwrap();

        // 对方机器当天又敲了 50 次：n 从 100 涨到 150
        pack.stats[0].n = 150;
        import_pack(&pack, "src1", "p2.json", &meta, Some(&events), None, &settings).unwrap();
        let n: i64 = meta
            .query_row("SELECT n FROM daily_stats", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 150, "第二次只应累加差值 50");

        // 登记表应该有两条
        let cnt: i64 = meta
            .query_row("SELECT COUNT(*) FROM imported_packs", [], |r| r.get(0))
            .unwrap();
        assert_eq!(cnt, 2);
    }

    #[test]
    fn categories_union_merge_respects_local() {
        let meta = mem_meta();
        let events = Connection::open_in_memory().unwrap();
        let settings = Arc::new(Mutex::new(AppSettings::default()));
        let mut pack = sample_pack("src1");

        // 对方带一个分类与一条映射
        let game = CategoryDef {
            id: "game".into(),
            name: "对手游戏".into(),
            icon: "🎮".into(),
            color: "#ff0000".into(),
            parent_id: None,
            builtin: false,
        };
        pack.categories =
            serde_json::to_value(vec![game]).unwrap().as_array().cloned().unwrap();
        pack.app_categories = vec![("game.exe".to_string(), "game".to_string())];

        import_pack(&pack, "src1", "p1.json", &meta, Some(&events), None, &settings).unwrap();
        {
            let s = settings.lock().unwrap();
            assert!(s.categories.iter().any(|c| c.id == "game"));
            assert_eq!(
                s.app_categories.get("game.exe").map(String::as_str),
                Some("game")
            );
        }

        // 本机后来把 game.exe 归到了别的分类：对方的旧映射不得覆盖。
        // 注意 id 不能撞默认分类（work/game 等已存在），否则 normalize 去重
        // 会被误判成「分类有变化」。
        {
            let mut s = settings.lock().unwrap();
            s.categories.push(CategoryDef {
                id: "study".into(),
                name: "学习".into(),
                icon: "📚".into(),
                color: "#0000ff".into(),
                parent_id: None,
                builtin: false,
            });
            s.app_categories.insert("game.exe".into(), "study".into());
        }
        let merged =
            import_pack(&pack, "src1", "p2.json", &meta, Some(&events), None, &settings).unwrap();
        let s = settings.lock().unwrap();
        assert_eq!(
            s.app_categories.get("game.exe").map(String::as_str),
            Some("study")
        );
        assert!(!merged, "本机优先时再次导入不应再产生分类变化");
    }

    #[test]
    fn app_overrides_union_merge_respects_local() {
        let meta = mem_meta();
        let events = Connection::open_in_memory().unwrap();
        let settings = Arc::new(Mutex::new(AppSettings::default()));
        let mut pack = sample_pack("src1");
        pack.app_display_names = vec![("chrome.exe".to_string(), "谷歌浏览器".to_string())];
        pack.app_ignored = vec![("spotify.exe".to_string(), true)];

        import_pack(&pack, "src1", "p1.json", &meta, Some(&events), None, &settings).unwrap();
        {
            let s = settings.lock().unwrap();
            assert_eq!(
                s.app_display_names.get("chrome.exe").map(String::as_str),
                Some("谷歌浏览器")
            );
            assert_eq!(s.app_ignored.get("spotify.exe"), Some(&true));
        }

        // 本机已改过显示名：对方的旧名不得覆盖，且再次导入不应再报「有变化」
        {
            let mut s = settings.lock().unwrap();
            s.app_display_names
                .insert("chrome.exe".to_string(), "我的浏览器".to_string());
        }
        let merged =
            import_pack(&pack, "src1", "p2.json", &meta, Some(&events), None, &settings).unwrap();
        let s = settings.lock().unwrap();
        assert_eq!(
            s.app_display_names.get("chrome.exe").map(String::as_str),
            Some("我的浏览器")
        );
        assert!(!merged, "本机优先时再次导入不应再产生标注变化");
    }

    #[test]
    fn propfind_parse_handles_namespaces_and_closing_tags() {
        let xml = r#"<?xml version="1.0"?>
<D:multistatus xmlns:D="DAV:">
  <D:response><D:href>/dav/KuroCatSync/</D:href></D:response>
  <D:response><D:href>/dav/KuroCatSync/abcd1234/</D:href></D:response>
  <D:response><D:href>/dav/KuroCatSync/abcd1234/0001234567890_000001_abcd1234.json</D:href></D:response>
</D:multistatus>"#;
        let hrefs = parse_propfind_hrefs(xml);
        assert_eq!(hrefs.len(), 3, "闭合标签不应被当成值：{}", hrefs.len());
        assert!(hrefs[0].ends_with('/'));
    }

    #[test]
    fn percent_decode_works() {
        assert_eq!(percent_decode("a%20b"), "a b");
        assert_eq!(percent_decode("%E7%8C%AB"), "猫");
        assert_eq!(percent_decode("plain.json"), "plain.json");
        assert_eq!(percent_decode("bad%2"), "bad%2");
    }

    #[test]
    fn day_before_rolls_over() {
        assert_eq!(day_before("2026-11-01"), "2026-10-31");
        assert_eq!(day_before("2026-03-01"), "2026-02-28");
        assert_eq!(day_before("garbage"), "garbage");
    }

    #[test]
    fn pack_name_is_sortable_and_parseable() {
        let name = pack_name(1_700_000_000_000, 42, "abcd1234abcd1234");
        assert_eq!(name, "1700000000000_000042_abcd1234abcd1234.json");
        let ts: i64 = name[..13].parse().unwrap();
        assert_eq!(ts, 1_700_000_000_000);
    }

    #[test]
    fn ensure_src_column_is_idempotent() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE t (ts INTEGER NOT NULL);",
        )
        .unwrap();
        ensure_src_column(&conn, "t").unwrap();
        ensure_src_column(&conn, "t").unwrap();
        conn.execute("INSERT INTO t(ts, src) VALUES(1, 'x')", [])
            .unwrap();
    }
}
