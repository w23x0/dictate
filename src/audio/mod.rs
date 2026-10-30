//! 采集 → 单声道 → 16 kHz → WAV。
//!
//! 采集用 cpal（默认输入设备，设备原生采样率与声道数），
//! 重采样用 rubato 的 `FftFixedIn`（任意率互转，流式）。

pub mod agc;
pub mod vad;

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
            return Ok(Self {
                inner: None,
                pending: Vec::new(),
                chunk: 0,
            });
        }
        let chunk = 1024;
        let inner = FftFixedIn::<f32>::new(in_rate as usize, TARGET_RATE as usize, chunk, 2, 1)
            .map_err(|e| anyhow!("重采样器构造失败（{in_rate} → {TARGET_RATE}）：{e}"))?;
        Ok(Self {
            inner: Some(inner),
            pending: Vec::with_capacity(chunk * 2),
            chunk,
        })
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
        if let Some(inner) = self.inner.as_mut() {
            if self.pending.is_empty() {
                return Ok(());
            }
            let mut block = std::mem::take(&mut self.pending);
            block.resize(self.chunk, 0.0);
            let res = inner
                .process(&[block], None)
                .map_err(|e| anyhow!("重采样收尾失败：{e}"))?;
            out.extend_from_slice(&res[0]);
        }
        Ok(())
    }
}

/// 在闭包执行期间把 fd 2 指向 /dev/null——绕过 ALSA→JACK 插件往 stderr 喷的噪声。
#[cfg(unix)]
pub fn quiet<R>(f: impl FnOnce() -> R) -> R {
    use std::os::unix::io::AsRawFd;
    let devnull = std::fs::OpenOptions::new()
        .write(true)
        .open("/dev/null")
        .ok();
    let saved = unsafe { libc::dup(2) };
    if let Some(d) = &devnull {
        unsafe { libc::dup2(d.as_raw_fd(), 2) };
    }
    let r = f();
    if saved >= 0 {
        unsafe { libc::dup2(saved, 2) };
        unsafe { libc::close(saved) };
    }
    r
}

#[cfg(not(unix))]
pub fn quiet<R>(f: impl FnOnce() -> R) -> R {
    f()
}

/// 按名字找输入设备；名字为空就用系统默认。
pub fn open_input(name: Option<&str>) -> anyhow::Result<cpal::Device> {
    use cpal::traits::{DeviceTrait, HostTrait};
    let host = cpal::default_host();
    let Some(want) = name.filter(|n| !n.trim().is_empty()) else {
        return host
            .default_input_device()
            .ok_or_else(|| anyhow!("没有可用的输入设备"));
    };
    host.input_devices()?
        .find(|d| d.name().map(|n| n == want).unwrap_or(false))
        .ok_or_else(|| anyhow!("找不到输入设备「{want}」（用 `dictate devices` 看列表）"))
}

/// 列出所有输入设备名，默认设备加 `*`。
pub fn input_names() -> Vec<String> {
    quiet(input_names_inner)
}

fn input_names_inner() -> Vec<String> {
    use cpal::traits::{DeviceTrait, HostTrait};
    let host = cpal::default_host();
    let default = host
        .default_input_device()
        .and_then(|d| d.name().ok())
        .unwrap_or_default();
    host.input_devices()
        .map(|it| {
            it.filter_map(|d| d.name().ok())
                .map(|n| {
                    if n == default {
                        format!("* {n}")
                    } else {
                        format!("  {n}")
                    }
                })
                .collect()
        })
        .unwrap_or_default()
}

pub fn default_input_name() -> Option<String> {
    use cpal::traits::{DeviceTrait, HostTrait};
    quiet(|| {
        cpal::default_host()
            .default_input_device()
            .and_then(|d| d.name().ok())
    })
}

/// 录一段固定时长，返回（设备采样率，单声道 f32）。
pub fn record_seconds(seconds: f32, device_name: Option<&str>) -> Result<(u32, Vec<f32>)> {
    use cpal::traits::{DeviceTrait, StreamTrait};

    let device = quiet(|| open_input(device_name))?;
    let supported = quiet(|| device.default_input_config()).context("读默认输入配置失败")?;
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
pub fn record_until(
    max_seconds: f32,
    silence_ms: u32,
    device_name: Option<&str>,
) -> Result<(u32, Vec<f32>)> {
    use cpal::traits::{DeviceTrait, StreamTrait};

    let device = quiet(|| open_input(device_name))?;
    let supported = quiet(|| device.default_input_config()).context("读默认输入配置失败")?;
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
    // 只复制尾部窗口来判静音：判定「尾巴是否安静够久」不需要整段录音，
    // 也避免拿着采集锁做 O(n) 扫描阻塞音频回调。窗口多留一帧，保证
    // 尾部的语音不会因为帧边界被漏掉。
    let tail_ms = silence_ms.saturating_add(crate::audio::vad::FRAME_MS);
    let tail_len = (rate as u64 * tail_ms as u64 / 1000) as usize;
    let mut vad_tail: Vec<f32> = Vec::new();
    loop {
        std::thread::sleep(std::time::Duration::from_millis(50));
        if started.elapsed().as_secs_f32() >= max_seconds {
            break;
        }
        if silence_ms > 0 {
            let total_ms;
            {
                let snap = buf.lock().unwrap();
                total_ms = (snap.len() as f32 / rate as f32 * 1000.0) as u32;
                let start = snap.len().saturating_sub(tail_len);
                vad_tail.clear();
                vad_tail.extend_from_slice(&snap[start..]);
            }
            let quiet = crate::audio::vad::trailing_silence_ms(&vad_tail, rate);
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

/// 在内存里生成 16 kHz 单声道 16 bit PCM WAV 字节（不落盘）。
/// 头是 44 字节标准 RIFF/PCM，手写比让 hound 包一层 Cursor 干净（它拿不回 writer）；
/// 正确性由测试用 hound 的 reader 读回来把关。
pub fn wav_bytes(samples: &[f32]) -> Vec<u8> {
    let data_len = (samples.len() * 2) as u32;
    let mut v = Vec::with_capacity(44 + data_len as usize);
    v.extend_from_slice(b"RIFF");
    v.extend_from_slice(&(36 + data_len).to_le_bytes());
    v.extend_from_slice(b"WAVE");
    v.extend_from_slice(b"fmt ");
    v.extend_from_slice(&16u32.to_le_bytes()); // fmt 段长度
    v.extend_from_slice(&1u16.to_le_bytes()); // PCM
    v.extend_from_slice(&1u16.to_le_bytes()); // 单声道
    v.extend_from_slice(&TARGET_RATE.to_le_bytes());
    v.extend_from_slice(&(TARGET_RATE * 2).to_le_bytes()); // 每秒字节数
    v.extend_from_slice(&2u16.to_le_bytes()); // 块对齐
    v.extend_from_slice(&16u16.to_le_bytes()); // 位深
    v.extend_from_slice(b"data");
    v.extend_from_slice(&data_len.to_le_bytes());
    for s in samples {
        let q = (s.clamp(-1.0, 1.0) * 32767.0).round() as i16;
        v.extend_from_slice(&q.to_le_bytes());
    }
    v
}

/// 写 16 kHz 单声道 16 bit PCM WAV。
pub fn write_wav(path: &std::path::Path, samples: &[f32]) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).ok();
    }
    let bytes = wav_bytes(samples);
    std::fs::write(path, bytes).with_context(|| format!("写 WAV 失败：{}", path.display()))
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

// ───────────────────────── 热键守护用的可停采集 ─────────────────────────

/// 流式输出：采集线程把单声道样本（设备原生采样率）实时吐给转写会话。
pub enum CaptureOut {
    /// 一段录音开始，带设备采样率（会话据此建重采样器）
    Begin { rate: u32 },
    /// 单声道 f32 块
    Chunk(Vec<f32>),
    /// 本段录音结束（stop 之后由采集线程发出）
    Finish,
}

/// 采集线程的命令。`Start` 带最长时长，到点由采集线程自己停；
/// `fwd` 有值时同时把样本实时转发出去（真流式用）。
pub enum CaptureCmd {
    Start {
        max_seconds: f32,
        fwd: Option<tokio::sync::mpsc::UnboundedSender<CaptureOut>>,
    },
    Stop,
    Quit,
}

/// 一条采集会话的句柄：`start` 之后 `stop` 拿样本。
pub struct Capture {
    cmd: std::sync::mpsc::Sender<CaptureCmd>,
    result: std::sync::mpsc::Receiver<(u32, Vec<f32>)>,
}

impl Capture {
    pub fn start(&self, max_seconds: f32) -> anyhow::Result<()> {
        self.cmd
            .send(CaptureCmd::Start {
                max_seconds,
                fwd: None,
            })
            .map_err(|_| anyhow!("采集线程已退出"))?;
        Ok(())
    }

    /// 开录并实时转发样本（真流式）。
    pub fn start_stream(
        &self,
        max_seconds: f32,
        fwd: tokio::sync::mpsc::UnboundedSender<CaptureOut>,
    ) -> anyhow::Result<()> {
        self.cmd
            .send(CaptureCmd::Start {
                max_seconds,
                fwd: Some(fwd),
            })
            .map_err(|_| anyhow!("采集线程已退出"))?;
        Ok(())
    }

    pub fn stop(&self) -> anyhow::Result<(u32, Vec<f32>)> {
        self.cmd
            .send(CaptureCmd::Stop)
            .map_err(|_| anyhow!("采集线程已退出"))?;
        self.result
            .recv_timeout(std::time::Duration::from_secs(5))
            .map_err(|_| anyhow!("采集线程没有在 5 秒内交回样本"))
    }

    /// 非阻塞取一次样本：采集线程到 `max_seconds` 自停时样本会先落进结果通道，
    /// 守护进程在「流式先出结果」的竞态窗口里用它把样本及时取走，避免滞留到下一次录音。
    pub fn try_take(&self) -> Option<(u32, Vec<f32>)> {
        self.result.try_recv().ok()
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        let _ = self.cmd.send(CaptureCmd::Quit);
    }
}

/// 起一个常驻采集线程。cpal 的 `Stream` 不是 `Send`，所以流必须待在这条线程里。
pub fn spawn_capture(device_name: Option<String>) -> anyhow::Result<Capture> {
    let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<CaptureCmd>();
    let (res_tx, res_rx) = std::sync::mpsc::channel::<(u32, Vec<f32>)>();

    std::thread::spawn(move || {
        use cpal::traits::{DeviceTrait, StreamTrait};
        while let Ok(cmd) = cmd_rx.recv() {
            match cmd {
                CaptureCmd::Quit => return,
                CaptureCmd::Stop => {}
                CaptureCmd::Start { max_seconds, fwd } => {
                    let Ok(device) = quiet(|| open_input(device_name.as_deref())) else {
                        eprintln!("[audio] 打不开输入设备");
                        continue;
                    };
                    let Ok(supported) = quiet(|| device.default_input_config()) else {
                        eprintln!("[audio] 读默认输入配置失败");
                        continue;
                    };
                    let rate = supported.sample_rate().0;
                    let channels = supported.channels() as usize;
                    let format = supported.sample_format();
                    let config: cpal::StreamConfig = supported.into();
                    let buf: Arc<Mutex<Vec<f32>>> = Arc::new(Mutex::new(Vec::new()));
                    let sink = Arc::clone(&buf);
                    let err_fn = |e| eprintln!("[audio] 采集流错误：{e}");

                    // 先告诉流式会话采样率（之后回调里的块都在它后面进 channel）
                    if let Some(tx) = &fwd {
                        let _ = tx.send(CaptureOut::Begin { rate });
                    }

                    let stream = match format {
                        cpal::SampleFormat::F32 => {
                            let fwd = fwd.clone();
                            device.build_input_stream(
                                &config,
                                move |data: &[f32], _: &cpal::InputCallbackInfo| {
                                    let mono = to_mono(data, channels);
                                    sink.lock().unwrap().extend_from_slice(&mono);
                                    if let Some(tx) = &fwd {
                                        let _ = tx.send(CaptureOut::Chunk(mono));
                                    }
                                },
                                err_fn,
                                None,
                            )
                        }
                        cpal::SampleFormat::I16 => {
                            let fwd = fwd.clone();
                            device.build_input_stream(
                                &config,
                                move |data: &[i16], _: &cpal::InputCallbackInfo| {
                                    let f: Vec<f32> =
                                        data.iter().map(|s| *s as f32 / 32768.0).collect();
                                    let mono = to_mono(&f, channels);
                                    sink.lock().unwrap().extend_from_slice(&mono);
                                    if let Some(tx) = &fwd {
                                        let _ = tx.send(CaptureOut::Chunk(mono));
                                    }
                                },
                                err_fn,
                                None,
                            )
                        }
                        other => {
                            eprintln!("[audio] 不支持的采样格式：{other:?}");
                            continue;
                        }
                    };
                    let Ok(stream) = stream else {
                        eprintln!("[audio] 建采集流失败");
                        continue;
                    };
                    if stream.play().is_err() {
                        eprintln!("[audio] 启动采集失败");
                        continue;
                    }
                    let started = std::time::Instant::now();
                    loop {
                        std::thread::sleep(std::time::Duration::from_millis(15));
                        match cmd_rx.try_recv() {
                            Ok(CaptureCmd::Stop) => break,
                            Ok(CaptureCmd::Quit) => return,
                            Ok(CaptureCmd::Start { .. }) => {}
                            Err(std::sync::mpsc::TryRecvError::Disconnected) => return,
                            Err(_) => {}
                        }
                        if started.elapsed().as_secs_f32() >= max_seconds {
                            break;
                        }
                    }
                    drop(stream);
                    let data = Arc::try_unwrap(buf)
                        .map(|m| m.into_inner().unwrap())
                        .unwrap_or_default();
                    if let Some(tx) = &fwd {
                        let _ = tx.send(CaptureOut::Finish);
                    }
                    if res_tx.send((rate, data)).is_err() {
                        return;
                    }
                }
            }
        }
    });

    Ok(Capture {
        cmd: cmd_tx,
        result: res_rx,
    })
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

    #[test]
    fn wav_bytes_is_a_valid_wav() {
        let samples: Vec<f32> = vec![0.0, 0.25, -0.25, 0.5];
        let bytes = wav_bytes(&samples);
        assert_eq!(&bytes[..4], b"RIFF");
        let dir = std::env::temp_dir().join(format!("dictate-wavb-{}", std::process::id()));
        let p = dir.join("t.wav");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&p, &bytes).unwrap();
        let (rate, back) = read_wav(&p).unwrap();
        assert_eq!(rate, TARGET_RATE);
        assert_eq!(back.len(), samples.len());
        std::fs::remove_dir_all(&dir).ok();
    }
}
