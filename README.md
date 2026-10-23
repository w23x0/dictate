# dictate

把「按住热键说话」变成文字：通义千问 ASR + 本机托盘 + 终端界面。
从 LazyTyper 里只留下一条链路——**录音 → 转写 → 交付**，其余（润色、翻译、笔记、词汇表、本地模型、账号、遥测）全部没有。

```
按住 ctrl+space 说话 → 松开 → 文字进剪贴板并自动粘贴到当前窗口，同时记入历史
```

## 安装（一行）

```bash
curl -fsSL https://raw.githubusercontent.com/<owner>/dictate/main/install.sh | bash
```

它会：装二进制到 `~/.local/bin/dictate`、写应用入口（能被应用搜索找到）、设置开机自启、后台起托盘。

从源码装：

```bash
git clone https://github.com/<owner>/dictate && cd dictate && ./install.sh --local
```

卸载：`./uninstall.sh`（加 `--purge` 连配置和历史一起删）。

> 为什么不是 `apt`：一个自用的小工具没必要进发行版仓库（要打包、签名、审核、跟同步）。GitHub Release 出 `dictate-<target>.tar.gz`，脚本下载解压即可，升级也就是再跑一遍。

## 日常使用

```bash
dictate              # 托盘常驻（开机自启跑的就是这个）：热键说话，托盘菜单看历史/设置
dictate history      # 终端里看历史（实时刷新，跟着守护进程走）
dictate settings     # 改配置（模型、地址、key、热键、交付方式）
```

第一次用必须先配置（没有 key 什么都转不出来）：

```bash
dictate settings     # 模型/地址用列表选，key 直接填，改完自动保存
dictate doctor       # 不录音，验 key / 区域 / 模型 / 粘贴能力
```

## 交付方式（`paste`）

| 值 | 行为 |
|---|---|
| `auto`（默认） | 存原剪贴板 → 写入文本 → 模拟粘贴键（`wtype`/`ydotool`/`xdotool` 里有哪个用哪个）→ 400 ms 后还原剪贴板 |
| `clipboard` | 只写剪贴板，不按键 |
| `off` | 只进历史 |

Wayland（GNOME）下没有 `wtype`/`ydotool` 时只能做到写剪贴板，程序会在状态里说明，按 `Ctrl+V` 即可。
`dictate doctor` 会列出当前机器上有哪些工具。

## 热键

| `hotkey_mode` | 行为 |
|---|---|
| `hold`（默认） | 按住说话，松开提交；按一下就松（<200 ms）判为误触，直接丢弃 |
| `toggle` | 按一下开始，再按一下停止 |

默认 `ctrl+space`，改：`dictate settings` 或 `/hotkey alt+shift+d`。

- **Linux**：直读 `/dev/input/event*`（X11 / Wayland 通用），需要 `input` 组
- **Windows**：`WH_KEYBOARD_LL` 低级钩子（未在真机验证，仅 `cargo check` 通过）

## 终端界面

扁平布局，底部一行命令（和 grok-cli 一样，不背快捷键）：

```
◆ dictate  v0.1.0  ctrl+space · qwen3-asr-flash          ● 就绪   12 条

▌ 22:37  ❯ 今天下午三点跟产品过一下这个需求，顺便把接口文档同步给后端。  1.8s
  22:35  ❯ 帮我把这段代码里的 unwrap 全部换成 anyhow 的 context。        0.9s

❯ /                                   ← 真的输入框，光标停在这（输入法候选框跟着走）
  /settings 打开设置   /model 切换模型   /record 手动录音   /help 帮助
```

| 命令 | 作用 |
|---|---|
| `/settings` | 设置页（`Esc` 返回，改完自动保存） |
| `/model` `/base` | 不带参数 = 列表选；带参数 = 直接设 |
| `/key sk-…` · `/hotkey ctrl+space` · `/mode hold\|toggle` | 改了立即写配置 |
| `/record` | 手动开关录音 |
| `/search 文字` | 过滤历史（直接打字也实时过滤） |
| `/clear` · `/daemon stop` | 清空历史 / 退守护进程（回车确认） |
| `/help` · `/quit` | 帮助 / 退出视图 |

键：`↑↓` 选择、`⏎` 输入为空时复制选中、`Del` 删除、`Esc` 清空输入/退出。

## 支持的模型

| 模型 | 报文形状 | 端点 |
|---|---|---|
| `qwen3-asr-flash` | OpenAI 兼容 | `{base}/compatible-mode/v1/chat/completions` |
| `qwen-audio-3.1-asr-flash` · `qwen-audio-3.0-asr-flash` | DashScope 同步 | `{base}/api/v1/services/aigc/multimodal-generation/generation` |
| `qwen-audio-3.1-asr-flash-message` · `-streaming` · `qwen3-asr-flash-realtime` | WebSocket | `wss://{host}/api-ws/v1/inference` |
| `*-filetrans` | — | 只支持异步任务接口，**没实现**（选到会直接报错说明） |

形状由模型名自动决定；`api_style` 可强制。

## 配置

`~/.config/dictate/config.toml`（权限 600，含 key）。key 也认环境变量 `DASHSCOPE_API_KEY`（优先）。
模板见 [`config.example.toml`](config.example.toml)。

## 目录结构

```
src/
├── main.rs          CLI 入口
├── app/             常驻进程：daemon（热键→录音→转写→交付）、tray、desktop、inject（粘贴）
├── core/            平台无关：config、history、pipeline（WAV 加工 + 转写 + 落库）
├── audio/           cpal 采集、rubato 重采样、agc（自动增益）、vad（静音门）
├── asr/             三种报文形状 + 本地假服务端端到端测试
└── ui/              终端界面：tui（列表+命令）、settings_ui、theme、icon
```

数据都在 `~/.local/share/dictate/`：`history.jsonl`、`status.json`、`control`、`daemon.pid`。

## 开发

```bash
cargo test            # 52 条：AGC/VAD/重采样/WAV、三种报文形状真 socket 端到端、命令输入、历史 CRUD
cargo clippy --all-targets
cargo fmt
```

发布：打 tag 后 `.github/workflows/release.yml` 会编译并上传 `dictate-<target>.tar.gz`。
