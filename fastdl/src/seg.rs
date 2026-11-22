//! 分段规划 + 断点续传状态

use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Segment {
    pub start: u64,
    pub end: u64, // 含
}

impl Segment {
    pub fn len(&self) -> u64 {
        self.end - self.start + 1
    }
}

/// 把 [0, total) 切成 n 段（余数摊到前几段，保证覆盖完整且连续）
pub fn plan(total: u64, n: usize) -> Vec<Segment> {
    let n = n.max(1).min(total as usize);
    let base = total / n as u64;
    let extra = total % n as u64;
    (0..n)
        .map(|i| {
            let start = (i as u64) * base + i.min(extra as usize) as u64;
            let end = if (i as u64) < extra {
                start + base + 1
            } else {
                start + base
            };
            Segment {
                start,
                end: end.saturating_sub(1),
            }
        })
        .collect()
}

#[derive(Serialize, Deserialize)]
pub struct State {
    pub url: String,
    pub out: String,
    pub total: u64,
    pub etag: Option<String>,
    pub segments: Vec<Segment>,
    pub done: Vec<u64>, // 每段已写字节（< len 表示该段未完成）
}

impl State {
    pub fn new(url: &str, out: &str, total: u64, etag: Option<&str>, segs: Vec<Segment>) -> Self {
        let n = segs.len();
        Self {
            url: url.to_string(),
            out: out.to_string(),
            total,
            etag: etag.map(|s| s.to_string()),
            segments: segs,
            done: vec![0; n],
        }
    }

    pub fn path(out: &Path) -> std::path::PathBuf {
        // 注意：不能用 with_extension（它会替换 .bin 而不是追加）
        let mut s = out.as_os_str().to_os_string();
        s.push(".fastdl.state.json");
        std::path::PathBuf::from(s)
    }

    pub fn save(&self, out: &Path) -> std::io::Result<()> {
        let p = Self::path(out);
        let tmp = p.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string(self).unwrap())?;
        std::fs::rename(&tmp, p)
    }

    pub fn load(out: &Path, url: &str, etag: Option<&str>) -> Option<Self> {
        let p = Self::path(out);
        let raw = std::fs::read_to_string(p).ok()?;
        let st: State = serde_json::from_str(&raw).ok()?;
        // 一致性校验：url 必须同；若服务器带 ETag，必须匹配，否则状态作废
        if st.url != url {
            return None;
        }
        if let (Some(a), Some(b)) = (&st.etag, etag) {
            if a != b {
                return None;
            }
        }
        Some(st)
    }

    pub fn finished(&self) -> bool {
        self.done
            .iter()
            .enumerate()
            .all(|(i, d)| *d >= self.segments[i].len())
    }

    pub fn clear(out: &Path) {
        let _ = std::fs::remove_file(Self::path(out));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_covers_and_is_contiguous() {
        for (total, n) in [(100u64, 1), (100, 4), (100, 64), (7, 64), (1, 1), (1, 8)] {
            let segs = plan(total, n);
            let n = segs.len();
            assert_eq!(n.min(1), 1);
            assert_eq!(segs[0].start, 0);
            for (i, s) in segs.iter().enumerate() {
                assert!(s.end >= s.start);
                if i + 1 < segs.len() {
                    assert_eq!(s.end + 1, segs[i + 1].start, "gap/overlap at {i}");
                }
            }
            let last = segs[n - 1];
            assert_eq!(last.end, total - 1);
            let sum: u64 = segs.iter().map(|s| s.len()).sum();
            assert_eq!(sum, total);
        }
    }
}
