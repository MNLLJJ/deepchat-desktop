fn main() {
    // 在构建期告知 tauri 我们应用自己的 #[tauri::command] 列表，
    // 让 tauri-build 自动生成 app ACL manifest（__app-acl__），
    // 使远程域（如 chat.deepseek.com）能够在 capability 中授权调用这些命令。
    // dump_all_cookies / restore_all_cookies 始终注册（不随 feature 条件化）：
    // 这样 ACL 权限（allow-dump-all-cookies 等）在所有构建中稳定存在，capability 可无条件引用；
    // feature 未启用时命令未注册，远程调用会得到 "command not found" → 前端自动回退 document.cookie，
    // 行为与"命令不存在"完全一致（回退路径已按此设计）。
    let manifest = tauri_build::AppManifest::new().commands(&[
        "save_session",
        "load_session",
        "clear_session",
        "debug_log",
        "open_external",
        "dump_all_cookies",
        "restore_all_cookies",
    ]);

    tauri_build::try_build(tauri_build::Attributes::new().app_manifest(manifest))
        .expect("failed to run tauri-build");
}
