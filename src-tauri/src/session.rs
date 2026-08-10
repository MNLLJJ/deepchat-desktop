//! 会话持久化：把 Cookie 与 localStorage 快照读写到本地文件（app_data_dir/session.json）。
//!
//! 持久化采用双层策略：
//! 1. 原生 WebView 存储（macOS WKWebView defaultDataStore / Windows WebView2 profile），
//!    自动持久化全部 Cookie（含 HttpOnly）与 localStorage —— 这是登录态保持的主路径；
//! 2. 本模块提供的文件快照 —— 双保险，覆盖原生存储被清除 / 换构建 / 系统清理等场景，
//!    由注入页面的 init.js 定期同步并在启动时恢复。

use std::path::PathBuf;
use tauri::{AppHandle, Manager};

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

/// 保存会话快照（JSON 字符串，原子写入：先写临时文件再改名）
#[tauri::command]
pub fn save_session(app: AppHandle, data: String) -> Result<(), String> {
    const MAX_SIZE: usize = 10 * 1024 * 1024; // 10MB 上限
    if data.len() > MAX_SIZE {
        return Err("快照数据过大".into());
    }
    let path = session_path(&app)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, data).map_err(|e| format!("写入快照失败: {e}"))?;
    std::fs::rename(&tmp, &path).map_err(|e| format!("落盘快照失败: {e}"))?;
    Ok(())
}

/// 读取会话快照；不存在时返回 None
#[tauri::command]
pub fn load_session(app: AppHandle) -> Result<Option<String>, String> {
    let path = session_path(&app)?;
    if !path.exists() {
        return Ok(None);
    }
    let content = std::fs::read_to_string(&path).map_err(|e| format!("读取快照失败: {e}"))?;
    if content.trim().is_empty() {
        return Ok(None);
    }
    Ok(Some(content))
}

/// 清空会话快照（用于退出登录后的清理，避免残留脏数据）
#[tauri::command]
pub fn clear_session(app: AppHandle) -> Result<(), String> {
    let path = session_path(&app)?;
    if path.exists() {
        std::fs::remove_file(&path).map_err(|e| format!("清理快照失败: {e}"))?;
    }
    Ok(())
}

/// 调试日志（仅自检模式使用；写入 app_data_dir/debug.log）
#[tauri::command]
pub fn debug_log(app: AppHandle, msg: String) -> Result<(), String> {
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
