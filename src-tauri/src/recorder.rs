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
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const KIND_KEY_DOWN: i32 = 0;
pub const KIND_MOUSE_DOWN: i32 = 1;

/// `flags` 位定义（与方案第二节一致）。
pub const FLAG_SHIFT: i32 = 1 << 1;
pub const FLAG_CAPS: i32 = 1 << 2;

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
/// 前台计时采样间隔。2 秒是精度与开销的折中：一次采样只是一次 `OpenProcess`，
/// 而 2 秒的切换误差即使一天切 500 次窗口也只有约 16 分钟误差里的可忽略部分。
const FOCUS_SAMPLE_MS: i64 = 2_000;
/// 两次采样间隔超过这个值，判定为休眠/挂起（合盖、待机、进程被挂起），
/// 这段直接丢弃——否则合盖一晚会把 8 小时算给当时的前台程序。
const FOCUS_STALE_MS: i64 = 60_000;

pub(crate) const META_SCHEMA: &str = r#"
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
-- 前台计时：某程序在某个小时里真实占据了前台多少秒。
-- 与 daily_apps.active_seconds 的口径完全不同：那个是「有多少输入在发生」，
-- 这个是「窗口在前面挂了多久」，看视频、读文档都会被算进来。
CREATE TABLE IF NOT EXISTS daily_app_focus (
    day     TEXT    NOT NULL,
    hour    INTEGER NOT NULL,
    app_id  INTEGER NOT NULL,
    seconds INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (day, hour, app_id)
) WITHOUT ROWID;

-- ---------------------------------------------------------------------------
-- 数据同步（sync.rs 专用）。键值对放一张通用表：machine_id、导出/导入水位线、
-- 上次同步时间与结果都往里塞，以后加状态不用改表结构。
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS sync_state (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
-- 已成功导入过的「来源机器 × 合并包名」。同名包绝不二次导入，这是防重复计数的
-- 第一道闸（第二道是导入侧的差值合并，见 merge_state）。
CREATE TABLE IF NOT EXISTS imported_packs (
    source_id   TEXT    NOT NULL,
    pack_name   TEXT    NOT NULL,
    imported_at INTEGER NOT NULL,
    PRIMARY KEY (source_id, pack_name)
) WITHOUT ROWID;
-- 每个来源机器、每个统计键上次导入时的取值。导入时只应用「本次值 − 上次值」的
-- 差值，所以同一个包重复导入、甚至登记表整个丢失，计数也不会翻倍。
-- gkey 里的应用维度用进程名而不是对方库的 app_id：对方清空重建后 id 会变，
-- 进程名才是跨机器稳定的身份。
CREATE TABLE IF NOT EXISTS merge_state (
    source_id TEXT    NOT NULL,
    gkey      TEXT    NOT NULL,
    last_n    INTEGER NOT NULL,
    PRIMARY KEY (source_id, gkey)
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
    /// 用户忽略名单变了：清前台缓存，让新名单立刻生效（最多延迟一次采样周期）。
    IgnoredChanged,
}

static TX: OnceLock<Sender<Msg>> = OnceLock::new();
static RECORDING: AtomicBool = AtomicBool::new(false);
static STARTED: AtomicBool = AtomicBool::new(false);
/// 活跃判定阈值的毫秒值。线程启动前就要能读到，所以单独放一个原子量。
static GAP_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(300_000);
/// 用户显式忽略的程序（去 .exe 的小写主干）。独立于写线程的消息通道：
/// ingest 每 200 ms 才解析一次前台进程，直接读这个集合比来回发消息查询便宜，
/// 修改时再发 IgnoredChanged 清缓存，两边加起来延迟不超过一次采样周期。
static USER_IGNORED: OnceLock<Mutex<std::collections::HashSet<String>>> = OnceLock::new();

fn user_ignored_set() -> &'static Mutex<std::collections::HashSet<String>> {
    USER_IGNORED.get_or_init(|| Mutex::new(std::collections::HashSet::new()))
}

/// "Chrome.EXE" / "chrome.exe" → "chrome"。忽略匹配只看主干，
/// 大小写与扩展名差异不构成漏网之鱼。
fn normalize_ignored_key(app: &str) -> String {
    let lower = app.trim().to_ascii_lowercase();
    let stem = lower.strip_suffix(".exe").unwrap_or(&lower);
    stem.chars().take(120).collect()
}

fn is_user_ignored(name: &str) -> bool {
    let key = normalize_ignored_key(name);
    if key.is_empty() {
        return false;
    }
    user_ignored_set()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains(&key)
}

/// 更新用户忽略名单并通知记录线程。名单为空是合法状态（全部取消忽略）。
pub fn set_ignored_apps(apps: &[String]) {
    let set: std::collections::HashSet<String> =
        apps.iter().map(|a| normalize_ignored_key(a)).collect();
    *user_ignored_set().lock().unwrap_or_else(|e| e.into_inner()) = set;
    if let Some(tx) = TX.get() {
        let _ = tx.send(Msg::IgnoredChanged);
    }
}

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
pub(crate) fn day_hour_local(ts_ms: i64) -> (String, u32) {
    match Local.timestamp_millis_opt(ts_ms).single() {
        Some(dt) => (
            format!("{:04}-{:02}-{:02}", dt.year(), dt.month(), dt.day()),
            dt.hour(),
        ),
        None => ("1970-01-01".to_string(), 0),
    }
}

/// 事件月表用的月份：6 位纯数字 "202611"。只服务 events 月表命名与保留期比较。
///
/// 注意：meta.db 的 `monthly_app_keys.month` 存的是带横杠的 "2026-11"（见
/// record() 里的 `day[..7]`，前端也按该格式查询），两套格式并存是有意的，
/// 别互相「统一」——2026-11-25 之前 `month_of` 曾错误地返回 "2026-11"，
/// 被 `is_valid_month` 拒绝导致事件明细从未落盘（见 write_events）。
pub(crate) fn month_of(ts_ms: i64) -> String {
    let (day, _) = day_hour_local(ts_ms);
    format!("{}{}", &day[..4], &day[5..7])
}

/// 只接受 6 位纯数字的月份，任何其它输入一律拒绝——这是拼表名前的白名单校验。
fn is_valid_month(m: &str) -> bool {
    m.len() == 6 && m.bytes().all(|b| b.is_ascii_digit())
}

pub(crate) fn month_table_name(month: &str) -> String {
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
    /// (day, hour, app_id) -> 前台秒数
    daily_app_focus: HashMap<(String, u32, i64), i64>,
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
    /// (缓存时刻, (进程名, app_id))。进程名随缓存一起留一份，ingest 要拿它对
    /// 忽略名单做判断；None 表示那一刻解析不出前台程序。
    app_cache: Option<(i64, Option<(String, i64)>)>,
    privacy_ranges: Vec<(i64, i64)>,
    privacy_open: bool,
    gap_ms: i64,
    dir: PathBuf,
    /// 上一次前台采样的时刻（ms）。`None` 表示还没采过样。
    focus_last_ts: Option<i64>,
    /// 上一次采样时占据前台的 app_id（-1 表示当时没有可归属的程序）。
    focus_last_app: i64,
    /// 采样间隔不是整秒时留下的毫秒余数，攒够 1 秒再入账，避免每次采样都截断。
    focus_carry_ms: i64,
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
            focus_last_ts: None,
            focus_last_app: -1,
            focus_carry_ms: 0,
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
            // 每轮迭代都问一次前台计时（内部按 FOCUS_SAMPLE_MS 限流）。
            // 放在 match 之前而不是只挂在 Timeout 上：有连续输入时 channel 一直有消息，
            // 永远走不到 Timeout 分支，前台时间就会被整段漏掉。
            self.sample_focus();
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
                    self.flush_force();
                    last_flush = Instant::now();
                }
                Ok(Msg::SetGap(minutes)) => {
                    self.gap_ms = minutes.clamp(1, 120) as i64 * 60_000;
                }
                Ok(Msg::IgnoredChanged) => {
                    // 名单刚变：丢弃进程缓存，避免被忽略的程序在缓存窗口内继续入账；
                    // 前台游标归零，忽略目标已占据的那两秒不再继续结算给它。
                    self.app_cache = None;
                    self.focus_last_app = -1;
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
                    self.flush_force();
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
        let fg = self.resolve_fg(r.ts);
        // 用户忽略的程序：事件、活跃时长一个字节都不记。名单是全局量，
        // 读一次锁的成本被 200 ms 的进程缓存摊薄。
        if let Some((name, _)) = &fg {
            if is_user_ignored(name) {
                return;
            }
        }
        let app_id = fg.map(|(_, id)| id).unwrap_or(-1);
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

    /// 前台进程解析（带 200 ms 缓存）。返回 (进程名, app_id)；
    /// 解析不出前台程序时返回 None，调用方按 app_id = -1 记账。
    fn resolve_fg(&mut self, now: i64) -> Option<(String, i64)> {
        if let Some((cached_at, hit)) = &self.app_cache {
            if now - cached_at <= APP_CACHE_MS {
                return hit.clone();
            }
        }
        let hit = match foreground_process() {
            Some(name) => {
                let id = self.upsert_app(&name).unwrap_or(-1);
                Some((name, id))
            }
            None => None,
        };
        self.app_cache = Some((now, hit.clone()));
        hit
    }

    // -- 前台计时 ----------------------------------------------------------

    /// 每 `FOCUS_SAMPLE_MS` 采一次前台窗口，把「上次采样到现在」这段结算给
    /// **上一次**占据前台的程序。
    ///
    /// 分段结算而不是「每次 +2 秒」：切窗口时那 2 秒算给旧窗口，既不丢也不多算。
    /// 误差上限是一次采样间隔，一整天切几百次窗口也只有几分钟量级。
    fn sample_focus(&mut self) {
        let now = now_ms();
        let Some(last) = self.focus_last_ts else {
            // 第一次只立标杆，不记账——没有「上一次」可结算。
            self.focus_last_ts = Some(now);
            return;
        };
        let elapsed = now - last;
        if elapsed < FOCUS_SAMPLE_MS {
            return;
        }

        // 记录关闭 / 暂停 / 正处于隐私窗口：一律不计时。
        // 前者是用户的明确意图；后者是「连在这个窗口待了多久都不该知道」。
        // 顺带把游标推到 now，恢复后不会把中间那段补记上。
        if !RECORDING.load(Ordering::Relaxed) || self.privacy_open {
            self.focus_last_ts = Some(now);
            self.focus_last_app = -1;
            self.focus_carry_ms = 0;
            return;
        }

        // 合盖、待机、进程被系统挂起：这段不是「在用」，整段丢弃。
        // 没有这一条，合盖一晚就会把 8 小时算给关盖前的那个程序。
        if elapsed > FOCUS_STALE_MS {
            self.focus_last_ts = Some(now);
            self.focus_last_app = -1;
            self.focus_carry_ms = 0;
            return;
        }

        // 系统进入「没人在用」的状态：锁屏、屏保、切出当前用户。同样把游标推到现在。
        //
        // 这里刻意**不看键鼠空闲**。看视频、读长文档时手可以一动不动十几分钟，
        // 用空闲判定会把整段时长吃掉；而「人到底在不在」没有可靠的系统接口可问
        // （Windows Hello 的存在检测不对普通进程开放）。所以只认系统状态：
        // 没锁屏就认为在用，代价是不锁屏离开会照记，但行为可预测。
        if is_system_absent() {
            self.focus_last_ts = Some(now);
            self.focus_last_app = -1;
            self.focus_carry_ms = 0;
            return;
        }

        if self.focus_last_app >= 0 && elapsed > 0 {
            let total = elapsed + self.focus_carry_ms;
            let secs = total / 1000;
            self.focus_carry_ms = total % 1000;
            if secs > 0 {
                // 一段 2 秒跨整点时按结束时间归属，误差上限就是采样间隔。
                let (day, hour) = day_hour_local(now);
                *self
                    .pending
                    .daily_app_focus
                    .entry((day, hour, self.focus_last_app))
                    .or_insert(0) += secs;
            }
        }

        self.focus_last_app = self.focus_app_now(now);
        self.focus_last_ts = Some(now);
    }

    /// 采样专用的前台程序解析。**故意不复用 `resolve_app_id`**：那条路径会写
    /// `app_cache`，而锁屏界面被过滤成 -1 后如果写进缓存，紧接着的真实按键就会
    /// 跟着记成 NULL，200 ms 内都恢复不过来。
    fn focus_app_now(&mut self, _now: i64) -> i64 {
        let Some(name) = foreground_process() else {
            return -1;
        };
        if is_ignored_focus_process(&name) || is_user_ignored(&name) {
            return -1;
        }
        // 复用 upsert_app：进程名原样入库，与按键记录共用同一张 apps 表，
        // 这样应用库、分类映射看到的就是同一行，不会凭空多出一个「Chrome」。
        self.upsert_app(&name).unwrap_or(-1)
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
        self.flush_inner(false);
    }

    /// 退出、显式 Flush 时用：不看攒够阈值，有多少写多少。
    fn flush_force(&mut self) {
        self.flush_inner(true);
    }

    fn flush_inner(&mut self, force: bool) {
        // 隐私兜底：翻转之前进来、还没提交的那批再滤一次（缓冲 ≤ 500 行，成本可忽略）
        self.drop_private_rows();
        if self.buf.is_empty() {
            if self.pending.daily_app_focus.is_empty() {
                return;
            }
            // 只有前台时长在增长（看视频、读文档，一个键都不按）时，攒够 30 秒再落盘。
            // 否则空闲状态下会变成每 2 秒一次 SQLite 事务，纯粹拿磁盘寿命换实时性。
            // 强制 flush 不走这个阈值，否则退出时会静默丢掉最后不到 30 秒的时长。
            if !force {
                let pending_secs: i64 = self.pending.daily_app_focus.values().sum();
                if pending_secs < 30 {
                    return;
                }
            }
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

            let mut focus = tx
                .prepare(
                    "INSERT INTO daily_app_focus(day, hour, app_id, seconds) VALUES(?1, ?2, ?3, ?4) \
                     ON CONFLICT(day, hour, app_id) DO UPDATE SET \
                     seconds = seconds + excluded.seconds",
                )
                .map_err(|e| e.to_string())?;
            for ((day, hour, app_id), secs) in &acc.daily_app_focus {
                focus
                    .execute(params![day, hour, app_id, secs])
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
        // 前台计时的游标也要一并归零，否则清空后的第一次采样会把「清空前那段」
        // 结算给新库，凭空冒出一条不该存在的时长。
        self.focus_last_ts = None;
        self.focus_last_app = -1;
        self.focus_carry_ms = 0;

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
        // 清理前把待写数据全部落盘，否则内存里那几十秒前台时长会被直接丢掉。
        self.flush_force();
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
                "DELETE FROM daily_app_focus WHERE day < ?1",
                params![cutoff_day],
            )
            .map_err(|e| e.to_string())?;
        self.meta
            .execute(
                "DELETE FROM monthly_app_keys WHERE month < ?1",
                // monthly_app_keys.month 是 "YYYY-MM" 格式，与事件表名的 6 位数字
                // 是两套格式；cutoff_month（month_of）不能拿来跟它比。
                params![&cutoff_day[..7]],
            )
            .map_err(|e| e.to_string())?;

        Ok(RetentionReport {
            cutoff_day,
            deleted_rows,
            dropped_months,
        })
    }
}

pub(crate) fn ensure_month_table(conn: &Connection, month: &str) -> rusqlite::Result<()> {
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

pub(crate) fn list_event_months(conn: &Connection) -> Result<Vec<String>, String> {
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

/// 系统当前是否处于「没人在用」的状态：锁屏、屏保运行、或会话非交互。
///
/// 用 `SHQueryUserNotificationState` 而不是键鼠空闲：它回答的是「用户还在不在」，
/// 不回答「刚才有没有动鼠标」，所以看视频、读文档这种长时间不碰键鼠的场景不会被误杀。
/// 只把 `QUNS_NOT_PRESENT` 当作不在场；`QUNS_BUSY`（全屏应用）和
/// `QUNS_RUNNING_D3D_FULL_SCREEN`（全屏游戏/视频）恰恰是在用，必须照记。
#[cfg(windows)]
fn is_system_absent() -> bool {
    use windows_sys::Win32::UI::Shell::{
        SHQueryUserNotificationState, QUNS_NOT_PRESENT, QUERY_USER_NOTIFICATION_STATE,
    };

    unsafe {
        let mut state: QUERY_USER_NOTIFICATION_STATE = 0;
        // 返回 HRESULT，S_OK 才是查到了值。
        if SHQueryUserNotificationState(&mut state) != 0 {
            // 查不到就按「在场」处理：宁可多记，也不要因为一次 API 失败丢整段时长。
            // 无 shell 的会话（服务、某些 de-elevated 进程）会走到这里。
            return false;
        }
        state == QUNS_NOT_PRESENT
    }
}

#[cfg(not(windows))]
fn is_system_absent() -> bool {
    // macOS 侧本来就没有前台程序可记（`foreground_process` 恒返回 None），这里恒 false。
    false
}

/// 不该进前台时长榜的进程。分两类：
/// - **人不在**：锁屏、登录界面、屏保。这几秒确实有窗口在前台，但不是「在用」。
/// - **KuroCat 自己**：打开统计窗口看数据这个动作本身会把 KuroCat 顶上第一，
///   污染真实排名。
fn is_ignored_focus_process(name: &str) -> bool {
    // 比的是去掉 .exe 的主干，这样 `KuroCat` / `KuroCat.exe` 都能命中。
    const IGNORED: [&str; 4] = ["lockapp", "logonui", "winlogon", "kurocat"];
    let lower = name.to_ascii_lowercase();
    let stem = lower.strip_suffix(".exe").unwrap_or(&lower);
    IGNORED.contains(&stem) || lower.ends_with(".scr")
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
    /// 今天各程序占据前台的秒数合计。与 `active_seconds_today` 是两个不同口径，
    /// 前者看「动手多久」，后者看「窗口在前面挂多久」。
    pub focus_seconds_today: i64,
    pub first_day: Option<String>,
    pub last_day: Option<String>,
    pub month_tables: usize,
    pub app_count: i64,
    pub retention_days: u32,
    pub active_gap_minutes: u32,
    /// 用户是否开启了轨迹采样（不是「此刻在采」，暂停时它是 true）。
    pub trail_enabled: bool,
}

pub fn status(
    enabled: bool,
    paused: bool,
    retention_days: u32,
    gap_minutes: u32,
    trail_enabled: bool,
) -> LogStatus {
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
        focus_seconds_today: 0,
        first_day: None,
        last_day: None,
        month_tables: 0,
        app_count: 0,
        retention_days,
        active_gap_minutes: gap_minutes,
        trail_enabled,
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
        st.focus_seconds_today = conn
            .query_row(
                "SELECT COALESCE(SUM(seconds), 0) FROM daily_app_focus WHERE day = ?1",
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
    /// 输入活跃时长：相邻输入间隔累加，衡量「动手的密度」。
    pub active_seconds: i64,
    /// 前台时长：窗口真实占据前台的秒数，与有没有输入无关。
    /// 两个数放一起才看得出「开着但没动」和「真的在操作」的差别。
    pub focus_seconds: i64,
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
                        focus_seconds: 0,
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

            // 前台时长同样单独聚合。它与按键是两张独立的表，join 会互相放大。
            let focus_sql = format!(
                "SELECT {expr} AS label, COALESCE(SUM(seconds), 0) \
                 FROM daily_app_focus WHERE day >= ?1 AND day <= ?2 \
                 GROUP BY label"
            );
            let mut focus_stmt = conn.prepare(&focus_sql).map_err(|e| e.to_string())?;
            let focus_rows = focus_stmt
                .query_map(params![from_day, to_day], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
                })
                .map_err(|e| e.to_string())?;
            let focus_map: HashMap<String, i64> = focus_rows.flatten().collect();
            for p in out.iter_mut() {
                if let Some(v) = focus_map.get(&p.label) {
                    p.focus_seconds = *v;
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
                        focus_seconds: 0,
                    })
                })
                .map_err(|e| e.to_string())?;
            out.extend(rows.flatten());

            let mut focus_stmt = conn
                .prepare(
                    "SELECT hour, COALESCE(SUM(seconds), 0) FROM daily_app_focus \
                     WHERE day >= ?1 AND day <= ?2 GROUP BY hour",
                )
                .map_err(|e| e.to_string())?;
            let focus_rows = focus_stmt
                .query_map(params![from_day, to_day], |r| {
                    Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))
                })
                .map_err(|e| e.to_string())?;
            let focus_map: HashMap<i64, i64> = focus_rows.flatten().collect();
            for p in out.iter_mut() {
                if let Ok(hour) = p.label.parse::<i64>() {
                    if let Some(v) = focus_map.get(&hour) {
                        p.focus_seconds = *v;
                    }
                }
            }
        }
        SummaryDim::App => {
            // 程序集合取「有按键的」∪「有前台时长的」：只用不敲的程序（看视频、
            // 读文档）在 daily_apps 里一行都没有，必须靠 UNION 才进得来。
            let mut stmt = conn
                .prepare(
                    "WITH agg_keys AS ( \
                        SELECT app_id, \
                          COALESCE(SUM(CASE WHEN kind = 0 THEN n END), 0) AS kd, \
                          COALESCE(SUM(CASE WHEN kind = 1 THEN n END), 0) AS md, \
                          COALESCE(SUM(CASE WHEN kind = 0 THEN active_seconds END), 0) AS act \
                        FROM daily_apps WHERE day >= ?1 AND day <= ?2 GROUP BY app_id \
                     ), agg_focus AS ( \
                        SELECT app_id, COALESCE(SUM(seconds), 0) AS fsec \
                        FROM daily_app_focus WHERE day >= ?1 AND day <= ?2 GROUP BY app_id \
                     ), ids AS ( \
                        SELECT app_id FROM agg_keys UNION SELECT app_id FROM agg_focus \
                     ) \
                     SELECT COALESCE(a.process, 'unknown'), \
                       COALESCE(k.kd, 0), COALESCE(k.md, 0), COALESCE(k.act, 0), COALESCE(f.fsec, 0) \
                     FROM ids \
                     LEFT JOIN agg_keys k ON k.app_id = ids.app_id \
                     LEFT JOIN agg_focus f ON f.app_id = ids.app_id \
                     LEFT JOIN apps a ON a.id = ids.app_id \
                     ORDER BY (COALESCE(k.kd, 0) + COALESCE(f.fsec, 0)) DESC LIMIT 60",
                )
                .map_err(|e| e.to_string())?;
            let rows = stmt
                .query_map(params![from_day, to_day], |r| {
                    Ok(SummaryPoint {
                        label: r.get(0)?,
                        keydown: r.get(1)?,
                        mousedown: r.get(2)?,
                        active_seconds: r.get(3)?,
                        focus_seconds: r.get(4)?,
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
                        focus_seconds: 0,
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

/// 某个程序最后一次有记录是哪天。前端据此显示「今天 / 3 天前 / 26 天前」。
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppActivity {
    pub process: String,
    pub last_day: String,
}

/// 按应用取最后活跃日期。只认键盘那一条（kind = 0），和「总计时长」口径一致。
pub fn app_last_seen(from_day: &str, to_day: &str) -> Result<Vec<AppActivity>, String> {
    let conn = open_meta_readonly()?;
    let mut stmt = conn
        .prepare(
            "SELECT COALESCE(a.process, 'unknown'), MAX(e.day) \
             FROM daily_apps e LEFT JOIN apps a ON a.id = e.app_id \
             WHERE e.day >= ?1 AND e.day <= ?2 AND e.kind = 0 \
             GROUP BY a.process",
        )
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map(params![from_day, to_day], |r| {
            Ok(AppActivity {
                process: r.get(0)?,
                last_day: r.get(1)?,
            })
        })
        .map_err(|e| e.to_string())?;
    Ok(rows.flatten().collect())
}

// ---------------------------------------------------------------------------
// 应用详情统计（统计窗口的应用弹窗专用）
// ---------------------------------------------------------------------------

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppStatsDaily {
    pub day: String,
    pub keydown: i64,
    pub mousedown: i64,
    pub active_seconds: i64,
    pub focus_seconds: i64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppStatsHeatCell {
    pub day: String,
    pub hour: i32,
    pub seconds: i64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppStatsKey {
    pub label: String,
    pub n: i64,
}

/// 单个程序的统计快照。found = false 表示库里从没见过这个进程名（参数传错或
/// 记录期间进程名改了），前端据此显示空态而不是一排 0。
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppStats {
    pub process: String,
    pub found: bool,
    pub keydown: i64,
    pub mousedown: i64,
    pub active_seconds: i64,
    pub focus_seconds: i64,
    pub first_day: Option<String>,
    pub last_day: Option<String>,
    /// 按日趋势：区间内每天都有键鼠或前台数据的日期。
    pub daily: Vec<AppStatsDaily>,
    /// 热力图：day × hour 的前台秒数（daily_app_focus，口径为「窗口在前面挂多久」）。
    pub heatmap: Vec<AppStatsHeatCell>,
    /// 该程序最常按的键（monthly_app_keys 按月聚合，上限 10）。
    pub top_keys: Vec<AppStatsKey>,
}

/// 查询单个应用的统计。参数是枚举化的日期字符串，与 summary 一致，
/// 不接受任何 SQL 片段。
pub fn app_stats(process: &str, from_day: &str, to_day: &str) -> Result<AppStats, String> {
    let conn = open_meta_readonly()?;
    let mut out = AppStats {
        process: process.to_string(),
        found: false,
        keydown: 0,
        mousedown: 0,
        active_seconds: 0,
        focus_seconds: 0,
        first_day: None,
        last_day: None,
        daily: Vec::new(),
        heatmap: Vec::new(),
        top_keys: Vec::new(),
    };

    // 先精确匹配（apps.process 是 UNIQUE），再大小写不敏感兜底：
    // Windows 的进程名大小写不稳定，前端传来的 label 与入库值偶有差异。
    let app_id: Option<i64> = conn
        .query_row(
            "SELECT id FROM apps WHERE process = ?1",
            params![process],
            |r| r.get(0),
        )
        .ok()
        .or_else(|| {
            conn.query_row(
                "SELECT id FROM apps WHERE lower(process) = lower(?1)",
                params![process],
                |r| r.get(0),
            )
            .ok()
        });
    let Some(app_id) = app_id else {
        return Ok(out);
    };
    out.found = true;

    let (kd, md, act) = conn
        .query_row(
            "SELECT \
               COALESCE(SUM(CASE WHEN kind = 0 THEN n END), 0), \
               COALESCE(SUM(CASE WHEN kind = 1 THEN n END), 0), \
               COALESCE(SUM(CASE WHEN kind = 0 THEN active_seconds END), 0) \
             FROM daily_apps WHERE app_id = ?1 AND day >= ?2 AND day <= ?3",
            params![app_id, from_day, to_day],
            |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?)),
        )
        .map_err(|e| e.to_string())?;
    out.keydown = kd;
    out.mousedown = md;
    out.active_seconds = act;

    out.focus_seconds = conn
        .query_row(
            "SELECT COALESCE(SUM(seconds), 0) FROM daily_app_focus \
             WHERE app_id = ?1 AND day >= ?2 AND day <= ?3",
            params![app_id, from_day, to_day],
            |r| r.get(0),
        )
        .map_err(|e| e.to_string())?;

    if let Ok((first, last)) = conn.query_row(
        "SELECT MIN(day), MAX(day) FROM ( \
            SELECT day FROM daily_apps WHERE app_id = ?1 AND day >= ?2 AND day <= ?3 \
            UNION SELECT day FROM daily_app_focus WHERE app_id = ?1 AND day >= ?2 AND day <= ?3 \
         )",
        params![app_id, from_day, to_day],
        |r| Ok((r.get::<_, Option<String>>(0)?, r.get::<_, Option<String>>(1)?)),
    ) {
        out.first_day = first;
        out.last_day = last;
    }

    // 按日趋势：键鼠来自 daily_apps（按天分组），前台来自 daily_app_focus，
    // 两张独立表在代码里按天合并，join 会互相放大行数。
    {
        let mut stmt = conn
            .prepare(
                "SELECT day, \
                   COALESCE(SUM(CASE WHEN kind = 0 THEN n END), 0), \
                   COALESCE(SUM(CASE WHEN kind = 1 THEN n END), 0), \
                   COALESCE(SUM(CASE WHEN kind = 0 THEN active_seconds END), 0) \
                 FROM daily_apps WHERE app_id = ?1 AND day >= ?2 AND day <= ?3 \
                 GROUP BY day ORDER BY day",
            )
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map(params![app_id, from_day, to_day], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, i64>(3)?,
                ))
            })
            .map_err(|e| e.to_string())?;
        let mut daily: Vec<AppStatsDaily> = rows
            .flatten()
            .map(|(day, kd, md, act)| AppStatsDaily {
                day,
                keydown: kd,
                mousedown: md,
                active_seconds: act,
                focus_seconds: 0,
            })
            .collect();

        let mut focus_stmt = conn
            .prepare(
                "SELECT day, SUM(seconds) FROM daily_app_focus \
                 WHERE app_id = ?1 AND day >= ?2 AND day <= ?3 GROUP BY day",
            )
            .map_err(|e| e.to_string())?;
        let focus_rows = focus_stmt
            .query_map(params![app_id, from_day, to_day], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
            })
            .map_err(|e| e.to_string())?;
        let focus_map: HashMap<String, i64> = focus_rows.flatten().collect();
        for row in daily.iter_mut() {
            if let Some(v) = focus_map.get(&row.day) {
                row.focus_seconds = *v;
            }
        }
        out.daily = daily;
    }

    // 热力图：daily_app_focus 天然带 day + hour 维度，直接整表捞出该程序区间内的行。
    {
        let mut stmt = conn
            .prepare(
                "SELECT day, hour, seconds FROM daily_app_focus \
                 WHERE app_id = ?1 AND day >= ?2 AND day <= ?3 ORDER BY day, hour",
            )
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map(params![app_id, from_day, to_day], |r| {
                Ok(AppStatsHeatCell {
                    day: r.get(0)?,
                    hour: r.get(1)?,
                    seconds: r.get(2)?,
                })
            })
            .map_err(|e| e.to_string())?;
        out.heatmap = rows.flatten().collect();
    }

    // top 按键：monthly_app_keys 的 month 是 "YYYY-MM"（见 META_SCHEMA 注释），
    // 边界用日期字符串截前 7 位，与保留期清理的口径一致。
    {
        let from_month = &from_day[..7.min(from_day.len())];
        let to_month = &to_day[..7.min(to_day.len())];
        let mut stmt = conn
            .prepare(
                "SELECT code, SUM(n) FROM monthly_app_keys \
                 WHERE app_id = ?1 AND month >= ?2 AND month <= ?3 \
                 GROUP BY code ORDER BY 2 DESC LIMIT 10",
            )
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map(params![app_id, from_month, to_month], |r| {
                Ok(AppStatsKey {
                    label: vk_label(r.get::<_, i64>(0)?),
                    n: r.get(1)?,
                })
            })
            .map_err(|e| e.to_string())?;
        out.top_keys = rows.flatten().collect();
    }

    Ok(out)
}

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

    /// 回归：month_of 必须产出 6 位纯数字（合法表名成分）。
    /// 2026-11-25 之前它返回 "2026-11"，被 is_valid_month 拒绝，
    /// 导致事件明细从未写入 events.db（写路径静默 continue）。
    #[test]
    fn month_of_is_table_safe() {
        // 2023-11-14 22:13:20 UTC → 本地时区的月份
        let m = month_of(1_700_000_000_000);
        assert!(is_valid_month(&m), "month_of({m}) 必须通过表名白名单");
        assert_eq!(m.len(), 6);
        assert!(!m.contains('-'));

        // 月中时间点，任何时区（UTC±14 以内）都落在同一个月
        let mid = month_of(1_750_000_000_000); // 2025-06-15 15:06 UTC
        assert_eq!(mid, "202506");
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
    fn focus_ignores_lock_screen_and_itself() {
        // 锁屏 / 登录界面不是「在用」，KuroCat 自己看统计也不算
        assert!(is_ignored_focus_process("LockApp.exe"));
        assert!(is_ignored_focus_process("LogonUI.exe"));
        assert!(is_ignored_focus_process("KuroCat.exe"));
        assert!(is_ignored_focus_process("KuroCat"));
        assert!(is_ignored_focus_process("mystify.scr"));
        // 正常程序必须放行，大小写不影响
        assert!(!is_ignored_focus_process("chrome.exe"));
        assert!(!is_ignored_focus_process("Code.exe"));
        assert!(!is_ignored_focus_process("WeChat.exe"));
    }

    #[test]
    fn accum_splits_kinds() {
        let mut acc = Accum::default();
        acc.daily_hours.insert(("2026-11-24".into(), 10), [3, 2]);
        assert_eq!(acc.daily_hours[&("2026-11-24".to_string(), 10)], [3, 2]);
    }

    #[test]
    fn user_ignore_key_normalizes_case_and_suffix() {
        assert_eq!(normalize_ignored_key("Chrome.EXE"), "chrome");
        assert_eq!(normalize_ignored_key("  Notepad.exe "), "notepad");
        assert_eq!(normalize_ignored_key("Code"), "code");
        assert_eq!(normalize_ignored_key(""), "");
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
