//! 输入记录器：把「键盘按下」与「鼠标点击」落到本地 SQLite。
//!
//! # 三条硬约束（代码评审按此卡）
//!
//! 1. **本模块的任何函数都不得从低级钩子回调线程调用。**
//!    `WH_KEYBOARD_LL` 回调超过 `LowLevelHooksTimeout`（注册表默认 300 ms）不返回，
//!    Windows 会**静默摘掉钩子**——进程还活着、猫还在动，但再也不响应按键，且用户
//!    毫不知情。所以 [`push`] 只做一次 `mpsc::send`，所有 IO 都在 recorder 线程。
//! 2. **MCP 工具表里一行都不许加**读取或导出本模块数据的能力。项目有两个本地监听
//!    （MCP 127.0.0.1:27420、OBS 127.0.0.1:27421），一旦它们能读走日志，就从「改外观」
//!    升级成「导出全部输入历史」的通道。
//! 3. 日志目录硬编码为 `app_data_dir()/input-log`，**不进配置文件的任何可写字段**，
//!    避免被 `update_settings` 改到 OBS 可服务的目录下。
//!
//! # 记录什么
//!
//! - 键盘：一次物理按下一条（`repeat` 长按重复与全部抬起事件在入口丢弃）
//! - 鼠标：左/右/中键**按下**一条，带按下时的屏幕坐标
//! - **不记录**鼠标移动、滚轮、窗口标题、剪贴板、任何文本内容
//!
//! 猫咪动画完全不受影响：`input_hook` 照旧把移动与滚轮 `emit` 给前端。

use chrono::{Datelike, Local, TimeZone, Timelike};
use rusqlite::{params, Connection, OpenFlags};
use serde::Serialize;
use std::collections::HashMap;
use std::fs;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::OnceLock;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const KIND_KEY_DOWN: i32 = 0;
pub const KIND_MOUSE_DOWN: i32 = 1;

/// `flags` 位定义（与方案第二节一致）。
pub const FLAG_SHIFT: i32 = 1 << 1;
pub const FLAG_CAPS: i32 = 1 << 2;
pub const FLAG_INJECTED: i32 = 1 << 3;

/// 批量提交间隔。与 `PRAGMA wal_autocheckpoint = 32` 配套，把断电窗口压到约 0.5 s。
const FLUSH_INTERVAL: Duration = Duration::from_millis(500);
/// 缓冲上限，达到即提前提交。
const FLUSH_BATCH: usize = 500;
/// 隐私状态翻转前后各丢弃这么多毫秒（补 `privacy.rs` 140 ms 轮询的时间窗）。
const PRIVACY_GUARD_MS: i64 = 300;
/// 前台进程名缓存时长，避免每个键都走一遍 `OpenProcess`。
const APP_CACHE_MS: i64 = 200;
/// 隐私窗口最多保留几段，防止长时间运行后无限增长。
const PRIVACY_RANGES_MAX: usize = 8;

const META_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS apps (
    id      INTEGER PRIMARY KEY,
    process TEXT NOT NULL UNIQUE
);
CREATE TABLE IF NOT EXISTS daily_stats (
    day  TEXT    NOT NULL,
    kind INTEGER NOT NULL,
    code INTEGER NOT NULL,
    n    INTEGER NOT NULL,
    PRIMARY KEY (day, kind, code)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS daily_hours (
    day       TEXT    NOT NULL,
    hour      INTEGER NOT NULL,
    keydown   INTEGER NOT NULL DEFAULT 0,
    mousedown INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (day, hour)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS daily_apps (
    day            TEXT    NOT NULL,
    app_id         INTEGER NOT NULL,
    kind           INTEGER NOT NULL,
    n              INTEGER NOT NULL DEFAULT 0,
    active_seconds INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (day, app_id, kind)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS monthly_app_keys (
    month  TEXT    NOT NULL,
    app_id INTEGER NOT NULL,
    code   INTEGER NOT NULL,
    n      INTEGER NOT NULL,
    PRIMARY KEY (month, app_id, code)
) WITHOUT ROWID;
"#;

// ---------------------------------------------------------------------------
// 线程间消息
// ---------------------------------------------------------------------------

struct RawRecord {
    ts: i64,
    kind: i32,
    code: i32,
    flags: i32,
    x: Option<i32>,
    y: Option<i32>,
}

enum Msg {
    Rec(RawRecord),
    /// 隐私状态翻转。`active=true` = 刚进入密码框。
    PrivacyFlip { active: bool, ts: i64 },
    /// 立即提交（正常退出前调用）。
    Flush,
    /// 关闭连接 → 删除整个日志目录 → 重新打开。
    Wipe { reply: Sender<Result<(), String>> },
    /// 在 recorder 线程执行保留期清理，避免与写入抢锁。
    Retention {
        days: u32,
        reply: Sender<Result<RetentionReport, String>>,
    },
    /// 调整活跃判定阈值（分钟）。
    SetGap(u32),
}

static TX: OnceLock<Sender<Msg>> = OnceLock::new();
static RECORDING: AtomicBool = AtomicBool::new(false);
static STARTED: AtomicBool = AtomicBool::new(false);
/// 活跃判定阈值的毫秒值。线程启动前就要能读到，所以单独放一个原子量。
static GAP_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(300_000);

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// 日志目录。硬编码，不读配置——见模块注释第 3 条。
pub fn log_dir() -> PathBuf {
    crate::config::app_data_dir().join("input-log")
}

fn events_db_path() -> PathBuf {
    log_dir().join("events.db")
}

fn meta_db_path() -> PathBuf {
    log_dir().join("meta.db")
}

// ---------------------------------------------------------------------------
// 对外入口
// ---------------------------------------------------------------------------

/// 投递一条待记录事件。**只允许在 dispatcher 线程调用**（不是钩子回调线程）。
pub fn push(kind: i32, code: i32, flags: i32, x: Option<i32>, y: Option<i32>) {
    if !RECORDING.load(Ordering::Relaxed) {
        return;
    }
    if let Some(tx) = TX.get() {
        // 无界 channel：send 永不阻塞，绝不反压到输入链路
        let _ = tx.send(Msg::Rec(RawRecord {
            ts: now_ms(),
            kind,
            code,
            flags,
            x,
            y,
        }));
    }
}

/// 隐私状态翻转时调用（`privacy.rs` 的 watcher 线程）。
pub fn mark_privacy_flip(active: bool) {
    if let Some(tx) = TX.get() {
        let _ = tx.send(Msg::PrivacyFlip {
            active,
            ts: now_ms(),
        });
    }
}

/// 启动记录线程。默认关闭时**不创建任何文件**，等用户真正开启再建库——
/// 「没开过就一个字节都不落盘」比「开了但写了空表」干净。
pub fn init(enabled: bool, gap_minutes: u32) {
    GAP_MS.store(
        gap_minutes.clamp(1, 120) as u64 * 60_000,
        Ordering::SeqCst,
    );
    RECORDING.store(enabled, Ordering::SeqCst);
    if enabled {
        start_thread();
    }
}

fn start_thread() {
    if STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    let (tx, rx) = mpsc::channel::<Msg>();
    let _ = TX.set(tx);
    let dir = log_dir();
    let gap = (GAP_MS.load(Ordering::SeqCst) / 60_000).max(1) as u32;
    let spawned = std::thread::Builder::new()
        .name("kuro-input-recorder".to_string())
        .spawn(move || match Recorder::open(&dir, gap) {
            Ok(mut rec) => rec.run(rx),
            Err(e) => eprintln!("[recorder] 初始化失败，输入记录不可用: {e}"),
        });
    if spawned.is_err() {
        eprintln!("[recorder] 无法创建记录线程");
    }
}

/// 运行中切换记录开关。第一次开启时才真正建库、起线程。
pub fn set_enabled(enabled: bool) {
    RECORDING.store(enabled, Ordering::SeqCst);
    if enabled {
        start_thread();
    }
}

pub fn is_recording() -> bool {
    RECORDING.load(Ordering::Relaxed)
}

pub fn set_gap_minutes(minutes: u32) {
    GAP_MS.store(
        minutes.clamp(1, 120) as u64 * 60_000,
        Ordering::SeqCst,
    );
    if let Some(tx) = TX.get() {
        let _ = tx.send(Msg::SetGap(minutes));
    }
}

/// 正常退出前调用：把缓冲里未提交的记录刷盘，最多等 2 秒。
pub fn shutdown() {
    RECORDING.store(false, Ordering::SeqCst);
    if let Some(tx) = TX.get() {
        let _ = tx.send(Msg::Flush);
    }
    // 线程会在 500 ms 周期内完成 flush；这里只给一点余量，不做 join（避免卡住退出流程）
    std::thread::sleep(Duration::from_millis(120));
}

// ---------------------------------------------------------------------------
// 时间换算
// ---------------------------------------------------------------------------

/// ts → (本地日期 "YYYY-MM-DD", 本地小时)。
///
/// 换算放在 Rust 侧而不是 SQL 的 `'localtime'` 修饰符：后者依赖运行环境时区、
/// 受 `TZ` 影响，不同机器会给出不同结果。
fn day_hour_local(ts_ms: i64) -> (String, u32) {
    match Local.timestamp_millis_opt(ts_ms).single() {
        Some(dt) => (
            format!("{:04}-{:02}-{:02}", dt.year(), dt.month(), dt.day()),
            dt.hour(),
        ),
        None => ("1970-01-01".to_string(), 0),
    }
}

fn month_of(ts_ms: i64) -> String {
    let (day, _) = day_hour_local(ts_ms);
    day[..7].to_string()
}

/// 只接受 6 位纯数字的月份，任何其它输入一律拒绝——这是拼表名前的白名单校验。
fn is_valid_month(m: &str) -> bool {
    m.len() == 6 && m.bytes().all(|b| b.is_ascii_digit())
}

fn month_table_name(month: &str) -> String {
    format!("events_{month}")
}

// ---------------------------------------------------------------------------
// 内存累加器
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Accum {
    daily_stats: HashMap<(String, i32, i32), i64>,
    daily_hours: HashMap<(String, u32), [i64; 2]>,
    daily_apps: HashMap<(String, i64, i32), (i64, i64)>,
    monthly_app_keys: HashMap<(String, i64, i32), i64>,
}

#[derive(Clone, Copy)]
struct BufRow {
    ts: i64,
    kind: i32,
    code: i32,
    flags: i32,
    x: Option<i32>,
    y: Option<i32>,
    app_id: i64,
}

// ---------------------------------------------------------------------------
// recorder 线程
// ---------------------------------------------------------------------------

struct Recorder {
    events: Connection,
    meta: Connection,
    buf: Vec<BufRow>,
    pending: Accum,
    last_ts_by_app: HashMap<i64, i64>,
    app_cache: Option<(i64, i64)>,
    privacy_ranges: Vec<(i64, i64)>,
    privacy_open: bool,
    gap_ms: i64,
    dir: PathBuf,
}

impl Recorder {
    fn open(dir: &Path, gap_minutes: u32) -> Result<Self, String> {
        fs::create_dir_all(dir).map_err(|e| format!("创建日志目录失败: {e}"))?;
        let events = Self::open_events(&dir.join("events.db"))?;
        let meta = Self::open_meta(&dir.join("meta.db"))?;
        Ok(Self {
            events,
            meta,
            buf: Vec::with_capacity(FLUSH_BATCH),
            pending: Accum::default(),
            last_ts_by_app: HashMap::new(),
            app_cache: None,
            privacy_ranges: Vec::new(),
            privacy_open: false,
            gap_ms: gap_minutes.clamp(1, 120) as i64 * 60_000,
            dir: dir.to_path_buf(),
        })
    }

    fn open_events(path: &Path) -> Result<Connection, String> {
        let conn = Connection::open(path).map_err(|e| format!("打开 events.db 失败: {e}"))?;
        conn.busy_timeout(Duration::from_millis(3000))
            .map_err(|e| e.to_string())?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA wal_autocheckpoint = 32;
             PRAGMA auto_vacuum = INCREMENTAL;",
        )
        .map_err(|e| format!("配置 events.db 失败: {e}"))?;
        Ok(conn)
    }

    fn open_meta(path: &Path) -> Result<Connection, String> {
        let conn = Connection::open(path).map_err(|e| format!("打开 meta.db 失败: {e}"))?;
        conn.busy_timeout(Duration::from_millis(3000))
            .map_err(|e| e.to_string())?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA wal_autocheckpoint = 64;",
        )
        .map_err(|e| format!("配置 meta.db 失败: {e}"))?;
        conn.execute_batch(META_SCHEMA)
            .map_err(|e| format!("建汇总表失败: {e}"))?;
        Ok(conn)
    }

    fn run(&mut self, rx: Receiver<Msg>) {
        let mut last_flush = Instant::now();
        loop {
            let wait = FLUSH_INTERVAL.saturating_sub(last_flush.elapsed());
            match rx.recv_timeout(wait) {
                Ok(Msg::Rec(r)) => {
                    self.ingest(r);
                    if self.buf.len() >= FLUSH_BATCH {
                        self.flush();
                        last_flush = Instant::now();
                    }
                }
                Ok(Msg::PrivacyFlip { active, ts }) => self.apply_privacy_flip(active, ts),
                Ok(Msg::Flush) => {
                    self.flush();
                    last_flush = Instant::now();
                }
                Ok(Msg::SetGap(minutes)) => {
                    self.gap_ms = minutes.clamp(1, 120) as i64 * 60_000;
                }
                Ok(Msg::Wipe { reply }) => {
                    let result = self.wipe();
                    let _ = reply.send(result);
                }
                Ok(Msg::Retention { days, reply }) => {
                    let result = self.apply_retention(days);
                    let _ = reply.send(result);
                }
                Err(RecvTimeoutError::Timeout) => {
                    self.flush();
                    last_flush = Instant::now();
                }
                Err(RecvTimeoutError::Disconnected) => {
                    self.flush();
                    break;
                }
            }
        }
    }

    // -- 隐私窗口 ----------------------------------------------------------

    fn apply_privacy_flip(&mut self, active: bool, ts: i64) {
        if active && !self.privacy_open {
            self.privacy_ranges.push((ts - PRIVACY_GUARD_MS, i64::MAX));
            self.privacy_open = true;
            if self.privacy_ranges.len() > PRIVACY_RANGES_MAX {
                self.privacy_ranges.remove(0);
            }
        } else if !active && self.privacy_open {
            if let Some(last) = self.privacy_ranges.last_mut() {
                last.1 = ts + PRIVACY_GUARD_MS;
            }
            self.privacy_open = false;
        } else {
            return;
        }
        // 翻转之前已经进入缓冲的记录需要回头清掉
        self.drop_private_rows();
    }

    // -- 摄入 --------------------------------------------------------------

    /// 缓冲里落在隐私窗口内的行，全部丢弃。宁可少记 0.3 秒，不能把密码开头几个字母漏进日志。
    fn drop_private_rows(&mut self) {
        if self.privacy_ranges.is_empty() {
            return;
        }
        let ranges = std::mem::take(&mut self.privacy_ranges);
        self.buf
            .retain(|r| !ranges.iter().any(|(s, e)| r.ts >= *s && r.ts <= *e));
        self.privacy_ranges = ranges;
    }

    fn ingest(&mut self, r: RawRecord) {
        let app_id = self.resolve_app_id(r.ts);
        self.account_active(r.ts, app_id);

        let (day, hour) = day_hour_local(r.ts);
        let month = day[..7].to_string();

        *self
            .pending
            .daily_stats
            .entry((day.clone(), r.kind, r.code))
            .or_insert(0) += 1;

        let hour_bucket = self
            .pending
            .daily_hours
            .entry((day.clone(), hour))
            .or_insert([0, 0]);
        if r.kind == KIND_KEY_DOWN {
            hour_bucket[0] += 1;
        } else {
            hour_bucket[1] += 1;
        }

        self.pending
            .daily_apps
            .entry((day, app_id, r.kind))
            .or_insert((0, 0))
            .0 += 1;

        if r.kind == KIND_KEY_DOWN {
            *self
                .pending
                .monthly_app_keys
                .entry((month, app_id, r.code))
                .or_insert(0) += 1;
        }

        self.buf.push(BufRow {
            ts: r.ts,
            kind: r.kind,
            code: r.code,
            flags: r.flags,
            x: r.x,
            y: r.y,
            app_id,
        });
    }

    /// 会话式活跃统计：与同一程序内上一次输入的间隔 ≤ 阈值时，把这段间隔算作活跃。
    /// O(1)，不排序不回溯。单独按一下就走记 0 秒——这是算法的固有特性，见方案 374 行。
    fn account_active(&mut self, ts: i64, app_id: i64) {
        if let Some(&last) = self.last_ts_by_app.get(&app_id) {
            let gap = ts - last;
            if gap > 0 && gap <= self.gap_ms {
                let (day, _) = day_hour_local(ts);
                let slot = self
                    .pending
                    .daily_apps
                    .entry((day, app_id, KIND_KEY_DOWN))
                    .or_insert((0, 0));
                slot.1 += gap;
            }
        }
        self.last_ts_by_app.insert(app_id, ts);
    }

    fn resolve_app_id(&mut self, now: i64) -> i64 {
        if let Some((cached_at, id)) = self.app_cache {
            if now - cached_at <= APP_CACHE_MS {
                return id;
            }
        }
        let id = match foreground_process() {
            Some(name) => self.upsert_app(&name).unwrap_or(-1),
            None => -1,
        };
        self.app_cache = Some((now, id));
        id
    }

    fn upsert_app(&self, process: &str) -> rusqlite::Result<i64> {
        self.meta
            .execute("INSERT OR IGNORE INTO apps(process) VALUES(?1)", params![process])?;
        self.meta.query_row(
            "SELECT id FROM apps WHERE process = ?1",
            params![process],
            |row| row.get(0),
        )
    }

    // -- 提交 --------------------------------------------------------------

    fn flush(&mut self) {
        // 隐私兜底：翻转之前进来、还没提交的那批再滤一次（缓冲 ≤ 500 行，成本可忽略）
        self.drop_private_rows();
        if self.buf.is_empty() {
            return;
        }

        let acc = std::mem::take(&mut self.pending);
        match self.write_events() {
            Ok(()) => {
                // 明细已落地，汇总失败只影响聚合，不影响原始数据完整性
                if let Err(e) = self.write_aggregates(&acc) {
                    eprintln!("[recorder] 写汇总失败（原始明细已保存）: {e}");
                }
                self.buf.clear();
            }
            Err(e) => {
                eprintln!("[recorder] 写明细失败，保留缓冲待重试: {e}");
                self.pending = acc;
                // 缓冲超限时丢弃最旧的一半，避免内存无限增长
                if self.buf.len() > FLUSH_BATCH * 8 {
                    let drop_n = self.buf.len() / 2;
                    self.buf.drain(0..drop_n);
                }
            }
        }
    }

    fn write_events(&mut self) -> Result<(), String> {
        let tx = self.events.transaction().map_err(|e| e.to_string())?;
        let mut by_month: HashMap<String, Vec<usize>> = HashMap::new();
        for (i, row) in self.buf.iter().enumerate() {
            by_month.entry(month_of(row.ts)).or_default().push(i);
        }
        for (month, idxs) in by_month {
            if !is_valid_month(&month) {
                continue;
            }
            ensure_month_table(&tx, &month).map_err(|e| e.to_string())?;
            let sql = format!(
                "INSERT INTO {} (ts, kind, code, flags, x, y, app_id) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                month_table_name(&month)
            );
            let mut stmt = tx.prepare(&sql).map_err(|e| e.to_string())?;
            for i in idxs {
                let row = self.buf[i];
                stmt.execute(params![
                    row.ts,
                    row.kind,
                    row.code,
                    row.flags,
                    row.x,
                    row.y,
                    if row.app_id < 0 {
                        None
                    } else {
                        Some(row.app_id)
                    },
                ])
                .map_err(|e| e.to_string())?;
            }
        }
        tx.commit().map_err(|e| e.to_string())
    }

    fn write_aggregates(&self, acc: &Accum) -> Result<(), String> {
        let tx = self.meta.unchecked_transaction().map_err(|e| e.to_string())?;
        {
            let mut stats = tx
                .prepare(
                    "INSERT INTO daily_stats(day, kind, code, n) VALUES(?1, ?2, ?3, ?4) \
                     ON CONFLICT(day, kind, code) DO UPDATE SET n = n + excluded.n",
                )
                .map_err(|e| e.to_string())?;
            for ((day, kind, code), n) in &acc.daily_stats {
                stats
                    .execute(params![day, kind, code, n])
                    .map_err(|e| e.to_string())?;
            }

            let mut hours = tx
                .prepare(
                    "INSERT INTO daily_hours(day, hour, keydown, mousedown) VALUES(?1, ?2, ?3, ?4) \
                     ON CONFLICT(day, hour) DO UPDATE SET \
                     keydown = keydown + excluded.keydown, \
                     mousedown = mousedown + excluded.mousedown",
                )
                .map_err(|e| e.to_string())?;
            for ((day, hour), [kd, md]) in &acc.daily_hours {
                hours
                    .execute(params![day, hour, kd, md])
                    .map_err(|e| e.to_string())?;
            }

            let mut apps = tx
                .prepare(
                    "INSERT INTO daily_apps(day, app_id, kind, n, active_seconds) \
                     VALUES(?1, ?2, ?3, ?4, ?5) \
                     ON CONFLICT(day, app_id, kind) DO UPDATE SET \
                     n = n + excluded.n, active_seconds = active_seconds + excluded.active_seconds",
                )
                .map_err(|e| e.to_string())?;
            for ((day, app_id, kind), (n, active_ms)) in &acc.daily_apps {
                apps.execute(params![day, app_id, kind, n, active_ms / 1000])
                    .map_err(|e| e.to_string())?;
            }

            let mut app_keys = tx
                .prepare(
                    "INSERT INTO monthly_app_keys(month, app_id, code, n) VALUES(?1, ?2, ?3, ?4) \
                     ON CONFLICT(month, app_id, code) DO UPDATE SET n = n + excluded.n",
                )
                .map_err(|e| e.to_string())?;
            for ((month, app_id, code), n) in &acc.monthly_app_keys {
                app_keys
                    .execute(params![month, app_id, code, n])
                    .map_err(|e| e.to_string())?;
            }
        }
        tx.commit().map_err(|e| e.to_string())
    }

    // -- 清空 / 保留期 -----------------------------------------------------

    fn wipe(&mut self) -> Result<(), String> {
        // Windows 上被打开的 db 文件删不掉，必须先换成一个空连接再删目录
        let was_recording = RECORDING.swap(false, Ordering::SeqCst);
        let placeholder = Connection::open_in_memory().map_err(|e| e.to_string())?;
        let old_events = std::mem::replace(&mut self.events, placeholder);
        let old_meta = std::mem::replace(
            &mut self.meta,
            Connection::open_in_memory().map_err(|e| e.to_string())?,
        );
        drop(old_events);
        drop(old_meta);
        self.buf.clear();
        self.pending = Accum::default();
        self.last_ts_by_app.clear();
        self.privacy_ranges.clear();
        self.privacy_open = false;
        self.app_cache = None;

        let result = fs::remove_dir_all(&self.dir).map_err(|e| format!("删除日志目录失败: {e}"));
        // 无论删除是否成功都要把连接恢复出来，否则记录功能彻底停摆
        self.events = Self::open_events(&events_db_path())?;
        self.meta = Self::open_meta(&meta_db_path())?;
        RECORDING.store(was_recording, Ordering::SeqCst);
        result
    }

    fn apply_retention(&mut self, days: u32) -> Result<RetentionReport, String> {
        if days == 0 {
            return Ok(RetentionReport::default());
        }
        self.flush();
        let cutoff_ts = now_ms() - days as i64 * 86_400_000;
        let cutoff_month = month_of(cutoff_ts);
        let cutoff_day = day_hour_local(cutoff_ts).0;

        let months = list_event_months(&self.events)?;
        let mut dropped_months = Vec::new();
        let mut deleted_rows: i64 = 0;

        for month in months {
            if !is_valid_month(&month) {
                continue;
            }
            let table = month_table_name(&month);
            if month < cutoff_month {
                deleted_rows += count_rows(&self.events, &table).unwrap_or(0);
                self.events
                    .execute_batch(&format!("DROP TABLE IF EXISTS {table};"))
                    .map_err(|e| format!("删除 {table} 失败: {e}"))?;
                dropped_months.push(month);
            } else if month == cutoff_month {
                let n = self
                    .events
                    .execute(
                        &format!("DELETE FROM {table} WHERE ts < ?1"),
                        params![cutoff_ts],
                    )
                    .map_err(|e| format!("清理 {table} 边界失败: {e}"))?;
                deleted_rows += n as i64;
            }
        }

        if !dropped_months.is_empty() {
            let _ = self.events.execute_batch("PRAGMA incremental_vacuum;");
        }

        self.meta
            .execute("DELETE FROM daily_stats WHERE day < ?1", params![cutoff_day])
            .map_err(|e| e.to_string())?;
        self.meta
            .execute("DELETE FROM daily_hours WHERE day < ?1", params![cutoff_day])
            .map_err(|e| e.to_string())?;
        self.meta
            .execute("DELETE FROM daily_apps WHERE day < ?1", params![cutoff_day])
            .map_err(|e| e.to_string())?;
        self.meta
            .execute(
                "DELETE FROM monthly_app_keys WHERE month < ?1",
                params![cutoff_month],
            )
            .map_err(|e| e.to_string())?;

        Ok(RetentionReport {
            cutoff_day,
            deleted_rows,
            dropped_months,
        })
    }
}

fn ensure_month_table(conn: &Connection, month: &str) -> rusqlite::Result<()> {
    let table = month_table_name(month);
    conn.execute_batch(&format!(
        "CREATE TABLE IF NOT EXISTS {table} (
            ts     INTEGER NOT NULL,
            kind   INTEGER NOT NULL,
            code   INTEGER NOT NULL,
            flags  INTEGER NOT NULL,
            x      INTEGER,
            y      INTEGER,
            app_id INTEGER
        );
        CREATE INDEX IF NOT EXISTS idx_{table}_ts ON {table}(ts);"
    ))
}

fn list_event_months(conn: &Connection) -> Result<Vec<String>, String> {
    let mut stmt = conn
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table' AND name LIKE 'events\\_%' ESCAPE '\\'")
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    for r in rows.flatten() {
        if let Some(month) = r.strip_prefix("events_") {
            if is_valid_month(month) {
                out.push(month.to_string());
            }
        }
    }
    out.sort();
    Ok(out)
}

fn count_rows(conn: &Connection, table: &str) -> rusqlite::Result<i64> {
    conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
}

// ---------------------------------------------------------------------------
// 前台进程名（只取 basename，全程不碰窗口标题）
// ---------------------------------------------------------------------------

/// `GetForegroundWindow` → `GetWindowThreadProcessId` → `QueryFullProcessImageNameW` → basename。
///
/// **一次都不调用 `GetWindowTextW`**，所以文档名、网页标题、聊天对象不会进入内存，
/// 更不会落盘。这是可以逐行核对的隐私承诺。
#[cfg(windows)]
fn foreground_process() -> Option<String> {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetForegroundWindow, GetWindowThreadProcessId,
    };

    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.is_null() {
            return None;
        }
        let mut pid: u32 = 0;
        GetWindowThreadProcessId(hwnd, &mut pid);
        if pid == 0 {
            return None;
        }
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            return None;
        }
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(handle, 0, buf.as_mut_ptr(), &mut len);
        CloseHandle(handle);
        if ok == 0 || len == 0 {
            return None;
        }
        let path = String::from_utf16_lossy(&buf[..len as usize]);
        let name = path
            .rsplit(|c| c == '\\' || c == '/')
            .next()
            .unwrap_or("")
            .to_string();
        if name.is_empty() {
            None
        } else {
            Some(name)
        }
    }
}

/// macOS 侧暂不解析前台应用（需 `objc2-app-kit` 的 `NSWorkspace` feature，本机无法验证编译）。
/// 此路径下 `app_id` 记为 NULL，按键与点击本身照常记录，只缺「按应用分组」这一维。
#[cfg(not(windows))]
fn foreground_process() -> Option<String> {
    None
}

// ---------------------------------------------------------------------------
// ---------------------------------------------------------------------------
// 查询连接（UI 线程，独立于写连接——绝不与 recorder 线程共享）
//
// 这里用 READ_WRITE 而不是 READ_ONLY：WAL 模式下的只读连接需要能访问 `-shm`
// 文件，纯 READ_ONLY 在写端刚启动/刚关闭时容易报 "unable to open database file",
// 实际也不写任何东西（没有任何 INSERT/UPDATE）。真正的隔离靠「独立连接」实现：
// WAL 下读写互不阻塞，一次「查 10 年」的长查询不会把写入卡住。
// ---------------------------------------------------------------------------

fn open_meta_readonly() -> Result<Connection, String> {
    let path = meta_db_path();
    if !path.exists() {
        return Err("尚无记录".to_string());
    }
    let conn = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_WRITE)
        .map_err(|e| format!("打开 meta.db 失败: {e}"))?;
    conn.busy_timeout(Duration::from_millis(2000))
        .map_err(|e| e.to_string())?;
    Ok(conn)
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LogStatus {
    pub enabled: bool,
    pub paused: bool,
    pub recording: bool,
    pub log_dir: String,
    pub total_bytes: u64,
    pub total_events: i64,
    pub today_keydown: i64,
    pub today_mousedown: i64,
    pub active_seconds_today: i64,
    pub first_day: Option<String>,
    pub last_day: Option<String>,
    pub month_tables: usize,
    pub app_count: i64,
    pub retention_days: u32,
    pub active_gap_minutes: u32,
}

pub fn status(enabled: bool, paused: bool, retention_days: u32, gap_minutes: u32) -> LogStatus {
    let mut st = LogStatus {
        enabled,
        paused,
        recording: is_recording() && enabled && !paused,
        log_dir: log_dir().to_string_lossy().to_string(),
        total_bytes: dir_size(&log_dir()),
        total_events: 0,
        today_keydown: 0,
        today_mousedown: 0,
        active_seconds_today: 0,
        first_day: None,
        last_day: None,
        month_tables: 0,
        app_count: 0,
        retention_days,
        active_gap_minutes: gap_minutes,
    };

    let today = day_hour_local(now_ms()).0;

    if let Ok(conn) = open_meta_readonly() {
        st.total_events = conn
            .query_row("SELECT COALESCE(SUM(n), 0) FROM daily_stats", [], |r| {
                r.get::<_, i64>(0)
            })
            .unwrap_or(0);
        st.app_count = conn
            .query_row("SELECT COUNT(*) FROM apps", [], |r| r.get::<_, i64>(0))
            .unwrap_or(0);
        if let Ok((first, last)) = conn.query_row(
            "SELECT MIN(day), MAX(day) FROM daily_stats",
            [],
            |r| Ok((r.get::<_, Option<String>>(0)?, r.get::<_, Option<String>>(1)?)),
        ) {
            st.first_day = first;
            st.last_day = last;
        }
        if let Ok((kd, md)) = conn.query_row(
            "SELECT \
               COALESCE(SUM(CASE WHEN kind = 0 THEN n END), 0), \
               COALESCE(SUM(CASE WHEN kind = 1 THEN n END), 0) \
             FROM daily_stats WHERE day = ?1",
            params![today],
            |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)),
        ) {
            st.today_keydown = kd;
            st.today_mousedown = md;
        }
        st.active_seconds_today = conn
            .query_row(
                "SELECT COALESCE(SUM(active_seconds), 0) FROM daily_apps \
                 WHERE day = ?1 AND kind = 0",
                params![today],
                |r| r.get::<_, i64>(0),
            )
            .unwrap_or(0);
    }

    if let Ok(conn) = Connection::open_with_flags(
        events_db_path(),
        OpenFlags::SQLITE_OPEN_READ_WRITE,
    ) {
        if let Ok(months) = list_event_months(&conn) {
            st.month_tables = months.len();
        }
    }

    st
}

fn dir_size(dir: &Path) -> u64 {
    let mut total = 0u64;
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            if let Ok(meta) = entry.metadata() {
                if meta.is_file() {
                    total += meta.len();
                }
            }
        }
    }
    total
}

#[derive(Clone, Copy, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SummaryDim {
    Day,
    Month,
    Year,
    Hour,
    App,
    Key,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SummaryPoint {
    pub label: String,
    pub keydown: i64,
    pub mousedown: i64,
    pub active_seconds: i64,
}

/// 固定维度聚合。只接受枚举 + 日期区间，**不接受任何 SQL 片段**——
/// 前端 webview 一旦被打穿，任意 SQL 就能导出全部历史，参数化把这个口子焊死。
pub fn summary(from_day: &str, to_day: &str, dim: SummaryDim) -> Result<Vec<SummaryPoint>, String> {
    let conn = open_meta_readonly()?;
    let mut out: Vec<SummaryPoint> = Vec::new();

    match dim {
        SummaryDim::Day | SummaryDim::Month | SummaryDim::Year => {
            let expr = match dim {
                SummaryDim::Day => "day",
                SummaryDim::Month => "substr(day, 1, 7)",
                _ => "substr(day, 1, 4)",
            };
            // 用位置参数两次绑定同一个值，避免 SQL 里出现字面量
            let sql = format!(
                "SELECT {expr} AS label, \
                   COALESCE(SUM(CASE WHEN kind = 0 THEN n END), 0), \
                   COALESCE(SUM(CASE WHEN kind = 1 THEN n END), 0) \
                 FROM daily_stats WHERE day >= ?1 AND day <= ?2 \
                 GROUP BY label ORDER BY label"
            );
            let mut stmt = conn.prepare(&sql).map_err(|e| e.to_string())?;
            let rows = stmt
                .query_map(params![from_day, to_day], |r| {
                    Ok(SummaryPoint {
                        label: r.get(0)?,
                        keydown: r.get(1)?,
                        mousedown: r.get(2)?,
                        active_seconds: 0,
                    })
                })
                .map_err(|e| e.to_string())?;
            out.extend(rows.flatten());

            // 活跃时长单独聚合后合并（daily_apps 是 (day, app, kind) 粒度）
            let act_sql = format!(
                "SELECT {expr} AS label, COALESCE(SUM(active_seconds), 0) \
                 FROM daily_apps WHERE day >= ?1 AND day <= ?2 AND kind = 0 \
                 GROUP BY label"
            );
            let mut act_stmt = conn.prepare(&act_sql).map_err(|e| e.to_string())?;
            let act_rows = act_stmt
                .query_map(params![from_day, to_day], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
                })
                .map_err(|e| e.to_string())?;
            let act_map: HashMap<String, i64> = act_rows.flatten().collect();
            for p in out.iter_mut() {
                if let Some(v) = act_map.get(&p.label) {
                    p.active_seconds = *v;
                }
            }
        }
        SummaryDim::Hour => {
            let mut stmt = conn
                .prepare(
                    "SELECT hour, COALESCE(SUM(keydown), 0), COALESCE(SUM(mousedown), 0) \
                     FROM daily_hours WHERE day >= ?1 AND day <= ?2 \
                     GROUP BY hour ORDER BY hour",
                )
                .map_err(|e| e.to_string())?;
            let rows = stmt
                .query_map(params![from_day, to_day], |r| {
                    Ok(SummaryPoint {
                        label: format!("{:02}", r.get::<_, i64>(0)?),
                        keydown: r.get(1)?,
                        mousedown: r.get(2)?,
                        active_seconds: 0,
                    })
                })
                .map_err(|e| e.to_string())?;
            out.extend(rows.flatten());
        }
        SummaryDim::App => {
            let mut stmt = conn
                .prepare(
                    "SELECT COALESCE(a.process, 'unknown'), \
                       COALESCE(SUM(CASE WHEN e.kind = 0 THEN e.n END), 0), \
                       COALESCE(SUM(CASE WHEN e.kind = 1 THEN e.n END), 0), \
                       COALESCE(SUM(CASE WHEN e.kind = 0 THEN e.active_seconds END), 0) \
                     FROM daily_apps e LEFT JOIN apps a ON a.id = e.app_id \
                     WHERE e.day >= ?1 AND e.day <= ?2 \
                     GROUP BY a.process ORDER BY 2 DESC LIMIT 40",
                )
                .map_err(|e| e.to_string())?;
            let rows = stmt
                .query_map(params![from_day, to_day], |r| {
                    Ok(SummaryPoint {
                        label: r.get(0)?,
                        keydown: r.get(1)?,
                        mousedown: r.get(2)?,
                        active_seconds: r.get(3)?,
                    })
                })
                .map_err(|e| e.to_string())?;
            out.extend(rows.flatten());
        }
        SummaryDim::Key => {
            let mut stmt = conn
                .prepare(
                    "SELECT code, SUM(n) FROM daily_stats \
                     WHERE kind = 0 AND day >= ?1 AND day <= ?2 \
                     GROUP BY code ORDER BY 2 DESC LIMIT 40",
                )
                .map_err(|e| e.to_string())?;
            let rows = stmt
                .query_map(params![from_day, to_day], |r| {
                    Ok(SummaryPoint {
                        label: vk_label(r.get::<_, i64>(0)?),
                        keydown: r.get(1)?,
                        mousedown: 0,
                        active_seconds: 0,
                    })
                })
                .map_err(|e| e.to_string())?;
            out.extend(rows.flatten());
        }
    }

    Ok(out)
}

// ---------------------------------------------------------------------------
// 键名映射（只用于导出与图表标签，不入库）
// ---------------------------------------------------------------------------

pub fn vk_label(code: i64) -> String {
    match code {
        0x08 => "Backspace".into(),
        0x09 => "Tab".into(),
        0x0D => "Enter".into(),
        0x10D => "NumEnter".into(),
        0x13 => "Pause".into(),
        0x14 => "CapsLock".into(),
        0x1B => "Esc".into(),
        0x20 => "Space".into(),
        0x21 => "PgUp".into(),
        0x22 => "PgDn".into(),
        0x23 => "End".into(),
        0x24 => "Home".into(),
        0x25 => "Left".into(),
        0x26 => "Up".into(),
        0x27 => "Right".into(),
        0x28 => "Down".into(),
        0x2C => "PrtSc".into(),
        0x2D => "Insert".into(),
        0x2E => "Delete".into(),
        0x5B => "Win".into(),
        0x5C => "WinR".into(),
        0x5D => "Menu".into(),
        0x90 => "NumLock".into(),
        0x91 => "ScrollLock".into(),
        0xA0 => "LShift".into(),
        0xA1 => "RShift".into(),
        0xA2 => "LCtrl".into(),
        0xA3 => "RCtrl".into(),
        0xA4 => "LAlt".into(),
        0xA5 => "RAlt".into(),
        0xBA => ";".into(),
        0xBB => "=".into(),
        0xBC => ",".into(),
        0xBD => "-".into(),
        0xBE => ".".into(),
        0xBF => "/".into(),
        0xC0 => "`".into(),
        0xDB => "[".into(),
        0xDC => "\\".into(),
        0xDD => "]".into(),
        0xDE => "'".into(),
        c if (0x30..=0x39).contains(&c) => ((c as u8) as char).to_string(),
        c if (0x41..=0x5A).contains(&c) => ((c as u8) as char).to_string(),
        c if (0x60..=0x69).contains(&c) => format!("Num{}", c - 0x60),
        c if (0x6A..=0x6F).contains(&c) => match c {
            0x6A => "Num*".into(),
            0x6B => "Num+".into(),
            0x6D => "Num-".into(),
            0x6E => "Num.".into(),
            _ => "Num/".into(),
        },
        c if (0x70..=0x7B).contains(&c) => format!("F{}", c - 0x6F),
        c if (1..=3).contains(&c) => ["MouseLeft", "MouseRight", "MouseMiddle"][(c - 1) as usize]
            .to_string(),
        c => format!("0x{c:02X}"),
    }
}

// ---------------------------------------------------------------------------
// 保留期预览与清理（UI 线程触发）
// ---------------------------------------------------------------------------

#[derive(Serialize, Clone, Default)]
#[serde(rename_all = "camelCase")]
pub struct RetentionReport {
    pub cutoff_day: String,
    pub deleted_rows: i64,
    pub dropped_months: Vec<String>,
}

#[derive(Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct RetentionPreview {
    pub cutoff_day: String,
    pub cutoff_ts: i64,
    pub rows: i64,
    pub months: Vec<String>,
}

/// 只统计将被删除的量，不动任何数据——UI 用它弹二次确认。
pub fn retention_preview(days: u32) -> Result<RetentionPreview, String> {
    if days == 0 {
        return Ok(RetentionPreview::default());
    }
    let cutoff_ts = now_ms() - days as i64 * 86_400_000;
    let mut preview = RetentionPreview {
        cutoff_day: day_hour_local(cutoff_ts).0,
        cutoff_ts,
        rows: 0,
        months: Vec::new(),
    };
    if !events_db_path().exists() {
        return Ok(preview);
    }
    let conn = Connection::open_with_flags(&events_db_path(), OpenFlags::SQLITE_OPEN_READ_WRITE)
        .map_err(|e| e.to_string())?;
    let cutoff_month = month_of(cutoff_ts);
    for month in list_event_months(&conn)? {
        let table = month_table_name(&month);
        if month < cutoff_month {
            preview.rows += count_rows(&conn, &table).unwrap_or(0);
            preview.months.push(month);
        } else if month == cutoff_month {
            preview.rows += conn
                .query_row(
                    &format!("SELECT COUNT(*) FROM {table} WHERE ts < ?1"),
                    params![cutoff_ts],
                    |r| r.get::<_, i64>(0),
                )
                .unwrap_or(0);
        }
    }
    Ok(preview)
}

pub fn apply_retention(days: u32) -> Result<RetentionReport, String> {
    let Some(sender) = TX.get() else {
        // 记录线程从未启动过，说明一条数据都没有
        return Ok(RetentionReport::default());
    };
    let (tx, rx) = mpsc::channel();
    sender
        .send(Msg::Retention { days, reply: tx })
        .map_err(|e| e.to_string())?;
    rx.recv_timeout(Duration::from_secs(120))
        .map_err(|e| format!("清理超时: {e}"))?
}

pub fn wipe_all() -> Result<(), String> {
    let Some(sender) = TX.get() else {
        // 用户从未开启过记录，目录可能还不存在，直接删干净即可
        let dir = log_dir();
        if dir.exists() {
            fs::remove_dir_all(&dir).map_err(|e| format!("删除日志目录失败: {e}"))?;
        }
        return Ok(());
    };
    let (tx, rx) = mpsc::channel();
    sender
        .send(Msg::Wipe { reply: tx })
        .map_err(|e| e.to_string())?;
    rx.recv_timeout(Duration::from_secs(60))
        .map_err(|e| format!("清空超时: {e}"))?
}

// ---------------------------------------------------------------------------
// CSV 导出（流式，不在内存里堆积）
// ---------------------------------------------------------------------------

/// 导出 `[from_ts, to_ts)` 区间明细到 CSV。返回写入行数。
pub fn export_csv(from_ts: i64, to_ts: i64, out: &Path) -> Result<i64, String> {
    if !events_db_path().exists() {
        return Err("尚无记录可导出".to_string());
    }
    let conn = Connection::open_with_flags(&events_db_path(), OpenFlags::SQLITE_OPEN_READ_WRITE)
        .map_err(|e| e.to_string())?;
    let meta = meta_db_path();
    if meta.exists() {
        let _ = conn.execute(
            "ATTACH DATABASE ?1 AS meta",
            params![meta.to_string_lossy().to_string()],
        );
    }

    let (from_day, _) = day_hour_local(from_ts);
    let (to_day, _) = day_hour_local(to_ts);
    let months: Vec<String> = list_event_months(&conn)?
        .into_iter()
        .filter(|m| {
            // 物理裁剪：只拼命中的月份（月份串已通过白名单校验）
            let month_start = format!("{}-{}-01", &m[..4], &m[4..]);
            let month_key = format!("{}-{}", &m[..4], &m[4..]);
            month_start <= to_day && month_key.as_str() >= &from_day[..7]
        })
        .collect();
    if months.is_empty() {
        return Err("该区间没有数据".to_string());
    }

    let union = months
        .iter()
        .map(|m| {
            format!(
                "SELECT ts, kind, code, flags, x, y, app_id FROM {}",
                month_table_name(m)
            )
        })
        .collect::<Vec<_>>()
        .join(" UNION ALL ");
    let sql = format!(
        "SELECT e.ts, e.kind, e.code, e.flags, e.x, e.y, a.process \
         FROM ({union}) e LEFT JOIN meta.apps a ON a.id = e.app_id \
         WHERE e.ts >= ?1 AND e.ts < ?2 ORDER BY e.ts"
    );

    let file = fs::File::create(out).map_err(|e| format!("创建导出文件失败: {e}"))?;
    let mut w = BufWriter::new(file);
    writeln!(w, "timestamp,local_time,kind,target,flags,app,x,y").map_err(|e| e.to_string())?;

    let mut stmt = conn.prepare(&sql).map_err(|e| e.to_string())?;
    let mut rows = stmt.query(params![from_ts, to_ts]).map_err(|e| e.to_string())?;
    let mut count: i64 = 0;
    while let Some(row) = rows.next().map_err(|e| e.to_string())? {
        let ts: i64 = row.get(0).map_err(|e| e.to_string())?;
        let kind: i32 = row.get(1).map_err(|e| e.to_string())?;
        let code: i32 = row.get(2).map_err(|e| e.to_string())?;
        let flags: i32 = row.get(3).map_err(|e| e.to_string())?;
        let x: Option<i32> = row.get(4).map_err(|e| e.to_string())?;
        let y: Option<i32> = row.get(5).map_err(|e| e.to_string())?;
        let app: Option<String> = row.get(6).map_err(|e| e.to_string())?;
        let local = Local
            .timestamp_millis_opt(ts)
            .single()
            .map(|dt| dt.format("%Y-%m-%d %H:%M:%S%.3f").to_string())
            .unwrap_or_default();
        let kind_label = if kind == KIND_KEY_DOWN { "key" } else { "mouse" };
        writeln!(
            w,
            "{ts},{local},{kind_label},{},{flags},{},{},{}",
            vk_label(code as i64),
            app.as_deref().unwrap_or(""),
            x.map(|v| v.to_string()).unwrap_or_default(),
            y.map(|v| v.to_string()).unwrap_or_default(),
        )
        .map_err(|e| e.to_string())?;
        count += 1;
    }
    w.flush().map_err(|e| e.to_string())?;
    Ok(count)
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn month_guard_rejects_injection() {
        assert!(is_valid_month("202611"));
        assert!(!is_valid_month("20261"));
        assert!(!is_valid_month("202611; DROP TABLE x"));
        assert!(!is_valid_month("../etc"));
    }

    #[test]
    fn vk_labels_cover_required_keys() {
        assert_eq!(vk_label(0x1B), "Esc");
        assert_eq!(vk_label(0x70), "F1");
        assert_eq!(vk_label(0x7B), "F12");
        assert_eq!(vk_label(0x10D), "NumEnter");
        assert_eq!(vk_label(0x41), "A");
        assert_eq!(vk_label(0x0D), "Enter");
    }

    #[test]
    fn day_hour_is_local_and_stable() {
        let (day, hour) = day_hour_local(now_ms());
        assert_eq!(day.len(), 10);
        assert!(hour < 24);
    }

    #[test]
    fn accum_splits_kinds() {
        let mut acc = Accum::default();
        acc.daily_hours.insert(("2026-11-24".into(), 10), [3, 2]);
        assert_eq!(acc.daily_hours[&("2026-11-24".to_string(), 10)], [3, 2]);
    }

    #[test]
    fn schema_builds_on_memory_db() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(META_SCHEMA).unwrap();
        ensure_month_table(&conn, "202611").unwrap();
        // 重复建表必须幂等
        ensure_month_table(&conn, "202611").unwrap();
        let months = list_event_months(&conn).unwrap();
        assert_eq!(months, vec!["202611".to_string()]);
    }
}
