use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tauri::{AppHandle, Emitter};

const REPO_API_LATEST: &str = "https://api.github.com/repos/keshuishiliu/kurocat/releases/latest";
const REPO_RELEASES_PAGE: &str = "https://github.com/keshuishiliu/kurocat/releases/latest";

pub const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateInfo {
    pub current_version: String,
    pub latest_version: String,
    pub has_update: bool,
    pub release_url: String,
    pub asset_name: Option<String>,
    pub download_url: Option<String>,
    pub asset_sha256: Option<String>,
    pub asset_size: Option<u64>,
    pub install_kind: String,
    pub notes_summary: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateProgressEvent {
    pub stage: String,
    pub message: String,
    pub info: Option<UpdateInfo>,
}

/// 绿色版（便携版）下载完但**尚未应用**的更新包。
///
/// 便携版不再在下载完成后立刻热替换 exe：宠物是常驻进程，强杀重启会打断正在打字的
/// 用户。所以改为「下载 → 落盘 → 提示」，等用户点「重启更新」才真正替换二进制。
/// 标记存在 `updates\pending.json`，跟着绿色版文件夹走，拷走目录也不会丢状态。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingPortableUpdate {
    /// 目标版本号，保留原始 tag 形态（可能带 `v` 前缀）。
    pub version: String,
    /// 已落盘的更新包绝对路径（`.exe.new`，双击不会误启动）。
    pub file_path: String,
    /// 发布页给出的 SHA-256，应用前再复验一次，防止目录里的文件被替换或损坏。
    pub sha256: Option<String>,
}

impl PendingPortableUpdate {
    pub fn file(&self) -> PathBuf {
        PathBuf::from(&self.file_path)
    }
}

#[derive(Debug, Deserialize)]
struct GhRelease {
    tag_name: String,
    html_url: Option<String>,
    body: Option<String>,
    #[serde(default)]
    assets: Vec<GhAsset>,
}

#[derive(Debug, Deserialize)]
struct GhAsset {
    name: String,
    browser_download_url: String,
    size: Option<u64>,
    digest: Option<String>,
}

pub fn parse_semver(raw: &str) -> (u64, u64, u64) {
    let clean = raw.trim().trim_start_matches(['v', 'V']);
    let main = clean.split('-').next().unwrap_or(clean);
    let mut parts = main.split('.');
    let major = parts.next().and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
    let minor = parts.next().and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
    let patch = parts.next().and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
    (major, minor, patch)
}

pub fn is_newer_version(latest: &str, current: &str) -> bool {
    parse_semver(latest) > parse_semver(current)
}

fn extract_sha256(digest: &Option<String>) -> Option<String> {
    let raw = digest.as_deref()?.trim();
    let hex = raw.strip_prefix("sha256:").unwrap_or(raw).trim().to_ascii_lowercase();
    if hex.len() == 64 && hex.chars().all(|c| c.is_ascii_hexdigit()) {
        Some(hex)
    } else {
        None
    }
}

fn detect_install_kind() -> &'static str {
    #[cfg(all(windows, feature = "win7"))]
    {
        return "windows-win7";
    }
    #[cfg(all(windows, not(feature = "win7")))]
    {
        if let Ok(exe) = std::env::current_exe() {
            if let Some(dir) = exe.parent() {
                if dir.join("uninstall.exe").exists() {
                    return "windows-nsis";
                }
            }
        }
        return "windows-portable";
    }
    #[cfg(target_os = "macos")]
    {
        return "macos-app";
    }
    #[cfg(target_os = "linux")]
    {
        if cfg!(target_arch = "aarch64") {
            return "linux-deb-arm64";
        }
        return "linux-deb-amd64";
    }
    #[cfg(all(not(windows), not(target_os = "macos"), not(target_os = "linux")))]
    {
        return "manual";
    }
}

fn select_asset<'a>(assets: &'a [GhAsset], install_kind: &str) -> Option<&'a GhAsset> {
    let want_108 = cfg!(feature = "layout-108");
    match install_kind {
        "windows-nsis" => assets
            .iter()
            .find(|a| {
                let n = a.name.to_ascii_lowercase();
                n.ends_with("-setup.exe") && n.contains("x64") && (n.contains("108key") == want_108)
            })
            .or_else(|| {
                assets.iter().find(|a| {
                    let n = a.name.to_ascii_lowercase();
                    n.ends_with("-setup.exe") && n.contains("x64")
                })
            })
            .or_else(|| {
                assets
                    .iter()
                    .find(|a| a.name.to_ascii_lowercase().ends_with("-windows-x64.exe"))
            }),
        "windows-portable" => assets
            .iter()
            .find(|a| {
                let n = a.name.to_ascii_lowercase();
                n.ends_with("-windows-x64.exe") && (n.contains("108key") == want_108)
            })
            .or_else(|| {
                assets
                    .iter()
                    .find(|a| a.name.to_ascii_lowercase().ends_with("-windows-x64.exe"))
            })
            .or_else(|| {
                assets
                    .iter()
                    .find(|a| a.name.to_ascii_lowercase().ends_with("-setup.exe"))
            }),
        "windows-win7" => assets.iter().find(|a| {
            let n = a.name.to_ascii_lowercase();
            n.contains("win7") && n.ends_with(".zip")
        }),
        "macos-app" => assets
            .iter()
            .find(|a| a.name.to_ascii_lowercase().ends_with("-macos-universal.app.tar.gz"))
            .or_else(|| {
                assets
                    .iter()
                    .find(|a| a.name.to_ascii_lowercase().ends_with("-macos-universal.dmg"))
            }),
        "linux-deb-arm64" => assets
            .iter()
            .find(|a| {
                let n = a.name.to_ascii_lowercase();
                n.ends_with(".deb") && (n.contains("arm64") || n.contains("aarch64"))
            })
            .or_else(|| {
                assets.iter().find(|a| {
                    let n = a.name.to_ascii_lowercase();
                    n.ends_with(".appimage") && (n.contains("arm64") || n.contains("aarch64"))
                })
            }),
        "linux-deb-amd64" => assets
            .iter()
            .find(|a| {
                let n = a.name.to_ascii_lowercase();
                n.ends_with(".deb") && (n.contains("amd64") || n.contains("x86_64"))
            })
            .or_else(|| {
                assets.iter().find(|a| {
                    let n = a.name.to_ascii_lowercase();
                    n.ends_with(".appimage") && (n.contains("amd64") || n.contains("x86_64"))
                })
            }),
        _ => None,
    }
}

fn summarize_notes(body: Option<&str>) -> String {
    let Some(raw) = body else {
        return String::new();
    };
    raw.lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .take(3)
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(180)
        .collect()
}

#[cfg(any(windows, test))]
fn parse_http_url(url: &str) -> Result<(bool, String, u16, String), String> {
    let (is_https, rest) = if let Some(r) = url.strip_prefix("https://") {
        (true, r)
    } else if let Some(r) = url.strip_prefix("http://") {
        (false, r)
    } else {
        return Err(format!("不支持的更新地址协议: {}", url));
    };

    let (authority, path_and_query) = match rest.find('/') {
        Some(idx) => (&rest[..idx], &rest[idx..]),
        None => (rest, "/"),
    };
    if authority.is_empty() {
        return Err("更新地址缺少主机名".to_string());
    }

    let default_port = if is_https { 443 } else { 80 };
    let (host, port) = if let Some(colon) = authority.rfind(':') {
        let port_str = &authority[colon + 1..];
        if let Ok(p) = port_str.parse::<u16>() {
            (&authority[..colon], p)
        } else {
            (authority, default_port)
        }
    } else {
        (authority, default_port)
    };

    Ok((
        is_https,
        host.to_string(),
        port,
        path_and_query.to_string(),
    ))
}

/// Compact pure-Rust FIPS 180-4 SHA-256 hasher (zero external crate overhead, zero PowerShell).
#[cfg(any(windows, test))]
struct Sha256 {
    state: [u32; 8],
    buf: [u8; 64],
    buf_len: usize,
    total_bytes: u64,
}

#[cfg(any(windows, test))]
impl Sha256 {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
        0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
        0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
        0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
        0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
        0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
        0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
    ];

    fn new() -> Self {
        Self {
            state: [
                0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a,
                0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
            ],
            buf: [0u8; 64],
            buf_len: 0,
            total_bytes: 0,
        }
    }

    fn compress(state: &mut [u32; 8], block: &[u8; 64]) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([
                block[i * 4],
                block[i * 4 + 1],
                block[i * 4 + 2],
                block[i * 4 + 3],
            ]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }

        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = *state;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let t1 = h
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(Self::K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);

            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }

        state[0] = state[0].wrapping_add(a);
        state[1] = state[1].wrapping_add(b);
        state[2] = state[2].wrapping_add(c);
        state[3] = state[3].wrapping_add(d);
        state[4] = state[4].wrapping_add(e);
        state[5] = state[5].wrapping_add(f);
        state[6] = state[6].wrapping_add(g);
        state[7] = state[7].wrapping_add(h);
    }

    fn update(&mut self, mut data: &[u8]) {
        self.total_bytes = self.total_bytes.wrapping_add(data.len() as u64);
        if self.buf_len > 0 {
            let take = (64 - self.buf_len).min(data.len());
            self.buf[self.buf_len..self.buf_len + take].copy_from_slice(&data[..take]);
            self.buf_len += take;
            data = &data[take..];
            if self.buf_len == 64 {
                let block = self.buf;
                Self::compress(&mut self.state, &block);
                self.buf_len = 0;
            }
        }
        while data.len() >= 64 {
            let mut block = [0u8; 64];
            block.copy_from_slice(&data[..64]);
            Self::compress(&mut self.state, &block);
            data = &data[64..];
        }
        if !data.is_empty() {
            self.buf[..data.len()].copy_from_slice(data);
            self.buf_len = data.len();
        }
    }

    fn finish_hex(mut self) -> String {
        let bit_len = self.total_bytes.wrapping_mul(8);
        self.buf[self.buf_len] = 0x80;
        self.buf_len += 1;
        if self.buf_len > 56 {
            self.buf[self.buf_len..64].fill(0);
            let block = self.buf;
            Self::compress(&mut self.state, &block);
            self.buf_len = 0;
        }
        self.buf[self.buf_len..56].fill(0);
        self.buf[56..64].copy_from_slice(&bit_len.to_be_bytes());
        let block = self.buf;
        Self::compress(&mut self.state, &block);

        let mut out = String::with_capacity(64);
        for word in self.state {
            use std::fmt::Write;
            let _ = write!(&mut out, "{:08x}", word);
        }
        out
    }
}

#[cfg(windows)]
mod winhttp_client {
    use super::parse_http_url;
    use std::ffi::c_void;
    use std::ptr::{null, null_mut};
    use windows_sys::Win32::Foundation::GetLastError;
    use windows_sys::Win32::Networking::WinHttp::{
        WinHttpCloseHandle, WinHttpConnect, WinHttpOpen, WinHttpOpenRequest,
        WinHttpQueryDataAvailable, WinHttpQueryHeaders, WinHttpReadData, WinHttpReceiveResponse,
        WinHttpSendRequest, WinHttpSetOption, WinHttpSetTimeouts,
        WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, WINHTTP_ACCESS_TYPE_DEFAULT_PROXY, WINHTTP_FLAG_SECURE,
        WINHTTP_FLAG_SECURE_PROTOCOL_TLS1_2, WINHTTP_FLAG_SECURE_PROTOCOL_TLS1_3,
        WINHTTP_OPTION_SECURE_PROTOCOLS, WINHTTP_QUERY_FLAG_NUMBER, WINHTTP_QUERY_STATUS_CODE,
    };

    struct Handle(*mut c_void);

    impl Drop for Handle {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe {
                    let _ = WinHttpCloseHandle(self.0);
                }
            }
        }
    }

    fn to_wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// Performs a native WinHTTP GET request (no PowerShell / curl child process),
    /// streaming response chunks into `on_chunk`. Supports Win7 SP1 through Win11
    /// and honors system / PAC proxies automatically.
    pub fn get_stream(
        url: &str,
        user_agent: &str,
        extra_headers: Option<&str>,
        receive_timeout_ms: i32,
        mut on_chunk: impl FnMut(&[u8]) -> Result<(), String>,
    ) -> Result<(), String> {
        let (is_https, host, port, path) = parse_http_url(url)?;
        let ua_w = to_wide(user_agent);

        // Win8.1/10/11: AUTOMATIC_PROXY reads user IE/system proxy & PAC automatically.
        // Win7 SP1 fallback: DEFAULT_PROXY.
        let session_raw = unsafe {
            let h = WinHttpOpen(
                ua_w.as_ptr(),
                WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
                null(),
                null(),
                0,
            );
            if !h.is_null() {
                h
            } else {
                WinHttpOpen(
                    ua_w.as_ptr(),
                    WINHTTP_ACCESS_TYPE_DEFAULT_PROXY,
                    null(),
                    null(),
                    0,
                )
            }
        };
        if session_raw.is_null() {
            let err = unsafe { GetLastError() };
            return Err(format!("初始化 WinHTTP 会话失败 (错误码 {})", err));
        }
        let session = Handle(session_raw);

        unsafe {
            let _ = WinHttpSetTimeouts(session.0, 15_000, 15_000, 30_000, receive_timeout_ms);
            let mut protocols =
                WINHTTP_FLAG_SECURE_PROTOCOL_TLS1_2 | WINHTTP_FLAG_SECURE_PROTOCOL_TLS1_3;
            if WinHttpSetOption(
                session.0,
                WINHTTP_OPTION_SECURE_PROTOCOLS,
                &protocols as *const u32 as *const c_void,
                std::mem::size_of::<u32>() as u32,
            ) == 0
            {
                protocols = WINHTTP_FLAG_SECURE_PROTOCOL_TLS1_2;
                let _ = WinHttpSetOption(
                    session.0,
                    WINHTTP_OPTION_SECURE_PROTOCOLS,
                    &protocols as *const u32 as *const c_void,
                    std::mem::size_of::<u32>() as u32,
                );
            }
        }

        let host_w = to_wide(&host);
        let connect_raw = unsafe { WinHttpConnect(session.0, host_w.as_ptr(), port, 0) };
        if connect_raw.is_null() {
            let err = unsafe { GetLastError() };
            return Err(format!("连接更新服务器失败 (错误码 {})", err));
        }
        let connect = Handle(connect_raw);

        let verb_w = to_wide("GET");
        let path_w = to_wide(&path);
        let flags = if is_https { WINHTTP_FLAG_SECURE } else { 0 };
        let req_raw = unsafe {
            WinHttpOpenRequest(
                connect.0,
                verb_w.as_ptr(),
                path_w.as_ptr(),
                null(),
                null(),
                null(),
                flags,
            )
        };
        if req_raw.is_null() {
            let err = unsafe { GetLastError() };
            return Err(format!("创建更新请求失败 (错误码 {})", err));
        }
        let request = Handle(req_raw);

        let headers_w = extra_headers.map(to_wide);
        let (hdr_ptr, hdr_len) = match &headers_w {
            Some(buf) => (buf.as_ptr(), (buf.len().saturating_sub(1)) as u32),
            None => (null(), 0),
        };

        let sent = unsafe {
            WinHttpSendRequest(request.0, hdr_ptr, hdr_len, null(), 0, 0, 0)
        };
        if sent == 0 {
            let err = unsafe { GetLastError() };
            return Err(format!("连接 GitHub 检查更新失败，请检查网络或代理 (错误码 {})", err));
        }

        let received = unsafe { WinHttpReceiveResponse(request.0, null_mut()) };
        if received == 0 {
            let err = unsafe { GetLastError() };
            return Err(format!("等待 GitHub 响应失败，请检查网络或代理 (错误码 {})", err));
        }

        let mut status_code: u32 = 0;
        let mut status_size = std::mem::size_of::<u32>() as u32;
        let got_status = unsafe {
            WinHttpQueryHeaders(
                request.0,
                WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
                null(),
                &mut status_code as *mut u32 as *mut c_void,
                &mut status_size,
                null_mut(),
            )
        };
        if got_status == 0 {
            let err = unsafe { GetLastError() };
            return Err(format!("读取 HTTP 状态码失败 (错误码 {})", err));
        }
        if !(200..=299).contains(&status_code) {
            return Err(format!("服务器返回 HTTP {}", status_code));
        }

        let mut buf = vec![0u8; 32 * 1024];
        loop {
            let mut available: u32 = 0;
            if unsafe { WinHttpQueryDataAvailable(request.0, &mut available) } == 0 {
                let err = unsafe { GetLastError() };
                return Err(format!("读取网络数据流失败 (错误码 {})", err));
            }
            if available == 0 {
                break;
            }
            let mut remaining = available as usize;
            while remaining > 0 {
                let to_read = remaining.min(buf.len()) as u32;
                let mut bytes_read: u32 = 0;
                if unsafe {
                    WinHttpReadData(
                        request.0,
                        buf.as_mut_ptr() as *mut c_void,
                        to_read,
                        &mut bytes_read,
                    )
                } == 0
                {
                    let err = unsafe { GetLastError() };
                    return Err(format!("读取响应内容失败 (错误码 {})", err));
                }
                if bytes_read == 0 {
                    break;
                }
                on_chunk(&buf[..bytes_read as usize])?;
                remaining = remaining.saturating_sub(bytes_read as usize);
            }
        }

        Ok(())
    }
}

#[cfg(windows)]
fn fetch_latest_release_json() -> Result<String, String> {
    let ua = format!("KuroCat/{}", CURRENT_VERSION);
    let mut body = Vec::with_capacity(8192);
    winhttp_client::get_stream(
        REPO_API_LATEST,
        &ua,
        Some("Accept: application/vnd.github+json\r\n"),
        20_000,
        |chunk| {
            if body.len() + chunk.len() > 2 * 1024 * 1024 {
                return Err("更新接口响应过大".to_string());
            }
            body.extend_from_slice(chunk);
            Ok(())
        },
    )?;

    let text = String::from_utf8_lossy(&body).trim().to_string();
    if text.is_empty() {
        return Err("GitHub 返回空响应".to_string());
    }
    Ok(text)
}

#[cfg(not(windows))]
fn fetch_latest_release_json() -> Result<String, String> {
    use std::process::Command;

    let curl_bin = if cfg!(target_os = "macos") {
        "/usr/bin/curl"
    } else {
        "curl"
    };
    let output = Command::new(curl_bin)
        .args([
            "-fsSL",
            "--connect-timeout",
            "10",
            "--max-time",
            "20",
            "-H",
            &format!("User-Agent: KuroCat/{}", CURRENT_VERSION),
            "-H",
            "Accept: application/vnd.github+json",
            REPO_API_LATEST,
        ])
        .output()
        .map_err(|e| format!("无法调用 curl 检查更新: {}", e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(if stderr.is_empty() {
            "连接 GitHub 检查更新失败，请检查网络或代理".to_string()
        } else {
            format!("检查更新失败: {}", stderr)
        });
    }

    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if text.is_empty() {
        return Err("GitHub 返回空响应".to_string());
    }
    Ok(text)
}

pub fn check_latest_update() -> Result<UpdateInfo, String> {
    let raw_json = fetch_latest_release_json()?;
    let rel: GhRelease = serde_json::from_str(&raw_json)
        .map_err(|e| format!("解析最新版本信息失败: {}", e))?;

    let latest_clean = rel
        .tag_name
        .trim()
        .trim_start_matches(['v', 'V'])
        .to_string();
    let has_update = is_newer_version(&latest_clean, CURRENT_VERSION);
    let install_kind = detect_install_kind();
    let asset = select_asset(&rel.assets, install_kind);

    Ok(UpdateInfo {
        current_version: CURRENT_VERSION.to_string(),
        latest_version: latest_clean,
        has_update,
        release_url: rel
            .html_url
            .unwrap_or_else(|| REPO_RELEASES_PAGE.to_string()),
        asset_name: asset.map(|a| a.name.clone()),
        download_url: asset.map(|a| a.browser_download_url.clone()),
        asset_sha256: asset.and_then(|a| extract_sha256(&a.digest)),
        asset_size: asset.and_then(|a| a.size),
        install_kind: install_kind.to_string(),
        notes_summary: summarize_notes(rel.body.as_deref()),
    })
}

fn emit_progress(app: &AppHandle, stage: &str, message: impl Into<String>, info: Option<UpdateInfo>) {
    let _ = app.emit(
        "kuro-update-status",
        UpdateProgressEvent {
            stage: stage.to_string(),
            message: message.into(),
            info,
        },
    );
}

/// Only plain http(s) URLs may ever be handed to the OS shell.
/// Without this, a compromised renderer could pass `file:///...exe` or a UNC path
/// (`\\host\share\x.exe`) to `ShellExecuteW` and get arbitrary code launched.
fn is_safe_web_url(url: &str) -> bool {
    let trimmed = url.trim();
    let lower = trimmed.to_ascii_lowercase();
    let rest = if let Some(r) = lower.strip_prefix("https://") {
        r
    } else if let Some(r) = lower.strip_prefix("http://") {
        r
    } else {
        return false;
    };
    !rest.is_empty()
        && !rest.contains('\\')
        && !rest.contains('@')
        && !rest.chars().any(|c| c.is_whitespace() || c.is_control())
}

pub fn open_url_in_browser(url: &str) {
    if !is_safe_web_url(url) {
        return;
    }
    #[cfg(windows)]
    {
        use std::ptr::{null, null_mut};
        use windows_sys::Win32::UI::Shell::ShellExecuteW;
        use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

        let op: Vec<u16> = "open\0".encode_utf16().collect();
        let target: Vec<u16> = url.encode_utf16().chain(std::iter::once(0)).collect();
        unsafe {
            ShellExecuteW(
                null_mut(),
                op.as_ptr(),
                target.as_ptr(),
                null(),
                null(),
                SW_SHOWNORMAL,
            );
        }
    }
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("/usr/bin/open").arg(url).spawn();
    }
    #[cfg(target_os = "linux")]
    {
        let _ = std::process::Command::new("xdg-open").arg(url).spawn();
    }
}

// ---------------------------------------------------------------------------
// 绿色版（便携版）更新包的落盘位置与待更新标记
// ---------------------------------------------------------------------------

/// 把任意版本串收成安全的文件名片段，防止 tag 里混进路径分隔符。
#[cfg_attr(not(windows), allow(dead_code))]
fn sanitize_version(raw: &str) -> String {
    let cleaned: String = raw
        .trim()
        .trim_start_matches(['v', 'V'])
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '.' || *c == '-' || *c == '_')
        .collect();
    if cleaned.is_empty() {
        "unknown".to_string()
    } else {
        cleaned
    }
}

/// 绿色版更新包目录：优先 exe 同级的 `updates\`，让「整个文件夹拷走就是搬家」的语义
/// 继续成立，用户也能自己找到下载好的包。该目录不可写（装进 Program Files、只读介质、
/// 直接双击 zip 里的 exe 而落在临时解压目录）时退回 `%APPDATA%\kuro-cat-pet\updates`。
#[cfg(windows)]
pub fn portable_update_dir() -> PathBuf {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let cand = dir.join("updates");
            if std::fs::create_dir_all(&cand).is_ok() {
                let probe = cand.join(format!(".kurocat-probe-{}", std::process::id()));
                let _ = std::fs::remove_file(&probe);
                let writable = std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&probe)
                    .is_ok();
                if writable {
                    let _ = std::fs::remove_file(&probe);
                    return cand;
                }
            }
        }
    }
    let fallback = std::env::var("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
        .join("kuro-cat-pet")
        .join("updates");
    let _ = std::fs::create_dir_all(&fallback);
    fallback
}

#[cfg(windows)]
fn pending_marker_path() -> PathBuf {
    portable_update_dir().join("pending.json")
}

#[cfg(windows)]
fn clear_pending_portable_update() {
    let _ = std::fs::remove_file(pending_marker_path());
}

#[cfg(windows)]
fn write_pending_portable_update(pending: &PendingPortableUpdate) -> Result<(), String> {
    let path = pending_marker_path();
    let json = serde_json::to_string_pretty(pending)
        .map_err(|e| format!("序列化更新标记失败: {}", e))?;
    std::fs::write(&path, json).map_err(|e| format!("写入更新标记失败: {}", e))
}

/// 读取待应用的更新包。四重过滤：标记可读、文件还在（或已随文件夹移动）、
/// 版本确实比当前新、过期残留自动清理。
///
/// 绿色版目录被整个拷到另一台机器时，标记会跟着走，版本比对可以避免假提示。
#[cfg(windows)]
fn read_pending_windows() -> Option<PendingPortableUpdate> {
    let raw = std::fs::read_to_string(pending_marker_path()).ok()?;
    let mut pending: PendingPortableUpdate = serde_json::from_str(&raw).ok()?;

    if !pending.file().is_file() {
        // 用户把绿色版文件夹整体挪了位置：绝对路径失效，但包还在同名的 updates\ 里。
        let relocated = pending
            .file()
            .file_name()
            .map(|name| portable_update_dir().join(name));
        match relocated {
            Some(path) if path.is_file() => {
                pending.file_path = path.to_string_lossy().to_string();
            }
            _ => {
                clear_pending_portable_update();
                return None;
            }
        }
    }

    if !is_newer_version(&pending.version, CURRENT_VERSION) {
        let _ = std::fs::remove_file(pending.file());
        clear_pending_portable_update();
        return None;
    }
    Some(pending)
}

pub fn read_pending_portable_update() -> Option<PendingPortableUpdate> {
    #[cfg(windows)]
    {
        return read_pending_windows();
    }
    #[cfg(not(windows))]
    {
        None
    }
}

/// 打开更新包所在文件夹并选中文件（`explorer /select`）。
pub fn reveal_pending_update_folder() -> Result<(), String> {
    #[cfg(windows)]
    {
        let target = read_pending_portable_update()
            .map(|p| p.file())
            .unwrap_or_else(portable_update_dir);
        // explorer 的 /select 需要「一个」参数，路径含空格时由 Command 自行加引号。
        std::process::Command::new("explorer")
            .arg(format!("/select,{}", target.to_string_lossy()))
            .spawn()
            .map_err(|e| format!("打开文件夹失败: {}", e))?;
        return Ok(());
    }
    #[cfg(not(windows))]
    {
        Err("当前平台没有需要手动查看的更新包".to_string())
    }
}

/// 复验已落盘的更新包：目录里的文件可能被用户替换、被安全软件改写或写坏。
#[cfg(windows)]
fn hash_file_sha256(path: &PathBuf) -> Result<String, String> {
    use std::io::Read;

    let mut file = std::fs::File::open(path).map_err(|e| format!("打开更新包失败: {}", e))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buf)
            .map_err(|e| format!("读取更新包失败: {}", e))?;
        if read == 0 {
            break;
        }
        hasher.update(&buf[..read]);
    }
    Ok(hasher.finish_hex())
}

/// 安装器/新进程已经接棒，旧进程留一点时间把日志刷完再退。
#[cfg(windows)]
fn schedule_process_exit(app: &AppHandle) {
    let app_clone = app.clone();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(450));
        app_clone.exit(0);
    });
}

#[cfg(windows)]
fn download_and_verify_windows(url: &str, dest: &PathBuf, expected_sha256: Option<&str>) -> Result<(), String> {
    use std::io::Write;

    let part_path = dest.with_extension("part");
    let _ = std::fs::remove_file(&part_path);
    let mut file = std::fs::File::create(&part_path)
        .map_err(|e| format!("无法创建下载临时文件: {}", e))?;

    let ua = format!("KuroCat/{}", CURRENT_VERSION);
    let mut hasher = Sha256::new();
    let mut total_bytes: u64 = 0;

    let download_res = winhttp_client::get_stream(url, &ua, None, 180_000, |chunk| {
        file.write_all(chunk)
            .map_err(|e| format!("写入安装包失败: {}", e))?;
        hasher.update(chunk);
        total_bytes += chunk.len() as u64;
        Ok(())
    });

    drop(file);
    if let Err(err) = download_res {
        let _ = std::fs::remove_file(&part_path);
        return Err(err);
    }

    if total_bytes < 500_000 {
        let _ = std::fs::remove_file(&part_path);
        return Err("下载文件不完整".to_string());
    }

    if let Some(expected) = expected_sha256 {
        let expected_clean = expected.trim().to_ascii_lowercase();
        if !expected_clean.is_empty() {
            let actual = hasher.finish_hex();
            if actual != expected_clean {
                let _ = std::fs::remove_file(&part_path);
                return Err(format!("SHA-256 校验失败: {}", actual));
            }
        }
    }

    let _ = std::fs::remove_file(dest);
    std::fs::rename(&part_path, dest).map_err(|e| {
        let _ = std::fs::remove_file(&part_path);
        format!("保存安装包失败: {}", e)
    })?;
    Ok(())
}

#[cfg(windows)]
fn install_windows_update(app: &AppHandle, info: &UpdateInfo) -> Result<(), String> {
    use std::process::Command;

    if info.install_kind == "windows-win7"
        || info
            .asset_name
            .as_deref()
            .map(|n| n.to_ascii_lowercase().ends_with(".zip"))
            .unwrap_or(false)
    {
        let target_url = info.download_url.as_deref().unwrap_or(&info.release_url);
        open_url_in_browser(target_url);
        emit_progress(
            app,
            "up_to_date",
            format!("已在浏览器打开 v{} Win7 兼容整合包下载页", info.latest_version),
            Some(info.clone()),
        );
        return Ok(());
    }

    let url = info
        .download_url
        .as_deref()
        .ok_or_else(|| "未找到匹配的 Windows 更新包".to_string())?;
    let asset_name = info
        .asset_name
        .as_deref()
        .unwrap_or("KuroCat-update.exe");
    let is_setup_installer = asset_name.to_ascii_lowercase().ends_with("-setup.exe");

    emit_progress(
        app,
        "downloading",
        format!("正在下载 v{}...", info.latest_version),
        Some(info.clone()),
    );

    // 安装版装完由 NSIS 安装器接管，落在 %TEMP% 就行；
    // 绿色版要让用户之后能自己找到这个包，所以落到 exe 同级的 updates\。
    let target_file = if is_setup_installer {
        let temp_dir = std::env::temp_dir().join("kurocat-updates");
        let _ = std::fs::create_dir_all(&temp_dir);
        let safe_name = asset_name.replace(['\\', '/', ':', '*', '?', '"', '<', '>', '|'], "_");
        temp_dir.join(safe_name)
    } else {
        // `.exe.new` 后缀：用户误双击 updates\ 里的包时不会启动一个
        // 「数据目录变成 updates\data」的野生实例。
        portable_update_dir().join(format!(
            "KuroCat-{}.exe.new",
            sanitize_version(&info.latest_version)
        ))
    };

    download_and_verify_windows(url, &target_file, info.asset_sha256.as_deref())?;

    if is_setup_installer {
        // Launch Tauri NSIS installer in passive auto-relaunch update mode (/P /R /UPDATE)
        emit_progress(
            app,
            "installing",
            format!("校验通过，正在安装 v{} 并重启...", info.latest_version),
            Some(info.clone()),
        );
        Command::new(&target_file)
            .args(["/P", "/R", "/UPDATE"])
            .spawn()
            .map_err(|e| format!("启动安装程序失败: {}", e))?;
        schedule_process_exit(app);
        return Ok(());
    }

    // 绿色版：只落盘 + 写待更新标记，**不**替换二进制、**不**重启。
    // 宠物是常驻进程，下载完就强杀重启会打断用户正在做的事；交给用户点「重启更新」。
    let pending = PendingPortableUpdate {
        version: info.latest_version.clone(),
        file_path: target_file.to_string_lossy().to_string(),
        sha256: info.asset_sha256.clone(),
    };
    if let Err(err) = write_pending_portable_update(&pending) {
        let _ = std::fs::remove_file(&target_file);
        return Err(err);
    }

    emit_progress(
        app,
        "ready",
        format!(
            "v{} 已下载完成，点「重启更新」即可生效",
            sanitize_version(&info.latest_version)
        ),
        Some(info.clone()),
    );
    Ok(())
}

/// 把已落盘的绿色版更新包应用到位，然后拉起新进程接管。
///
/// 这一步由用户点「重启更新」触发，或者用户在下次启动后再次点「检查更新」触发。
/// 顺序与旧的自替换逻辑一致：先给旧 exe 改名让位（Windows 允许 rename 正在运行的
/// exe），再复制新文件进来，最后拉新进程并把 `.exe.old` 交给它清理。任一步失败都
/// 回滚到旧 exe，保证程序始终有一个可运行的二进制。
#[cfg(windows)]
pub fn apply_portable_update(app: &AppHandle) -> Result<(), String> {
    use std::process::Command;

    let Some(pending) = read_pending_portable_update() else {
        return Err("没有已下载的更新包，请先检查更新".to_string());
    };
    let update_file = pending.file();

    // 复验摘要：包落在用户可写的目录里，下载之后可能被替换或写坏。
    if let Some(expected) = pending.sha256.as_deref() {
        let actual = hash_file_sha256(&update_file)?;
        if !actual.eq_ignore_ascii_case(expected.trim()) {
            let _ = std::fs::remove_file(&update_file);
            clear_pending_portable_update();
            return Err("更新包校验失败，已删除，请重新下载".to_string());
        }
    }

    emit_progress(
        app,
        "installing",
        format!("正在应用 v{} 并重启...", pending.version),
        None,
    );

    let current_exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let pid = std::process::id();
    let old_exe = current_exe.with_extension("exe.old");
    let _ = std::fs::remove_file(&old_exe);

    std::fs::rename(&current_exe, &old_exe)
        .map_err(|e| format!("备份旧版本程序失败: {}", e))?;

    if let Err(e) = std::fs::copy(&update_file, &current_exe) {
        let _ = std::fs::rename(&old_exe, &current_exe);
        return Err(format!("替换新版本程序失败: {}", e));
    }
    let _ = std::fs::remove_file(&update_file);
    clear_pending_portable_update();

    if let Err(e) = Command::new(&current_exe)
        .args([
            "--wait-pid",
            &pid.to_string(),
            "--cleanup-old-exe",
            &old_exe.to_string_lossy(),
        ])
        .spawn()
    {
        let _ = std::fs::remove_file(&current_exe);
        let _ = std::fs::rename(&old_exe, &current_exe);
        return Err(format!("启动新版本程序失败: {}", e));
    }

    schedule_process_exit(app);
    Ok(())
}

#[cfg(not(windows))]
pub fn apply_portable_update(_app: &AppHandle) -> Result<(), String> {
    Err("当前平台不需要手动应用更新包".to_string())
}

#[cfg(target_os = "macos")]
fn find_current_macos_app_bundle() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    for ancestor in exe.ancestors() {
        if ancestor.extension().and_then(|s| s.to_str()) == Some("app") {
            return Some(ancestor.to_path_buf());
        }
    }
    None
}

#[cfg(target_os = "macos")]
fn install_macos_update(app: &AppHandle, info: &UpdateInfo) -> Result<(), String> {
    use std::process::Command;

    let url = info
        .download_url
        .as_deref()
        .ok_or_else(|| "未找到匹配的 macOS 更新包".to_string())?;
    let asset_name = info
        .asset_name
        .as_deref()
        .unwrap_or("KuroCat-macos-universal.app.tar.gz");

    if !asset_name.to_ascii_lowercase().ends_with(".tar.gz") {
        open_url_in_browser(url);
        return Ok(());
    }

    let app_bundle = find_current_macos_app_bundle()
        .unwrap_or_else(|| PathBuf::from("/Applications/KuroCat.app"));

    emit_progress(
        app,
        "downloading",
        format!("正在下载 macOS v{}...", info.latest_version),
        Some(info.clone()),
    );

    let temp_dir = std::env::temp_dir().join(format!("kurocat-update-{}", info.latest_version));
    let _ = std::fs::remove_dir_all(&temp_dir);
    std::fs::create_dir_all(&temp_dir).map_err(|e| e.to_string())?;
    let archive_path = temp_dir.join("KuroCat-update.app.tar.gz");

    let dl = Command::new("/usr/bin/curl")
        .args([
            "-fsSL",
            "--connect-timeout",
            "15",
            "--max-time",
            "240",
            "-H",
            &format!("User-Agent: KuroCat/{}", CURRENT_VERSION),
            "-o",
            archive_path.to_str().unwrap_or("/tmp/KuroCat-update.app.tar.gz"),
            url,
        ])
        .output()
        .map_err(|e| format!("下载失败: {}", e))?;

    if !dl.status.success() {
        return Err("下载 macOS 更新包失败".to_string());
    }

    // Verify SHA-256 if GitHub Release provided a digest
    if let Some(expected_sha) = info.asset_sha256.as_deref() {
        emit_progress(
            app,
            "verifying",
            "正在校验安装包完整性...",
            Some(info.clone()),
        );
        let shasum = Command::new("/usr/bin/shasum")
            .args(["-a", "256", archive_path.to_str().unwrap_or("")])
            .output()
            .map_err(|e| format!("校验失败: {}", e))?;
        let out_str = String::from_utf8_lossy(&shasum.stdout);
        let actual = out_str.split_whitespace().next().unwrap_or("").to_ascii_lowercase();
        if actual != expected_sha.to_ascii_lowercase() {
            return Err("安装包 SHA-256 校验未通过".to_string());
        }
    }

    let extract_dir = temp_dir.join("extracted");
    std::fs::create_dir_all(&extract_dir).map_err(|e| e.to_string())?;
    let untar = Command::new("/usr/bin/tar")
        .args([
            "-xzf",
            archive_path.to_str().unwrap_or(""),
            "-C",
            extract_dir.to_str().unwrap_or(""),
        ])
        .output()
        .map_err(|e| format!("解压失败: {}", e))?;
    if !untar.status.success() {
        return Err("解压更新包失败".to_string());
    }

    // Find extracted .app (KuroCat.app or KuroCatPet.app)
    let new_app = std::fs::read_dir(&extract_dir)
        .ok()
        .and_then(|mut it| {
            it.find_map(|entry| {
                let p = entry.ok()?.path();
                if p.extension().and_then(|s| s.to_str()) == Some("app") {
                    Some(p)
                } else {
                    None
                }
            })
        })
        .ok_or_else(|| "更新包内未找到 .app 程序".to_string())?;

    // Clear quarantine flag so macOS Gatekeeper won't block the freshly updated open-source app
    let _ = Command::new("/usr/bin/xattr")
        .args(["-dr", "com.apple.quarantine", new_app.to_str().unwrap_or("")])
        .status();

    emit_progress(
        app,
        "installing",
        format!("正在安装 v{} 并重启...", info.latest_version),
        Some(info.clone()),
    );

    let pid = std::process::id();
    let dst_str = app_bundle.to_string_lossy().replace('\'', "'\\''");
    let src_str = new_app.to_string_lossy().replace('\'', "'\\''");
    let tmp_str = temp_dir.to_string_lossy().replace('\'', "'\\''");
    let swap_sh = format!(
        "for i in $(seq 1 40); do kill -0 {pid} 2>/dev/null || break; sleep 0.2; done; \
         sleep 0.25; \
         rm -rf '{dst}.bak'; \
         if [ -d '{dst}' ]; then mv '{dst}' '{dst}.bak'; fi; \
         if mv '{src}' '{dst}'; then \
           rm -rf '{dst}.bak' '{tmp}'; \
           /usr/bin/xattr -dr com.apple.quarantine '{dst}' 2>/dev/null || true; \
           /usr/bin/open -n '{dst}'; \
         else \
           if [ -d '{dst}.bak' ]; then mv '{dst}.bak' '{dst}'; fi; \
           /usr/bin/open '{url}'; \
         fi",
        pid = pid,
        dst = dst_str,
        src = src_str,
        tmp = tmp_str,
        url = info.release_url.replace('\'', "'\\''"),
    );

    Command::new("/bin/sh")
        .args(["-c", &swap_sh])
        .spawn()
        .map_err(|e| format!("启动更新脚本失败: {}", e))?;

    let app_clone = app.clone();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(450));
        app_clone.exit(0);
    });

    Ok(())
}

/// Our own release channel. `install_update` is invoked from the renderer with a full
/// `UpdateInfo`, so every field it supplies is untrusted until re-anchored to this path.
const RELEASE_PAGE_PREFIX: &str = "https://github.com/keshuishiliu/kurocat/releases";
const RELEASE_DOWNLOAD_PREFIX: &str = "https://github.com/keshuishiliu/kurocat/releases/download/";

/// Re-anchors a renderer-supplied `UpdateInfo` to the project's own GitHub release path and
/// refuses anything that cannot be integrity-checked. Without this, a compromised renderer
/// could point `downloadUrl` at an attacker-hosted installer and have it silently executed.
fn validate_update_target(info: &UpdateInfo) -> Result<(), String> {
    if !info.release_url.starts_with(RELEASE_PAGE_PREFIX) {
        return Err(format!("更新来源不在本项目发布页，已拒绝：{}", info.release_url));
    }
    if let Some(url) = info.download_url.as_deref() {
        if !url.is_empty() && !url.starts_with(RELEASE_DOWNLOAD_PREFIX) {
            return Err(format!("更新包地址不在本项目 release 下，已拒绝：{}", url));
        }
    }
    let hash_ok = info
        .asset_sha256
        .as_deref()
        .map(|h| h.len() == 64 && h.chars().all(|c| c.is_ascii_hexdigit()))
        .unwrap_or(false);
    if !hash_ok {
        return Err("更新包缺少可校验的 SHA-256 摘要，已拒绝安装".to_string());
    }
    Ok(())
}

pub fn perform_update_install(app: &AppHandle, info: UpdateInfo) -> Result<(), String> {
    if !info.has_update {
        emit_progress(
            app,
            "up_to_date",
            format!("已是最新版本 v{}", info.current_version),
            Some(info),
        );
        return Ok(());
    }

    // SECURITY: fail closed before any download or process launch.
    if let Err(err) = validate_update_target(&info) {
        emit_progress(app, "error", err.clone(), Some(info));
        return Err(err);
    }

    #[cfg(windows)]
    {
        return install_windows_update(app, &info);
    }

    #[cfg(target_os = "macos")]
    {
        return install_macos_update(app, &info);
    }

    #[cfg(all(not(windows), not(target_os = "macos")))]
    {
        let target_url = info.download_url.as_deref().unwrap_or(&info.release_url);
        open_url_in_browser(target_url);
        emit_progress(
            app,
            "up_to_date",
            format!("已在浏览器打开 v{} 安装包下载页", info.latest_version),
            Some(info),
        );
        Ok(())
    }
}

pub fn start_background_update_checker(app: AppHandle) {
    std::thread::spawn(move || {
        // Gentle initial delay after launch so startup stays instant & quiet
        std::thread::sleep(std::time::Duration::from_secs(8));
        loop {
            if let Ok(info) = check_latest_update() {
                if info.has_update {
                    emit_progress(
                        &app,
                        "available",
                        format!("发现新版本 v{}", info.latest_version),
                        Some(info),
                    );
                }
            }
            // Re-check every 6 hours while running
            std::thread::sleep(std::time::Duration::from_secs(6 * 3600));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn portable_asset_name_is_sanitized() {
        // tag 里混进路径分隔符时不能拼出目录穿越的文件名
        assert_eq!(sanitize_version("v4.13.0"), "4.13.0");
        assert_eq!(sanitize_version("V4.13.0"), "4.13.0");
        assert_eq!(sanitize_version("4.13.0-beta.1"), "4.13.0-beta.1");
        assert_eq!(sanitize_version("../../evil"), "....evil");
        let traversal = sanitize_version("../\\/x");
        assert!(!traversal.contains('/'));
        assert!(!traversal.contains('\\'));
        assert_eq!(sanitize_version("v"), "unknown");
        assert_eq!(sanitize_version("   "), "unknown");
    }

    #[test]
    fn semver_comparison_works() {
        assert!(is_newer_version("4.4.0", "4.3.0"));
        assert!(is_newer_version("v4.10.0", "4.4.0"));
        assert!(is_newer_version("5.0.0", "4.9.9"));
        assert!(!is_newer_version("4.4.0", "4.4.0"));
        assert!(!is_newer_version("v4.3.9", "4.4.0"));
    }

    #[test]
    fn sha256_extraction_validates_hex() {
        let valid = Some("sha256:05c1be2c444f5303770f6d59e216bc0ed7b69321fc44368e4ec231405570f950".to_string());
        assert_eq!(
            extract_sha256(&valid).as_deref(),
            Some("05c1be2c444f5303770f6d59e216bc0ed7b69321fc44368e4ec231405570f950")
        );
        assert_eq!(extract_sha256(&Some("sha256:short".to_string())), None);
    }

    #[test]
    fn pure_rust_sha256_matches_nist_vectors() {
        let h_empty = Sha256::new();
        assert_eq!(
            h_empty.finish_hex(),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        let mut h_abc = Sha256::new();
        h_abc.update(b"abc");
        assert_eq!(
            h_abc.finish_hex(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let mut h_long = Sha256::new();
        h_long.update(b"abcdbcdecdefdefgefghfghighijhijk");
        h_long.update(b"ijkljklmklmnlmnomnopnopq");
        assert_eq!(
            h_long.finish_hex(),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
    }

    #[test]
    fn only_web_urls_reach_the_os_shell() {
        assert!(is_safe_web_url("https://github.com/keshuishiliu/kurocat/releases/latest"));
        assert!(is_safe_web_url("http://127.0.0.1:27421/obs"));
        assert!(!is_safe_web_url("file:///C:/Windows/System32/cmd.exe"));
        assert!(!is_safe_web_url("\\\\attacker\\share\\payload.exe"));
        assert!(!is_safe_web_url("C:\\Windows\\System32\\calc.exe"));
        assert!(!is_safe_web_url("https://localhost@evil.example/x"));
        assert!(!is_safe_web_url("javascript:alert(1)"));
        assert!(!is_safe_web_url(""));
    }

    #[test]
    fn rejects_update_info_that_escapes_the_official_release_path() {
        let official = UpdateInfo {
            current_version: "4.9.1".to_string(),
            latest_version: "4.9.2".to_string(),
            has_update: true,
            release_url: "https://github.com/keshuishiliu/kurocat/releases/tag/v4.10.1".to_string(),
            asset_name: Some("KuroCat_4.9.2_x64-setup.exe".to_string()),
            download_url: Some(
                "https://github.com/keshuishiliu/kurocat/releases/download/v4.10.1/KuroCat-4.10.1-x64-setup.exe"
                    .to_string(),
            ),
            asset_sha256: Some(
                "05c1be2c444f5303770f6d59e216bc0ed7b69321fc44368e4ec231405570f950".to_string(),
            ),
            asset_size: Some(9_700_000),
            install_kind: "windows".to_string(),
            notes_summary: String::new(),
        };
        assert!(validate_update_target(&official).is_ok());

        let mut foreign_host = official.clone();
        foreign_host.download_url =
            Some("https://evil.example/KuroCat-setup.exe".to_string());
        assert!(validate_update_target(&foreign_host).is_err());

        // A look-alike path on github.com itself must also be refused.
        let mut other_repo = official.clone();
        other_repo.download_url =
            Some("https://github.com/attacker/kurocat/releases/download/v1/x-setup.exe".to_string());
        assert!(validate_update_target(&other_repo).is_err());

        let mut no_hash = official.clone();
        no_hash.asset_sha256 = None;
        assert!(validate_update_target(&no_hash).is_err());

        let mut short_hash = official.clone();
        short_hash.asset_sha256 = Some("deadbeef".to_string());
        assert!(validate_update_target(&short_hash).is_err());
    }

    #[test]
    fn http_url_parser_handles_github_endpoints() {
        let (https, host, port, path) =
            parse_http_url("https://api.github.com/repos/keshuishiliu/kurocat/releases/latest").unwrap();
        assert!(https);
        assert_eq!(host, "api.github.com");
        assert_eq!(port, 443);
        assert_eq!(path, "/repos/keshuishiliu/kurocat/releases/latest");

        let (https2, host2, port2, path2) =
            parse_http_url("http://127.0.0.1:8080/dl?v=1").unwrap();
        assert!(!https2);
        assert_eq!(host2, "127.0.0.1");
        assert_eq!(port2, 8080);
        assert_eq!(path2, "/dl?v=1");
    }

    #[test]
    #[ignore = "requires live internet access to GitHub API"]
    fn live_check_latest_update_uses_native_http() {
        let info = check_latest_update().expect("native WinHTTP release check should succeed");
        assert!(!info.latest_version.is_empty());
        assert!(info.download_url.is_some());
    }
}
