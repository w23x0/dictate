# dictate

把「按住热键说话」变成文字：通义千问 ASR + 本机托盘 + 终端界面。
从 LazyTyper 里只留下一条链路——**录音 → 转写 → 交付**，其余（润色、翻译、笔记、词汇表、本地模型、账号、遥测）全部没有。

```
按住 ctrl+space 说话 → 松开 → 文字进剪贴板并自动粘贴到当前窗口，同时记入历史
```

## 安装（一行）

```bash
curl -fsSL https://raw.githubusercontent.com/w23x0/dictate/main/install.sh | bash
```

它会：装二进制到 `~/.local/bin/dictate`、写应用入口（能被应用搜索找到）、设置开机自启、后台起托盘。

从源码装：

```bash
git clone https://github.com/w23x0/dictate && cd dictate && ./install.sh --local
```

卸载：`./uninstall.sh`（加 `--purge` 连配置和历史一起删）。

> 为什么不是 `apt`：一个自用的小工具没必要进发行版仓库（要打包、签名、审核、跟同步）。GitHub Release 出 `dictate-<target>.tar.gz`，脚本下载解压即可，升级也就是再跑一遍。

## 日常使用

```bash
dictate              # 托盘常驻（开机自启跑的就是这个）：热键说话，托盘菜单手动录音/看历史/设置
dictate history      # 终端里看历史（实时刷新，跟着守护进程走）
dictate settings     # 改配置（模型、地址、key、热键、交付方式）——保存后托盘即时生效，不用重启
```

第一次用必须先配置（没有 key 什么都转不出来）：

```bash
dictate settings     # 模型/地址用列表选，key 直接填，改完自动保存
dictate doctor       # 不录音，验 key / 区域 / 模型 / 粘贴能力 / 热键设备权限
```

## 交付方式（`paste`）

| 值 | 行为 |
|---|---|
| `auto`（默认） | 存原剪贴板 → 写入文本 → 模拟粘贴键（`wtype`/`ydotool`/`xdotool` 中有哪个用哪个）→ **粘贴成功后** 400 ms 还原剪贴板 |
| `clipboard` | 只写剪贴板，不按键（文本会留在剪贴板里等你手动粘贴） |
| `off` | 只进历史 |

没粘上（比如 Wayland 下没有 `wtype`/`ydotool`）时同样保留新文本，状态里会说明，按 `Ctrl+V` 即可。
`dictate doctor` 会列出当前机器上有哪些工具；`dictate inject-test "文字"` 可以单独验证这条链路。

## 热键

| `hotkey_mode` | 行为 |
|---|---|
| `hold`（默认） | 按住说话，松开提交；按一下就松（<200 ms）判为误触，直接丢弃 |
| `toggle` | 按一下开始，再按一下停止 |

默认 `ctrl+space`，改：`dictate settings` 或 `/hotkey alt+shift+d`。改完即时生效——托盘进程盯着配置文件，热键监听线程每次按键都重读定义（Linux）；Windows 端钩子装的时候定死，要重启进程。

托盘菜单里还有「录一段 / 停止录音」，没进 `input` 组、热键用不了时的兜底。

- **Linux**：直读 `/dev/input/event*`（X11 / Wayland 通用），需要 `input` 组
- **Windows**：`WH_KEYBOARD_LL` 低级钩子（未在真机验证，仅 `cargo check` 通过）

## 终端界面

扁平布局，底部一行输入（和 grok-cli 一样，不背快捷键）：直接打字实时过滤历史；
`/` 弹出 opencode 式命令面板——模糊过滤、命中字符高亮、`↑↓` 选、`⏎` 执行、`Tab` 补全。

```
◆ dictate  v0.1.0  ctrl+space · qwen3-asr-flash                       ● 就绪   12 条

 今天 ────────────────────────────────────────────────────────────
▌ 22:37  ❯ 今天下午三点跟产品过一下这个需求，顺便把接口文档同步给后端， 4.2s → 0.9s
           会议纪要发给所有相关同学。
 22:35  ❯ 帮我把这段代码里的 unwrap 全部换成 anyhow 的 context。   0.9s → 0.3s
 昨天 ────────────────────────────────────────────────────────────
 18:02  ❯ 长文本自动折行、按天分组；右边是 时长 → 转写耗时        2.1s → 0.7s

  ╭─ 命令 ──────────────────────────────────────╮
  │ /settings  打开设置                         │
  │ /search [文字]  按内容过滤历史               │
  ╰─────────────────────────────────────────────╯
❯ /set▏                          ← 输入法候选框跟着终端光标走
  直接打字 = 实时过滤 · ⏎ 复制选中 · 空格录音 · F1 帮助
```

| 命令 | 作用 |
|---|---|
| `/settings` | 设置页（`Esc` 返回，改完自动保存） |
| `/model` `/base` | 敲个空格后面板里选；直接带参数 = 原样设置 |
| `/key sk-…` · `/hotkey ctrl+space` · `/mode hold\|toggle` | 改了立即写配置 |
| `/record` | 手动开关录音 |
| `/search 文字` | 过滤历史（直接打字也实时过滤） |
| `/clear` · `/daemon stop` | 清空历史 / 退守护进程（回车确认） |
| `/help` · `/quit` | 帮助 / 退出视图 |

键：`↑↓`/`j k` 选择 · `g` 最新 `G` 最旧 · `Ctrl+D/U` 翻半页 · `m` 标记多条 ·
`⏎` 复制选中（有标记则整批复制，OSC 52）· `Del` 删除 · `空格` 手动录音 ·
`F1` 帮助 · `F2` 设置 · `F10`/`Esc`/`Ctrl+C` 退出视图 ·
`Ctrl+R` 刷新 · `Ctrl+L` 清空历史 · `Ctrl+Q` 退守护进程。

顶栏第二行常驻显示守护进程最近一次结果（完成/失败），命令反馈以 toast 形式显示几秒后消失。

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
cargo test            # 69 条：AGC/VAD/重采样/WAV、三种报文形状真 socket 端到端、命令/参数面板、标记复制、离屏渲染快照、历史 CRUD
cargo clippy --all-targets
cargo fmt
```

发布：打 tag 后 `.github/workflows/release.yml` 会编译并上传 `dictate-<target>.tar.gz`。
