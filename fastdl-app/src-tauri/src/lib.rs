//! FastDL — macOS GUI 下载器（Tauri 2 壳 + fastdl 引擎）

pub mod manager;

use manager::Manager;

/// 应用构建器（run 专用；测试用 mock_builder 自建，不挂 Wry 插件）
pub fn app_builder() -> tauri::Builder<tauri::Wry> {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .manage(Manager::new())
        .invoke_handler(tauri::generate_handler![
            manager::start_download,
            manager::pause_task,
            manager::resume_task,
            manager::remove_task,
            manager::list_tasks,
            manager::pick_dir,
            manager::reveal,
            manager::default_dir_cmd
        ])
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    app_builder()
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
