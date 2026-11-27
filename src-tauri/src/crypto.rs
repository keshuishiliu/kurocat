//! 录入数据加密层：整库 SQLCipher + 随机密钥（系统保管）+ 同步包 AES-256-GCM。
//!
//! # 威胁模型与设计
//!
//! 1. **库密钥与「录入设置密码」彻底分离。**
//!    - 库密钥：首次需要时随机生成 256 位，交给系统凭据库保管
//!      （Windows=凭据管理器 DPAPI / macOS=Keychain / Linux=Secret Service）。
//!      每次启动程序自动取回，**录制完全不受影响**；`.db` 文件被拷到别的
//!      电脑或别的 Windows 账户也解不开。
//!    - 设置密码：PBKDF2 哈希存在配置里，只做统计窗口「录入设置区」的门禁。
//!      忘记密码不丢数据，重置即可。密码永不参与加解密。
//! 2. **同步包**用 `PBKDF2(WebDAV密码)` 派生密钥加密（AES-256-GCM）。
//!    两台机器同步用的是同一个 WebDAV 账户 ⇒ 密码一致 ⇒ 互相能解，
//!    库密钥因此**永远不需要跨机器传递**。
//! 3. 明文旧库在第一次加密打开时自动迁移（`sqlcipher_export`），迁移成功后
//!    原明文文件必须删除——留备份等于没加密。

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::Engine as _;
use rand::RngCore;
use sha2::Sha256;
use std::path::Path;
use std::sync::OnceLock;

const KEY_LEN: usize = 32;
/// PBKDF2 迭代次数（OWASP 2023 对 PBKDF2-SHA256 的推荐下限 60 万）。
const KDF_ITER: u32 = 600_000;
const KEYRING_SERVICE: &str = "kuro-cat-pet";
const KEYRING_USER: &str = "input-log-db-key";
/// 同步包信封格式标识。没有这个头的包按旧版明文 JSON 处理。
pub const PACK_MAGIC: &str = "KUROPACK1";
/// 调试/测试逃生口：设置此环境变量（64 位 hex）时跳过 keyring。
/// 正常用户不会设置；CI 无 Secret Service 时测试也走这条路。
const ENV_DB_KEY: &str = "KURO_DB_KEY";

// ---------------------------------------------------------------------------
// 库密钥（系统凭据库保管）
// ---------------------------------------------------------------------------

static KEY_CACHE: OnceLock<Result<String, String>> = OnceLock::new();

/// 取（或首次创建）录入库密钥，64 位 hex 字符串。
///
/// 进程内缓存：recorder 线程与 sync 线程可能并发首次调用，
/// 若不加缓存，两个线程会各生成一把随机钥匙互相覆盖，后开库的线程直接解密失败。
pub fn db_key_hex() -> Result<String, String> {
    if let Some(cached) = KEY_CACHE.get() {
        return cached.clone();
    }
    let res = load_or_create_db_key();
    let _ = KEY_CACHE.set(res.clone());
    res
}

fn load_or_create_db_key() -> Result<String, String> {
    if let Ok(env_key) = std::env::var(ENV_DB_KEY) {
        let k = env_key.trim().to_ascii_lowercase();
        if k.len() == KEY_LEN * 2 && k.chars().all(|c| c.is_ascii_hexdigit()) {
            return Ok(k);
        }
        return Err(format!("{ENV_DB_KEY} 格式不合法（需要 {0} 位 hex）", KEY_LEN * 2));
    }
    let entry = keyring::Entry::new(KEYRING_SERVICE, KEYRING_USER)
        .map_err(|e| format!("无法访问系统凭据库: {e}"))?;
    if let Ok(existing) = entry.get_password() {
        let k = existing.trim().to_ascii_lowercase();
        if k.len() == KEY_LEN * 2 && k.chars().all(|c| c.is_ascii_hexdigit()) {
            return Ok(k);
        }
        // 存了垃圾值（手工误动过凭据管理器）：重新生成覆盖，旧库认命作废。
        eprintln!("[crypto] 系统凭据库中的库密钥格式异常，重新生成（旧加密库将无法打开）");
    }
    let mut buf = [0u8; KEY_LEN];
    rand::thread_rng().fill_bytes(&mut buf);
    let hex = to_hex(&buf);
    entry
        .set_password(&hex)
        .map_err(|e| format!("无法写入系统凭据库: {e}"))?;
    Ok(hex)
}

pub fn apply_key(conn: &rusqlite::Connection, key_hex: &str) -> Result<(), String> {
    conn.execute_batch(&format!("PRAGMA key = \"x'{key_hex}'\";"))
        .map_err(|e| format!("应用库密钥失败: {e}"))
}

/// 打开（必要时创建）录入目录下的一个 SQLite 库。
///
/// - 新文件：直接建为 SQLCipher 加密库。
/// - 明文旧库（历史版本留下的）：自动迁移为加密库，迁移成功后删除明文原件。
/// - 加密库但密钥不匹配：明确报错（keyring 被清空/换机器），绝不静默降级为明文。
pub fn open_log_db(path: &Path, label: &str) -> Result<rusqlite::Connection, String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("创建日志目录失败: {e}"))?;
    }
    let key = db_key_hex()?;
    let opened = {
        let conn = raw_open(path, label)?;
        apply_key(&conn, &key)?;
        if probe(&conn).is_ok() {
            Some(conn)
        } else {
            None
        }
    }; // 打不开的连接在这里关闭，腾出文件句柄给迁移
    if let Some(conn) = opened {
        return Ok(conn);
    }
    if !is_plaintext_db(path) {
        return Err(format!(
            "{label} 无法用保管密钥打开：密钥不匹配或文件损坏。\
             若确认要放弃这份数据，删除该 .db 文件后可重新开始。"
        ));
    }
    migrate_plaintext_db(path, &key, label)?;
    let conn = raw_open(path, label)?;
    apply_key(&conn, &key)?;
    probe(&conn).map_err(|e| format!("{label} 迁移后仍无法打开: {e}"))?;
    Ok(conn)
}

/// 用显式密钥打开并验证一个库。备份恢复用：验证快照文件没坏、密钥对得上。
/// 与 open_log_db 不同：不迁移、不读 keyring，密钥不匹配直接报错。
pub fn open_with_explicit_key(
    path: &Path,
    key_hex: &str,
    label: &str,
) -> Result<rusqlite::Connection, String> {
    let conn = raw_open(path, label)?;
    apply_key(&conn, key_hex)?;
    probe(&conn).map_err(|_| {
        format!(
            "{label} 无法用给定密钥打开：文件损坏或密钥不匹配"
        )
    })?;
    Ok(conn)
}

/// 把 src 库（密钥 src_key）整体导出为 dst 库（密钥 dst_key）。
/// 跨机恢复用：快照库是对端机器的密钥，落回本机前先改密到本机密钥。
pub fn rekey_db(
    src: &Path,
    src_key: &str,
    dst: &Path,
    dst_key: &str,
    label: &str,
) -> Result<(), String> {
    if dst.exists() {
        std::fs::remove_file(dst).map_err(|e| format!("清理 {label} 临时文件失败: {e}"))?;
    }
    let conn = open_with_explicit_key(src, src_key, label)?;
    let dst_q = dst.to_string_lossy().replace('\\', "/");
    let uv: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .map_err(|e| format!("读取 {label} user_version 失败: {e}"))?;
    conn.execute_batch(&format!(
        "ATTACH DATABASE '{dst_q}' AS rekey KEY \"x'{dst_key}'\";
         SELECT sqlcipher_export('rekey');
         PRAGMA rekey.user_version = {uv};
         DETACH DATABASE rekey;"
    ))
    .map_err(|e| format!("{label} 改密失败: {e}"))?;
    // 验证改密产物，失败即报错（调用方据以中止恢复）
    drop(conn);
    open_with_explicit_key(dst, dst_key, label).map(|_| ())
}

/// 校验一个 64 位 hex 库密钥字符串。
pub fn is_valid_key_hex(s: &str) -> bool {
    s.len() == KEY_LEN * 2 && s.chars().all(|c| c.is_ascii_hexdigit())
}

fn raw_open(path: &Path, label: &str) -> Result<rusqlite::Connection, String> {
    rusqlite::Connection::open(path).map_err(|e| format!("打开 {label} 失败: {e}"))
}

/// SQLCipher 在第一次真正读页（而不是 open/PRAGMA key）时才报「not a database」。
fn probe(conn: &rusqlite::Connection) -> Result<(), String> {
    conn.query_row("SELECT COUNT(*) FROM sqlite_master", [], |r| r.get::<_, i64>(0))
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// 明文库文件头是固定的 "SQLite format 3\0"；SQLCipher 加密库前 16 字节是随机盐。
fn is_plaintext_db(path: &Path) -> bool {
    use std::io::Read;
    let Ok(mut f) = std::fs::File::open(path) else {
        return false;
    };
    let mut header = [0u8; 16];
    match f.read_exact(&mut header) {
        Ok(()) => header == *b"SQLite format 3\0",
        Err(_) => false,
    }
}

/// 明文旧库 → SQLCipher 加密库。迁移成功后删除明文原件（含 -wal/-shm）。
///
/// 失败时保留原文件不动，报错上抛——绝不出现「原件被删但加密库没建成」的状态。
pub fn migrate_plaintext_db(path: &Path, key_hex: &str, label: &str) -> Result<(), String> {
    if !is_plaintext_db(path) {
        return Err(format!("{label} 不是明文 SQLite 库，拒绝迁移"));
    }
    // 1. 把 WAL 收拢进主库，迁移才能看到完整数据
    {
        let conn = raw_open(path, label)?;
        conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
            .map_err(|e| format!("checkpoint {label} 失败: {e}"))?;
    }
    // 2. 明文连接上 ATTACH 加密目标并导出
    let tmp = path.with_extension(format!(
        "enc-migrate-{}.tmp",
        std::process::id()
    ));
    {
        let conn = raw_open(path, label)?;
        // PRAGMA 不接受子查询表达式，user_version 只能先读出来再赋字面量
        let uv: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .map_err(|e| format!("读取 {label} user_version 失败: {e}"))?;
        let tmp_q = tmp.to_string_lossy().replace('\\', "/");
        conn.execute_batch(&format!(
            "ATTACH DATABASE '{tmp_q}' AS enc KEY \"x'{key_hex}'\";
             SELECT sqlcipher_export('enc');
             PRAGMA enc.user_version = {uv};
             DETACH DATABASE enc;"
        ))
        .map_err(|e| format!("导出 {label} 到加密库失败: {e}"))?;
    }
    // 3. 验证：临时库能用密钥打开
    {
        let conn = raw_open(&tmp, label)?;
        apply_key(&conn, key_hex)?;
        probe(&conn).map_err(|e| format!("{label} 加密副本验证失败: {e}"))?;
    }
    // 4. 原子替换：删原件与 WAL 残留 → 改名。此后明文数据只剩这一处入口。
    std::fs::remove_file(path).map_err(|e| format!("移除明文 {label} 失败: {e}"))?;
    for suffix in ["-wal", "-shm"] {
        let mut s = path.as_os_str().to_os_string();
        s.push(suffix);
        let _ = std::fs::remove_file(Path::new(&s));
    }
    std::fs::rename(&tmp, path).map_err(|e| format!("启用加密 {label} 失败: {e}"))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// 录入设置密码（门禁用，不参与加解密）
// ---------------------------------------------------------------------------

/// 生成密码哈希：`pbkdf2-sha256$<iter>$<salt_b64>$<hash_b64>`。只存这个，永不存明文。
pub fn hash_password(password: &str) -> String {
    let mut salt = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut salt);
    let mut out = [0u8; KEY_LEN];
    pbkdf2::pbkdf2_hmac::<Sha256>(password.as_bytes(), &salt, KDF_ITER, &mut out);
    format!(
        "pbkdf2-sha256${KDF_ITER}${}${}",
        b64_encode(&salt),
        b64_encode(&out)
    )
}

/// 校验密码。哈希格式不合法一律返回 false。
pub fn verify_password(password: &str, stored: &str) -> bool {
    let parts: Vec<&str> = stored.split('$').collect();
    if parts.len() != 4 || parts[0] != "pbkdf2-sha256" {
        return false;
    }
    let Ok(iter) = parts[1].parse::<u32>() else {
        return false;
    };
    if !(1_000..=10_000_000).contains(&iter) {
        return false;
    }
    let (Ok(salt), Ok(expect)) = (b64_decode(parts[2]), b64_decode(parts[3])) else {
        return false;
    };
    if salt.is_empty() || expect.len() != KEY_LEN {
        return false;
    }
    let mut got = [0u8; KEY_LEN];
    pbkdf2::pbkdf2_hmac::<Sha256>(password.as_bytes(), &salt, iter, &mut got);
    constant_time_eq(&got, &expect)
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

// ---------------------------------------------------------------------------
// 同步包加密（密钥 = PBKDF2(WebDAV 密码)）
// ---------------------------------------------------------------------------

/// 把合并包明文 JSON 加密成信封 JSON（同规模、可直接 PUT）。
pub fn encrypt_pack(plain: &[u8], password: &str) -> Result<Vec<u8>, String> {
    let password = password.trim();
    if password.is_empty() {
        return Err("WebDAV 密码为空，无法加密同步包".to_string());
    }
    let mut salt = [0u8; 16];
    let mut nonce_b = [0u8; 12];
    rand::thread_rng().fill_bytes(&mut salt);
    rand::thread_rng().fill_bytes(&mut nonce_b);
    let mut key = [0u8; KEY_LEN];
    pbkdf2::pbkdf2_hmac::<Sha256>(password.as_bytes(), &salt, KDF_ITER, &mut key);
    let cipher = Aes256Gcm::new((&key).into());
    let ct = cipher
        .encrypt(Nonce::from_slice(&nonce_b), plain)
        .map_err(|_| "同步包加密失败".to_string())?;
    serde_json::to_vec(&serde_json::json!({
        "magic": PACK_MAGIC,
        "iter": KDF_ITER,
        "salt": b64_encode(&salt),
        "nonce": b64_encode(&nonce_b),
        "ct": b64_encode(&ct),
    }))
    .map_err(|e| format!("序列化同步包信封失败: {e}"))
}

/// 解密同步包。
///
/// - 信封 + 密码正确 → `Ok(Some(明文 JSON))`
/// - 不是信封（旧版明文包 / 别人的文件）→ `Ok(None)`，由调用方按明文处理
/// - 是信封但解不开（密码不一致 / 数据被改）→ `Err`
pub fn try_decrypt_pack(data: &[u8], password: &str) -> Result<Option<Vec<u8>>, String> {
    let v: serde_json::Value = match serde_json::from_slice(data) {
        Ok(v) => v,
        Err(_) => return Ok(None),
    };
    if v.get("magic").and_then(|m| m.as_str()) != Some(PACK_MAGIC) {
        return Ok(None);
    }
    let password = password.trim();
    if password.is_empty() {
        return Err("同步包已加密但本机 WebDAV 密码为空，无法解密".to_string());
    }
    let get_b64 = |k: &str| -> Result<Vec<u8>, String> {
        match v.get(k).and_then(|x| x.as_str()) {
            Some(s) => b64_decode(s),
            None => Err(format!("同步包缺少 {k} 字段")),
        }
    };
    let salt = get_b64("salt")?;
    let nonce_b = get_b64("nonce")?;
    let ct = get_b64("ct")?;
    let iter = v
        .get("iter")
        .and_then(|x| x.as_u64())
        .ok_or_else(|| "同步包缺少 iter 字段".to_string())?
        .min(10_000_000) as u32;
    let mut key = [0u8; KEY_LEN];
    pbkdf2::pbkdf2_hmac::<Sha256>(password.as_bytes(), &salt, iter.max(1), &mut key);
    let cipher = Aes256Gcm::new((&key).into());
    let nonce = Nonce::from_slice(&nonce_b); // 长度不对会在 decrypt 报错
    let plain = cipher
        .decrypt(nonce, ct.as_slice())
        .map_err(|_| "同步包解密失败：请确认两台机器的 WebDAV 密码一致，或该包已损坏".to_string())?;
    Ok(Some(plain))
}

// ---------------------------------------------------------------------------
// 小工具
// ---------------------------------------------------------------------------

fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn b64_encode(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn b64_decode(s: &str) -> Result<Vec<u8>, String> {
    base64::engine::general_purpose::STANDARD
        .decode(s)
        .map_err(|e| format!("base64 解码失败: {e}"))
}

// ---------------------------------------------------------------------------
// 测试（不依赖 keyring：迁移与包加解密都直接传显式密钥/密码）
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!(
            "kuro-crypto-{}-{}",
            tag,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn test_key() -> String {
        to_hex(b"0123456789abcdef0123456789abcdef")
    }

    #[test]
    fn password_hash_roundtrip_and_reject() {
        let h = hash_password("s3cret!密");
        assert!(verify_password("s3cret!密", &h));
        assert!(!verify_password("wrong", &h));
        assert!(!verify_password("s3cret!密", "garbage"));
        // 两次哈希盐不同
        assert_ne!(hash_password("x"), hash_password("x"));
    }

    #[test]
    fn pack_encrypt_decrypt_roundtrip() {
        let plain = br#"{"events":[1,2,3],"exported_at":123}"#;
        let env = encrypt_pack(plain, "dav-password").unwrap();
        // 信封是合法 JSON 且带 magic
        let v: serde_json::Value = serde_json::from_slice(&env).unwrap();
        assert_eq!(v["magic"], PACK_MAGIC);
        // 解密还原
        let out = try_decrypt_pack(&env, "dav-password").unwrap().unwrap();
        assert_eq!(out.as_slice(), plain);
        // 错误密码报错，而不是返回明文
        assert!(try_decrypt_pack(&env, "wrong").is_err());
        // 明文旧包 → None
        assert!(try_decrypt_pack(plain, "dav-password").unwrap().is_none());
        // 篡改密文 → 报错。注意得改信封里的 ct 字段本身——
        // 信封 JSON 的结尾字节（引号/花括号）翻转会直接让 JSON 解析失败，测不到 GCM。
        let mut v: serde_json::Value = serde_json::from_slice(&env).unwrap();
        let mut ct = b64_decode(v["ct"].as_str().unwrap()).unwrap();
        let last = ct.len() - 1;
        ct[last] ^= 0xff;
        v["ct"] = serde_json::Value::String(b64_encode(&ct));
        let tampered = serde_json::to_vec(&v).unwrap();
        assert!(try_decrypt_pack(&tampered, "dav-password").is_err());
    }

    #[test]
    fn plaintext_db_migrates_and_reopens_encrypted() {
        let dir = temp_dir("migrate");
        let db = dir.join("events.db");
        // 造一个明文库
        {
            let conn = rusqlite::Connection::open(&db).unwrap();
            conn.execute_batch(
                "CREATE TABLE t(a INTEGER); INSERT INTO t VALUES (7), (9);",
            )
            .unwrap();
        }
        assert!(is_plaintext_db(&db));
        let key = test_key();
        migrate_plaintext_db(&db, &key, "events.db").unwrap();
        assert!(!is_plaintext_db(&db));
        // 迁移后数据完整、必须带密钥才能读
        {
            let conn = rusqlite::Connection::open(&db).unwrap();
            assert!(probe(&conn).is_err(), "无密钥应读不出加密库");
        }
        {
            let conn = rusqlite::Connection::open(&db).unwrap();
            apply_key(&conn, &key).unwrap();
            let n: i64 = conn
                .query_row("SELECT COUNT(*) FROM t", [], |r| r.get(0))
                .unwrap();
            assert_eq!(n, 2);
        }
        // 明文备份/wal 不能残留
        assert!(!dir.join("events.db-wal").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn open_log_db_creates_encrypted_and_reopens() {
        // 走 ENV 逃生口绕开 keyring
        // 注意：cargo test 并行跑多个用例时 env 是进程级，这里只在本用例内读写。
        // open_log_db 的 KEY_CACHE 是 OnceLock，首个用例先把结果定下来，
        // 所以本用例之前先把 env 设好。
        std::env::set_var(ENV_DB_KEY, test_key());
        let dir = temp_dir("openlog");
        let db = dir.join("meta.db");
        {
            let conn = open_log_db(&db, "meta.db").unwrap();
            conn.execute_batch("CREATE TABLE IF NOT EXISTS t(a INTEGER);")
                .unwrap();
        }
        assert!(!is_plaintext_db(&db));
        {
            // 重开（含 KEY_CACHE 命中路径）
            let conn = open_log_db(&db, "meta.db").unwrap();
            conn.execute_batch("INSERT INTO t VALUES (1);").unwrap();
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rekey_db_moves_data_between_keys() {
        let dir = temp_dir("rekey");
        let db = dir.join("events.db");
        let key_a = to_hex(b"aaaaaaaabbbbbbbbccccccccdddddddd");
        let key_b = to_hex(b"11111111222222223333333344444444");
        // 用 key_a 建库
        {
            let conn = open_with_explicit_key(&db, &key_a, "events.db").unwrap();
            conn.execute_batch("CREATE TABLE t(a); INSERT INTO t VALUES (42);")
                .unwrap();
        }
        let out = dir.join("events.rekeyed.db");
        rekey_db(&db, &key_a, &out, &key_b, "events.db").unwrap();
        // 旧 key 解不开新库，新 key 数据完整
        {
            let conn = rusqlite::Connection::open(&out).unwrap();
            assert!(
                apply_key(&conn, &key_a).and_then(|_| probe(&conn)).is_err(),
                "旧密钥不应能打开改密后的库"
            );
        }
        {
            let conn = open_with_explicit_key(&out, &key_b, "events.db").unwrap();
            let n: i64 = conn
                .query_row("SELECT COUNT(*) FROM t", [], |r| r.get(0))
                .unwrap();
            assert_eq!(n, 1);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn key_format_validation() {
        let mut salt = [0u8; 16];
        rand::thread_rng().fill_bytes(&mut salt);
        let mut out = [0u8; KEY_LEN];
        pbkdf2::pbkdf2_hmac::<Sha256>(b"pw", &salt, 1000, &mut out);
        let h = format!("pbkdf2-sha256$1000${}${}", b64_encode(&salt), b64_encode(&out));
        assert!(verify_password("pw", &h));
        // 迭代数越界拒绝
        let h2 = format!("pbkdf2-sha256$5${}${}", b64_encode(&salt), b64_encode(&out));
        assert!(!verify_password("pw", &h2));
    }
}
