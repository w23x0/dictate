//! 配置：单个 TOML，落在 `~/.config/dictate/config.toml`。
//!
//! 密钥优先级：环境变量 `DASHSCOPE_API_KEY` > 配置文件 `api_key`。
//! 配置文件里写了 key 时，本进程不打印它，也不回写。

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// OpenAI 兼容模式的 base_url（不带 `/compatible-mode/v1` 也行，见 `endpoint()`）。
    pub base_url: String,
    pub model: String,
    /// 空 = 自动识别；否则 zh / en / yue / ja …
    pub language: Option<String>,
    /// 逆文本规范化（数字、单位）。听写默认关：它会把字面改写。
    pub enable_itn: bool,
    pub api_key: Option<String>,
    /// AGC 目标电平（dBFS）。
    pub target_rms_dbfs: f32,
    /// 连续静音多久自动结束（毫秒），0 = 关闭。
    pub silence_stop_ms: u32,
    /// 单次录音上限（秒）。10 MB / 5 分钟是服务端硬限，这里留余量。
    pub max_seconds: f32,
    /// 请求超时（秒）。
    pub timeout_secs: u64,
    /// 全局热键：`ctrl+space` / `f9` / `alt+shift+d`。
    pub hotkey: String,
    /// 热键语义：`hold`（按住说话，默认）或 `toggle`（按一下开、再按一下停）。
    pub hotkey_mode: String,
    /// 强制报文形状：`openai` / `dashscope` / `ws`；留空则按模型名推断。
    pub api_style: Option<String>,
    /// 配色：`auto`（默认，问终端）/ `dark` / `light`。
    pub theme: Option<String>,
    /// 颜色开关：`auto`（默认，尊重 NO_COLOR）/ `always`（强制彩色）/ `never`。
    pub color: Option<String>,
    /// 输入设备名（留空 = 系统默认）。`dictate devices` 可以列出来。
    pub input_device: Option<String>,
    /// 把每次送到服务端的 WAV 留一份到 `~/.local/share/dictate/last.wav`。
    pub keep_audio: bool,
    /// 转写完成后怎么交付：`auto`（剪贴板 + 模拟粘贴）/ `clipboard` / `off`。
    pub paste: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            base_url: "https://dashscope.aliyuncs.com".into(),
            model: "qwen3-asr-flash".into(),
            language: None,
            enable_itn: false,
            api_key: None,
            target_rms_dbfs: -20.0,
            silence_stop_ms: 1500,
            max_seconds: 240.0,
            timeout_secs: 60,
            hotkey: "ctrl+space".into(),
            hotkey_mode: "hold".into(),
            api_style: None,
            theme: None,
            color: None,
            input_device: None,
            keep_audio: false,
            paste: "auto".into(),
        }
    }
}

impl Config {
    /// 用户填 `https://dashscope.aliyuncs.com` 或填全 `…/compatible-mode/v1` 都接受。
    pub fn endpoint(&self) -> String {
        let base = self.base_url.trim_end_matches('/');
        if base.ends_with("/compatible-mode/v1") {
            format!("{base}/chat/completions")
        } else {
            format!("{base}/compatible-mode/v1/chat/completions")
        }
    }

    pub fn api_key(&self) -> Option<String> {
        std::env::var("DASHSCOPE_API_KEY")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .or_else(|| self.api_key.clone().filter(|s| !s.trim().is_empty()))
    }
}

pub fn config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("dictate")
        .join("config.toml")
}

/// 守护进程写给终端视图看的状态文件。
pub fn status_path() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("dictate")
        .join("status.json")
}

/// 终端视图给守护进程下命令的投递箱（单行文本，守护进程读走后删除）。
pub fn control_path() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("dictate")
        .join("control")
}

pub fn history_path() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("dictate")
        .join("history.jsonl")
}

/// 写回配置文件；unix 上顺手把权限压到 0600（里面有 key）。
pub fn write(path: &std::path::Path, cfg: &Config) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).ok();
    }
    let body = format!(
        "# dictate 配置（由 `dictate settings` 写入；手改也认）\n\n{}",
        toml::to_string_pretty(cfg)?
    );
    std::fs::write(path, body)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

pub fn load() -> Result<Config> {
    let path = config_path();
    if !path.exists() {
        return Ok(Config::default());
    }
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("读配置失败：{}", path.display()))?;
    toml::from_str(&text).with_context(|| format!("解析配置失败：{}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_accepts_bare_host_and_full_base() {
        let mut c = Config::default();
        assert_eq!(
            c.endpoint(),
            "https://dashscope.aliyuncs.com/compatible-mode/v1/chat/completions"
        );
        c.base_url = "https://dashscope-intl.aliyuncs.com/compatible-mode/v1/".into();
        assert_eq!(
            c.endpoint(),
            "https://dashscope-intl.aliyuncs.com/compatible-mode/v1/chat/completions"
        );
    }
}
