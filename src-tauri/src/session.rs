//! 会话持久化：把 Cookie 与 localStorage 快照读写到本地文件（app_data_dir/session.json）。
//!
//! 持久化采用双层策略：
//! 1. 原生 WebView 存储（macOS WKWebView defaultDataStore / Windows WebView2 profile），
//!    自动持久化全部 Cookie（含 HttpOnly）与 localStorage —— 这是登录态保持的主路径；
//! 2. 本模块提供的文件快照 —— 双保险，覆盖原生存储被清除 / 换构建 / 系统清理等场景，
//!    由注入页面的 init.js 定期同步并在启动时恢复。
//!
//! 可选能力（Cargo feature）：
//! - `encrypt-session`：快照落盘前加密（Windows DPAPI / macOS Keychain+AES-256-GCM），
//!   旧版明文快照仍可读取（向后兼容）；
//! - `full-cookie-snapshot`：通过原生 WebView 存储接口读写 HttpOnly Cookie，
//!   让登录态在原生存储丢失后也能从文件快照完整恢复（Windows WebView2 / macOS WKWebView）。

use std::path::PathBuf;
use tauri::{AppHandle, Manager, Webview};

/// 快照文件相对 app_data_dir 的文件名
const SESSION_FILE: &str = "session.json";

fn session_path(app: &AppHandle) -> Result<PathBuf, String> {
    let dir = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("无法获取应用数据目录: {e}"))?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("无法创建应用数据目录: {e}"))?;
    Ok(dir.join(SESSION_FILE))
}

/// 判断快照 JSON 是否"空会话"（既无 cookie 也无 localStorage 数据）。
/// 用于防污染：页面加载中 / 未登录中间态产生的空快照，不得覆盖已有的好快照。
fn is_empty_snapshot(value: &serde_json::Value) -> bool {
    let ls_empty = value
        .get("localStorage")
        .map(|v| v.as_object().map(|o| o.is_empty()).unwrap_or(true))
        .unwrap_or(true);
    let cookies_empty = value
        .get("cookies")
        .map(|v| v.as_str().map(|s| s.is_empty()).unwrap_or(true))
        .unwrap_or(true);
    ls_empty && cookies_empty
}

// =========================================================================
// 会话快照加密（feature: encrypt-session）
// =========================================================================
#[cfg(feature = "encrypt-session")]
mod cipher {
    /// 加密快照文件前缀：`DSENC1:` + 密文（二进制）。无此前缀视为旧版明文快照（向后兼容）。
    pub const MAGIC: &str = "DSENC1:";

    // ---------- Windows：DPAPI（当前用户上下文加密，无需额外密钥存储） ----------
    #[cfg(target_os = "windows")]
    pub fn encrypt(data: &[u8]) -> Result<Vec<u8>, String> {
        use windows::core::PWSTR;
        use windows::Win32::Security::Cryptography::{
            CryptProtectData, CRYPTPROTECT_UI_FORBIDDEN, DATA_BLOB,
        };
        use windows::Win32::System::Memory::LocalFree;

        let mut input = DATA_BLOB {
            cbData: data.len() as u32,
            pbData: data.as_ptr() as *mut u8,
        };
        let mut output = DATA_BLOB::default();
        unsafe {
            CryptProtectData(
                &input,
                PWSTR::null(),
                None,
                None,
                None,
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
            .map_err(|e| format!("DPAPI 加密失败: {e}"))?;
        }
        let bytes =
            unsafe { std::slice::from_raw_parts(output.pbData, output.cbData as usize) }.to_vec();
        unsafe {
            let _ = LocalFree(output.pbData as _);
        }
        Ok(bytes)
    }

    #[cfg(target_os = "windows")]
    pub fn decrypt(data: &[u8]) -> Result<Vec<u8>, String> {
        use windows::core::PWSTR;
        use windows::Win32::Security::Cryptography::{
            CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, DATA_BLOB,
        };
        use windows::Win32::System::Memory::LocalFree;

        let mut input = DATA_BLOB {
            cbData: data.len() as u32,
            pbData: data.as_ptr() as *mut u8,
        };
        let mut output = DATA_BLOB::default();
        unsafe {
            CryptUnprotectData(
                &input,
                PWSTR::null(),
                None,
                None,
                None,
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
            .map_err(|e| format!("DPAPI 解密失败: {e}"))?;
        }
        let bytes =
            unsafe { std::slice::from_raw_parts(output.pbData, output.cbData as usize) }.to_vec();
        unsafe {
            let _ = LocalFree(output.pbData as _);
        }
        Ok(bytes)
    }

    // ---------- macOS：Keychain 存 AES-256-GCM 密钥 ----------
    #[cfg(target_os = "macos")]
    const SERVICE: &str = "com.deepchat.desktop";
    #[cfg(target_os = "macos")]
    const ACCOUNT: &str = "session-key";

    #[cfg(target_os = "macos")]
    fn get_or_create_key() -> Result<[u8; 32], String> {
        use rand::RngCore;
        use security_framework::passwords::{
            delete_generic_password, get_generic_password, set_generic_password,
        };

        if let Ok(key) = get_generic_password(SERVICE, ACCOUNT) {
            if key.len() == 32 {
                let mut arr = [0u8; 32];
                arr.copy_from_slice(&key);
                return Ok(arr);
            }
            let _ = delete_generic_password(SERVICE, ACCOUNT); // 长度异常，重建
        }
        let mut key = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut key);
        set_generic_password(SERVICE, ACCOUNT, &key)
            .map_err(|e| format!("Keychain 写入失败: {e}"))?;
        Ok(key)
    }

    #[cfg(target_os = "macos")]
    pub fn encrypt(data: &[u8]) -> Result<Vec<u8>, String> {
        use aes_gcm::aead::{Aead, KeyInit};
        use aes_gcm::{Aes256Gcm, Key, Nonce};
        use rand::RngCore;

        let key = get_or_create_key()?;
        let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&key));
        let mut nonce = [0u8; 12];
        rand::rngs::OsRng.fill_bytes(&mut nonce);
        let ct = cipher
            .encrypt(Nonce::from_slice(&nonce), data)
            .map_err(|_| "AES-256-GCM 加密失败".to_string())?;
        let mut out = nonce.to_vec();
        out.extend_from_slice(&ct);
        Ok(out)
    }

    #[cfg(target_os = "macos")]
    pub fn decrypt(data: &[u8]) -> Result<Vec<u8>, String> {
        use aes_gcm::aead::{Aead, KeyInit};
        use aes_gcm::{Aes256Gcm, Key, Nonce};

        if data.len() < 12 {
            return Err("加密数据不完整".into());
        }
        let key = get_or_create_key()?;
        let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&key));
        let (nonce, ct) = data.split_at(12);
        cipher
            .decrypt(Nonce::from_slice(nonce), ct)
            .map_err(|_| "AES-256-GCM 解密失败（密钥可能已变更）".to_string())
    }

    // ---------- 其他平台：feature 开启但不支持，直接报错 ----------
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    pub fn encrypt(_data: &[u8]) -> Result<Vec<u8>, String> {
        Err("当前平台不支持会话加密（encrypt-session）".into())
    }

    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    pub fn decrypt(_data: &[u8]) -> Result<Vec<u8>, String> {
        Err("当前平台不支持会话解密（encrypt-session）".into())
    }
}

/// 读取快照文件并返回明文内容；不存在 / 空文件返回 None。
/// feature 开启时自动识别并解密 `DSENC1:` 前缀的密文；旧明文快照直接读取。
fn read_snapshot_plain(path: &std::path::Path) -> Result<Option<String>, String> {
    if !path.exists() {
        return Ok(None);
    }
    let bytes = std::fs::read(path).map_err(|e| format!("读取快照失败: {e}"))?;

    #[cfg(feature = "encrypt-session")]
    {
        if bytes.starts_with(cipher::MAGIC.as_bytes()) {
            let plain = cipher::decrypt(&bytes[cipher::MAGIC.len()..])?;
            let s = String::from_utf8(plain).map_err(|e| format!("快照解码失败: {e}"))?;
            return Ok(if s.trim().is_empty() { None } else { Some(s) });
        }
        // 无前缀 → 旧版明文快照，继续走下方通用读取
    }

    let s = String::from_utf8(bytes).map_err(|e| format!("快照编码非法: {e}"))?;
    Ok(if s.trim().is_empty() { None } else { Some(s) })
}

/// 判断文件中的旧快照是否为空（不存在 / 空内容 / 空会话都算空）。
/// 读取/解密失败时视为"非空"，阻止空快照覆盖（安全优先，避免误毁数据）。
fn existing_is_empty(path: &std::path::Path) -> bool {
    match read_snapshot_plain(path) {
        Ok(Some(s)) => serde_json::from_str::<serde_json::Value>(&s)
            .map(|v| is_empty_snapshot(&v))
            .unwrap_or(true),
        Ok(None) => true, // 不存在 / 空文件
        Err(_) => false,  // 读取或解密失败 → 视为非空，拒绝空快照覆盖
    }
}

/// 保存会话快照（JSON 字符串，原子写入：先写临时文件再改名）。
///
/// 防污染兜底（与前端 init.js 的双保险）：
/// - 新快照必须为合法 JSON 对象；
/// - 若新快照是"空会话"（无 cookie 且无 localStorage），且本地已存在非空旧快照，
///   则拒绝覆盖 —— 防止"加载中未登录中间态"毁掉最后一份好快照，导致重启后登录失效。
///   用户主动登出的清理由前端调用 clear_session 完成。
///
/// `encrypt-session` feature 开启时，落盘内容为 `DSENC1:` + 密文。
#[tauri::command]
pub fn save_session(app: AppHandle, webview: Webview, data: String) -> Result<(), String> {
    crate::ensure_deepseek_origin(&webview)?;

    // 快照体积上限：前端 init.js 生成上限 8MB（单值 512KB），此处接收上限 10MB（留余量）。
    const MAX_SIZE: usize = 10 * 1024 * 1024;
    if data.len() > MAX_SIZE {
        return Err("快照数据过大".into());
    }
    // 校验 JSON 结构
    let parsed: serde_json::Value =
        serde_json::from_str(&data).map_err(|e| format!("快照 JSON 无效: {e}"))?;
    if !parsed.is_object() {
        return Err("快照必须是 JSON 对象".into());
    }
    let path = session_path(&app)?;
    if is_empty_snapshot(&parsed) && path.exists() && !existing_is_empty(&path) {
        return Err("空快照拒绝覆盖已有会话（可能为未登录中间态）".into());
    }

    // 组装落盘字节：明文 或 加密（feature）
    let bytes: Vec<u8> = {
        #[cfg(feature = "encrypt-session")]
        {
            let mut v = Vec::with_capacity(cipher::MAGIC.len() + data.len() + 32);
            v.extend_from_slice(cipher::MAGIC.as_bytes());
            let enc = cipher::encrypt(data.as_bytes())?;
            v.extend_from_slice(&enc);
            v
        }
        #[cfg(not(feature = "encrypt-session"))]
        {
            data.into_bytes()
        }
    };

    let tmp = path.with_extension("json.tmp");
    // 清理历史崩溃可能残留的临时文件（P0-4）
    let _ = std::fs::remove_file(&tmp);
    std::fs::write(&tmp, &bytes).map_err(|e| format!("写入快照失败: {e}"))?;
    std::fs::rename(&tmp, &path).map_err(|e| format!("落盘快照失败: {e}"))?;
    Ok(())
}

/// 读取会话快照（返回明文 JSON 字符串给前端）；不存在时返回 None
#[tauri::command]
pub fn load_session(app: AppHandle, webview: Webview) -> Result<Option<String>, String> {
    crate::ensure_deepseek_origin(&webview)?;

    let path = session_path(&app)?;
    read_snapshot_plain(&path)
}

/// 清空会话快照（用于退出登录后的清理，避免残留脏数据）
#[tauri::command]
pub fn clear_session(app: AppHandle, webview: Webview) -> Result<(), String> {
    crate::ensure_deepseek_origin(&webview)?;

    let path = session_path(&app)?;
    if path.exists() {
        std::fs::remove_file(&path).map_err(|e| format!("清理快照失败: {e}"))?;
    }
    Ok(())
}

/// 调试日志（仅自检模式使用；写入 app_data_dir/debug.log）
#[tauri::command]
pub fn debug_log(app: AppHandle, webview: Webview, msg: String) -> Result<(), String> {
    crate::ensure_deepseek_origin(&webview)?;

    let path = session_path(&app)?;
    let log_path = path.with_file_name("debug.log");
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .map_err(|e| format!("打开日志失败: {e}"))?;
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    writeln!(f, "[{ts}] {msg}").map_err(|e| format!("写日志失败: {e}"))?;
    Ok(())
}

// =========================================================================
// HttpOnly Cookie 全量快照（feature: full-cookie-snapshot）
// =========================================================================
// 原生 WebView 存储中 HttpOnly 的登录 Cookie 无法通过 document.cookie 读取/恢复，
// 该 feature 通过平台原生接口（Windows WebView2 CookieManager / macOS WKHTTPCookieStore）
// 全量读写 Cookie，使登录态在原生存储丢失后也能从文件快照完整恢复。

/// 单条 Cookie 的全量描述（与 WebView2 / WKHTTPCookieStore 字段对齐）。
#[cfg(feature = "full-cookie-snapshot")]
#[derive(serde::Serialize, serde::Deserialize, Clone)]
pub struct CookieData {
    pub name: String,
    pub value: String,
    pub domain: String,
    pub path: String,
    pub http_only: bool,
    pub secure: bool,
    /// 是否为会话级 Cookie（不持久化）
    pub session: bool,
    /// 过期时间（Unix 秒）；会话 Cookie 为 0
    pub expires: f64,
}

// ---------- Windows：WebView2 CookieManager（webview2-com，与 tauri/wry 同版本） ----------
// 注意：COM 完成回调在 WebView2 UI 线程（Tauri 主线程消息循环）执行，
// 因此这里采用"with_webview 回调内注册、外层 channel 等待"模式，避免死锁。
#[cfg(all(feature = "full-cookie-snapshot", target_os = "windows"))]
mod win_cookie {
    use std::cell::RefCell;
    use std::sync::mpsc;
    use std::time::Duration;

    use tauri::Webview;
    use webview2_com::Microsoft::Web::WebView2::Win32::{
        ICoreWebView2AddOrUpdateCookieCompletedHandler, ICoreWebView2Cookie,
        ICoreWebView2CookieList, ICoreWebView2CookieManager,
        ICoreWebView2GetCookiesCompletedHandler,
    };
    use windows::core::{implement, Error, HSTRING, PCWSTR, PWSTR};
    use windows::Win32::Foundation::BOOL;

    use super::CookieData;

    fn pwstr(pw: PWSTR) -> String {
        if pw.is_null() {
            return String::new();
        }
        unsafe { pw.to_string().unwrap_or_default() }
    }

    fn cookie_to_data(cookie: &ICoreWebView2Cookie) -> Result<CookieData, String> {
        unsafe {
            Ok(CookieData {
                name: pwstr(cookie.Name().map_err(|e| format!("读 Cookie Name 失败: {e}"))?),
                value: pwstr(cookie.Value().map_err(|e| format!("读 Cookie Value 失败: {e}"))?),
                domain: pwstr(cookie.Domain().map_err(|e| format!("读 Cookie Domain 失败: {e}"))?),
                path: pwstr(cookie.Path().map_err(|e| format!("读 Cookie Path 失败: {e}"))?),
                http_only: cookie
                    .IsHttpOnly()
                    .map_err(|e| format!("读 Cookie HttpOnly 失败: {e}"))?
                    .as_bool(),
                secure: cookie
                    .IsSecure()
                    .map_err(|e| format!("读 Cookie Secure 失败: {e}"))?
                    .as_bool(),
                session: cookie
                    .IsSession()
                    .map_err(|e| format!("读 Cookie Session 失败: {e}"))?
                    .as_bool(),
                expires: cookie
                    .Expires()
                    .map_err(|e| format!("读 Cookie Expires 失败: {e}"))?,
            })
        }
    }

    // 完成回调：GetCookiesAsync(uri, handler) —— handler.Invoke(errorCode, cookieList)
    #[implement(ICoreWebView2GetCookiesCompletedHandler)]
    struct GetCookiesHandler(RefCell<Option<mpsc::Sender<Vec<CookieData>>>>);

    impl ICoreWebView2GetCookiesCompletedHandler_Impl for GetCookiesHandler {
        fn Invoke(
            &self,
            _error: Option<&Error>,
            result: Option<&ICoreWebView2CookieList>,
        ) -> windows::core::Result<()> {
            let mut cookies = Vec::new();
            if let Some(list) = result {
                let count = unsafe { list.Count() }?;
                for i in 0..count {
                    let cookie = unsafe { list.GetValueAtIndex(i) }?;
                    cookies.push(cookie_to_data(&cookie).unwrap_or_default());
                }
            }
            if let Some(tx) = self.0.borrow_mut().take() {
                let _ = tx.send(cookies);
            }
            Ok(())
        }
    }

    // 完成回调：AddOrUpdateCookie(cookie, handler) —— handler.Invoke(errorCode)
    #[implement(ICoreWebView2AddOrUpdateCookieCompletedHandler)]
    struct AddCookieHandler(RefCell<Option<mpsc::Sender<()>>>);

    impl ICoreWebView2AddOrUpdateCookieCompletedHandler_Impl for AddCookieHandler {
        fn Invoke(&self, _error: Option<&Error>) -> windows::core::Result<()> {
            if let Some(tx) = self.0.borrow_mut().take() {
                let _ = tx.send(());
            }
            Ok(())
        }
    }

    pub fn dump_all_cookies(webview: &Webview) -> Result<Vec<CookieData>, String> {
        let (tx, rx) = mpsc::channel();
        let tx2 = tx.clone();
        webview
            .with_webview(move |pw| {
                let manager = (|| -> Result<ICoreWebView2CookieManager, String> {
                    let core = unsafe { pw.controller().CoreWebView2() }
                        .map_err(|e| format!("获取 CoreWebView2 失败: {e}"))?;
                    unsafe { core.GetCookieManager() }
                        .map_err(|e| format!("获取 CookieManager 失败: {e}"))
                })();
                match manager {
                    Ok(manager) => {
                        let handler: ICoreWebView2GetCookiesCompletedHandler =
                            GetCookiesHandler(RefCell::new(Some(tx2))).into();
                        let uri = HSTRING::from("https://chat.deepseek.com");
                        if let Err(e) = unsafe {
                            manager.GetCookiesAsync(PCWSTR(uri.as_ptr()), &handler)
                        } {
                            let _ = tx.send(Err(format!("GetCookiesAsync 失败: {e}")));
                        }
                    }
                    Err(e) => {
                        let _ = tx.send(Err(e));
                    }
                }
            })
            .map_err(|e| format!("with_webview 失败: {e}"))?;
        match rx.recv_timeout(Duration::from_secs(10)) {
            Ok(Ok(cookies)) => Ok(cookies),
            Ok(Err(e)) => Err(e),
            Err(_) => Err("获取 Cookie 超时".to_string()),
        }
    }

    pub fn restore_all_cookies(webview: &Webview, cookies: Vec<CookieData>) -> Result<(), String> {
        let (tx, rx) = mpsc::channel();
        let total = cookies.len();
        let tx_count = tx.clone();
        webview
            .with_webview(move |pw| {
                let manager = (|| -> Result<ICoreWebView2CookieManager, String> {
                    let core = unsafe { pw.controller().CoreWebView2() }
                        .map_err(|e| format!("获取 CoreWebView2 失败: {e}"))?;
                    unsafe { core.GetCookieManager() }
                        .map_err(|e| format!("获取 CookieManager 失败: {e}"))
                })();
                let manager = match manager {
                    Ok(m) => m,
                    Err(e) => {
                        let _ = tx.send(Err(e));
                        return;
                    }
                };
                for c in cookies {
                    let name = HSTRING::from(c.name.as_str());
                    let value = HSTRING::from(c.value.as_str());
                    let domain = HSTRING::from(c.domain.as_str());
                    let path = HSTRING::from(c.path.as_str());
                    let cookie = match unsafe {
                        manager.CreateCookie(
                            PCWSTR(name.as_ptr()),
                            PCWSTR(value.as_ptr()),
                            PCWSTR(domain.as_ptr()),
                            PCWSTR(path.as_ptr()),
                        )
                    } {
                        Ok(c) => c,
                        Err(e) => {
                            let _ = tx.send(Err(format!("CreateCookie 失败 ({}): {e}", c.name)));
                            return;
                        }
                    };
                    if let Err(e) = unsafe {
                        cookie.put_IsHttpOnly(BOOL::from(c.http_only))
                            .and_then(|_| cookie.put_IsSecure(BOOL::from(c.secure)))
                            .and_then(|_| {
                                if !c.session && c.expires > 0.0 {
                                    cookie.put_Expires(c.expires)
                                } else {
                                    Ok(())
                                }
                            })
                    } {
                        let _ = tx.send(Err(format!("设置 Cookie 属性失败 ({}): {e}", c.name)));
                        return;
                    }
                    let txc = tx_count.clone();
                    let handler: ICoreWebView2AddOrUpdateCookieCompletedHandler =
                        AddCookieHandler(RefCell::new(Some(txc))).into();
                    if let Err(e) = unsafe { manager.AddOrUpdateCookie(&cookie, &handler) } {
                        let _ = tx.send(Err(format!("AddOrUpdateCookie 失败 ({}): {e}", c.name)));
                        return;
                    }
                }
                if total == 0 {
                    let _ = tx.send(Ok(()));
                }
            })
            .map_err(|e| format!("with_webview 失败: {e}"))?;
        // 等待全部 cookie 写入完成
        for _ in 0..total {
            match rx.recv_timeout(Duration::from_secs(5)) {
                Ok(Ok(())) => {}
                Ok(Err(e)) => return Err(e),
                Err(_) => return Err("写入 Cookie 超时".to_string()),
            }
        }
        Ok(())
    }
}

// ---------- macOS：WKHTTPCookieStore（objc2-web-kit 0.3 + objc2-foundation 0.3） ----------
// 注意：完成回调在 WebKit 主队列（Tauri 主线程 runloop）执行，
// 采用"with_webview 回调内注册、外层 channel 等待"模式，避免主线程死锁。
#[cfg(all(feature = "full-cookie-snapshot", target_os = "macos"))]
mod mac_cookie {
    use std::sync::mpsc;
    use std::time::Duration;

    use block2::RcBlock;
    use objc2::rc::Retained;
    use objc2::runtime::{AnyObject, ProtocolObject};
    use objc2_foundation::{
        NSArray, NSDate, NSDictionary, NSHTTPCookie, NSHTTPCookieDomain, NSHTTPCookieExpires,
        NSHTTPCookieName, NSHTTPCookiePath, NSHTTPCookiePropertyKey, NSHTTPCookieSecure,
        NSHTTPCookieValue, NSCopying, NSNumber, NSString,
    };
    use objc2_web_kit::{WKHTTPCookieStore, WKWebView};
    use tauri::Webview;

    use super::CookieData;

    fn nsstring(s: &str) -> Retained<NSString> {
        NSString::from_str(s)
    }

    fn cookie_to_data(cookie: &NSHTTPCookie) -> CookieData {
        let expires = cookie
            .expiresDate()
            .map(|d| d.timeIntervalSince1970())
            .unwrap_or(0.0);
        CookieData {
            name: cookie.name().to_string(),
            value: cookie.value().to_string(),
            domain: cookie.domain().to_string(),
            path: cookie.path().to_string(),
            http_only: cookie.isHTTPOnly(),
            secure: cookie.isSecure(),
            session: cookie.isSessionOnly(),
            expires,
        }
    }

    /// 从 with_webview 回调里拿 WKWebView，再取其 CookieStore。
    unsafe fn cookie_store_of(pw: &tauri::webview::PlatformWebview) -> Retained<WKHTTPCookieStore> {
        let wv: &WKWebView = unsafe { &*(pw.inner() as *const WKWebView) };
        wv.configuration().websiteDataStore().httpCookieStore()
    }

    pub fn dump_all_cookies(webview: &Webview) -> Result<Vec<CookieData>, String> {
        let (tx, rx) = mpsc::channel();
        let tx2 = tx.clone();
        webview
            .with_webview(move |pw| {
                let store = unsafe { cookie_store_of(&pw) };
                let block = RcBlock::new(move |cookies: std::ptr::NonNull<NSArray<NSHTTPCookie>>| {
                    let mut list = Vec::new();
                    let arr = unsafe { cookies.as_ref() };
                    for i in 0..arr.count() {
                        let c = arr.objectAtIndex(i);
                        list.push(cookie_to_data(&c));
                    }
                    let _ = tx2.send(list);
                });
                unsafe { store.getAllCookies(&block) };
            })
            .map_err(|e| format!("with_webview 失败: {e}"))?;
        rx.recv_timeout(Duration::from_secs(10))
            .map_err(|_| "获取 Cookie 超时".to_string())
    }

    pub fn restore_all_cookies(webview: &Webview, cookies: Vec<CookieData>) -> Result<(), String> {
        let (tx, rx) = mpsc::channel();
        let total = cookies.len();
        let tx_count = tx.clone();
        webview
            .with_webview(move |pw| {
                let store = unsafe { cookie_store_of(&pw) };

                // Foundation 的 NSHTTPCookie 属性键不含 HttpOnly/SessionOnly（WebKit 私有键），
                // 恢复时这两个标志会丢失（name/value/domain/path/expires/Secure 均保留），
                // 登录态可恢复；Windows 分支无此限制（CookieManager 支持全部属性）。
                // （keys 需在闭包内构造：ProtocolObject 非 Send，闭包须满足 Send 约束）
                let keys: Retained<NSArray<ProtocolObject<dyn NSCopying>>> = NSArray::from_slice(&[
                    unsafe { ProtocolObject::from_ref(NSHTTPCookieName) },
                    unsafe { ProtocolObject::from_ref(NSHTTPCookieValue) },
                    unsafe { ProtocolObject::from_ref(NSHTTPCookieDomain) },
                    unsafe { ProtocolObject::from_ref(NSHTTPCookiePath) },
                    unsafe { ProtocolObject::from_ref(NSHTTPCookieExpires) },
                    unsafe { ProtocolObject::from_ref(NSHTTPCookieSecure) },
                ]);

                for c in cookies {
                    let values: Retained<NSArray<AnyObject>> = NSArray::from_retained_slice(&[
                        nsstring(&c.name).into(),
                        nsstring(&c.value).into(),
                        nsstring(&c.domain).into(),
                        nsstring(&c.path).into(),
                        NSDate::dateWithTimeIntervalSince1970(c.expires.max(0.0)).into(),
                        NSNumber::numberWithBool(c.secure).into(),
                    ]);
                    let dict: Retained<NSDictionary<NSHTTPCookiePropertyKey, AnyObject>> =
                        unsafe { NSDictionary::dictionaryWithObjects_forKeys(&values, &keys) };
                    let cookie = match unsafe { NSHTTPCookie::cookieWithProperties(&dict) } {
                        Some(c) => c,
                        None => {
                            let _ = tx.send(Err(format!("创建 Cookie 失败: {}", c.name)));
                            return;
                        }
                    };
                    let txc = tx_count.clone();
                    let block = RcBlock::new(move || {
                        let _ = txc.send(Ok(()));
                    });
                    unsafe { store.setCookie_completionHandler(&cookie, Some(&block)) };
                }
                if total == 0 {
                    let _ = tx.send(Ok(()));
                }
            })
            .map_err(|e| format!("with_webview 失败: {e}"))?;
        // 等待全部 cookie 写入完成
        for _ in 0..total {
            match rx.recv_timeout(Duration::from_secs(5)) {
                Ok(Ok(())) => {}
                Ok(Err(e)) => return Err(e),
                Err(_) => return Err("写入 Cookie 超时".to_string()),
            }
        }
        Ok(())
    }
}

// ---------- 其他平台：feature 开启但不支持，返回错误（前端回退 document.cookie） ----------
#[cfg(all(feature = "full-cookie-snapshot", not(any(target_os = "windows", target_os = "macos"))))]
mod other_cookie {
    use super::CookieData;
    use tauri::Webview;

    pub fn dump_all_cookies(_w: &Webview) -> Result<Vec<CookieData>, String> {
        Err("当前平台不支持 HttpOnly Cookie 全量快照（full-cookie-snapshot）".into())
    }

    pub fn restore_all_cookies(_w: &Webview, _c: Vec<CookieData>) -> Result<(), String> {
        Err("当前平台不支持 HttpOnly Cookie 全量快照（full-cookie-snapshot）".into())
    }
}

/// 导出全部 Cookie（含 HttpOnly）—— 供 init.js 在 feature 启用时优先调用
#[cfg(feature = "full-cookie-snapshot")]
#[tauri::command]
pub fn dump_all_cookies(webview: Webview) -> Result<Vec<CookieData>, String> {
    crate::ensure_deepseek_origin(&webview)?;

    #[cfg(target_os = "windows")]
    {
        win_cookie::dump_all_cookies(&webview)
    }
    #[cfg(target_os = "macos")]
    {
        mac_cookie::dump_all_cookies(&webview)
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        other_cookie::dump_all_cookies(&webview)
    }
}

/// 全量恢复 Cookie（含 HttpOnly）—— 供 init.js 在 feature 启用时优先调用
#[cfg(feature = "full-cookie-snapshot")]
#[tauri::command]
pub fn restore_all_cookies(webview: Webview, cookies: Vec<CookieData>) -> Result<(), String> {
    crate::ensure_deepseek_origin(&webview)?;

    #[cfg(target_os = "windows")]
    {
        win_cookie::restore_all_cookies(&webview, cookies)
    }
    #[cfg(target_os = "macos")]
    {
        mac_cookie::restore_all_cookies(&webview, cookies)
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        other_cookie::restore_all_cookies(&webview, cookies)
    }
}
