use std::path::PathBuf;
use std::time::Duration;

use anyhow::Context;
use clap::Parser;
use fastdl::engine::{download, download_single, DownloadOptions, DownloadReport};
use fastdl::probe::{probe, ProbeResult};
use indicatif::{ProgressBar, ProgressStyle};
use reqwest::blocking::Client;
use sha2::{Digest, Sha256};

#[derive(Parser)]
#[command(name = "fastdl", about = "高速分段并行下载器（macOS/通用）")]
struct Cli {
    /// 下载 URL
    url: String,

    /// 输出文件路径（默认取 URL 文件名或 "download"）
    #[arg(short = 'o', long)]
    out: Option<PathBuf>,

    /// 最大分段数（默认 32；服务器不支持 Range 时自动单流）
    #[arg(short = 's', long, default_value_t = 32)]
    segments: usize,

    /// 并行工作线程数（默认 8）
    #[arg(short = 't', long, default_value_t = 8)]
    threads: usize,

    /// 断点续传（读取已有进度状态）
    #[arg(short = 'r', long)]
    resume: bool,

    /// 只做服务器能力探测，不下载
    #[arg(short = 'p', long)]
    probe_only: bool,

    /// 下载完成后校验 SHA-256
    #[arg(long)]
    verify: Option<String>,
}

fn default_out(url: &str) -> String {
    url.rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or("download")
        .to_string()
}

fn human(n: u64) -> String {
    const U: [&str; 6] = ["B", "KB", "MB", "GB", "TB", "PB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else {
        format!("{v:.2} {}", U[i])
    }
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    // 尊重 HTTP(S)_PROXY 环境变量（reqwest 默认行为）
    let client = Client::builder()
        .timeout(Duration::from_secs(300))
        .build()
        .context("HTTP client 初始化")?;

    println!("fastdl 探测 {}", cli.url);
    let p: ProbeResult = probe(&client, &cli.url).with_context(|| "探测请求")?;
    println!(
        "  HTTP {} · Range 支持: {} · 大小: {}{}",
        p.status,
        p.range_supported,
        p.total_size.map(human).unwrap_or_else(|| "?".into()),
        match &p.etag {
            Some(e) => format!(" · ETag {e}"),
            None => String::new(),
        }
    );
    if cli.probe_only {
        return Ok(());
    }
    if !p.range_supported && !matches!(p.status, 200..=299) {
        anyhow::bail!("服务器返回 {} 且不支持 Range", p.status);
    }

    let out = cli.out.unwrap_or_else(|| PathBuf::from(default_out(&cli.url)));
    let out_str = out.to_string_lossy().to_string();

    let opts = DownloadOptions {
        url: cli.url.clone(),
        out: out_str.clone(),
        segments: cli.segments,
        threads: cli.threads,
        resume: cli.resume,
        state_flush_every: 1024 * 1024,
        cancel: None,
    };

    let pb = ProgressBar::new(0);
    pb.set_style(
        ProgressStyle::with_template(
            "{bar:30} {percent:>3}% {bytes}/{total_bytes}  ↓ {bytes_per_sec}  ETA {eta}",
        )
        .unwrap_or_else(|_| ProgressStyle::default_bar()),
    );
    pb.enable_steady_tick(Duration::from_millis(100));

    let mut progress = |done: u64, total: u64, bps: f64| {
        pb.set_length(total);
        pb.set_position(done);
        let _ = bps; // 速率由 indicatif 依时间戳自动算
    };

    let rep: DownloadReport = if p.range_supported {
        let pb2 = pb.clone();
        download(
            &client,
            &p,
            &opts,
            Some(Box::new(move |done: u64, total: u64, _bps: f64| {
                pb2.set_length(total);
                pb2.set_position(done);
            })),
        )?
    } else {
        let written = download_single(
            &client,
            &cli.url,
            &out,
            p.total_size,
            Some(&mut progress),
            None,
        )?;
        DownloadReport {
            total: p.total_size.unwrap_or(written),
            used_segments: 1,
            used_threads: 1,
            range_supported: false,
            resumed_from: 0,
            output: out_str,
            cancelled: false,
        }
    };

    pb.finish_and_clear();
    println!(
        "完成 {} · {} 段 × {} 线程 · 续传起点 {} · 输出 {}",
        human(rep.total),
        rep.used_segments,
        rep.used_threads,
        human(rep.resumed_from),
        rep.output
    );

    if let Some(want) = &cli.verify {
        let want = want.to_lowercase();
        let bytes = std::fs::read(&out).with_context(|| "读取输出文件")?;
        let got = hex::encode(Sha256::digest(&bytes));
        println!("SHA-256: {got}");
        if got != want {
            anyhow::bail!("校验失败: 期望 {want}");
        }
        println!("校验通过 ✓");
    }
    Ok(())
}
