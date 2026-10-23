//! 自动增益（AGC）：按块估 RMS，向目标电平收敛，块内线性插值，输出接软限幅。
//!
//! 设计口径（与 LazyTyper 一致）：
//! * 控制块 10 ms；增益**只升不降得慢**——抬增益用 release（200 ms），压增益用 attack（10 ms）。
//! * 底噪以下不抬（否则静音段会被放大成嘶声）。
//! * 上限 +30 dB；输出过 `tanh` 软限幅到 −1 dBFS。
//! * 全程原地处理，**不分配**。

const LIMIT: f32 = 0.891; // −1 dBFS

#[derive(Debug, Clone, Copy)]
pub struct AgcConfig {
    pub target_dbfs: f32,
    pub attack_ms: f32,
    pub release_ms: f32,
    pub max_gain_db: f32,
    /// 低于这个电平（dBFS）视为静音，不抬增益。
    pub noise_floor_dbfs: f32,
}

impl Default for AgcConfig {
    fn default() -> Self {
        Self {
            target_dbfs: -20.0,
            attack_ms: 10.0,
            release_ms: 200.0,
            max_gain_db: 30.0,
            noise_floor_dbfs: -55.0,
        }
    }
}

pub struct Agc {
    target: f32,
    max_gain: f32,
    attack: f32,
    release: f32,
    floor: f32,
    block: usize,
    gain: f32,
}

impl Agc {
    pub fn new(sample_rate: u32, cfg: AgcConfig) -> Self {
        let block = ((sample_rate as f32) * 0.010).round().max(1.0) as usize;
        // 系数按「控制块」算，不是按样本：时间常数 ms 换成块数，1 - e^(-1/块数)。
        let coef = |ms: f32| -> f32 {
            let blocks = (ms / 10.0).max(1.0);
            1.0 - (-1.0f32 / blocks).exp()
        };
        Self {
            target: db_to_lin(cfg.target_dbfs),
            max_gain: 10f32.powf(cfg.max_gain_db / 20.0),
            attack: coef(cfg.attack_ms),
            release: coef(cfg.release_ms),
            floor: db_to_lin(cfg.noise_floor_dbfs),
            block,
            gain: 1.0,
        }
    }

    /// 原地处理一整段。返回应用过的最大增益（线性），给日志用。
    pub fn process(&mut self, buf: &mut [f32]) -> f32 {
        let mut peak_gain = self.gain;
        let mut i = 0;
        while i < buf.len() {
            let end = (i + self.block).min(buf.len());
            let block = &buf[i..end];

            let mut sum = 0.0f64;
            for s in block.iter() {
                sum += (*s as f64) * (*s as f64);
            }
            let rms = (sum / block.len() as f64).sqrt() as f32;

            if rms > self.floor && rms > 0.0 {
                let desired = (self.target / rms).clamp(0.5, self.max_gain);
                let coef = if desired < self.gain {
                    self.attack
                } else {
                    self.release
                };
                self.gain += (desired - self.gain) * coef;
            }

            let from = peak_gain;
            let to = self.gain;
            let step = (to - from) / block.len() as f32;
            let mut g = from;
            for s in buf[i..end].iter_mut() {
                g += step;
                *s = soft_limit(*s * g);
            }
            peak_gain = to;
            i = end;
        }
        peak_gain
    }

    #[cfg(test)]
    fn gain(&self) -> f32 {
        self.gain
    }
}

#[inline]
fn soft_limit(x: f32) -> f32 {
    if x.abs() <= 0.5 {
        x
    } else {
        // tanh 在 |x|>0.5 才启用：小信号完全不变，大信号平滑压到 ±0.891
        LIMIT * (x / LIMIT).tanh()
    }
}

#[inline]
pub fn db_to_lin(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

#[inline]
pub fn lin_to_db(x: f32) -> f32 {
    20.0 * x.max(1e-9).log10()
}

pub fn rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum: f64 = samples.iter().map(|s| (*s as f64) * (*s as f64)).sum();
    (sum / samples.len() as f64).sqrt() as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(amp: f32, freq: f32, rate: u32, secs: f32) -> Vec<f32> {
        let n = (rate as f32 * secs) as usize;
        (0..n)
            .map(|i| amp * (2.0 * std::f32::consts::PI * freq * i as f32 / rate as f32).sin())
            .collect()
    }

    /// 目标 −20 dBFS：−40 dBFS 与 −10 dBFS 两段，1 秒内都要收敛到 ±3 dB。
    #[test]
    fn converges_to_target_from_both_sides() {
        let rate = 16_000;
        for in_dbfs in [-40.0f32, -10.0] {
            let mut agc = Agc::new(rate, AgcConfig::default());
            let mut buf = sine(db_to_lin(in_dbfs), 440.0, rate, 2.0);
            agc.process(&mut buf);
            // 只看后半段（收敛后）
            let tail = &buf[buf.len() / 2..];
            let out = lin_to_db(rms(tail));
            assert!(
                (out - (-20.0)).abs() < 3.0,
                "in={in_dbfs} dBFS → out={out:.1} dBFS（应 ≈ −20）"
            );
        }
    }

    #[test]
    fn silence_is_not_amplified() {
        let mut agc = Agc::new(16_000, AgcConfig::default());
        let mut buf = vec![0.0f32; 16_000];
        agc.process(&mut buf);
        assert_eq!(agc.gain(), 1.0);
        assert!(buf.iter().all(|s| *s == 0.0));
    }

    #[test]
    fn limiter_caps_output() {
        let mut agc = Agc::new(16_000, AgcConfig::default());
        let mut buf = sine(0.99, 200.0, 16_000, 0.5);
        agc.process(&mut buf);
        let peak = buf.iter().fold(0.0f32, |m, s| m.max(s.abs()));
        assert!(peak <= LIMIT + 1e-3, "peak={peak}");
    }

    #[test]
    fn small_signals_pass_through_untouched_shape() {
        // |x| ≤ 0.5 时软限幅是恒等，避免无谓失真
        for x in [-0.5f32, -0.1, 0.0, 0.25, 0.5] {
            assert_eq!(soft_limit(x), x);
        }
    }
}
