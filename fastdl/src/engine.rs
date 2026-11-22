//! 并行下载引擎：预分配 + N 线程 work-stealing 拉取 + 按偏移落盘 + 断点续传

use std::collections::VecDeque;
use std::io::{Read, Seek, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::probe::ProbeResult;
use crate::seg::{plan, Segment, State};
use crate::Error;
use reqwest::blocking::Client;

#[derive(Debug, Clone)]
pub struct DownloadOptions {
    pub url: String,
    pub out: String,
    /// 分段数（上限）；服务器不支持 Range 时自动退化为单流
    pub segments: usize,
    /// 工作线程数（实际参与并行拉取的线程）
    pub threads: usize,
    /// 支持断点续传（读取 <out>.fastdl.state.json）
    pub resume: bool,
    /// 每次落盘进度更新的状态写入节流（字节）
    pub state_flush_every: u64,
    /// 取消标志（暂停/停止）：置位后 worker 停止拉取，state 保留供续传
    pub cancel: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
}

#[derive(Debug, Clone)]
pub struct DownloadReport {
    pub total: u64,
    pub used_segments: usize,
    pub used_threads: usize,
    pub range_supported: bool,
    pub resumed_from: u64,
    pub output: String,
    /// 是否因取消而中途停止（state 已保留，可 -r 续传）
    pub cancelled: bool,
}

pub fn download(
    client: &Client,
    probe: &ProbeResult,
    opts: &DownloadOptions,
    mut progress: Option<Box<dyn FnMut(u64, u64, f64)>>,
) -> Result<DownloadReport, Error> {
    // 总大小未知（服务器既不报 Content-Range 也不报 Content-Length）→
    // 无法规划分段，退化为单流下载（进度 total=0 = UI 不确定态）
    let total = match probe.total_size {
        Some(t) => Some(t),
        None => {
            return download_unknown_total(client, probe, opts, progress);
        }
    };
    let total = total.unwrap();

    // 输出路径
    let out_path = Path::new(&opts.out);
    if let Some(parent) = out_path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).ok();
        }
    }

    // 分段规划
    let n = if probe.range_supported {
        plan(total, opts.segments).len()
    } else {
        1
    };
    let segs = plan(total, n);

    // 断点续传
    let state = if opts.resume {
        State::load(out_path, &opts.url, probe.etag.as_deref()).filter(|s| {
            s.total == total && s.segments.len() == segs.len()
        })
    } else {
        None
    };
    let state = state.unwrap_or_else(|| {
        State::new(
            &opts.url,
            &opts.out,
            total,
            probe.etag.as_deref(),
            segs.clone(),
        )
    });
    let resumed_from: u64 = state.done.iter().sum();

    // 预分配（sparse，不耗磁盘写带宽）
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(state.finished())
        .open(out_path)?;
    if !state.finished() || state.total != total {
        file.set_len(total)?;
    }
    drop(file);

    // 工作队列：跳过已完成段
    let queue: Mutex<VecDeque<usize>> = Mutex::new(
        (0..segs.len())
            .filter(|&i| state.done[i] < segs[i].len())
            .collect(),
    );

    let done_bytes = Arc::new(AtomicU64::new(resumed_from));
    let state_mtx = Arc::new(Mutex::new(state));
    let queue = Arc::new(queue);

    if queue.lock().unwrap().is_empty() {
        // 已全部完成
        State::clear(out_path);
        return Ok(DownloadReport {
            total,
            used_segments: segs.len(),
            used_threads: 1,
            range_supported: probe.range_supported,
            resumed_from,
            output: opts.out.clone(),
            cancelled: false,
        });
    }

    let t0 = std::time::Instant::now();
    let client = client.clone();
    let url = opts.url.clone();
    let flush_every = opts.state_flush_every.max(64 * 1024);
    let threads = opts.threads.max(1).min(queue.lock().unwrap().len());

    let mut workers = Vec::new();
    for _ in 0..threads {
        let client = client.clone();
        let url = url.clone();
        let queue = Arc::clone(&queue);
        let state_mtx = Arc::clone(&state_mtx);
        let done_bytes = Arc::clone(&done_bytes);
        let out = opts.out.clone();
        let flush_every = flush_every;
        let cancel = opts.cancel.clone();
        workers.push(std::thread::spawn(move || {
            loop {
                // 取消检查（暂停）：停止拉新段，state 保留
                if cancel.as_ref().map_or(false, |c| c.load(Ordering::Relaxed)) {
                    break;
                }
                let idx = { queue.lock().unwrap().pop_front() };
                let Some(idx) = idx else { break };
                let seg: Segment = {
                    let s = state_mtx.lock().unwrap();
                    s.segments[idx]
                };
                let start_from = seg.start + {
                    let s = state_mtx.lock().unwrap();
                    s.done[idx]
                };
                if start_from >= seg.end + 1 {
                    continue;
                }
                if let Err(e) = fetch_segment(
                    &client,
                    &url,
                    Path::new(&out),
                    idx,
                    seg,
                    start_from,
                    flush_every,
                    &state_mtx,
                    &done_bytes,
                    cancel.as_ref(),
                ) {
                    let s = state_mtx.lock().unwrap();
                    let _ = s.save(Path::new(&out));
                    if matches!(e, Error::Cancelled) {
                        eprintln!("fastdl: 已暂停（进度已保存）");
                        break;
                    }
                    eprintln!("fastdl: segment {idx} failed: {e}");
                    // 放回队列重试一次
                    queue.lock().unwrap().push_back(idx);
                }
            }
        }));
    }

    // 主线程：进度渲染 + 等待（每 100ms 采样一次；若 progress 为 None 则静默等待）
    let out_path = out_path.to_path_buf();
    loop {
        let cancelled = opts.cancel.as_ref().map_or(false, |c| c.load(Ordering::Relaxed));
        let all_done = queue.lock().unwrap().is_empty();
        if all_done || cancelled {
            break;
        }
        if let Some(cb) = &mut progress {
            let d = done_bytes.load(Ordering::Relaxed);
            let elapsed = t0.elapsed().as_secs_f64().max(0.001);
            let bps = d as f64 / elapsed;
            cb(d, total, bps);
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    for w in workers {
        w.join().ok();
    }

    // 暂停/取消路径：state 保留（供续传），提前返回
    if opts.cancel.as_ref().is_some_and(|c| c.load(Ordering::Relaxed)) {
        {
            let s = state_mtx.lock().unwrap();
            let _ = s.save(&out_path);
        }
        return Ok(DownloadReport {
            total,
            used_segments: segs.len(),
            used_threads: threads,
            range_supported: probe.range_supported,
            resumed_from,
            output: opts.out.clone(),
            cancelled: true,
        });
    }

    // 收尾：校验 + 清状态
    let meta = std::fs::metadata(&out_path)?;
    if meta.len() != total {
        return Err(Error::Verify(format!(
            "输出文件 {} 字节 ≠ 预期 {} 字节",
            meta.len(),
            total
        )));
    }
    State::clear(&out_path);
    let total_done = done_bytes.load(Ordering::Relaxed);
    let elapsed = t0.elapsed().as_secs_f64();
    let bps = total_done as f64 / elapsed.max(0.001);
    if let Some(cb) = &mut progress {
        cb(total, total, bps);
    }
    Ok(DownloadReport {
        total,
        used_segments: segs.len(),
        used_threads: threads,
        range_supported: probe.range_supported,
        resumed_from,
        output: opts.out.clone(),
        cancelled: false,
    })
}

fn fetch_segment(
    client: &Client,
    url: &str,
    out: &Path,
    idx: usize,
    seg: Segment,
    start_from: u64,
    flush_every: u64,
    state_mtx: &Arc<Mutex<State>>,
    done_bytes: &Arc<AtomicU64>,
    cancel: Option<&std::sync::Arc<std::sync::atomic::AtomicBool>>,
) -> Result<(), Error> {
    let end = seg.end;
    let mut file = std::fs::OpenOptions::new().write(true).open(out)?;
    file.seek(std::io::SeekFrom::Start(start_from))?;
    let range = format!("bytes={}-{}", start_from, end);
    let resp = client
        .get(url)
        .header("Range", &range)
        .send()
        .map_err(|e| Error::Http(e.to_string()))?;
    if resp.status().as_u16() != 206 {
        return Err(Error::Http(format!(
            "段 {idx}: 期望 206，得到 {}",
            resp.status().as_u16()
        )));
    }
    let mut reader = resp;
    let mut pos = start_from;
    let mut since_flush: u64 = 0;
    loop {
        // 取消检查：保存已拉进度后立即中止（state 保留供续传）
        if cancel.map_or(false, |c| c.load(Ordering::Relaxed)) {
            let mut s = state_mtx.lock().unwrap();
            s.done[idx] = pos - seg.start;
            let _ = s.save(out);
            return Err(Error::Cancelled);
        }
        let mut buf = [0u8; 256 * 1024];
        let n = reader.read(&mut buf).map_err(|e| Error::Http(e.to_string()))?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n])?;
        pos += n as u64;
        done_bytes.fetch_add(n as u64, Ordering::Relaxed);
        since_flush += n as u64;
        if since_flush >= flush_every {
            let mut s = state_mtx.lock().unwrap();
            s.done[idx] = pos - seg.start;
            let _ = s.save(out);
            since_flush = 0;
        }
    }
    if pos != end + 1 {
        return Err(Error::Verify(format!(
            "段 {idx}: 拉取 {} 字节 ≠ 期望 {}",
            pos - start_from,
            end + 1 - start_from
        )));
    }
    let mut s = state_mtx.lock().unwrap();
    s.done[idx] = seg.len();
    s.save(out)?;
    Ok(())
}

/// 总大小未知时的单流下载（无法分段；支持 Range 断点 + cancel；进度 total=0 表示不确定）
fn download_unknown_total(
    client: &Client,
    probe: &ProbeResult,
    opts: &DownloadOptions,
    mut progress: Option<Box<dyn FnMut(u64, u64, f64)>>,
) -> Result<DownloadReport, Error> {
    let out_path = Path::new(&opts.out);
    if let Some(parent) = out_path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).ok();
        }
    }

    // 断点：Range 可用且已有部分文件 → 从文件尾部续传；否则重新下载
    let mut resumed_from = if opts.resume && probe.range_supported {
        std::fs::metadata(out_path)
            .map(|m| m.len())
            .unwrap_or(0)
    } else {
        0
    };

    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(resumed_from == 0)
        .append(resumed_from > 0)
        .open(out_path)?;

    let resp = if resumed_from > 0 {
        client
            .get(&opts.url)
            .header("Range", format!("bytes={}-", resumed_from))
            .send()
            .map_err(|e| Error::Http(e.to_string()))?
    } else {
        client.get(&opts.url).send().map_err(|e| Error::Http(e.to_string()))?
    };
    let code = resp.status();
    let status = code.as_u16();
    if !code.is_success() {
        return Err(Error::Http(format!("HTTP {status}")));
    }
    // 续传请求被服务器忽略（回 200 全量）→ 从头重下，避免 append 写坏
    let restart = resumed_from > 0 && status != 206;
    if restart {
        resumed_from = 0;
        file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(out_path)?;
    }

    let mut reader = resp;
    let t0 = std::time::Instant::now();
    let mut written = resumed_from;
    let mut cancel_hit = false;
    if let Some(cb) = &mut progress {
        cb(resumed_from, 0, 0.0);
    }
    loop {
        if opts.cancel.as_ref().map_or(false, |c| c.load(Ordering::Relaxed)) {
            cancel_hit = true;
            break;
        }
        let mut buf = [0u8; 256 * 1024];
        let n = reader.read(&mut buf).map_err(|e| Error::Http(e.to_string()))?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n])?;
        written += n as u64;
        if let Some(cb) = &mut progress {
            let bps = written as f64 / t0.elapsed().as_secs_f64().max(0.001);
            cb(written, 0, bps); // total=0 → 不确定进度
        }
    }
    let elapsed = t0.elapsed().as_secs_f64();
    let bps = (written - resumed_from) as f64 / elapsed.max(0.001);
    if let Some(cb) = &mut progress {
        if !cancel_hit {
            cb(written, 0, bps);
        }
    }
    Ok(DownloadReport {
        total: written, // 未知总大小：报告实际已写入量
        used_segments: 1,
        used_threads: 1,
        range_supported: probe.range_supported,
        resumed_from,
        output: opts.out.clone(),
        cancelled: cancel_hit,
    })
}

/// 单流下载（服务器不支持 Range 时的退化路径）
pub fn download_single(
    client: &Client,
    url: &str,
    out: &Path,
    total: Option<u64>,
    mut progress: Option<&mut dyn FnMut(u64, u64, f64)>,
    cancel: Option<&std::sync::Arc<std::sync::atomic::AtomicBool>>,
) -> Result<u64, Error> {
    let resp = client.get(url).send().map_err(|e| Error::Http(e.to_string()))?;
    if !resp.status().is_success() {
        return Err(Error::Http(format!(
            "HTTP {}",
            resp.status().as_u16()
        )));
    }
    let mut file = std::fs::File::create(out)?;
    let mut reader = resp;
    let t0 = std::time::Instant::now();
    let mut written: u64 = 0;
    loop {
        if cancel.map_or(false, |c| c.load(Ordering::Relaxed)) {
            return Err(Error::Cancelled);
        }
        let mut buf = [0u8; 256 * 1024];
        let n = reader.read(&mut buf).map_err(|e| Error::Http(e.to_string()))?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n])?;
        written += n as u64;
        if let Some(cb) = &mut progress {
            let t = total.unwrap_or(written);
            let bps = written as f64 / t0.elapsed().as_secs_f64().max(0.001);
            cb(written, t, bps);
        }
    }
    Ok(written)
}
