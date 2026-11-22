//! GUI 命令管线端到端测试（无头）：
//! mock_app 跑真实的 Tauri 命令（start_download / pause_task / resume_task / list_tasks / remove_task），
//! 底层是 fastdl 引擎 + 内嵌的全局限速 Range 服务器 —— 验证 GUI 下载器逻辑全链路，
//! 不依赖屏幕自动化/TCC 权限。

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use fastdl_app_lib::manager::{TaskState, TaskView};
use tauri::ipc::CallbackFn;
use tauri::test::{get_ipc_response, mock_builder, mock_context, noop_assets, INVOKE_KEY};
use tauri::webview::InvokeRequest;
use tauri::WebviewWindowBuilder;

const FILE_LEN: u64 = 32 * 1024 * 1024; // 32MB
const RATE: u64 = 8 * 1024 * 1024; // 全局 8MB/s → 传输约 4s（可观测暂停）
const CH: u64 = 128 * 1024;

/// 确定性内容：第 i 字节的值
fn pat(i: u64) -> u8 {
    ((i as u128).wrapping_mul(0x9E3779B97F4A7C15 as u128) >> 56) as u8
}

/// 全局令牌桶（跨连接限速）
struct Bucket {
    lock: Mutex<(f64, Instant)>, // (tokens, last_refill_instant)
    rate: f64,
    cap: f64,
}

impl Bucket {
    fn new(rate: f64, cap: f64) -> Self {
        Self {
            lock: Mutex::new((0.0, Instant::now())),
            rate,
            cap,
        }
    }

    fn take(&self, n: u64) {
        loop {
            let mut g = self.lock.lock().unwrap();
            let now = Instant::now();
            let dt = now.duration_since(g.1).as_secs_f64();
            g.0 = (g.0 + dt * self.rate).min(self.cap);
            g.1 = now;
            if g.0 >= n as f64 {
                g.0 -= n as f64;
                return;
            }
            let need = n as f64 - g.0;
            drop(g);
            std::thread::sleep(Duration::from_millis((need / self.rate * 1000.0).ceil() as u64 + 10));
        }
    }
}

/// 起一个支持 Range + 全局限速的测试服务器。返回 (port, handle)。
fn start_server() -> (u16, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let bucket = Arc::new(Bucket::new(RATE as f64, RATE as f64));
    let h = std::thread::spawn(move || {
        for stream in listener.incoming() {
            let bucket = Arc::clone(&bucket);
            let stream = match stream {
                Ok(s) => s,
                Err(_) => continue,
            };
            std::thread::spawn(move || {
                let mut stream = stream;
                stream
                    .set_read_timeout(Some(Duration::from_secs(30)))
                    .ok();
                // 读请求（到空行）
                let mut req = Vec::new();
                let mut tmp = [0u8; 2048];
                while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                    match stream.read(&mut tmp) {
                        Ok(0) => return,
                        Ok(n) => {
                            req.extend_from_slice(&tmp[..n]);
                        }
                        Err(_) => return,
                    }
                }
                let text = String::from_utf8_lossy(&req).to_string();
                let range = text
                    .lines()
                    .find_map(|l| l.strip_prefix("Range:").or_else(|| l.strip_prefix("range:")))
                    .map(|v| v.trim().to_string());

                let (start, end, is_range) = match range {
                    Some(r) => {
                        let spec = r.strip_prefix("bytes=").unwrap_or(&r);
                        let (a, b) = spec.split_once('-').unwrap_or(("", ""));
                        let a: u64 = a.parse().unwrap_or(0);
                        let b: u64 = if b.is_empty() { FILE_LEN - 1 } else { b.parse().unwrap_or(FILE_LEN - 1) };
                        (a.min(FILE_LEN - 1), b.min(FILE_LEN - 1), true)
                    }
                    None => (0, FILE_LEN - 1, false),
                };
                let len = end - start + 1;

                let status = if is_range { 206 } else { 200 };
                let mut resp = format!(
                    "HTTP/1.1 {status} {}\r\n",
                    if is_range { "Partial Content" } else { "OK" }
                );
                if is_range {
                    resp.push_str(&format!("Content-Range: bytes {start}-{end}/{FILE_LEN}\r\n"));
                }
                resp.push_str(&format!(
                    "Content-Length: {len}\r\nAccept-Ranges: bytes\r\nETag: \"test32mb\"\r\nConnection: close\r\n\r\n"
                ));
                if stream.write_all(resp.as_bytes()).is_err() {
                    return;
                }
                let mut sent = 0u64;
                let mut buf = vec![0u8; CH.min(len) as usize];
                while sent < len {
                    let want = (CH as usize).min((len - sent) as usize);
                    for (j, b) in buf.iter_mut().take(want).enumerate() {
                        *b = pat(start + sent + j as u64);
                    }
                    bucket.take(want as u64);
                    if stream.write_all(&buf[..want]).is_err() {
                        return;
                    }
                    sent += want as u64;
                }
            });
        }
    });
    (port, h)
}

fn build_app() -> tauri::App<tauri::test::MockRuntime> {
    // 注意：dialog/opener 插件绑定 Wry 运行时，MockRuntime 下不能挂载；
    // 被测命令（start/pause/resume/list/remove）不依赖插件
    mock_builder()
        .manage(fastdl_app_lib::manager::Manager::new())
        .invoke_handler(tauri::generate_handler![
            fastdl_app_lib::manager::start_download,
            fastdl_app_lib::manager::pause_task,
            fastdl_app_lib::manager::resume_task,
            fastdl_app_lib::manager::remove_task,
            fastdl_app_lib::manager::list_tasks
        ])
        .build(mock_context(noop_assets()))
        .unwrap()
}

fn invoke(_app: &tauri::App<tauri::test::MockRuntime>, webview: &tauri::WebviewWindow<tauri::test::MockRuntime>, cmd: &str, body: serde_json::Value) -> serde_json::Value {
    let req = InvokeRequest {
        cmd: cmd.into(),
        callback: CallbackFn(0),
        error: CallbackFn(1),
        url: "tauri://localhost".parse().unwrap(),
        body: body.into(),
        headers: Default::default(),
        invoke_key: INVOKE_KEY.to_string(),
    };
    let resp = get_ipc_response(webview, req).expect("invoke failed");
    resp.deserialize::<serde_json::Value>().expect("bad response json")
}

fn task_view(app: &tauri::App<tauri::test::MockRuntime>, webview: &tauri::WebviewWindow<tauri::test::MockRuntime>, id: &str) -> TaskView {
    let list: Vec<TaskView> = invoke(app, webview, "list_tasks", serde_json::json!({}))
        .as_array()
        .cloned()
        .map(|a| serde_json::from_value(serde_json::Value::Array(a)).unwrap())
        .expect("list_tasks not array");
    list.into_iter().find(|t| t.id == id).expect("task not in list")
}

fn wait_state(
    app: &tauri::App<tauri::test::MockRuntime>,
    webview: &tauri::WebviewWindow<tauri::test::MockRuntime>,
    id: &str,
    want: TaskState,
    timeout: Duration,
) -> TaskView {
    let t0 = Instant::now();
    loop {
        let v = task_view(app, webview, id);
        if v.state == want {
            return v;
        }
        assert!(t0.elapsed() < timeout, "task {id} 未在 {:?} 内到达 {:?}（当前 {:?}，err={}", timeout, want, v.state, v.error);
        std::thread::sleep(Duration::from_millis(150));
    }
}

#[test]
fn gui_download_pipeline_download_pause_resume_verify() {
    let (port, _srv) = start_server();
    let url = format!("http://127.0.0.1:{port}/big.bin");
    let tmp = std::env::temp_dir().join(format!("fastdl_gui_test_{}", std::process::id()));
    std::fs::create_dir_all(&tmp).unwrap();
    let dir = tmp.to_string_lossy().to_string();
    let out_path = format!("{}/big.bin", dir);
    let state_path = format!("{out_path}.fastdl.state.json");
    let _ = std::fs::remove_file(&out_path);
    let _ = std::fs::remove_file(&state_path);

    let app = build_app();
    let webview = WebviewWindowBuilder::new(&app, "main", Default::default())
        .build()
        .unwrap();

    // 1) 启动下载（32 段 × 8 线程，引擎自动走 Range 并行）
    let id: String = invoke(
        &app,
        &webview,
        "start_download",
        serde_json::json!({
            "args": {
                "url": url,
                "dir": dir,
                "filename": "big.bin",
                "segments": 32,
                "threads": 8
            }
        }),
    )
    .as_str()
    .expect("id should be string")
    .to_string();

    // 2) 跑 ~1.5s 后暂停（总传输约 4s → 此时约 30-40%）
    std::thread::sleep(Duration::from_millis(1500));
    invoke(&app, &webview, "pause_task", serde_json::json!({ "id": id }));
    let v = wait_state(&app, &webview, &id, TaskState::Paused, Duration::from_secs(15));
    assert!(v.done > 0, "暂停时应已下载一部分: done=0");
    assert!(v.done < FILE_LEN, "暂停时应未完成: done={}", v.done);
    assert!(v.total == FILE_LEN, "total 应已探测到");
    assert!(std::path::Path::new(&state_path).exists(), "暂停后断点 state 文件应保留");

    // 3) 继续（引擎读断点 state 续传）
    invoke(&app, &webview, "resume_task", serde_json::json!({ "id": id }));
    let v = wait_state(&app, &webview, &id, TaskState::Done, Duration::from_secs(30));
    assert!(v.done == FILE_LEN && v.total == FILE_LEN);

    // 4) 字节级校验
    let data = std::fs::read(&out_path).expect("输出文件应存在");
    assert_eq!(data.len(), FILE_LEN as usize);
    let mut bad = 0usize;
    for (i, &b) in data.iter().enumerate() {
        if b != pat(i as u64) {
            bad += 1;
        }
        if bad > 3 {
            break;
        }
    }
    assert_eq!(bad, 0, "字节内容应与源不一致");
    assert!(!std::path::Path::new(&state_path).exists(), "完成后 state 应已清理");

    // 5) 移除任务
    invoke(&app, &webview, "remove_task", serde_json::json!({ "id": id }));
    let list: Vec<TaskView> = invoke(&app, &webview, "list_tasks", serde_json::json!({}))
        .as_array()
        .cloned()
        .map(|a| serde_json::from_value(serde_json::Value::Array(a)).unwrap())
        .unwrap();
    assert!(list.iter().all(|t| t.id != id), "移除后任务不应在列表中");

    let _ = std::fs::remove_dir_all(&tmp);
}
