//! 采集 → 单声道 → 16 kHz → WAV。
//!
//! 采集用 cpal（默认输入设备，设备原生采样率与声道数），
//! 重采样用 rubato 的 `FftFixedIn`（任意率互转，流式）。

use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Context, Result};
use rubato::{FftFixedIn, Resampler};

pub const TARGET_RATE: u32 = 16_000;

/// 把多声道交织数据混成单声道。
pub fn to_mono(interleaved: &[f32], channels: usize) -> Vec<f32> {
    if channels <= 1 {
        return interleaved.to_vec();
    }
    interleaved
        .chunks_exact(channels)
        .map(|f| f.iter().sum::<f32>() / channels as f32)
        .collect()
}

/// 流式重采样到 16 kHz。同率时直接透传（零拷贝地 extend）。
pub struct Downsampler {
    inner: Option<FftFixedIn<f32>>,
    pending: Vec<f32>,
    chunk: usize,
}

impl Downsampler {
    pub fn new(in_rate: u32) -> Result<Self> {
        if in_rate == TARGET_RATE {
            return Ok(Self { inner: None, pending: Vec::new(), chunk: 0 });
        }
        let chunk = 1024;
        let inner = FftFixedIn::<f32>::new(in_rate as usize, TARGET_RATE as usize, chunk, 2, 1)
            .map_err(|e| anyhow!("重采样器构造失败（{in_rate} → {TARGET_RATE}）：{e}"))?;
        Ok(Self { inner: Some(inner), pending: Vec::with_capacity(chunk * 2), chunk })
    }

    pub fn push(&mut self, samples: &[f32], out: &mut Vec<f32>) -> Result<()> {
        let Some(inner) = self.inner.as_mut() else {
            out.extend_from_slice(samples);
            return Ok(());
        };
        self.pending.extend_from_slice(samples);
        while self.pending.len() >= self.chunk {
            let block: Vec<f32> = self.pending.drain(..self.chunk).collect();
            let res = inner
                .process(&[block], None)
                .map_err(|e| anyhow!("重采样失败：{e}"))?;
            out.extend_from_slice(&res[0]);
        }
        Ok(())
    }

    /// 把缓冲里剩下的不足一块的样本补零后处理掉。
    pub fn finish(&mut self, out: &mut Vec<f32>) -> Result<()> {
        if self.inner.is_some() && !self.pending.is_empty() {
            let mut block = std::mem::take(&mut self.pending);
            block.resize(self.chunk, 0.0);
            let inner = self.inner.as_mut().unwrap();
            let res = inner
                .process(&[block], None)
                .map_err(|e| anyhow!("重采样收尾失败：{e}"))?;
            out.extend_from_slice(&res[0]);
        }
        Ok(())
    }
}

/// 录一段固定时长，返回（设备采样率，单声道 f32）。
pub fn record_seconds(seconds: f32) -> Result<(u32, Vec<f32>)> {
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

    let host = cpal::default_host();
    let device = host
        .default_input_device()
        .ok_or_else(|| anyhow!("没有可用的输入设备"))?;
    let supported = device.default_input_config().context("读默认输入配置失败")?;
    let rate = supported.sample_rate().0;
    let channels = supported.channels() as usize;
    let format = supported.sample_format();
    let config: cpal::StreamConfig = supported.into();

    let buf: Arc<Mutex<Vec<f32>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&buf);
    let err_fn = |e| eprintln!("[audio] 采集流错误：{e}");

    let stream = match format {
        cpal::SampleFormat::F32 => device.build_input_stream(
            &config,
            move |data: &[f32], _: &cpal::InputCallbackInfo| {
                let mono = to_mono(data, channels);
                sink.lock().unwrap().extend_from_slice(&mono);
            },
            err_fn,
            None,
        ),
        cpal::SampleFormat::I16 => device.build_input_stream(
            &config,
            move |data: &[i16], _: &cpal::InputCallbackInfo| {
                let f: Vec<f32> = data.iter().map(|s| *s as f32 / 32768.0).collect();
                let mono = to_mono(&f, channels);
                sink.lock().unwrap().extend_from_slice(&mono);
            },
            err_fn,
            None,
        ),
        other => return Err(anyhow!("不支持的采样格式：{other:?}")),
    }?;

    stream.play()?;
    std::thread::sleep(std::time::Duration::from_secs_f32(seconds));
    drop(stream);

    let data = Arc::try_unwrap(buf)
        .map_err(|_| anyhow!("采集缓冲仍被占用"))?
        .into_inner()
        .unwrap();
    Ok((rate, data))
}

/// 录到「最长 `max_seconds`」或「说完后静音 `silence_ms`」为止。
///
/// 只在已经出现过语音之后才计静音，否则一开口前就被自己的静音判停。
pub fn record_until(max_seconds: f32, silence_ms: u32) -> Result<(u32, Vec<f32>)> {
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

    let host = cpal::default_host();
    let device = host
        .default_input_device()
        .ok_or_else(|| anyhow!("没有可用的输入设备"))?;
    let supported = device.default_input_config().context("读默认输入配置失败")?;
    let rate = supported.sample_rate().0;
    let channels = supported.channels() as usize;
    let format = supported.sample_format();
    let config: cpal::StreamConfig = supported.into();

    let buf: Arc<Mutex<Vec<f32>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&buf);
    let err_fn = |e| eprintln!("[audio] 采集流错误：{e}");
    let stream = match format {
        cpal::SampleFormat::F32 => device.build_input_stream(
            &config,
            move |data: &[f32], _: &cpal::InputCallbackInfo| {
                let mono = to_mono(data, channels);
                sink.lock().unwrap().extend_from_slice(&mono);
            },
            err_fn,
            None,
        ),
        cpal::SampleFormat::I16 => device.build_input_stream(
            &config,
            move |data: &[i16], _: &cpal::InputCallbackInfo| {
                let f: Vec<f32> = data.iter().map(|s| *s as f32 / 32768.0).collect();
                let mono = to_mono(&f, channels);
                sink.lock().unwrap().extend_from_slice(&mono);
            },
            err_fn,
            None,
        ),
        other => return Err(anyhow!("不支持的采样格式：{other:?}")),
    }?;

    stream.play()?;
    let started = std::time::Instant::now();
    loop {
        std::thread::sleep(std::time::Duration::from_millis(50));
        if started.elapsed().as_secs_f32() >= max_seconds {
            break;
        }
        if silence_ms > 0 {
            let snap = buf.lock().unwrap().clone();
            let total_ms = (snap.len() as f32 / rate as f32 * 1000.0) as u32;
            let quiet = crate::vad::trailing_silence_ms(&snap, rate);
            // 出现过语音（总长 > 静音尾巴）且尾巴够长 → 收工
            if quiet >= silence_ms && total_ms > quiet {
                break;
            }
        }
    }
    drop(stream);

    let data = Arc::try_unwrap(buf)
        .map_err(|_| anyhow!("采集缓冲仍被占用"))?
        .into_inner()
        .unwrap();
    Ok((rate, data))
}

/// 写 16 kHz 单声道 16 bit PCM WAV。
pub fn write_wav(path: &std::path::Path, samples: &[f32]) -> Result<()> {
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: TARGET_RATE,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).ok();
    }
    let mut w = hound::WavWriter::create(path, spec)
        .with_context(|| format!("建 WAV 失败：{}", path.display()))?;
    for s in samples {
        let v = (s.clamp(-1.0, 1.0) * 32767.0).round() as i16;
        w.write_sample(v)?;
    }
    w.finalize()?;
    Ok(())
}

/// 读 WAV（只接受单声道或自动下混），返回（采样率，单声道 f32）。
pub fn read_wav(path: &std::path::Path) -> Result<(u32, Vec<f32>)> {
    let mut r = hound::WavReader::open(path)
        .with_context(|| format!("打开 WAV 失败：{}", path.display()))?;
    let spec = r.spec();
    let channels = spec.channels as usize;
    let samples: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Int => {
            let max = (1i32 << (spec.bits_per_sample - 1)) as f32;
            r.samples::<i32>()
                .map(|s| s.map(|v| v as f32 / max))
                .collect::<Result<Vec<_>, _>>()?
        }
        hound::SampleFormat::Float => r.samples::<f32>().collect::<Result<Vec<_>, _>>()?,
    };
    Ok((spec.sample_rate, to_mono(&samples, channels)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mono_mix_averages_channels() {
        let v = [1.0, 0.0, 0.5, 0.5];
        assert_eq!(to_mono(&v, 2), vec![0.5, 0.5]);
    }

    #[test]
    fn same_rate_is_passthrough() {
        let mut d = Downsampler::new(TARGET_RATE).unwrap();
        let mut out = Vec::new();
        d.push(&[0.1, 0.2, 0.3], &mut out).unwrap();
        assert_eq!(out, vec![0.1, 0.2, 0.3]);
    }

    #[test]
    fn resamples_48k_to_16k_length() {
        let mut d = Downsampler::new(48_000).unwrap();
        let input: Vec<f32> = (0..48_000)
            .map(|i| (2.0 * std::f32::consts::PI * 440.0 * i as f32 / 48_000.0).sin() * 0.5)
            .collect();
        let mut out = Vec::new();
        d.push(&input, &mut out).unwrap();
        d.finish(&mut out).unwrap();
        let secs = out.len() as f32 / TARGET_RATE as f32;
        assert!((secs - 1.0).abs() < 0.05, "out={} samples", out.len());
        // 幅度保持
        let peak = out.iter().fold(0.0f32, |m, s| m.max(s.abs()));
        assert!((peak - 0.5).abs() < 0.08, "peak={peak}");
    }

    #[test]
    fn wav_roundtrip() {
        let dir = std::env::temp_dir().join(format!("dictate-wav-{}", std::process::id()));
        let p = dir.join("t.wav");
        let samples: Vec<f32> = (0..1600).map(|i| (i as f32 / 1600.0) * 0.5).collect();
        write_wav(&p, &samples).unwrap();
        let (rate, back) = read_wav(&p).unwrap();
        assert_eq!(rate, TARGET_RATE);
        assert_eq!(back.len(), samples.len());
        std::fs::remove_dir_all(&dir).ok();
    }
}
