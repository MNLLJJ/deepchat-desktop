fn main() {
    // 在构建期告知 tauri 我们应用自己的 #[tauri::command] 列表，
    // 让 tauri-build 自动生成 app ACL manifest（__app-acl__），
    // 使远程域（如 chat.deepseek.com）能够在 capability 中授权调用这些命令。
    // feature `full-cookie-snapshot` 启用的命令（build.rs 通过 CARGO_FEATURE_* 环境变量感知 feature）。
    let manifest = if std::env::var("CARGO_FEATURE_FULL_COOKIE_SNAPSHOT").is_ok() {
        tauri_build::AppManifest::new().commands(&[
            "save_session",
            "load_session",
            "clear_session",
            "debug_log",
            "open_external",
            "dump_all_cookies",
            "restore_all_cookies",
        ])
    } else {
        tauri_build::AppManifest::new().commands(&[
            "save_session",
            "load_session",
            "clear_session",
            "debug_log",
            "open_external",
        ])
    };

    tauri_build::try_build(tauri_build::Attributes::new().app_manifest(manifest))
        .expect("failed to run tauri-build");
}
