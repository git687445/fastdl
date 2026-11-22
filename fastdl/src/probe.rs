//! 服务器能力探测 —— 逆向自 TDM Fast：最小探测请求 `Range: bytes=0-0`
//! 206 + Content-Range  →  支持分段（可拿总大小）
//! 200 + Content-Length  →  忽略 Range，只能单连接
//! 其他                   →  记录状态码

use crate::Error;
use reqwest::blocking::Client;

#[derive(Debug, Clone, PartialEq)]
pub struct ProbeResult {
    pub status: u16,
    /// 是否支持 Range 分段（206）
    pub range_supported: bool,
    /// 文件总大小（Content-Range 的 total 或 200 的 Content-Length）
    pub total_size: Option<u64>,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub content_type: Option<String>,
}

pub fn probe(client: &Client, url: &str) -> Result<ProbeResult, Error> {
    let resp = client
        .get(url)
        .header("Range", "bytes=0-0")
        .send()
        .map_err(|e| Error::Http(e.to_string()))?;
    let status = resp.status().as_u16();
    let range_supported = status == 206;

    let total_size = resp
        .headers()
        .get("content-range")
        .and_then(|v| v.to_str().ok())
        .and_then(|cr| cr.split('/').nth(1))
        .filter(|t| *t != "*")
        .and_then(|t| t.parse::<u64>().ok());

    let total_size = total_size.or_else(|| {
        if status == 200 {
            resp.headers()
                .get("content-length")
                .and_then(|v| v.to_str().ok())
                .and_then(|t| t.parse::<u64>().ok())
        } else {
            None
        }
    });

    // 回退：Range 探测拿不到 total（206 但无 Content-Range / total=*）时，
    // 再发一次不带 Range 的 GET 取 Content-Length（多数服务器对普通请求会给全量长度）
    let total_size = total_size.or_else(|| {
        let plain = client.get(url).send().ok()?;
        if plain.status().as_u16() == 200 {
            plain
                .headers()
                .get("content-length")
                .and_then(|v| v.to_str().ok())
                .and_then(|t| t.parse::<u64>().ok())
        } else {
            None
        }
    });

    Ok(ProbeResult {
        status,
        range_supported,
        total_size,
        etag: resp
            .headers()
            .get("etag")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string()),
        last_modified: resp
            .headers()
            .get("last-modified")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string()),
        content_type: resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string()),
    })
}
