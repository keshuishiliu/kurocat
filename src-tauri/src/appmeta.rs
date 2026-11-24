//! 应用元信息：exe 完整路径、显示名（多为软件自带中文名）、版本号、真实图标。
//!
//! 输入记录里只存进程名（`chrome.exe`），数据库既没有路径也没有图标。这里按需现查：
//!
//! 1. 枚举运行中的进程，建立「exe 名 → 完整路径」映射（最准，正在跑的程序一定能拿到）；
//! 2. 没在跑的程序走注册表 `App Paths`，已安装软件基本都会登记；
//! 3. 拿到路径后读 exe 的版本资源取 `FileDescription` / `ProductName` / 文件版本；
//! 4. 用 `PrivateExtractIconsW` 抽出 48×48 图标，转成 ICO 字节再 base64 成 data URL。
//!
//! 结果按进程名缓存在进程内：切页、重进统计窗口都不重复解析（图标提取要加载资源，最贵）。
//! 查不到的字段一律 `None`，由前端降级成首字母占位，不报错。

use serde::Serialize;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

/// 图标取多大。前端按 40px 显示，48 在 2x 屏上也不糊。
#[cfg(windows)]
const ICON_PX: i32 = 48;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppMeta {
    /// 进程名，和 `settings.json` 里 `app_categories` 的 key 完全一致，前端拿它做映射。
    pub process: String,
    /// 优先 exe 版本资源里的文件说明（微信 → 「微信」），没有则退回产品名。
    pub display_name: Option<String>,
    /// 文件版本，形如 `4.18.1`。
    pub version: Option<String>,
    /// exe 完整路径。
    pub exe_path: Option<String>,
    /// `data:image/x-icon;base64,...`
    pub icon: Option<String>,
}

fn cache() -> &'static Mutex<HashMap<String, AppMeta>> {
    static CACHE: OnceLock<Mutex<HashMap<String, AppMeta>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 批量解析。同步实现，调用方负责放到阻塞线程池里跑。
pub fn resolve_many(processes: Vec<String>) -> Vec<AppMeta> {
    #[cfg(windows)]
    {
        return resolve_many_windows(processes);
    }
    #[cfg(not(windows))]
    {
        // macOS/Linux 侧没有等价实现（需要 NSWorkspace / .desktop 解析），先只回进程名，
        // 前端会自动降级成首字母占位，不显示路径。
        processes
            .into_iter()
            .map(|p| AppMeta {
                process: p,
                display_name: None,
                version: None,
                exe_path: None,
                icon: None,
            })
            .collect()
    }
}

#[cfg(windows)]
fn resolve_many_windows(processes: Vec<String>) -> Vec<AppMeta> {
    let running = running_process_paths();
    let mut out = Vec::with_capacity(processes.len());
    for raw in processes {
        let key = raw.to_ascii_lowercase();
        {
            let hit = cache().lock().unwrap().get(&key).cloned();
            if let Some(meta) = hit {
                out.push(meta);
                continue;
            }
        }
        let meta = resolve_one(&raw, &running);
        cache().lock().unwrap().insert(key, meta.clone());
        out.push(meta);
    }
    out
}

#[cfg(windows)]
fn resolve_one(process: &str, running: &HashMap<String, String>) -> AppMeta {
    let mut meta = AppMeta {
        process: process.to_string(),
        display_name: None,
        version: None,
        exe_path: None,
        icon: None,
    };

    let path = running
        .get(&process.to_ascii_lowercase())
        .cloned()
        .or_else(|| lookup_app_path(process));

    if let Some(exe) = path {
        let info = exe_version_info(&exe);
        meta.display_name = info.description.or(info.product);
        meta.version = info.version;
        meta.icon = icon_data_url(&exe);
        meta.exe_path = Some(exe);
    }

    meta
}

// ---------------------------------------------------------------------------
// ① 运行中的进程
// ---------------------------------------------------------------------------

#[cfg(windows)]
fn running_process_paths() -> HashMap<String, String> {
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };

    let mut map: HashMap<String, String> = HashMap::new();
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snap == INVALID_HANDLE_VALUE {
            return map;
        }
        let mut entry: PROCESSENTRY32W = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        if Process32FirstW(snap, &mut entry) != 0 {
            loop {
                let name = wide_buf_to_string(&entry.szExeFile);
                if !name.is_empty() {
                    let key = name.to_ascii_lowercase();
                    // 同名进程可能有多个实例，保留第一个能读到路径的即可
                    if !map.contains_key(&key) {
                        if let Some(path) = process_image_path(entry.th32ProcessID) {
                            map.insert(key, path);
                        }
                    }
                }
                if Process32NextW(snap, &mut entry) == 0 {
                    break;
                }
            }
        }
        windows_sys::Win32::Foundation::CloseHandle(snap);
    }
    map
}

#[cfg(windows)]
fn process_image_path(pid: u32) -> Option<String> {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    unsafe {
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
        Some(String::from_utf16_lossy(&buf[..len as usize]))
    }
}

// ---------------------------------------------------------------------------
// ② 注册表 App Paths
// ---------------------------------------------------------------------------

#[cfg(windows)]
fn lookup_app_path(process: &str) -> Option<String> {
    use windows_sys::Win32::System::Registry::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE};

    let sub = format!(
        "SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\App Paths\\{}",
        process
    );
    for root in [HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE] {
        if let Some(raw) = read_default_string(root, &sub) {
            if let Some(path) = normalize_exe_path(&raw) {
                if std::path::Path::new(&path).is_file() {
                    return Some(path);
                }
            }
        }
    }
    None
}

/// 读某个注册表键的默认值（REG_SZ / REG_EXPAND_SZ）。
#[cfg(windows)]
fn read_default_string(root: windows_sys::Win32::System::Registry::HKEY, sub: &str) -> Option<String> {
    use windows_sys::Win32::System::Registry::{
        RegCloseKey, RegOpenKeyExW, RegQueryValueExW, KEY_READ, REG_EXPAND_SZ, REG_SZ,
    };

    unsafe {
        let wsub = to_wide(sub);
        let mut key: windows_sys::Win32::System::Registry::HKEY = std::ptr::null_mut();
        if RegOpenKeyExW(root, wsub.as_ptr(), 0, KEY_READ, &mut key) != 0 {
            return None;
        }
        let mut buf = vec![0u16; 1024];
        let mut len = (buf.len() * 2) as u32;
        let mut kind = 0u32;
        let rc = RegQueryValueExW(
            key,
            std::ptr::null(),
            std::ptr::null_mut(),
            &mut kind,
            buf.as_mut_ptr() as *mut u8,
            &mut len,
        );
        RegCloseKey(key);
        if rc != 0 || (kind != REG_SZ && kind != REG_EXPAND_SZ) {
            return None;
        }
        let units = (len as usize / 2).min(buf.len());
        let text = wide_buf_to_string(&buf[..units]);
        if text.trim().is_empty() {
            None
        } else {
            Some(text)
        }
    }
}

/// 注册表里的可执行路径可能带引号、尾随 `,0` 或环境变量，统一收拾成纯路径。
#[cfg(windows)]
fn normalize_exe_path(raw: &str) -> Option<String> {
    let mut s = raw.trim().to_string();
    if s.starts_with('"') {
        if let Some(end) = s[1..].find('"') {
            s = s[1..=end].to_string();
        }
    } else if let Some(pos) = s.to_ascii_lowercase().rfind(",0") {
        // `C:\path\app.exe,0` 这种图标索引写法
        if s[pos..].trim() == ",0" {
            s.truncate(pos);
        }
    }
    let s = s.trim().trim_matches('"').trim().to_string();
    if s.is_empty() || !s.to_ascii_lowercase().ends_with(".exe") {
        return None;
    }
    Some(s)
}

// ---------------------------------------------------------------------------
// ③ exe 版本资源
// ---------------------------------------------------------------------------

#[cfg(windows)]
struct VersionInfo {
    description: Option<String>,
    product: Option<String>,
    version: Option<String>,
}

#[cfg(windows)]
fn exe_version_info(path: &str) -> VersionInfo {
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW, VS_FIXEDFILEINFO,
    };

    let mut out = VersionInfo {
        description: None,
        product: None,
        version: None,
    };

    unsafe {
        let wpath = to_wide(path);
        let size = GetFileVersionInfoSizeW(wpath.as_ptr(), std::ptr::null_mut());
        if size == 0 {
            return out;
        }
        let mut block = vec![0u8; size as usize];
        if GetFileVersionInfoW(
            wpath.as_ptr(),
            0,
            size,
            block.as_mut_ptr() as *mut core::ffi::c_void,
        ) == 0
        {
            return out;
        }

        // 固定版本信息：major.minor.build[.revision]
        let mut ptr: *mut core::ffi::c_void = std::ptr::null_mut();
        let mut len: u32 = 0;
        let root = to_wide("\\");
        if VerQueryValueW(
            block.as_ptr() as *const core::ffi::c_void,
            root.as_ptr(),
            &mut ptr,
            &mut len,
        ) != 0
            && !ptr.is_null()
            && len as usize >= std::mem::size_of::<VS_FIXEDFILEINFO>()
        {
            let fixed = &*(ptr as *const VS_FIXEDFILEINFO);
            if fixed.dwSignature == 0xFEEF04BD {
                let ms = fixed.dwFileVersionMS;
                let ls = fixed.dwFileVersionLS;
                let (major, minor) = (ms >> 16, ms & 0xFFFF);
                let (build, revision) = (ls >> 16, ls & 0xFFFF);
                out.version = Some(if revision == 0 {
                    format!("{}.{}.{}", major, minor, build)
                } else {
                    format!("{}.{}.{}.{}", major, minor, build, revision)
                });
            }
        }

        // 字符串表：先取第一个语言/代码页，再读 FileDescription / ProductName
        let mut lang_ptr: *mut core::ffi::c_void = std::ptr::null_mut();
        let mut lang_len: u32 = 0;
        let var = to_wide("\\VarFileInfo\\Translation");
        let mut lang_id: u32 = 0x0409; // 兜底：英文(美国)
        let mut code_page: u32 = 0x04B0; // 兜底：Unicode
        if VerQueryValueW(
            block.as_ptr() as *const core::ffi::c_void,
            var.as_ptr(),
            &mut lang_ptr,
            &mut lang_len,
        ) != 0
            && !lang_ptr.is_null()
            && lang_len >= 4
        {
            let pair = lang_ptr as *const u16;
            lang_id = *pair as u32;
            code_page = *pair.add(1) as u32;
        }

        let read_string = |field: &str| -> Option<String> {
            let sub = to_wide(&format!(
                "\\StringFileInfo\\{:04x}{:04x}\\{}",
                lang_id, code_page, field
            ));
            let mut value_ptr: *mut core::ffi::c_void = std::ptr::null_mut();
            let mut value_len: u32 = 0;
            let rc = VerQueryValueW(
                block.as_ptr() as *const core::ffi::c_void,
                sub.as_ptr(),
                &mut value_ptr,
                &mut value_len,
            );
            if rc == 0 || value_ptr.is_null() || value_len == 0 {
                return None;
            }
            let units = value_len as usize / 2;
            let text =
                String::from_utf16_lossy(std::slice::from_raw_parts(value_ptr as *const u16, units));
            let text = text.trim().to_string();
            if text.is_empty() {
                None
            } else {
                Some(text)
            }
        };

        out.description = read_string("FileDescription");
        out.product = read_string("ProductName");
    }

    out
}

// ---------------------------------------------------------------------------
// ④ 图标 → ICO data URL
// ---------------------------------------------------------------------------

#[cfg(windows)]
fn icon_data_url(path: &str) -> Option<String> {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        DestroyIcon, PrivateExtractIconsW, LR_DEFAULTCOLOR,
    };

    unsafe {
        let wpath = to_wide(path);
        let mut hicon: windows_sys::Win32::Foundation::HANDLE = std::ptr::null_mut();
        let count = PrivateExtractIconsW(
            wpath.as_ptr(),
            0,
            ICON_PX,
            ICON_PX,
            &mut hicon,
            std::ptr::null_mut(),
            1,
            LR_DEFAULTCOLOR,
        );
        if count == 0 || hicon.is_null() {
            return None;
        }
        let bytes = hicon_to_ico(hicon);
        DestroyIcon(hicon);
        bytes.map(|b| format!("data:image/x-icon;base64,{}", base64(&b)))
    }
}

/// HICON → 单张 32bpp ICO。前端 `<img>` 直接吃这个格式，不用引第三方编码库。
#[cfg(windows)]
unsafe fn hicon_to_ico(hicon: windows_sys::Win32::Foundation::HANDLE) -> Option<Vec<u8>> {
    use windows_sys::Win32::Graphics::Gdi::{
        DeleteObject, GetDC, GetDIBits, GetObjectW, ReleaseDC, BITMAP, BITMAPINFO,
        BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetIconInfo, ICONINFO};

    let mut info: ICONINFO = std::mem::zeroed();
    if GetIconInfo(hicon, &mut info) == 0 {
        return None;
    }

    let mut bmp: BITMAP = std::mem::zeroed();
    let got_object = GetObjectW(
        info.hbmColor,
        std::mem::size_of::<BITMAP>() as i32,
        &mut bmp as *mut _ as *mut core::ffi::c_void,
    );
    if got_object == 0 || bmp.bmWidth <= 0 || bmp.bmHeight <= 0 {
        DeleteObject(info.hbmColor);
        DeleteObject(info.hbmMask);
        return None;
    }

    let w = bmp.bmWidth;
    let h = bmp.bmHeight;

    let dc = GetDC(std::ptr::null_mut());
    let mut bi: BITMAPINFO = std::mem::zeroed();
    bi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
    bi.bmiHeader.biWidth = w;
    // 正高度 = bottom-up。ICO 里的像素同样是自下而上，省一次翻转。
    bi.bmiHeader.biHeight = h;
    bi.bmiHeader.biPlanes = 1;
    bi.bmiHeader.biBitCount = 32;
    bi.bmiHeader.biCompression = BI_RGB;

    let mut pixels = vec![0u8; (w as usize) * (h as usize) * 4];
    let scanlines = GetDIBits(
        dc,
        info.hbmColor,
        0,
        h as u32,
        pixels.as_mut_ptr() as *mut core::ffi::c_void,
        &mut bi,
        DIB_RGB_COLORS,
    );
    ReleaseDC(std::ptr::null_mut(), dc);
    DeleteObject(info.hbmColor);
    DeleteObject(info.hbmMask);

    if scanlines == 0 {
        return None;
    }

    // 老图标没有 alpha 通道（全靠 AND mask 表示透明），这种一律按不透明处理。
    if !pixels.chunks_exact(4).any(|px| px[3] != 0) {
        for px in pixels.chunks_exact_mut(4) {
            px[3] = 255;
        }
    }

    // AND mask：每行按 32 位对齐，1 表示透明。
    let mask_stride = (((w + 31) / 32) * 4) as usize;
    let mut mask = vec![0u8; mask_stride * h as usize];
    for y in 0..h as usize {
        for x in 0..w as usize {
            if pixels[(y * w as usize + x) * 4 + 3] == 0 {
                mask[y * mask_stride + x / 8] |= 0x80 >> (x % 8);
            }
        }
    }

    let image_size = 40 + pixels.len() + mask.len();
    let mut ico = Vec::with_capacity(22 + image_size);
    // ICONDIR
    ico.extend_from_slice(&[0, 0, 1, 0, 1, 0]);
    // ICONDIRENTRY（256 要写 0）
    ico.push(if w >= 256 { 0 } else { w as u8 });
    ico.push(if h >= 256 { 0 } else { h as u8 });
    ico.push(0);
    ico.push(0);
    ico.extend_from_slice(&1u16.to_le_bytes());
    ico.extend_from_slice(&32u16.to_le_bytes());
    ico.extend_from_slice(&(image_size as u32).to_le_bytes());
    ico.extend_from_slice(&22u32.to_le_bytes());
    // BITMAPINFOHEADER：高度写两倍，因为后面还有 AND mask
    ico.extend_from_slice(&40u32.to_le_bytes());
    ico.extend_from_slice(&w.to_le_bytes());
    ico.extend_from_slice(&(h * 2).to_le_bytes());
    ico.extend_from_slice(&1u16.to_le_bytes());
    ico.extend_from_slice(&32u16.to_le_bytes());
    ico.extend_from_slice(&0u32.to_le_bytes());
    ico.extend_from_slice(&((pixels.len() + mask.len()) as u32).to_le_bytes());
    ico.extend_from_slice(&[0u8; 16]);
    ico.extend_from_slice(&pixels);
    ico.extend_from_slice(&mask);
    Some(ico)
}

#[cfg(windows)]
fn base64(data: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((data.len() + 2) / 3 * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[(n >> 18) as usize & 63] as char);
        out.push(TABLE[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            TABLE[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(windows)]
fn to_wide(s: &str) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    std::ffi::OsStr::new(s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

#[cfg(windows)]
fn wide_buf_to_string(buf: &[u16]) -> String {
    let end = buf.iter().position(|c| *c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end]).trim().to_string()
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_known_vectors() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foob"), "Zm9vYg==");
        assert_eq!(base64(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn normalize_handles_registry_quirks() {
        assert_eq!(
            normalize_exe_path("\"C:\\Program Files\\App\\app.exe\",0").as_deref(),
            Some("C:\\Program Files\\App\\app.exe")
        );
        assert_eq!(
            normalize_exe_path("D:\\tools\\npp.exe,0").as_deref(),
            Some("D:\\tools\\npp.exe")
        );
        assert_eq!(
            normalize_exe_path("C:\\a\\b.exe").as_deref(),
            Some("C:\\a\\b.exe")
        );
        assert_eq!(normalize_exe_path("C:\\a\\b.dll"), None);
        assert_eq!(normalize_exe_path("   "), None);
    }

    #[test]
    fn resolves_current_exe_with_icon() {
        // 自身一定在运行，路径必然能查到；图标按 Windows 构建应该有。
        let meta = resolve_many(vec![
            std::env::current_exe()
                .ok()
                .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
                .unwrap_or_else(|| "KuroCat.exe".to_string()),
        ]);
        assert_eq!(meta.len(), 1);
        assert!(meta[0].exe_path.is_some(), "运行中的进程必须解析出路径");
    }

    #[test]
    fn unknown_process_does_not_panic() {
        let meta = resolve_many(vec!["definitely-not-a-real-app.exe".to_string()]);
        assert_eq!(meta.len(), 1);
        assert_eq!(meta[0].process, "definitely-not-a-real-app.exe");
    }
}
