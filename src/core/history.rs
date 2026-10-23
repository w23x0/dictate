//! 历史：一行一条 JSON（`history.jsonl`）。
//!
//! 选 jsonl 而不是 SQLite：这个量级（几千条）线性扫足够快，而且坏了只坏一行。

use std::io::Write;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    pub id: String,
    /// RFC3339
    pub ts: String,
    pub duration_ms: u64,
    pub latency_ms: u64,
    pub model: String,
    /// "ok" 或 "error: …"
    pub status: String,
    pub text: String,
}

impl Entry {
    pub fn is_ok(&self) -> bool {
        self.status == "ok"
    }
}

pub struct History {
    path: PathBuf,
}

impl History {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn append(&self, entry: &Entry) -> Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir).ok();
        }
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .with_context(|| format!("打开历史失败：{}", self.path.display()))?;
        writeln!(f, "{}", serde_json::to_string(entry)?)?;
        Ok(())
    }

    /// 全部条目，按时间正序。坏行跳过（不因为一行坏掉整份历史）。
    pub fn load(&self) -> Vec<Entry> {
        let Ok(text) = std::fs::read_to_string(&self.path) else {
            return Vec::new();
        };
        text.lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| serde_json::from_str::<Entry>(l).ok())
            .collect()
    }

    pub fn delete(&self, id: &str) -> Result<()> {
        let entries: Vec<Entry> = self.load().into_iter().filter(|e| e.id != id).collect();
        self.rewrite(&entries)
    }

    pub fn clear(&self) -> Result<()> {
        self.rewrite(&[])
    }

    fn rewrite(&self, entries: &[Entry]) -> Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir).ok();
        }
        let tmp = self.path.with_extension("jsonl.tmp");
        {
            let mut f = std::fs::File::create(&tmp)?;
            for e in entries {
                writeln!(f, "{}", serde_json::to_string(e)?)?;
            }
            f.sync_all().ok();
        }
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }
}

/// 新条目的 id：时间戳 + 进程内计数，够唯一。
pub fn new_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    format!("{:x}-{:x}", now.as_millis(), n)
}

pub fn now_rfc3339() -> String {
    chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        std::env::temp_dir()
            .join(format!("dictate-hist-{}-{}", std::process::id(), name))
            .join("history.jsonl")
    }

    fn entry(id: &str, text: &str) -> Entry {
        Entry {
            id: id.into(),
            ts: now_rfc3339(),
            duration_ms: 1200,
            latency_ms: 800,
            model: "qwen3-asr-flash".into(),
            status: "ok".into(),
            text: text.into(),
        }
    }

    #[test]
    fn append_load_delete_clear() {
        let p = tmp("crud");
        std::fs::remove_dir_all(p.parent().unwrap()).ok();
        let h = History::new(&p);

        h.append(&entry("a", "第一句")).unwrap();
        h.append(&entry("b", "第二句")).unwrap();
        let got = h.load();
        assert_eq!(got.len(), 2);
        assert_eq!(got[1].text, "第二句");

        h.delete("a").unwrap();
        let got = h.load();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].id, "b");

        h.clear().unwrap();
        assert!(h.load().is_empty());
        std::fs::remove_dir_all(p.parent().unwrap()).ok();
    }

    #[test]
    fn corrupt_line_is_skipped() {
        let p = tmp("corrupt");
        std::fs::remove_dir_all(p.parent().unwrap()).ok();
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        let mut f = std::fs::File::create(&p).unwrap();
        writeln!(f, "{{\"broken\"").unwrap();
        writeln!(f, "{}", serde_json::to_string(&entry("ok", "好")).unwrap()).unwrap();
        let h = History::new(&p);
        let got = h.load();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].id, "ok");
        std::fs::remove_dir_all(p.parent().unwrap()).ok();
    }
}
