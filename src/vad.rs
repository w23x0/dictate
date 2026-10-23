//! 静音门：按 20 ms 帧算能量，裁掉首尾静音；另给"连续静音多久"的判定，
//! 供热键路径做自动结束。

use crate::agc::{lin_to_db, rms};

pub const FRAME_MS: u32 = 20;

/// 语音帧判定阈值（dBFS）。低于它算静音。
pub const SPEECH_DBFS: f32 = -45.0;

/// 裁掉首尾静音，两端各留 `pad_ms` 余量。全是静音时返回空切片。
pub fn trim_silence<'a>(samples: &'a [f32], rate: u32, pad_ms: u32) -> &'a [f32] {
    let frame = ((rate as f32 * FRAME_MS as f32) / 1000.0) as usize;
    if frame == 0 || samples.len() < frame {
        return samples;
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
    let (Some(a), Some(b)) = (first, last) else {
        return &samples[0..0];
    };
    let pad = ((rate as f32 * pad_ms as f32) / 1000.0) as usize;
    let start = (a * frame).saturating_sub(pad);
    let end = (((b + 1) * frame) + pad).min(samples.len());
    &samples[start..end]
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
    use crate::agc::db_to_lin;

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
        let t = trim_silence(&v, rate, 0);
        let dur = t.len() as f32 / rate as f32;
        assert!((dur - 0.5).abs() < 0.06, "dur={dur}");
    }

    #[test]
    fn all_silence_yields_empty() {
        let rate = 16_000;
        let v = vec![0.0f32; rate as usize];
        assert!(trim_silence(&v, rate, 0).is_empty());
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
