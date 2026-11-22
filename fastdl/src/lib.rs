pub mod engine;
pub mod probe;
pub mod seg;

pub use engine::{download, download_single, DownloadOptions, DownloadReport};
pub use probe::{probe, ProbeResult};
pub use seg::{plan, Segment, State};
/// 供上层（GUI 壳）直接复用的 HTTP 客户端类型
pub use reqwest::blocking::Client;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("HTTP: {0}")]
    Http(String),
    #[error("I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("校验失败: {0}")]
    Verify(String),
    /// 用户主动取消/暂停（state 已保留，可续传）
    #[error("已取消")]
    Cancelled,
}
