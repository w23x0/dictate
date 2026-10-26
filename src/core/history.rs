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
    /// RFC3339（本地时区，给旧工具与人看）
    pub ts: String,
    /// UTC 毫秒时间戳。排序、按天分组、统计的唯一依据；旧文件没有时由 `ts` 回填。
    #[serde(default)]
    pub ts_ms: u64,
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
        self.append_with_cap(entry, 0)
    }

    /// 追加一条；`cap > 0` 时超出上限（留 10% 滞回，避免每次追加都重写）就截掉最旧的。
    pub fn append_with_cap(&self, entry: &Entry, cap: usize) -> Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir).ok();
        }
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .with_context(|| format!("打开历史失败：{}", self.path.display()))?;
        writeln!(f, "{}", serde_json::to_string(entry)?)?;
        drop(f);
        if cap > 0 {
            let all = self.load();
            if all.len() > cap + cap / 10 {
                let keep: Vec<Entry> = all[all.len().saturating_sub(cap)..].to_vec();
                self.rewrite(&keep)?;
            }
        }
        Ok(())
    }

    /// 全部条目，按时间正序。坏行跳过（不因为一行坏掉整份历史）。
    /// 排序依据 `ts_ms`（没有就由 `ts` 回填），不再依赖文件追加序——
    /// 改钟、乱序写入后「最新在上」「按天分组」仍然成立。回填不落盘，读到时现算。
    pub fn load(&self) -> Vec<Entry> {
        let Ok(text) = std::fs::read_to_string(&self.path) else {
            return Vec::new();
        };
        let mut out: Vec<Entry> = text
            .lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| serde_json::from_str::<Entry>(l).ok())
            .map(|mut e| {
                if e.ts_ms == 0 {
                    e.ts_ms = parse_ts_ms(&e.ts).unwrap_or(0);
                }
                e
            })
            .collect();
        // 稳定排序：ts 解析不出的坏行（key=MAX）沉底且保持原相对顺序
        out.sort_by_key(|e| if e.ts_ms == 0 { u64::MAX } else { e.ts_ms });
        out
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

/// 新条目的 id：毫秒时间戳 + pid + 进程内计数，跨进程同毫秒也不撞。
pub fn new_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    format!("{:x}-{:x}-{:x}", now.as_millis(), std::process::id(), n)
}

pub fn now_rfc3339() -> String {
    chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// UTC 毫秒时间戳。
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// RFC3339 字符串 → UTC 毫秒；解析不出返回 None。
pub fn parse_ts_ms(ts: &str) -> Option<u64> {
    chrono::DateTime::parse_from_rfc3339(ts)
        .ok()
        .map(|dt| dt.timestamp_millis().max(0) as u64)
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
            ts_ms: now_ms(),
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
    fn load_sorts_by_ts_ms_not_file_order() {
        // 文件里乱序写入（模拟改钟/NTP 校正后补录），读出来必须按时间排好
        let p = tmp("order");
        std::fs::remove_dir_all(p.parent().unwrap()).ok();
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        let mk = |id: &str, ts: &str, ts_ms: u64| {
            serde_json::json!({
                "id": id, "ts": ts, "ts_ms": ts_ms, "duration_ms": 1000, "latency_ms": 10,
                "model": "m", "status": "ok", "text": id
            })
            .to_string()
        };
        std::fs::write(
            &p,
            format!(
                "{}\n{}\n{}\n",
                mk("new", "2026-10-26T10:00:00+08:00", 1_800_000_000_000),
                mk("old", "2026-10-24T10:00:00+08:00", 1_799_000_000_000),
                mk("mid", "2026-10-25T10:00:00+08:00", 1_799_500_000_000),
            ),
        )
        .unwrap();
        let ids: Vec<String> = History::new(&p).load().into_iter().map(|e| e.id).collect();
        assert_eq!(ids, vec!["old", "mid", "new"]);
        std::fs::remove_dir_all(p.parent().unwrap()).ok();
    }

    #[test]
    fn ts_ms_backfilled_from_legacy_ts() {
        // 旧版本写的行没有 ts_ms：读的时候由 ts 回填，排序照常
        let p = tmp("legacy");
        std::fs::remove_dir_all(p.parent().unwrap()).ok();
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(
            &p,
            "{\"id\":\"a\",\"ts\":\"2026-10-26T10:00:00+08:00\",\"duration_ms\":1,\"latency_ms\":1,\"model\":\"m\",\"status\":\"ok\",\"text\":\"x\"}\n",
        )
        .unwrap();
        let got = History::new(&p).load();
        assert_eq!(got.len(), 1);
        assert_eq!(
            got[0].ts_ms,
            parse_ts_ms("2026-10-26T10:00:00+08:00").unwrap()
        );
        std::fs::remove_dir_all(p.parent().unwrap()).ok();
    }

    #[test]
    fn cap_trims_oldest_with_hysteresis() {
        let p = tmp("cap");
        std::fs::remove_dir_all(p.parent().unwrap()).ok();
        let h = History::new(&p);
        for i in 0..12 {
            let mut e = entry(&format!("e{i}"), "x");
            e.ts_ms = 1_000_000 + i; // 有序递增，删掉的应该是最旧的
            h.append_with_cap(&e, 10).unwrap();
        }
        let got = h.load();
        assert_eq!(got.len(), 10, "滞回：12 > 10+1 才裁到 10");
        assert_eq!(got[0].id, "e2", "裁掉最旧的");
        assert_eq!(got[9].id, "e11");
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
