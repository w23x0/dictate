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
| 全局热键（Linux evdev / Windows LL hook） | ✅ Linux 真机监听；Windows 只做了 `cargo check` |
| 托盘常驻 / 无终端运行 | ✅ Linux 真机（GNOME + appindicator 扩展），托盘项已注册 |
| 应用搜索（.desktop + 图标） | ✅ 首次运行自动写 `~/.local/share/applications/dictate.desktop` |
| 终端内配置界面 | ✅ `dictate settings` |
| 实时 WS 转写 | ⬜ 未做 |

## 用法

```bash
dictate                 # 托盘常驻：无终端也能用（关掉终端不影响）
dictate settings        # 终端里改配置（不用手写 TOML）
dictate run             # 终端版：热键 + 历史列表常驻
cargo run --release -- doctor                               # 不发录音，验 key / 区域 / 模型
cargo run --release -- probe --seconds 3 --out /tmp/a.wav   # 验采集：打印电平、可落盘
cargo run --release -- record                               # 录到静音自动结束，直接转写
cargo run --release -- transcribe /tmp/a.wav                # 转写现成 WAV
cargo run --release -- history                              # 终端历史窗口
```

历史窗口按键：`↑↓/jk` 选择 · `Enter` 复制（OSC 52，SSH 里也有效）· `/` 搜索 · `d` 删除 ·
`c` 清空 · `r` 刷新 · `q` 退出。

## 热键

`dictate run` 常驻：按下热键开始录音，松开即提交，结果进历史窗。

- **Linux**：读 `/dev/input/event*`（X11 / Wayland 通用）。需要 `input` 组：
  `sudo usermod -aG input $USER` 然后重新登录。启动时会打印监听到的设备名。
- **Windows**：`WH_KEYBOARD_LL` 低级钩子，不需要管理员权限（`cargo check --target x86_64-pc-windows-gnu` 通过，未在真机跑过）。
- 默认 `ctrl+space`，可在配置里改：`hotkey = "alt+shift+d"` / `f9`（部分发行版把 `ctrl+space` 绑给了输入法切换，冲突就换一个）。

## 支持的模型

三种报文形状，按模型名自动选，也可用 `api_style` 强制（`openai` / `dashscope` / `ws`）：

| 模型 | 形状 | 端点 | 备注 |
|---|---|---|---|
| `qwen3-asr-flash` | OpenAI 兼容 | `{base}/compatible-mode/v1/chat/completions` | 10 MB / 5 分钟 |
| `qwen3-asr-flash`（`api_style="dashscope"`） | DashScope 同步 | `{base}/api/v1/services/aigc/multimodal-generation/generation` | 内容字段是 `{"audio": …}` |
| `qwen-audio-3.1-asr-flash`、`qwen-audio-3.0-asr-flash` | DashScope 同步 | 同上 | 内容字段是 `{"type":"input_audio",…}`，带 `format`/`sample_rate`；2 GB / 5 分钟 |
| `qwen-audio-3.1-asr-flash-message`、`qwen-audio-3.1-asr-flash-streaming`、`qwen3-asr-flash-realtime` | WebSocket | `wss://{host}/api-ws/v1/inference` | `run-task` → 二进制音频 → `finish-task`；只取最终句 |
| `*-filetrans` | — | — | 只能走「上传 → 异步任务 → 轮询」，**没实现**，选到会直接报错 |

`base_url` 填到主机即可（`https://dashscope.aliyuncs.com`、`https://{WorkspaceId}.cn-beijing.maas.aliyuncs.com`），
带 `/compatible-mode/v1` 之类的尾巴会自动剥掉。

三种形状都有本地假服务端的端到端测试（真 socket），见 `src/asr/e2e.rs`。

## 界面

扁平布局（grok-cli 那种），没有边框盒子；底部是一个填充色面板，左上是品牌行：

```
◆ dictate  v0.1.0  ctrl+space · qwen3-asr-flash        ● 就绪   12 条

 20:31  ❯ 今天下午三点跟产品过一下这个需求，顺便把接口文档同步给后端。  1.8s
 20:28  ❯ 帮我把这段代码里的 unwrap 全部换成 anyhow 的 context。        0.9s

  ❯ 按住热键说话
                        ↑↓ 选择   ⏎ 复制   / 搜索   d 删除   c 清空   q 退出
```

配色跟随终端明暗（OSC 11 探测，可在设置里强制 dark/light）。

## 托盘 / 无终端

```bash
dictate            # 常驻，托盘图标出现，热键全局可用
```

- 托盘菜单：**打开历史…** / **设置…** / **退出**（前两个会在终端模拟器里开对应的 TUI）
- 能出现在应用搜索里：首次运行写 `~/.local/share/applications/dictate.desktop` + 图标
- 单实例：重复启动会直接提示「已经在跑了」并退出
- **GNOME 需要 AppIndicator 扩展**才有托盘图标（Ubuntu 自带，但默认可能没启用）：
  `gnome-extensions enable ubuntu-appindicators@ubuntu.com`
- 托盘在 Linux 用 libayatana-appindicator（会有 deprecation 警告，正常）；Windows 用 tray-icon

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
