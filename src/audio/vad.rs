//! 静音门：按 20 ms 帧算能量，裁掉首尾静音；另给"连续静音多久"的判定，
//! 供热键路径做自动结束。

use crate::audio::agc::{lin_to_db, rms};

pub const FRAME_MS: u32 = 20;

/// 语音帧判定阈值（dBFS）。低于它算静音。
pub const SPEECH_DBFS: f32 = -45.0;

/// 裁掉首尾静音，两端各留 `pad_ms` 余量。全是静音时返回 `None`。
pub fn trim_range(samples: &[f32], rate: u32, pad_ms: u32) -> Option<(usize, usize)> {
    let frame = ((rate as f32 * FRAME_MS as f32) / 1000.0) as usize;
    if frame == 0 || samples.len() < frame {
        // 短于一个帧（或退化率）：不裁，按有语音处理
        return Some((0, samples.len()));
    }
    let frames = samples.len() / frame;
    let mut first = None;
    let mut last = None;
    for f in 0..frames {
        let seg = &samples[f * frame..(f + 1) * frame];
        if lin_to_db(rms(seg)) >= SPEECH_DBFS {
            first.get_or_insert(f);
            last = Some(f);
        }
    }
    let (a, b) = (first?, last?);
    let pad = ((rate as f32 * pad_ms as f32) / 1000.0) as usize;
    let start = (a * frame).saturating_sub(pad);
    let end = (((b + 1) * frame) + pad).min(samples.len());
    Some((start, end))
}

/// 原地裁掉首尾静音（两端各留 `pad_ms` 余量）；全是静音则清空返回 `false`。
/// 有语音时用 `copy_within` 原地收缩 buffer，不额外分配。
pub fn trim_in_place(buf: &mut Vec<f32>, rate: u32, pad_ms: u32) -> bool {
    if buf.is_empty() {
        return false;
    }
    let Some((start, end)) = trim_range(buf, rate, pad_ms) else {
        buf.clear();
        return false;
    };
    if start > 0 {
        buf.copy_within(start..end, 0);
    }
    buf.truncate(end - start);
    true
}

/// 从尾部往回看：末尾连续静音是否已经 ≥ `silence_ms`。
pub fn trailing_silence_ms(samples: &[f32], rate: u32) -> u32 {
    let frame = ((rate as f32 * FRAME_MS as f32) / 1000.0) as usize;
    if frame == 0 || samples.len() < frame {
        return 0;
    }
    let frames = samples.len() / frame;
    let mut quiet = 0;
    for f in (0..frames).rev() {
        let seg = &samples[f * frame..(f + 1) * frame];
        if lin_to_db(rms(seg)) >= SPEECH_DBFS {
            break;
        }
        quiet += 1;
    }
    quiet * FRAME_MS
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::agc::db_to_lin;

    fn tone(secs: f32, rate: u32) -> Vec<f32> {
        let n = (rate as f32 * secs) as usize;
        (0..n)
            .map(|i| 0.2 * (2.0 * std::f32::consts::PI * 300.0 * i as f32 / rate as f32).sin())
            .collect()
    }

    #[test]
    fn trims_leading_and_trailing_silence() {
        let rate = 16_000;
        let mut v = vec![0.0f32; rate as usize / 2]; // 0.5 s 静音
        v.extend(tone(0.5, rate));
        v.extend(vec![0.0f32; rate as usize / 2]);
        let (s, e) = trim_range(&v, rate, 0).unwrap();
        let t = &v[s..e];
        let dur = t.len() as f32 / rate as f32;
        assert!((dur - 0.5).abs() < 0.06, "dur={dur}");
    }

    #[test]
    fn all_silence_yields_empty() {
        let rate = 16_000;
        let v = vec![0.0f32; rate as usize];
        assert!(trim_range(&v, rate, 0).is_none());
    }

    #[test]
    fn trim_in_place_shrinks_and_preserves_samples() {
        let rate = 16_000;
        let mut v = vec![0.0f32; rate as usize / 2];
        v.extend(tone(0.5, rate));
        v.extend(vec![0.0f32; rate as usize / 2]);
        let cap = v.capacity();
        assert!(trim_in_place(&mut v, rate, 0));
        let dur = v.len() as f32 / rate as f32;
        assert!((dur - 0.5).abs() < 0.06, "dur={dur}");
        // 内容没被破坏：去掉首尾静音后，中间那段正弦仍在
        let expected = tone(0.5, rate);
        assert_eq!(v, expected);
        // 原地裁剪不该重新分配
        assert_eq!(v.capacity(), cap);
    }

    #[test]
    fn trailing_silence_counts_from_the_end() {
        let rate = 16_000;
        let mut v = tone(0.3, rate);
        v.extend(vec![0.0f32; rate as usize / 2]);
        let ms = trailing_silence_ms(&v, rate);
        assert!((450..=520).contains(&ms), "ms={ms}");
        // 语音在末尾时不应有静音尾巴
        let mut w = vec![0.0f32; rate as usize / 2];
        w.extend(tone(0.3, rate));
        assert_eq!(trailing_silence_ms(&w, rate), 0);
    }

    #[test]
    fn quiet_tone_below_threshold_counts_as_silence() {
        let rate = 16_000;
        let n = (rate as f32 * 0.2) as usize;
        let v: Vec<f32> = (0..n).map(|_| db_to_lin(-60.0)).collect();
        assert_eq!(trailing_silence_ms(&v, rate), 200);
    }
}
