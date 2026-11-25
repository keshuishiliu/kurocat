use serde::{Deserialize, Serialize};
use std::fs;
#[cfg(windows)]
use std::path::Path;
use std::path::PathBuf;
#[cfg(windows)]
use std::sync::OnceLock;

fn default_cat_breed() -> String {
    "black".to_string()
}

fn default_mcp_enabled() -> bool {
    true
}

fn default_privacy_password_guard() -> bool {
    true
}

fn default_mcp_port() -> u16 {
    27420
}

fn default_obs_enabled() -> bool {
    false
}

fn default_obs_port() -> u16 {
    27421
}

fn default_obs_hide_pet() -> bool {
    false
}

fn default_input_log_active_gap() -> u32 {
    5
}

// ---- 数据同步（WebDAV 直连，默认关闭） ----

fn default_sync_enabled() -> bool {
    false
}

fn default_sync_server() -> String {
    "https://dav.jianguoyun.com/dav/".to_string()
}

fn default_sync_interval() -> u32 {
    30
}

/// 相邻两次输入间隔不超过该分钟数，即视为连续活跃。只影响统计口径，不删除数据。
pub fn normalize_input_log_active_gap(raw: u32) -> u32 {
    raw.clamp(1, 120)
}

/// 保留天数。0 = 永久保留。正值会不可逆地删除更早的数据。
pub fn normalize_input_log_retention(raw: u32) -> u32 {
    match raw {
        0 => 0,
        d => d.clamp(1, 36500),
    }
}

/// 数据同步设置。只存「连哪个网盘、多久同步一次」；
/// 机器身份（machine_id）、水位线等同步状态都在 meta.db 里，不在这里。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncSettings {
    #[serde(default = "default_sync_enabled")]
    pub enabled: bool,
    /// WebDAV 基础地址。默认坚果云；任何标准 WebDAV 服务都能用。
    #[serde(default = "default_sync_server")]
    pub server: String,
    #[serde(default)]
    pub account: String,
    /// 网盘的「应用专用密码」，不是登录密码。
    #[serde(default)]
    pub password: String,
    /// 本机显示名，只用于界面展示，云端目录用 machine_id。
    #[serde(default)]
    pub machine_label: String,
    /// 自动同步周期（分钟）。0 = 仅手动。启动 / 关闭 / 手动三种时机不受此项影响，
    /// 但都要求 enabled 打开。
    #[serde(default = "default_sync_interval")]
    pub interval_minutes: u32,
    /// 云端合并包保留天数：超期旧包在每次同步后由本机自己删除（只删自己名下）。
    /// 网盘空间有限时调小；对端没来得及导入的包删掉就真没了，别设太小。
    #[serde(default = "default_sync_retain_days")]
    pub retain_days: u32,
}

fn default_sync_retain_days() -> u32 {
    15
}

impl Default for SyncSettings {
    fn default() -> Self {
        Self {
            enabled: default_sync_enabled(),
            server: default_sync_server(),
            account: String::new(),
            password: String::new(),
            machine_label: String::new(),
            interval_minutes: default_sync_interval(),
            retain_days: default_sync_retain_days(),
        }
    }
}

impl SyncSettings {
    /// 账号、密码、服务器三者齐备才算「配置完成」，才允许发起同步。
    pub fn configured(&self) -> bool {
        !self.server.trim().is_empty()
            && !self.account.trim().is_empty()
            && !self.password.is_empty()
    }
}

/// 落盘前归一化：服务器地址补协议与结尾斜杠，账号去首尾空白，
/// 周期限定在 15/30/60 分钟或 0（仅手动）。
pub fn normalize_sync_settings(mut raw: SyncSettings) -> SyncSettings {
    let mut server = raw.server.trim().to_string();
    if !server.is_empty() && !server.starts_with("http://") && !server.starts_with("https://") {
        server = format!("https://{server}");
    }
    while server.ends_with('/') {
        server.pop();
    }
    raw.server = server;
    raw.account = raw.account.trim().to_string();
    raw.machine_label = raw.machine_label.trim().chars().take(40).collect();
    raw.interval_minutes = match raw.interval_minutes {
        0 => 0,
        m if m <= 20 => 15,
        m if m <= 45 => 30,
        _ => 60,
    };
    // 云端保留期：1~365 天，默认 15
    raw.retain_days = raw.retain_days.clamp(1, 365);
    raw
}

pub fn default_keyboard_layout() -> String {
    if cfg!(feature = "layout-108") {
        "108".to_string()
    } else {
        "65".to_string()
    }
}

pub fn normalize_keyboard_layout(raw: &str) -> String {
    if raw.trim() == "108" {
        "108".to_string()
    } else {
        "65".to_string()
    }
}

pub fn default_keyboard_orientation() -> String {
    "cat".to_string()
}

pub fn normalize_keyboard_orientation(raw: &str) -> String {
    if raw.trim() == "viewer" {
        "viewer".to_string()
    } else {
        "cat".to_string()
    }
}

pub fn default_tail_key_mode() -> String {
    "smart".to_string()
}

pub fn normalize_tail_key_mode(raw: &str) -> String {
    match raw.trim().to_ascii_lowercase().as_str() {
        "custom" => "custom".to_string(),
        "edge" => "edge".to_string(),
        "off" => "off".to_string(),
        _ => "smart".to_string(),
    }
}

pub fn default_tail_key_ids() -> Vec<String> {
    vec![
        "enter".to_string(),
        "space".to_string(),
        "bs".to_string(),
        "esc".to_string(),
    ]
}

pub fn normalize_tail_key_ids(raw: &[String]) -> Vec<String> {
    let mut out = Vec::with_capacity(raw.len().min(120));
    for item in raw {
        let clean: String = item
            .trim()
            .to_ascii_lowercase()
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
            .take(24)
            .collect();
        if !clean.is_empty() && !out.contains(&clean) {
            out.push(clean);
            if out.len() >= 120 {
                break;
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// 分类
//
// 分类不是原始记录，但它是「怎么解读这些记录」的唯一真相，所以跟设置一起放在
// settings.json：换机器、备份数据目录时会跟着走，不会像 localStorage 那样
// 清缓存就没了。前端只负责发起修改，校验与落盘都在这里做。
// ---------------------------------------------------------------------------

/// 「未分类」的内置 id。永远存在、不可删除、不能挂到别的分类下面。
pub const UNCATEGORIZED_ID: &str = "uncat";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CategoryDef {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub icon: String,
    #[serde(default)]
    pub color: String,
    #[serde(default)]
    pub parent_id: Option<String>,
    #[serde(default)]
    pub builtin: bool,
}

fn cat(id: &str, name: &str, icon: &str, color: &str) -> CategoryDef {
    CategoryDef {
        id: id.to_string(),
        name: name.to_string(),
        icon: icon.to_string(),
        color: color.to_string(),
        parent_id: None,
        builtin: false,
    }
}

/// 首次启动的那一套分类。改这里等于改所有新用户的默认视图，
/// 已经保存过 settings.json 的用户不受影响。
pub fn default_categories() -> Vec<CategoryDef> {
    let mut out = vec![
        cat("game", "游戏", "🎮", "#f59e0b"),
        cat("media", "媒体", "🎬", "#ef4444"),
        cat("social", "社交", "💬", "#ec4899"),
        cat("work", "工作", "💼", "#3b82f6"),
        cat("creative", "创作", "🎨", "#9ca3af"),
        cat("reference", "参考", "🌐", "#22c55e"),
        cat("tools", "工具", "🛠️", "#8b5cf6"),
    ];
    let mut uncat = cat(UNCATEGORIZED_ID, "未分类", "📁", "#4b5563");
    uncat.builtin = true;
    out.push(uncat);
    out
}

fn is_hex_color(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 7 && b[0] == b'#' && b[1..].iter().all(|c| c.is_ascii_hexdigit())
}

fn clean_id(raw: &str) -> String {
    raw.trim()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
        .take(40)
        .collect()
}

/// 分类列表校验。前端也会校验一遍，但那只是「即时反馈」，
/// 真正落盘的形状由这里决定：不接受空名、重复 id、悬空父级、环形父级。
///
/// 上限 200 个：足够细，又不至于让下拉框崩掉。
pub fn normalize_categories(raw: &[CategoryDef]) -> Vec<CategoryDef> {
    let mut out: Vec<CategoryDef> = Vec::with_capacity(raw.len().min(200));
    for item in raw.iter().take(200) {
        let id = clean_id(&item.id);
        if id.is_empty() || out.iter().any(|c| c.id == id) {
            continue;
        }
        let name: String = item.name.trim().chars().take(20).collect();
        if name.is_empty() {
            continue;
        }
        let icon: String = item.icon.trim().chars().take(8).collect();
        out.push(CategoryDef {
            id,
            name,
            icon: if icon.is_empty() {
                "🏷️".to_string()
            } else {
                icon
            },
            color: if is_hex_color(item.color.trim()) {
                item.color.trim().to_ascii_lowercase()
            } else {
                "#8b5cf6".to_string()
            },
            parent_id: item.parent_id.clone(),
            builtin: item.builtin,
        });
    }

    // 「未分类」必须存在：整张分类列表被删空时回退到默认那套，
    // 否则统计页会出现「所有程序都没有分类」的空窗。
    if !out.iter().any(|c| c.id == UNCATEGORIZED_ID) {
        if out.is_empty() {
            return default_categories();
        }
        let mut uncat = cat(UNCATEGORIZED_ID, "未分类", "📁", "#4b5563");
        uncat.builtin = true;
        out.push(uncat);
    }

    let ids: Vec<String> = out.iter().map(|c| c.id.clone()).collect();
    for c in out.iter_mut() {
        if c.id == UNCATEGORIZED_ID {
            c.parent_id = None;
            c.builtin = true;
            continue;
        }
        let next = match c.parent_id.as_ref() {
            // 悬空父级、认自己、认「未分类」当爹，都不成立
            Some(p) if !ids.contains(p) || *p == c.id || *p == UNCATEGORIZED_ID => None,
            Some(p) => Some(p.clone()),
            None => None,
        };
        c.parent_id = next;
    }

    // 环形父级拍平。只允许两层，更深的结构在界面上也表达不出来。
    let parent_of: std::collections::BTreeMap<String, Option<String>> = out
        .iter()
        .map(|c| (c.id.clone(), c.parent_id.clone()))
        .collect();
    let mut break_parents: Vec<String> = Vec::new();
    for c in out.iter() {
        let Some(parent_id) = c.parent_id.as_ref() else {
            continue;
        };
        if let Some(Some(grand)) = parent_of.get(parent_id) {
            if *grand == c.id {
                // 互相认爹 → 把父级提到顶层，自己留在原位
                break_parents.push(parent_id.clone());
            }
        }
    }
    for c in out.iter_mut() {
        if break_parents.iter().any(|p| *p == c.id) {
            c.parent_id = None;
        }
    }
    out
}

/// 应用 → 分类。丢掉指向不存在分类的条目，避免删完分类还留一堆幽灵映射。
pub fn normalize_app_categories(
    raw: &std::collections::BTreeMap<String, String>,
    categories: &[CategoryDef],
) -> std::collections::BTreeMap<String, String> {
    let mut out = std::collections::BTreeMap::new();
    for (app, cid) in raw.iter().take(4000) {
        let key: String = app.trim().chars().take(120).collect();
        if key.is_empty() {
            continue;
        }
        if !categories.iter().any(|c| c.id == *cid) {
            continue;
        }
        out.insert(key, cid.chars().take(40).collect());
    }
    out
}

/// 应用 → 自定义显示名。空名视为「删除自定义」，键同样trim限长。
pub fn normalize_app_display_names(
    raw: &std::collections::BTreeMap<String, String>,
) -> std::collections::BTreeMap<String, String> {
    let mut out = std::collections::BTreeMap::new();
    for (app, name) in raw.iter().take(4000) {
        let key: String = app.trim().chars().take(120).collect();
        let val: String = name.trim().chars().take(40).collect();
        if key.is_empty() || val.is_empty() {
            continue;
        }
        out.insert(key, val);
    }
    out
}

/// 应用 → 忽略标记。只保留 true 的条目：false 与缺失等价，落盘前顺手清掉，
/// 名单不会随着用户反复开关而膨胀。
pub fn normalize_app_ignored(
    raw: &std::collections::BTreeMap<String, bool>,
) -> std::collections::BTreeMap<String, bool> {
    let mut out = std::collections::BTreeMap::new();
    for (app, ignored) in raw.iter().take(4000) {
        let key: String = app.trim().chars().take(120).collect();
        if key.is_empty() || !*ignored {
            continue;
        }
        out.insert(key, true);
    }
    out
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppSettings {
    pub x: Option<i32>,
    pub y: Option<i32>,
    #[cfg(target_os = "macos")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_id: Option<String>,
    #[cfg(windows)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub windows_display_id: Option<String>,
    pub scale: f64,
    pub always_on_top: bool,
    pub click_through_locked: bool,
    pub show_key_bubbles: bool,
    pub sound_enabled: bool,
    pub autostart: bool,
    #[serde(default = "default_cat_breed")]
    pub cat_breed: String,
    #[serde(default = "default_mcp_enabled")]
    pub mcp_enabled: bool,
    #[serde(default = "default_mcp_port")]
    pub mcp_port: u16,
    #[serde(default = "default_keyboard_layout")]
    pub keyboard_layout: String,
    #[serde(default = "default_keyboard_orientation")]
    pub keyboard_orientation: String,
    #[serde(default = "default_privacy_password_guard")]
    pub privacy_password_guard: bool,
    #[serde(default = "default_tail_key_mode")]
    pub tail_key_mode: String,
    #[serde(default = "default_tail_key_ids")]
    pub tail_key_ids: Vec<String>,
    #[serde(default = "default_obs_enabled")]
    pub obs_enabled: bool,
    #[serde(default = "default_obs_port")]
    pub obs_port: u16,
    #[serde(default = "default_obs_hide_pet")]
    pub obs_hide_pet: bool,
    // ---- 输入记录（默认关闭；开启后才落盘） ----
    #[serde(default)]
    pub input_log_enabled: bool,
    #[serde(default = "default_input_log_active_gap")]
    pub input_log_active_gap_minutes: u32,
    #[serde(default)]
    pub input_log_retention_days: u32,
    #[serde(default)]
    pub input_log_paused: bool,
    /// 鼠标轨迹采样。默认关闭；关闭时不落盘，开启后才建 trail.db。
    #[serde(default)]
    pub input_log_trail_recording: bool,
    // ---- 数据同步（只由统计窗口的 sync_configure 写入） ----
    #[serde(default)]
    pub sync: SyncSettings,
    // ---- 分类（只由统计窗口的 categories_save 写入） ----
    #[serde(default = "default_categories")]
    pub categories: Vec<CategoryDef>,
    #[serde(default)]
    pub app_categories: std::collections::BTreeMap<String, String>,
    // ---- 应用标注（只由统计窗口的 app_overrides_save 写入） ----
    /// 进程名 → 用户自定义显示名。展示优先级：自定义名 > exe 显示名 > 进程名。
    #[serde(default)]
    pub app_display_names: std::collections::BTreeMap<String, String>,
    /// 进程名 → true = 忽略：recorder 不再记录该程序的新数据，统计口径里也过滤掉。
    /// 历史数据保留，取消忽略即可恢复显示。
    #[serde(default)]
    pub app_ignored: std::collections::BTreeMap<String, bool>,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            x: None,
            y: None,
            #[cfg(target_os = "macos")]
            display_id: None,
            #[cfg(windows)]
            windows_display_id: None,
            scale: 1.0,
            always_on_top: true,
            click_through_locked: false,
            show_key_bubbles: true,
            sound_enabled: true,
            autostart: false,
            cat_breed: default_cat_breed(),
            mcp_enabled: default_mcp_enabled(),
            mcp_port: default_mcp_port(),
            keyboard_layout: default_keyboard_layout(),
            keyboard_orientation: default_keyboard_orientation(),
            privacy_password_guard: default_privacy_password_guard(),
            tail_key_mode: default_tail_key_mode(),
            tail_key_ids: default_tail_key_ids(),
            obs_enabled: default_obs_enabled(),
            obs_port: default_obs_port(),
            obs_hide_pet: default_obs_hide_pet(),
            input_log_enabled: false,
            input_log_active_gap_minutes: default_input_log_active_gap(),
            input_log_retention_days: 0,
            input_log_paused: false,
            input_log_trail_recording: false,
            sync: SyncSettings::default(),
            categories: default_categories(),
            app_categories: std::collections::BTreeMap::new(),
            app_display_names: std::collections::BTreeMap::new(),
            app_ignored: std::collections::BTreeMap::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// 数据目录
// ---------------------------------------------------------------------------
// 安装版与绿色版共用一套规则：优先使用可执行文件同级的 `data/` 目录，让程序、
// 设置、皮肤、输入记录都待在同一个文件夹里，整个目录拷走就等于搬家。
// 只有该目录不可写时才退回 `%APPDATA%\kuro-cat-pet`——典型场景是装在
// `C:\Program Files`、只读介质上，或 ACL 受限的共享目录。
//
// 解析结果进程内缓存：目录探测（要试写一个临时文件）每个进程只做一次。
//
// macOS / Linux 保持原样不动。macOS 往 `.app` 包内写文件会破坏代码签名，
// Linux 的 AppImage 挂载点是只读的，deb 装到 `/usr` 也无写权限。

#[cfg(windows)]
static DATA_DIR: OnceLock<PathBuf> = OnceLock::new();

/// `%APPDATA%\kuro-cat-pet`：Windows 上本地目录不可用时的兜底位置。
#[cfg(windows)]
fn roaming_dir() -> PathBuf {
    std::env::var("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
        .join("kuro-cat-pet")
}

/// 系统目录必然不可写，直接判定，省掉无谓的试写。
#[cfg(windows)]
fn is_system_dir(dir: &Path) -> bool {
    let lower = dir.to_string_lossy().to_ascii_lowercase();
    for var in [
        "PROGRAMFILES",
        "PROGRAMFILES(X86)",
        "PROGRAMW6432",
        "SYSTEMROOT",
        "WINDIR",
    ] {
        let Ok(base) = std::env::var(var) else { continue };
        let base = base
            .trim_end_matches(|c| c == '\\' || c == '/')
            .to_ascii_lowercase();
        if base.is_empty() {
            continue;
        }
        if lower == base
            || lower.starts_with(&format!("{base}\\"))
            || lower.starts_with(&format!("{base}/"))
        {
            return true;
        }
    }
    false
}

/// 试写一个临时文件判断目录可写性，写完立刻删除。
/// 用 pid 命名避免多实例撞名；先清理可能残留的同名文件，免得误判为不可写。
#[cfg(windows)]
fn probe_writable(dir: &Path) -> bool {
    let probe = dir.join(format!(".kurocat-probe-{}", std::process::id()));
    let _ = fs::remove_file(&probe);
    let ok = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)
        .is_ok();
    if ok {
        let _ = fs::remove_file(&probe);
    }
    ok
}

/// 可用的本地数据目录；不可用时返回 `None`。
#[cfg(windows)]
fn local_data_dir() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    if is_system_dir(dir) {
        return None;
    }
    if !probe_writable(dir) {
        return None;
    }
    Some(dir.join("data"))
}

#[cfg(windows)]
fn resolve_data_dir() -> PathBuf {
    local_data_dir().unwrap_or_else(roaming_dir)
}

/// 递归复制目录，返回复制的总字节数。符号链接等特殊文件跳过。
#[cfg(windows)]
fn copy_dir_all(from: &Path, to: &Path) -> std::io::Result<u64> {
    fs::create_dir_all(to)?;
    let mut total = 0u64;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let target = to.join(entry.file_name());
        if file_type.is_dir() {
            total += copy_dir_all(&entry.path(), &target)?;
        } else if file_type.is_file() {
            total += fs::copy(entry.path(), &target)?;
        }
    }
    Ok(total)
}

/// 启动时调用一次，务必在任何代码读写设置之前完成目录判定。
///
/// 升级时有个必须处理的坑：旧版本数据在 `%APPDATA%\kuro-cat-pet`，新规则改用
/// 程序同级 `data/`，直接切过去会让用户以为记录和皮肤全丢了。所以首次启用
/// 本地目录时把旧数据**复制**过来——只复制不删除，旧目录原样保留作备份。
/// 复制失败则整段放弃并继续用旧位置：宁可维持现状，也不能留下半份数据。
#[cfg(windows)]
pub fn prepare_data_dir() {
    let resolved = resolve_data_dir();
    if resolved == roaming_dir() || resolved.exists() {
        let _ = DATA_DIR.set(resolved);
        return;
    }
    let old = roaming_dir();
    if old.is_dir() {
        match copy_dir_all(&old, &resolved) {
            Ok(bytes) => eprintln!(
                "[config] 旧数据已复制到 {}（{bytes} 字节），原目录保留在 {}",
                resolved.display(),
                old.display()
            ),
            Err(e) => {
                let _ = fs::remove_dir_all(&resolved);
                eprintln!("[config] 复制到本地目录失败，继续使用 %APPDATA%: {e}");
                let _ = DATA_DIR.set(old);
                return;
            }
        }
    }
    let _ = DATA_DIR.set(resolved);
}

#[cfg(not(windows))]
pub fn prepare_data_dir() {}

pub fn app_data_dir() -> PathBuf {
    #[cfg(windows)]
    {
        DATA_DIR.get_or_init(resolve_data_dir).clone()
    }
    #[cfg(target_os = "macos")]
    {
        let home = std::env::var("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("."));
        home.join("Library")
            .join("Application Support")
            .join("kuro-cat-pet")
    }
    #[cfg(all(not(windows), not(target_os = "macos")))]
    {
        let home = std::env::var("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("."));
        home.join(".config").join("kuro-cat-pet")
    }
}

pub fn config_path() -> PathBuf {
    app_data_dir().join("settings.json")
}

pub fn load_settings() -> AppSettings {
    let path = config_path();
    if let Ok(content) = fs::read_to_string(&path) {
        if let Ok(mut parsed) = serde_json::from_str::<AppSettings>(&content) {
            parsed.scale = parsed.scale.clamp(0.20, 1.50);
            parsed.tail_key_mode = normalize_tail_key_mode(&parsed.tail_key_mode);
            parsed.tail_key_ids = normalize_tail_key_ids(&parsed.tail_key_ids);
            parsed.input_log_active_gap_minutes =
                normalize_input_log_active_gap(parsed.input_log_active_gap_minutes);
            parsed.input_log_retention_days =
                normalize_input_log_retention(parsed.input_log_retention_days);
            parsed.sync = normalize_sync_settings(parsed.sync);
            parsed.categories = normalize_categories(&parsed.categories);
            parsed.app_categories = normalize_app_categories(&parsed.app_categories, &parsed.categories);
            parsed.app_display_names = normalize_app_display_names(&parsed.app_display_names);
            parsed.app_ignored = normalize_app_ignored(&parsed.app_ignored);
            return parsed;
        }
    }
    AppSettings::default()
}

pub fn save_settings(settings: &AppSettings) {
    let path = config_path();
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_string_pretty(settings) {
        let _ = fs::write(path, json);
    }
}

#[cfg(windows)]
pub fn set_windows_autostart(enabled: bool) -> Result<(), String> {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::System::Registry::{
        RegCloseKey, RegDeleteValueW, RegOpenKeyExW, RegSetValueExW, HKEY, HKEY_CURRENT_USER,
        KEY_SET_VALUE, REG_SZ,
    };

    fn to_wide(s: &str) -> Vec<u16> {
        OsStr::new(s)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    let subkey = to_wide("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
    let value_name = to_wide("KuroCat");
    let legacy_value_name = to_wide("KuroCatPet");

    unsafe {
        let mut hkey: HKEY = std::ptr::null_mut();
        let status = RegOpenKeyExW(
            HKEY_CURRENT_USER,
            subkey.as_ptr(),
            0,
            KEY_SET_VALUE,
            &mut hkey,
        );
        if status != 0 {
            return Err(format!("RegOpenKeyExW failed with code {}", status));
        }

        // Clean up legacy autostart key so users upgrading from <=v4.2 never run two copies
        let _ = RegDeleteValueW(hkey, legacy_value_name.as_ptr());

        let result = if enabled {
            let exe = std::env::current_exe()
                .map_err(|e| e.to_string())?
                .to_string_lossy()
                .to_string();
            let quoted = format!("\"{}\"", exe);
            let data = to_wide(&quoted);
            let byte_len = (data.len() * std::mem::size_of::<u16>()) as u32;
            let st = RegSetValueExW(
                hkey,
                value_name.as_ptr(),
                0,
                REG_SZ,
                data.as_ptr() as *const u8,
                byte_len,
            );
            if st == 0 {
                Ok(())
            } else {
                Err(format!("RegSetValueExW failed with code {}", st))
            }
        } else {
            let _ = RegDeleteValueW(hkey, value_name.as_ptr());
            Ok(())
        };

        RegCloseKey(hkey);
        result
    }
}

#[cfg(target_os = "macos")]
pub fn set_windows_autostart(enabled: bool) -> Result<(), String> {
    let home = std::env::var("HOME").map_err(|e| e.to_string())?;
    let agents_dir = PathBuf::from(home).join("Library").join("LaunchAgents");
    let plist_path = agents_dir.join("com.kurocat.pet.plist");

    if enabled {
        fs::create_dir_all(&agents_dir).map_err(|e| e.to_string())?;
        let exe = std::env::current_exe()
            .map_err(|e| e.to_string())?
            .to_string_lossy()
            .to_string();
        let plist = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>com.kurocat.pet</string>
    <key>ProgramArguments</key>
    <array>
        <string>{}</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
</dict>
</plist>
"#,
            exe
        );
        fs::write(plist_path, plist).map_err(|e| e.to_string())
    } else {
        if plist_path.exists() {
            let _ = fs::remove_file(plist_path);
        }
        Ok(())
    }
}

#[cfg(all(not(windows), not(target_os = "macos")))]
pub fn set_windows_autostart(enabled: bool) -> Result<(), String> {
    let config_root = std::env::var("XDG_CONFIG_HOME")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
            PathBuf::from(home).join(".config")
        });
    let autostart_dir = config_root.join("autostart");
    let desktop_path = autostart_dir.join("kurocat.desktop");

    if enabled {
        fs::create_dir_all(&autostart_dir).map_err(|e| e.to_string())?;
        let exe = std::env::current_exe()
            .map_err(|e| e.to_string())?
            .to_string_lossy()
            .to_string();
        let entry = format!(
            "[Desktop Entry]\n\
             Type=Application\n\
             Name=KuroCat\n\
             Comment=Minimalist desktop cat typing & mouse companion\n\
             Exec=\"{exe}\"\n\
             Icon=KuroCat\n\
             Terminal=false\n\
             X-GNOME-Autostart-enabled=true\n",
            exe = exe.replace('"', "\\\"")
        );
        fs::write(desktop_path, entry).map_err(|e| e.to_string())
    } else {
        if desktop_path.exists() {
            let _ = fs::remove_file(desktop_path);
        }
        Ok(())
    }
}

#[cfg(windows)]
pub fn create_desktop_shortcut() -> Result<String, String> {
    use std::os::windows::process::CommandExt;
    use std::process::Command;

    const CREATE_NO_WINDOW: u32 = 0x08000000;

    let exe_path = std::env::current_exe().map_err(|e| e.to_string())?;
    let exe_str = exe_path.to_string_lossy().replace('\'', "''");
    let work_dir = exe_path
        .parent()
        .map(|p| p.to_string_lossy().replace('\'', "''"))
        .unwrap_or_default();

    let script = format!(
        "$desktop = [Environment]::GetFolderPath('Desktop'); \
         $lnk = Join-Path $desktop 'KuroCat.lnk'; \
         $ws = New-Object -ComObject WScript.Shell; \
         $s = $ws.CreateShortcut($lnk); \
         $s.TargetPath = '{exe}'; \
         $s.WorkingDirectory = '{wd}'; \
         $s.IconLocation = '{exe},0'; \
         $s.Description = 'KuroCat Desktop Companion'; \
         $s.Save(); \
         Write-Output $lnk",
        exe = exe_str,
        wd = work_dir,
    );

    let output = Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-WindowStyle", "Hidden", "-Command", &script])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|e| format!("Failed to invoke PowerShell: {}", e))?;

    if output.status.success() {
        let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
        Ok(path)
    } else {
        let err = String::from_utf8_lossy(&output.stderr).trim().to_string();
        Err(if err.is_empty() {
            "Failed to create desktop shortcut".to_string()
        } else {
            err
        })
    }
}

#[cfg(not(windows))]
pub fn create_desktop_shortcut() -> Result<String, String> {
    Ok("Desktop shortcut is managed via Applications on this OS".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 递归复制必须完整搬走子目录与文件，返回值累计字节数。
    /// 迁移旧数据全靠它，漏一层就会让用户以为记录丢了。
    #[cfg(windows)]
    #[test]
    fn copy_dir_all_copies_nested_tree() {
        let root = std::env::temp_dir().join(format!("kurocat-copy-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let from = root.join("from");
        let to = root.join("to");
        fs::create_dir_all(from.join("input-log")).unwrap();
        fs::create_dir_all(from.join("a/b")).unwrap();
        fs::write(from.join("settings.json"), b"{}").unwrap();
        fs::write(from.join("input-log/events.db"), b"db").unwrap();
        fs::write(from.join("a/b/deep.txt"), b"world!").unwrap();

        let bytes = copy_dir_all(&from, &to).unwrap();
        assert_eq!(bytes, 2 + 2 + 6);
        assert_eq!(fs::read_to_string(to.join("settings.json")).unwrap(), "{}");
        assert_eq!(
            fs::read_to_string(to.join("input-log/events.db")).unwrap(),
            "db"
        );
        assert_eq!(fs::read_to_string(to.join("a/b/deep.txt")).unwrap(), "world!");
        let _ = fs::remove_dir_all(&root);
    }

    /// Program Files / Windows 一定不可写，直接判死；普通目录不能误伤。
    /// 误判成系统目录的后果是数据被踢回 %APPDATA%，绿色版直接失效。
    #[cfg(windows)]
    #[test]
    fn system_dirs_are_rejected_but_normal_dirs_are_not() {
        for var in ["PROGRAMFILES", "SYSTEMROOT"] {
            if let Ok(base) = std::env::var(var) {
                if base.is_empty() {
                    continue;
                }
                assert!(is_system_dir(Path::new(&base)), "{var} itself");
                assert!(
                    is_system_dir(&Path::new(&base).join("KuroCat")),
                    "{var} subdir"
                );
            }
        }
        assert!(!is_system_dir(Path::new(r"D:\portable\kurocat")));
        assert!(!is_system_dir(Path::new(r"C:\Users")));
    }

    #[test]
    fn tail_key_mode_normalizes_safely() {
        assert_eq!(normalize_tail_key_mode("smart"), "smart");
        assert_eq!(normalize_tail_key_mode(" CUSTOM "), "custom");
        assert_eq!(normalize_tail_key_mode("edge"), "edge");
        assert_eq!(normalize_tail_key_mode("OFF"), "off");
        assert_eq!(normalize_tail_key_mode("invalid"), "smart");
    }

    #[test]
    fn tail_key_ids_dedupes_and_sanitizes() {
        let raw = vec![
            "Enter".to_string(),
            " enter ".to_string(),
            "NP_0".to_string(),
            "bad-key!$".to_string(),
            "".to_string(),
        ];
        assert_eq!(
            normalize_tail_key_ids(&raw),
            vec!["enter".to_string(), "np_0".to_string(), "badkey".to_string()]
        );
    }

    #[test]
    fn legacy_settings_json_deserializes_with_tail_defaults() {
        let legacy = r#"{
            "x": null,
            "y": null,
            "scale": 1.0,
            "alwaysOnTop": true,
            "clickThroughLocked": false,
            "showKeyBubbles": true,
            "soundEnabled": true,
            "autostart": false
        }"#;
        let parsed: AppSettings = serde_json::from_str(legacy).expect("legacy json should parse");
        assert_eq!(parsed.tail_key_mode, "smart");
        assert_eq!(parsed.tail_key_ids, default_tail_key_ids());
        // 老配置里没有分类字段，必须补上一整套默认分类，而不是空白列表
        assert_eq!(parsed.categories.len(), 8);
        assert!(parsed.app_categories.is_empty());
    }

    /// 前端可以传任何东西上来，落盘前必须被收拾成可用的形状。
    #[test]
    fn categories_are_sanitized_before_saving() {
        let raw = vec![
            CategoryDef {
                id: "game".into(),
                name: "  游戏  ".into(),
                icon: "".into(),
                color: "#F59E0B".into(),
                parent_id: Some("ghost".into()),
                builtin: false,
            },
            // 与上面 id 重复 → 丢弃，保留先来的那个
            CategoryDef {
                id: "game".into(),
                name: "重复".into(),
                icon: "🎮".into(),
                color: "#ffffff".into(),
                parent_id: None,
                builtin: false,
            },
            // 空名 → 丢弃
            CategoryDef {
                id: "blank".into(),
                name: "   ".into(),
                icon: "x".into(),
                color: "#ffffff".into(),
                parent_id: None,
                builtin: false,
            },
            CategoryDef {
                id: "badcol".into(),
                name: "坏颜色".into(),
                icon: "🎨".into(),
                color: "red".into(),
                parent_id: None,
                builtin: false,
            },
            // 自己当自己的父级 → 拍平成顶层
            CategoryDef {
                id: "selfy".into(),
                name: "自环".into(),
                icon: "🔁".into(),
                color: "#123456".into(),
                parent_id: Some("selfy".into()),
                builtin: false,
            },
        ];
        let out = normalize_categories(&raw);
        let ids: Vec<&str> = out.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, vec!["game", "badcol", "selfy", UNCATEGORIZED_ID]);
        assert_eq!(out[0].name, "游戏");
        assert_eq!(out[0].icon, "🏷️");
        assert_eq!(out[0].color, "#f59e0b");
        assert_eq!(out[0].parent_id, None, "悬空父级要断开");
        assert_eq!(out[1].color, "#8b5cf6");
        assert_eq!(out[2].parent_id, None);
        assert!(out[3].builtin, "未分类必须自动补上且标记为内置");
    }

    #[test]
    fn app_categories_drop_dangling_references() {
        let cats = normalize_categories(&default_categories());
        let mut raw = std::collections::BTreeMap::new();
        raw.insert("chrome.exe".to_string(), "work".to_string());
        raw.insert("ghost.exe".to_string(), "no-such-cat".to_string());
        raw.insert("   ".to_string(), "game".to_string());
        let out = normalize_app_categories(&raw, &cats);
        assert_eq!(out.len(), 1);
        assert_eq!(out.get("chrome.exe").map(String::as_str), Some("work"));
    }
}
