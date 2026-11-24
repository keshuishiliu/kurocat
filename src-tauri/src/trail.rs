//! 鼠标轨迹采样 —— 「轨迹」页的数据来源。
//!
//! # 为什么单独一个模块
//!
//! 输入明细（`recorder.rs`）记的是「按下」，鼠标移动是「连续量」：
//! 采样率、体积、清理策略都不一样，混进 `events.db` 会把键位统计和 CSV 导出一起污染。
//! 所以这里另开一个 `trail.db`，只存三列 `(ts, x, y)`。
//!
//! # 隐私边界
//!
//! - 只存 **时间戳 + 屏幕坐标**，不存画面、窗口标题、点击目标、任何内容。
//! - **默认关闭**。没打开开关时一个字节都不落盘，`push()` 里第一行就返回。
//! - 密码框激活期间不采样（由 `input_hook` 在调用点判断）。
//! - 与 `recorder` 共用 `input-log` 目录，清空 / 保留期清理会一起处理。
//!
//! # 线程纪律
//!
//! `push()` 在 hook dispatcher 线程被调用，只做节流 + 一次 `mpsc::send`，
//! 所有 SQLite IO 都在 `kuro-input-trail` 线程里，绝不反压到输入链路。

use rusqlite::{params, Connection, OpenFlags};
use serde::Serialize;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::OnceLock;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// 采样最短间隔：约 10 Hz。
const SAMPLE_MIN_MS: i64 = 100;
/// 位移小于这个像素数不记，静止时几乎不产生数据。
const SAMPLE_MIN_PX: i64 = 2;
const FLUSH_INTERVAL: Duration = Duration::from_millis(1000);
const FLUSH_BATCH: usize = 1000;
/// 单次查询返回上限，防止一次拉爆 webview。
const MAX_POINTS: usize = 20_000;

static TX: OnceLock<Sender<Msg>> = OnceLock::new();
static ENABLED: AtomicBool = AtomicBool::new(false);
static STARTED: AtomicBool = AtomicBool::new(false);
static LAST_TS: AtomicI64 = AtomicI64::new(0);
static LAST_X: AtomicI64 = AtomicI64::new(0);
static LAST_Y: AtomicI64 = AtomicI64::new(0);

#[derive(Clone, Copy)]
struct Row {
    ts: i64,
    x: i32,
    y: i32,
}

enum Msg {
    Sample(Row),
    Flush,
    Wipe { reply: Sender<()> },
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn db_path() -> PathBuf {
    // 目录硬编码在 recorder 那边，绝不从配置里读
    crate::recorder::log_dir().join("trail.db")
}

// ---------------------------------------------------------------------------
// 对外入口
// ---------------------------------------------------------------------------

/// 投递一个移动采样点。**只允许在 dispatcher 线程调用。**
pub fn push(x: i32, y: i32) {
    if !ENABLED.load(Ordering::Relaxed) {
        return;
    }
    let ts = now_ms();
    let last_ts = LAST_TS.load(Ordering::Relaxed);
    // last_ts == 0 表示这轮还没采过，第一个点直接收
    if last_ts != 0 && ts - last_ts < SAMPLE_MIN_MS {
        return;
    }
    if last_ts != 0 {
        let lx = LAST_X.load(Ordering::Relaxed);
        let ly = LAST_Y.load(Ordering::Relaxed);
        if (x as i64 - lx).abs() < SAMPLE_MIN_PX && (y as i64 - ly).abs() < SAMPLE_MIN_PX {
            return;
        }
    }
    LAST_TS.store(ts, Ordering::Relaxed);
    LAST_X.store(x as i64, Ordering::Relaxed);
    LAST_Y.store(y as i64, Ordering::Relaxed);
    if let Some(tx) = TX.get() {
        let _ = tx.send(Msg::Sample(Row { ts, x, y }));
    }
}

pub fn is_enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// 启动时调用。`enabled` 必须是「总开关 && 未暂停 && 轨迹开关」三者与完的结果。
pub fn init(enabled: bool) {
    ENABLED.store(enabled, Ordering::SeqCst);
    if enabled {
        start_thread();
    }
}

pub fn set_enabled(enabled: bool) {
    ENABLED.store(enabled, Ordering::SeqCst);
    if enabled {
        start_thread();
    } else {
        // 关掉时把缓冲里的点落盘，别让用户以为「关了还写着」
        flush_now();
    }
}

fn flush_now() {
    if let Some(tx) = TX.get() {
        let _ = tx.send(Msg::Flush);
    }
}

fn start_thread() {
    if STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    let (tx, rx) = mpsc::channel::<Msg>();
    let _ = TX.set(tx);
    let dir = crate::recorder::log_dir();
    let spawned = std::thread::Builder::new()
        .name("kuro-input-trail".to_string())
        .spawn(move || TrailRecorder::new(dir).run(rx));
    if spawned.is_err() {
        eprintln!("[trail] 无法创建轨迹线程");
    }
}

/// 正常退出前调用。
pub fn shutdown() {
    ENABLED.store(false, Ordering::SeqCst);
    flush_now();
    std::thread::sleep(Duration::from_millis(80));
}

/// 清空日志目录之前调用：先关连接、删 trail.db，否则 Windows 上目录删不掉。
pub fn wipe() {
    if let Some(tx) = TX.get() {
        let (rtx, rrx) = mpsc::channel::<()>();
        if tx.send(Msg::Wipe { reply: rtx }).is_ok() {
            let _ = rrx.recv_timeout(Duration::from_secs(5));
        }
    }
    remove_db_files();
}

fn remove_db_files() {
    let path = db_path();
    let base = path.to_string_lossy().to_string();
    for suffix in ["", "-wal", "-shm"] {
        let _ = fs::remove_file(format!("{base}{suffix}"));
    }
}

/// 保留期清理：删掉 `cutoff_ts` 之前的采样点。失败只打日志，不影响明细清理。
pub fn prune_before(cutoff_ts: i64) {
    let path = db_path();
    if !path.exists() {
        return;
    }
    match Connection::open(&path) {
        Ok(conn) => {
            let _ = conn.busy_timeout(Duration::from_millis(2000));
            if let Err(e) = conn.execute("DELETE FROM trail WHERE ts < ?1", params![cutoff_ts]) {
                eprintln!("[trail] 清理旧采样失败: {e}");
            }
        }
        Err(e) => eprintln!("[trail] 打开 trail.db 失败，跳过清理: {e}"),
    }
}

// ---------------------------------------------------------------------------
// 写入线程
// ---------------------------------------------------------------------------

struct TrailRecorder {
    dir: PathBuf,
    conn: Option<Connection>,
    buf: Vec<Row>,
}

impl TrailRecorder {
    fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            conn: None,
            buf: Vec::with_capacity(FLUSH_BATCH),
        }
    }

    fn run(mut self, rx: Receiver<Msg>) {
        let mut last_flush = Instant::now();
        loop {
            let wait = FLUSH_INTERVAL.saturating_sub(last_flush.elapsed());
            match rx.recv_timeout(wait) {
                Ok(Msg::Sample(row)) => {
                    self.buf.push(row);
                    if self.buf.len() >= FLUSH_BATCH {
                        self.flush();
                        last_flush = Instant::now();
                    }
                }
                Ok(Msg::Flush) => {
                    self.flush();
                    last_flush = Instant::now();
                }
                Ok(Msg::Wipe { reply }) => {
                    self.conn = None;
                    self.buf.clear();
                    remove_db_files();
                    let _ = reply.send(());
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

    fn flush(&mut self) {
        if self.buf.is_empty() {
            return;
        }
        match self.write() {
            Ok(()) => self.buf.clear(),
            Err(e) => {
                eprintln!("[trail] 写入失败，保留缓冲待重试: {e}");
                if self.buf.len() > FLUSH_BATCH * 8 {
                    let drop_n = self.buf.len() / 2;
                    // 丢掉最老的一半，别让一条坏库把内存吃光
                    let _: Vec<Row> = self.buf.drain(0..drop_n).collect();
                }
            }
        }
    }

    fn write(&mut self) -> Result<(), String> {
        if self.conn.is_none() {
            self.open_conn()?;
        }
        let Some(conn) = self.conn.as_mut() else {
            return Err("轨迹库未就绪".to_string());
        };
        let tx = conn.transaction().map_err(|e| e.to_string())?;
        {
            let mut stmt = tx
                .prepare("INSERT INTO trail(ts, x, y) VALUES(?1, ?2, ?3)")
                .map_err(|e| e.to_string())?;
            for row in &self.buf {
                stmt.execute(params![row.ts, row.x, row.y])
                    .map_err(|e| e.to_string())?;
            }
        }
        tx.commit().map_err(|e| e.to_string())
    }

    fn open_conn(&mut self) -> Result<(), String> {
        fs::create_dir_all(&self.dir).map_err(|e| format!("创建日志目录失败: {e}"))?;
        let conn = Connection::open(self.dir.join("trail.db"))
            .map_err(|e| format!("打开 trail.db 失败: {e}"))?;
        conn.busy_timeout(Duration::from_millis(3000))
            .map_err(|e| e.to_string())?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA wal_autocheckpoint = 64;
             CREATE TABLE IF NOT EXISTS trail (
                 ts INTEGER NOT NULL,
                 x  INTEGER NOT NULL,
                 y  INTEGER NOT NULL
             );
             CREATE INDEX IF NOT EXISTS idx_trail_ts ON trail(ts);",
        )
        .map_err(|e| format!("初始化 trail.db 失败: {e}"))?;
        self.conn = Some(conn);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// 查询（UI 线程，独立只写连接之外的读写连接）
// ---------------------------------------------------------------------------

fn open_read() -> Result<Connection, String> {
    let path = db_path();
    if !path.exists() {
        return Err("尚无轨迹记录".to_string());
    }
    let conn = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_WRITE)
        .map_err(|e| format!("打开 trail.db 失败: {e}"))?;
    conn.busy_timeout(Duration::from_millis(2000))
        .map_err(|e| e.to_string())?;
    Ok(conn)
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TrailRange {
    pub first_ts: Option<i64>,
    pub last_ts: Option<i64>,
    pub points: i64,
}

pub fn range() -> TrailRange {
    let mut out = TrailRange {
        first_ts: None,
        last_ts: None,
        points: 0,
    };
    let Ok(conn) = open_read() else {
        return out;
    };
    if let Ok((min, max, n)) = conn.query_row(
        "SELECT MIN(ts), MAX(ts), COUNT(*) FROM trail",
        [],
        |r| {
            Ok((
                r.get::<_, Option<i64>>(0)?,
                r.get::<_, Option<i64>>(1)?,
                r.get::<_, i64>(2)?,
            ))
        },
    ) {
        out.first_ts = min;
        out.last_ts = max;
        out.points = n;
    }
    out
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TrailPoint {
    pub t: i64,
    pub x: i32,
    pub y: i32,
}

/// 取 `[from_ts, to_ts]` 区间内的采样点，超过 `max_points` 时按 rowid 等间隔抽稀。
///
/// 抽稀用 `rowid % stride` 而不是随机 / 排序取前 N：插入是顺序的，
/// 取模天然均匀，而且能用上 rowid 索引，几百万行也是毫秒级。
pub fn points(from_ts: i64, to_ts: i64, max_points: usize) -> Result<Vec<TrailPoint>, String> {
    if to_ts <= from_ts {
        return Ok(Vec::new());
    }
    let path = db_path();
    if !path.exists() {
        return Ok(Vec::new());
    }
    let conn = open_read()?;
    let max = max_points.clamp(500, MAX_POINTS);
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM trail WHERE ts >= ?1 AND ts <= ?2",
            params![from_ts, to_ts],
            |r| r.get(0),
        )
        .unwrap_or(0);
    if count <= 0 {
        return Ok(Vec::new());
    }
    let stride = ((count as usize) / max + 1) as i64;
    let mut stmt = conn
        .prepare(
            "SELECT ts, x, y FROM trail \
             WHERE ts >= ?1 AND ts <= ?2 AND (rowid % ?3) = 0 \
             ORDER BY ts LIMIT ?4",
        )
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map(params![from_ts, to_ts, stride, max as i64], |r| {
            Ok(TrailPoint {
                t: r.get(0)?,
                x: r.get(1)?,
                y: r.get(2)?,
            })
        })
        .map_err(|e| e.to_string())?;
    Ok(rows.flatten().collect())
}
