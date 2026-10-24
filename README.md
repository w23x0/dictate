# dictate

按住热键说话，文字进当前窗口。通义千问 ASR + 托盘，只做这一条链路。

```
按住 ctrl+space → 说 → 松开 → 文字粘贴到光标处，同时记入历史
```

## 安装

```bash
curl -fsSL https://raw.githubusercontent.com/w23x0/dictate/main/install.sh | bash
```

装到 `~/.local/bin/dictate`，带应用入口、开机自启、托盘常驻。
源码安装：`./install.sh --local` · 卸载：`./uninstall.sh [--purge]`

## 用法

```bash
dictate            # 托盘常驻：热键可用，菜单里看历史/设置
dictate settings   # 模型、服务地址、key、热键、交付方式（改完自动保存）
dictate doctor     # 不录音，验 key / 区域 / 模型 / 粘贴能力 / 热键设备
dictate history    # 终端里看历史（实时）
```

首次使用先 `dictate settings` 填 API key，否则什么也转不出来。

## 模型

| 模型 | 端点 |
|---|---|
| `qwen3-asr-flash` | OpenAI 兼容 `compatible-mode/v1/chat/completions` |
| `qwen-audio-3.1-asr-flash` | DashScope 同步 `multimodal-generation/generation` |
| `qwen-audio-3.1-asr-flash-message` 等 | WebSocket `api-ws/v1/inference` |

`*-filetrans`（异步任务接口）未实现。

## 其它

- 键位、命令、配置项、目录结构：[README.full.md](README.full.md)
- Linux 热键需要 `input` 组：`sudo usermod -aG input $USER`
- 许可证：MIT
