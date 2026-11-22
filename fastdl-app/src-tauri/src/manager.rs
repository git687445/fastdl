//! 任务状态机：管理下载任务的生命周期（运行/暂停/继续/完成/失败）
//!
//! 设计（沿用 TDM Fast 的思路）：
//! - 每个任务一个 OS 线程跑 fastdl 引擎（阻塞式），进度通过 Atomic 变量共享
//! - 暂停 = 置位 cancel flag，引擎 worker 在下一个块边界（≤256KB）保存 state 后退出
//! - 继续 = 重置 flag + 重新 spawn（引擎读 .fastdl.state.json 断点续传）
//! - 状态变化通过 Tauri 事件 `task-progress` / `task-state` 推给前端

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use fastdl::{probe, Client, DownloadOptions, Error as EngineError, State};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, State as TauriState};

// ---------- 序列化给前端 ----------

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Debug)]
#[serde(rename_all = "lowercase")]
pub enum TaskState {
    Queued,
    Running,
    Paused,
    Done,
    Failed,
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct TaskView {
    pub id: String,
    pub url: String,
    pub out: String,
    pub state: TaskState,
    pub done: u64,
    pub total: u64, // 0 = 尚未探测到
    pub bps: u64,
    pub segments: usize,
    pub threads: usize,
    pub error: String,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ProgressEvent {
    pub task_id: String,
    pub done: u64,
    pub total: u64,
    pub bps: u64,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct StateEvent {
    pub task_id: String,
    pub state: TaskState,
    pub error: String,
    pub out: String,
}

// ---------- 运行时结构 ----------

pub struct Task {
    pub id: String,
    pub url: String,
    pub out: String,
    pub segments: usize,
    pub threads: usize,
    pub state: Mutex<TaskState>,
    pub error: Mutex<String>,
    pub done: AtomicU64,
    pub total: AtomicU64,
    pub bps: AtomicU64,
    pub cancel: Arc<AtomicBool>,
    pub handle: Mutex<Option<std::thread::JoinHandle<()>>>,
}

pub struct Manager {
    tasks: Mutex<HashMap<String, Arc<Task>>>,
    next_id: AtomicU32,
}

impl Manager {
    pub fn new() -> Self {
        Self {
            tasks: Mutex::new(HashMap::new()),
            next_id: AtomicU32::new(1),
        }
    }

    fn view(&self, t: &Task) -> TaskView {
        TaskView {
            id: t.id.clone(),
            url: t.url.clone(),
            out: t.out.clone(),
            state: *t.state.lock().unwrap(),
            done: t.done.load(Ordering::Relaxed),
            total: t.total.load(Ordering::Relaxed),
            bps: t.bps.load(Ordering::Relaxed),
            segments: t.segments,
            threads: t.threads,
            error: t.error.lock().unwrap().clone(),
        }
    }
}

impl Default for Manager {
    fn default() -> Self {
        Self::new()
    }
}

// ---------- 工具 ----------

fn default_download_dir() -> String {
    std::env::home_dir()
        .map(|h| h.join("Downloads").to_string_lossy().into_owned())
        .unwrap_or_else(|| ".".into())
}

fn filename_from_url(url: &str) -> String {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    let last = path.rsplit('/').find(|s| !s.is_empty()).unwrap_or("download");
    if last.is_empty() {
        "download".to_string()
    } else {
        last.to_string()
    }
}

// ---------- 任务线程（start / resume 共用） ----------

fn run_task<R: tauri::Runtime>(app: tauri::AppHandle<R>, task: Arc<Task>, resume: bool) {
    let client = Client::new();
    let url = task.url.clone();

    *task.state.lock().unwrap() = TaskState::Running;
    let _ = app.emit(
        "task-state",
        StateEvent {
            task_id: task.id.clone(),
            state: TaskState::Running,
            error: String::new(),
            out: task.out.clone(),
        },
    );

    // 探测服务器能力
    let probe_result = match probe::probe(&client, &url) {
        Ok(p) => p,
        Err(e) => {
            *task.state.lock().unwrap() = TaskState::Failed;
            *task.error.lock().unwrap() = format!("探测失败: {e}");
            let _ = app.emit(
                "task-state",
                StateEvent {
                    task_id: task.id.clone(),
                    state: TaskState::Failed,
                    error: task.error.lock().unwrap().clone(),
                    out: task.out.clone(),
                },
            );
            return;
        }
    };

    // 已有断点 state 则自动续传（resume 参数在"继续"按钮路径显式要求）
    let want_resume = resume || State::path(std::path::Path::new(&task.out)).exists();
    let opts = DownloadOptions {
        url,
        out: task.out.clone(),
        segments: task.segments,
        threads: task.threads,
        resume: want_resume,
        state_flush_every: 1024 * 1024,
        cancel: Some(task.cancel.clone()),
    };

    let task_m = Arc::clone(&task);
    let app_m = app.clone();
    let progress_cb: Option<Box<dyn FnMut(u64, u64, f64)>> = Some(Box::new(
        move |done, total, bps| {
            task_m.done.store(done, Ordering::Relaxed);
            task_m.total.store(total, Ordering::Relaxed);
            task_m.bps.store(bps as u64, Ordering::Relaxed);
            let _ = app_m.emit(
                "task-progress",
                ProgressEvent {
                    task_id: task_m.id.clone(),
                    done,
                    total,
                    bps: bps as u64,
                },
            );
        },
    ));

    match fastdl::download(&client, &probe_result, &opts, progress_cb) {
        Ok(rep) => {
            let st = if rep.cancelled {
                TaskState::Paused
            } else {
                TaskState::Done
            };
            *task.state.lock().unwrap() = st;
            *task.error.lock().unwrap() = String::new();
            // 取消路径不写 done=total（否则 UI 显示 100% 但实际没下完）
            if !rep.cancelled {
                task.done.store(rep.total, Ordering::Relaxed);
                task.total.store(rep.total, Ordering::Relaxed);
            }
        }
        Err(e) => {
            let cancelled = matches!(e, EngineError::Cancelled);
            *task.state.lock().unwrap() = if cancelled {
                TaskState::Paused
            } else {
                TaskState::Failed
            };
            if !cancelled {
                *task.error.lock().unwrap() = e.to_string();
            }
        }
    }

    let st = *task.state.lock().unwrap();
    let _ = app.emit(
        "task-state",
        StateEvent {
            task_id: task.id.clone(),
            state: st,
            error: task.error.lock().unwrap().clone(),
            out: task.out.clone(),
        },
    );
}

// ---------- Tauri commands ----------

#[derive(serde::Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct StartArgs {
    pub url: String,
    #[serde(default)]
    pub dir: Option<String>,
    #[serde(default)]
    pub filename: Option<String>,
    #[serde(default = "def_segs")]
    pub segments: usize,
    #[serde(default = "def_threads")]
    pub threads: usize,
}

fn def_segs() -> usize {
    32
}
fn def_threads() -> usize {
    8
}

/// 新建任务并启动下载。返回任务 id。
#[tauri::command]
pub fn start_download<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    args: StartArgs,
    manager: TauriState<'_, Manager>,
) -> Result<String, String> {
    let dir = args
        .dir
        .filter(|d| !d.is_empty())
        .unwrap_or_else(default_download_dir);
    let filename = args
        .filename
        .filter(|f| !f.is_empty())
        .unwrap_or_else(|| filename_from_url(&args.url));
    let out = format!("{}/{}", dir.trim_end_matches('/'), filename);

    let id = format!("{}", manager.next_id.fetch_add(1, Ordering::Relaxed));
    let task = Arc::new(Task {
        id: id.clone(),
        url: args.url.clone(),
        out,
        segments: args.segments,
        threads: args.threads,
        state: Mutex::new(TaskState::Queued),
        error: Mutex::new(String::new()),
        done: AtomicU64::new(0),
        total: AtomicU64::new(0),
        bps: AtomicU64::new(0),
        cancel: Arc::new(AtomicBool::new(false)),
        handle: Mutex::new(None),
    });
    manager.tasks.lock().unwrap().insert(id.clone(), Arc::clone(&task));

    let task_c = Arc::clone(&task);
    let app_c = app.clone();
    let h = std::thread::spawn(move || run_task(app_c, task_c, false));
    *task.handle.lock().unwrap() = Some(h);

    Ok(id)
}

/// 暂停任务：置位 cancel，引擎在下一个块边界保存 state 并退出线程
#[tauri::command]
pub fn pause_task(id: String, manager: TauriState<'_, Manager>) -> Result<(), String> {
    let t = manager.tasks.lock().unwrap().get(&id).cloned().ok_or("task not found")?;
    let st = *t.state.lock().unwrap();
    if st == TaskState::Running || st == TaskState::Queued {
        t.cancel.store(true, Ordering::Relaxed);
    }
    Ok(())
}

/// 继续/重试任务：重置 cancel + 重新 spawn（引擎读断点 state 续传）
#[tauri::command]
pub fn resume_task<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    id: String,
    manager: TauriState<'_, Manager>,
) -> Result<(), String> {
    let t = manager.tasks.lock().unwrap().get(&id).cloned().ok_or("task not found")?;
    let st = *t.state.lock().unwrap();
    if st != TaskState::Paused && st != TaskState::Failed {
        return Err(format!("任务当前状态 {st:?}，无法继续"));
    }
    t.cancel.store(false, Ordering::Relaxed);
    *t.error.lock().unwrap() = String::new();

    let t_c = Arc::clone(&t);
    let app_c = app.clone();
    let h = std::thread::spawn(move || run_task(app_c, t_c, true));
    *t.handle.lock().unwrap() = Some(h);
    Ok(())
}

/// 移除任务（运行中会先暂停；断点 state 保留在磁盘）
#[tauri::command]
pub fn remove_task(id: String, manager: TauriState<'_, Manager>) -> Result<(), String> {
    let t = manager.tasks.lock().unwrap().get(&id).cloned().ok_or("task not found")?;
    let st = *t.state.lock().unwrap();
    if st == TaskState::Running || st == TaskState::Queued {
        t.cancel.store(true, Ordering::Relaxed);
        if let Some(h) = t.handle.lock().unwrap().take() {
            h.join().ok();
        }
    }
    manager.tasks.lock().unwrap().remove(&id);
    Ok(())
}

/// 任务列表
#[tauri::command]
pub fn list_tasks(manager: TauriState<'_, Manager>) -> Vec<TaskView> {
    let tasks = manager.tasks.lock().unwrap();
    tasks.values().map(|t| manager.view(t)).collect()
}

/// 选目录（原生对话框）
#[tauri::command]
pub async fn pick_dir(app: AppHandle) -> Result<Option<String>, String> {
    use tauri_plugin_dialog::DialogExt;
    let dir = app.dialog().file().blocking_pick_folder();
    Ok(dir.map(|p| p.to_string()))
}

/// 在 Finder 中定位输出文件（macOS `open -R`）
#[tauri::command]
pub fn reveal(out: String) -> Result<(), String> {
    std::process::Command::new("open")
        .arg("-R")
        .arg(&out)
        .spawn()
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// 默认下载目录
#[tauri::command]
pub fn default_dir_cmd() -> String {
    default_download_dir()
}
