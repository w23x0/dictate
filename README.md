# dictate

通义千问 ASR 听写器。一条链路：**热键 → 采集 → 静音门 → AGC → 云端转写 → 历史**。
没有润色、没有翻译、没有笔记、没有词汇表、没有账号。

## 现状

| 模块 | 状态 |
|---|---|
| 采集（cpal，任意率/声道）→ 16 kHz 单声道 | ✅ 真机验证（44.1 kHz 设备） |
| 重采样（rubato `FftFixedIn`，流式） | ✅ 单测 |
| 静音门（20 ms 帧、裁首尾、尾部静音计时） | ✅ 单测 + `record` 自动结束 |
| AGC（−20 dBFS 目标、10 ms 攻击 / 200 ms 释放、+30 dB 上限、tanh 软限幅） | ✅ 单测（−40/−10 dBFS 双端收敛） |
| Qwen ASR（OpenAI 兼容 `chat/completions` + base64） | ✅ 真跑（mock 服务端收到正确报文） |
| 历史（`history.jsonl`，容错读） | ✅ 单测 + TUI 实测 |
| 终端历史窗口（ratatui，复制/搜索/删除/清空） | ✅ pty 实测（OSC52 复制内容逐字节核对） |
| 全局热键 + 托盘 | ⬜ 未做（下一步） |
| 实时 WS 转写 | ⬜ 未做 |

## 用法

```bash
cargo run --release -- probe --seconds 3 --out /tmp/a.wav   # 验采集：打印电平、可落盘
cargo run --release -- record                               # 录到静音自动结束，直接转写
cargo run --release -- transcribe /tmp/a.wav                # 转写现成 WAV
cargo run --release -- history                              # 终端历史窗口
```

历史窗口按键：`↑↓/jk` 选择 · `Enter` 复制（OSC 52，SSH 里也有效）· `/` 搜索 · `d` 删除 ·
`c` 清空 · `r` 刷新 · `q` 退出。

## 配置

`~/.config/dictate/config.toml`（模板见 `config.example.toml`）。key 优先级：
`DASHSCOPE_API_KEY` > 配置里的 `api_key`。

```toml
base_url = "https://dashscope.aliyuncs.com"   # 新加坡：dashscope-intl.aliyuncs.com
model = "qwen3-asr-flash"
enable_itn = false
silence_stop_ms = 1500
max_seconds = 240
```

## 为什么是 `chat/completions` 而不是 `/v1/audio/transcriptions`

Qwen3-ASR-Flash 的 OpenAI 兼容模式走的是 chat 接口：

```json
{
  "model": "qwen3-asr-flash",
  "messages": [{"role":"user","content":[
    {"type":"input_audio","input_audio":{"data":"data:audio/wav;base64,…"}}]}],
  "asr_options": {"enable_itn": false, "language": "zh"},
  "stream": false
}
```

结果在 `choices[0].message.content`。服务端硬限：**10 MB / 5 分钟**（`max_seconds` 默认 240 s 留余量）。

## 设计取舍

- **录音先落内存，失败不丢**：转写失败会写一条 `status: "error: …"` 的历史（含错误原因），下次可对照重录。
- **AGC 不抬静音**：低于 −55 dBFS 的块不参与增益计算，否则静音段会被放大成嘶声。
- **TUI 只认裸字母**：带修饰键的按键（Ctrl+D 是终端 EOF）不会触发删除——这是实测踩到的坑。
- **不自动粘贴**：转写结果只进历史，由你自己复制（`Enter` → OSC 52）。

## 测试

```bash
cargo test        # 21 条：AGC 收敛/限幅、VAD 裁切、重采样、WAV 往返、历史 CRUD、报文形状、OSC52
```
