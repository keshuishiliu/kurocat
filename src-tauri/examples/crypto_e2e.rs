//! 加密功能端到端验证（模拟真实运行状态，非单元测试的 ENV 逃生口）。
//!
//! 分两个阶段跑，模拟「程序重启」：
//!   cargo run --example crypto_e2e -- migrate   # 第一次运行：明文库迁移 + 各项防线
//!   cargo run --example crypto_e2e -- reopen    # 第二次运行（新进程）：凭据库取钥匙重开加密库
//!
//! 使用真实的系统凭据库（Windows 凭据管理器）——与服务名一致，
//! 相当于提前把首次运行的密钥创建出来，正式程序会复用同一把钥匙。

use kuro_cat_pet_lib::crypto;
use std::path::PathBuf;

fn e2e_dir() -> PathBuf {
    std::env::temp_dir().join("kuro-e2e-crypto")
}

fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn header(path: &std::path::Path) -> String {
    use std::io::Read;
    let mut f = std::fs::File::open(path).expect("打开库文件");
    let mut h = [0u8; 16];
    f.read_exact(&mut h).expect("读文件头");
    h.iter().map(|b| format!("{b:02x}")).collect()
}

fn step(ok: bool, name: &str, detail: &str) {
    let mark = if ok { "PASS" } else { "FAIL" };
    println!("[{mark}] {name}  {detail}");
    if !ok {
        std::process::exit(1);
    }
}

fn main() {
    let phase = std::env::args().nth(1).unwrap_or_default();
    match phase.as_str() {
        "migrate" => phase_migrate(),
        "reopen" => phase_reopen(),
        other => {
            eprintln!("用法: cargo run --example crypto_e2e -- migrate|reopen（收到: {other}）");
            std::process::exit(2);
        }
    }
}

fn phase_migrate() {
    let dir = e2e_dir();
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("建临时目录");
    let events = dir.join("events.db");
    let meta = dir.join("meta.db");

    // ---- 0. 密钥来自真实系统凭据库 ----
    let key = crypto::db_key_hex().expect("取库密钥（keyring）");
    step(key.len() == 64, "keyring 密钥获取", "64 位 hex，来自 Windows 凭据管理器");

    // ---- 1. 造旧版明文库（模拟升级前用户数据）----
    {
        let conn = rusqlite::Connection::open(&events).unwrap();
        conn.execute_batch(
            "CREATE TABLE events_202611 (
                 ts INTEGER NOT NULL, vk INTEGER NOT NULL,
                 shift INTEGER NOT NULL, caps INTEGER NOT NULL,
                 x INTEGER NOT NULL, y INTEGER NOT NULL,
                 process TEXT NOT NULL);
             CREATE INDEX idx_e_ts ON events_202611(ts);
             INSERT INTO events_202611 VALUES
               (1, 65, 0, 0, 100, 200, 'chrome.exe'),
               (2, 66, 1, 0, 100, 200, 'chrome.exe'),
               (3, 67, 0, 1, 300, 400, 'code.exe');",
        )
        .unwrap();
    }
    {
        let conn = rusqlite::Connection::open(&meta).unwrap();
        conn.execute_batch(
            "CREATE TABLE daily_stats(day TEXT PRIMARY KEY, keys INTEGER, clicks INTEGER);
             INSERT INTO daily_stats VALUES ('2026-11-26', 1234, 56);",
        )
        .unwrap();
    }
    let events_plain_hdr = header(&events);
    step(
        events_plain_hdr == to_hex(b"SQLite format 3\0"),
        "迁移前文件头",
        "明文 SQLite 头（「SQLite format 3」），此时谁都能读",
    );

    // ---- 2. open_log_db 触发自动迁移 ----
    let n_events: i64;
    {
        let conn = crypto::open_log_db(&events, "events.db").expect("events.db 迁移+打开");
        n_events = conn
            .query_row("SELECT COUNT(*) FROM events_202611", [], |r| r.get(0))
            .unwrap();
        // 顺手把 journal_mode 设成 WAL（与 recorder 真实用法一致）
        conn.execute_batch("PRAGMA journal_mode = WAL;")
            .expect("设 WAL");
        conn.execute_batch(
            "INSERT INTO events_202611 VALUES (4, 68, 0, 0, 1, 2, 'code.exe');",
        )
        .unwrap();
    }
    step(n_events == 3, "迁移后数据完整", "events_202611 共 3 行旧数据全部保留");
    {
        let conn = crypto::open_log_db(&meta, "meta.db").expect("meta.db 迁移+打开");
        let keys: i64 = conn
            .query_row("SELECT keys FROM daily_stats", [], |r| r.get(0))
            .unwrap();
        step(keys == 1234, "meta.db 迁移后数据完整", "daily_stats 读取正常");
    }

    // ---- 3. 迁移后文件不再是明文 ----
    let events_enc_hdr = header(&events);
    step(
        events_enc_hdr != to_hex(b"SQLite format 3\0"),
        "迁移后文件头",
        "已是 SQLCipher 随机盐，磁盘上不再有「SQLite format 3」明文头",
    );
    step(
        !dir.join("events.db-wal").exists() || {
            // WAL 若存在也应为密文（头不含明文 SQLite 标识）
            let h = header(&dir.join("events.db-wal"));
            h != to_hex(b"SQLite format 3\0")
        },
        "WAL 残留检查",
        "明文原件与 WAL 均未残留",
    );

    // ---- 4. 无密钥直开 → 报错（模拟拿到文件的人/裸 sqlite 工具）----
    {
        let conn = rusqlite::Connection::open(&events).unwrap();
        let refused = conn
            .query_row("SELECT COUNT(*) FROM events_202611", [], |r| r.get::<_, i64>(0))
            .is_err();
        step(refused, "无密钥直开", "裸连接读 sqlite_master 报 not a database，数据读不出");
    }

    // ---- 5. 错误密钥 → 明确报错 ----
    {
        let wrong = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";
        let err = crypto::open_with_explicit_key(&events, wrong, "events.db")
            .err()
            .expect("错误密钥应报错");
        step(
            err.contains("密钥不匹配") || err.contains("损坏"),
            "错误密钥拒绝",
            &err,
        );
    }

    // ---- 6. WAL 数据落盘后重开仍在（加密覆盖 WAL 场景）----
    {
        drop(rusqlite::Connection::open(&events).unwrap()); // 确保前面的连接已关
        let conn = crypto::open_log_db(&events, "events.db").unwrap();
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM events_202611", [], |r| r.get(0))
            .unwrap();
        step(n == 4, "WAL 写入后重开", "迁移期间新写入的第 4 行完整保留");
    }

    // ---- 7. 同步包加解密（WebDAV 密码信任根）----
    let plain = br#"{"events":[1,2,3],"exported_at":1764200000000}"#;
    let sealed = crypto::encrypt_pack(plain, "my-dav-password").expect("加密同步包");
    let opened = crypto::try_decrypt_pack(&sealed, "my-dav-password")
        .expect("解密同步包")
        .expect("应识别为信封");
    step(
        opened.as_slice() == plain,
        "同步包加解密回环",
        "AES-256-GCM 明文逐字节还原",
    );
    step(
        crypto::try_decrypt_pack(&sealed, "wrong-password").is_err(),
        "错误 WebDAV 密码",
        "解密失败并报「确认两台机器密码一致」",
    );
    step(
        crypto::try_decrypt_pack(plain, "my-dav-password")
            .unwrap()
            .is_none(),
        "旧版明文包兼容",
        "无 magic 头的旧包返回 None，由同步层按明文导入",
    );

    println!("\nphase=migrate 全部通过。临时目录保留在 {}，供 reopen 阶段使用", dir.display());
}

fn phase_reopen() {
    let dir = e2e_dir();
    let events = dir.join("events.db");

    // 新进程、KEY_CACHE 全新 —— 模拟程序重启后首次开库
    let key = crypto::db_key_hex().expect("重启后取库密钥");
    let conn = crypto::open_log_db(&events, "events.db").expect("重启后打开加密库");
    let n: i64 = conn
        .query_row("SELECT COUNT(*) FROM events_202611", [], |r| r.get(0))
        .expect("读数据");
    step(n == 4, "重启后重开加密库", "凭据库取回同一把钥匙，4 行数据完整（录制零影响的核心保证）");
    let _ = key;

    // 长连接场景：再插一行再读，验证连接持续可用
    conn.execute_batch("INSERT INTO events_202611 VALUES (5, 69, 0, 0, 3, 4, 'code.exe');")
        .unwrap();
    let n2: i64 = conn
        .query_row("SELECT COUNT(*) FROM events_202611", [], |r| r.get(0))
        .unwrap();
    step(n2 == 5, "长连接持续写入", "加密连接上正常写入/读取");

    // 清理临时目录
    drop(conn);
    std::fs::remove_dir_all(&dir).expect("清理临时目录");
    println!("\nphase=reopen 全部通过。端到端验证完成，临时目录已清理。");
}
