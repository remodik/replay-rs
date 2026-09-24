//! Настройки рекордера.

use std::path::PathBuf;
use std::str::FromStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Encoder {
    /// VA-API на Intel iGPU. Основной вариант: поддерживает VBR.
    Vah264enc,
    /// Low-power VA-API. На этом железе доступен только CQP.
    Vah264lpenc,
    /// Программный запасной вариант.
    X264enc,
}

impl Encoder {
    pub fn as_str(self) -> &'static str {
        match self {
            Encoder::Vah264enc => "vah264enc",
            Encoder::Vah264lpenc => "vah264lpenc",
            Encoder::X264enc => "x264enc",
        }
    }

    /// Аппаратный ли кодировщик (решает, нужен ли vapostproc вместо videoconvert).
    pub fn is_va(self) -> bool {
        matches!(self, Encoder::Vah264enc | Encoder::Vah264lpenc)
    }
}

impl FromStr for Encoder {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "vah264enc" => Ok(Encoder::Vah264enc),
            "vah264lpenc" => Ok(Encoder::Vah264lpenc),
            "x264enc" => Ok(Encoder::X264enc),
            other => Err(format!("неизвестный энкодер: {other}")),
        }
    }
}

/// Настройки звука. Меняются только вместе с пересборкой конвейера:
/// включение источника добавляет ветку, а не крутит ручку.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct AudioConfig {
    /// Системный звук — монитор устройства вывода по умолчанию.
    pub system: bool,
    /// Микрофон — источник записи по умолчанию.
    pub mic: bool,
    pub system_volume: f64,
    pub mic_volume: f64,
    /// кбит/с на дорожку после микширования.
    pub bitrate: u32,
    /// Какой кодек предпочесть. Доступность проверяется при запуске.
    pub codec: crate::audio::CodecPreference,
}

impl Default for AudioConfig {
    fn default() -> Self {
        // Микрофон по умолчанию выключен: писать его без спроса некрасиво.
        Self {
            system: true,
            mic: false,
            system_volume: 1.0,
            mic_volume: 1.0,
            bitrate: 128,
            codec: crate::audio::CodecPreference::default(),
        }
    }
}

impl AudioConfig {
    pub fn any_enabled(&self) -> bool {
        self.system || self.mic
    }
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Config {
    /// Длина кольцевого буфера, с.
    pub seconds: f64,
    /// Сколько сохранять по хоткею, с.
    pub save_seconds: f64,
    /// кбит/с
    pub bitrate: u32,
    pub fps: u32,
    /// Секунд между keyframe.
    pub gop: f64,
    /// Лимит буфера, МБ.
    pub max_mb: usize,
    pub output: PathBuf,
    pub encoder: Encoder,
    /// Windows: -1 = основной монитор, остальные — индексы DXGI.
    pub monitor: i32,
    pub width: u32,
    pub height: u32,
    pub audio: AudioConfig,
}

impl Config {
    pub fn max_bytes(&self) -> usize {
        self.max_mb * 1024 * 1024
    }

    /// Максимальное расстояние между keyframe в кадрах.
    pub fn key_int_max(&self) -> u32 {
        ((self.fps as f64 * self.gop) as u32).max(1)
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            seconds: 30.0,
            save_seconds: 30.0,
            bitrate: 15000,
            fps: 60,
            gop: 1.0,
            max_mb: 300,
            output: dirs_videos().join("replays"),
            encoder: if cfg!(windows) {
                Encoder::X264enc
            } else {
                Encoder::Vah264enc
            },
            monitor: -1,
            width: 1920,
            height: 1080,
            audio: AudioConfig::default(),
        }
    }
}

impl Config {
    /// Настройки, которые нельзя применить без пересборки конвейера.
    pub fn needs_restart(&self, other: &Self) -> bool {
        self.monitor != other.monitor
            || self.fps != other.fps
            || self.bitrate != other.bitrate
            || self.gop != other.gop
            || self.encoder != other.encoder
            || self.width != other.width
            || self.height != other.height
            || self.audio != other.audio
    }

    pub fn path() -> PathBuf {
        config_dir().join("config.json")
    }

    pub fn load() -> Self {
        match std::fs::read_to_string(Self::path()) {
            Ok(text) => match serde_json::from_str(&text) {
                Ok(cfg) => cfg,
                Err(e) => {
                    log::warn!("повреждённый конфиг, беру значения по умолчанию: {e}");
                    Self::default()
                }
            },
            Err(_) => Self::default(),
        }
    }

    pub fn store(&self) -> anyhow::Result<()> {
        use anyhow::Context;
        let path = Self::path();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("не удалось создать {}", dir.display()))?;
        }
        let text = serde_json::to_string_pretty(self).context("не удалось сериализовать конфиг")?;
        std::fs::write(&path, text)
            .with_context(|| format!("не удалось записать {}", path.display()))
    }
}

#[cfg(not(windows))]
pub fn config_dir() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".config"))
        .join("replay-rs")
}

#[cfg(not(windows))]
pub fn cache_dir() -> PathBuf {
    std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".cache"))
        .join("replay-rs")
}

fn home() -> PathBuf {
    std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
        .map(PathBuf::from)
        .unwrap_or_default()
}

fn dirs_videos() -> PathBuf {
    home().join("Videos")
}

#[cfg(windows)]
pub fn config_dir() -> PathBuf {
    std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join("AppData/Roaming"))
        .join("replay-rs")
}
#[cfg(windows)]
pub fn cache_dir() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join("AppData/Local"))
        .join("replay-rs/cache")
}
#[cfg(test)]
mod portability_tests {
    use super::*;
    #[test]
    fn old_config_defaults_to_primary_monitor_and_monitor_changes_restart() {
        let cfg: Config = serde_json::from_str(r#"{"seconds":45.0}"#).unwrap();
        assert_eq!(cfg.monitor, -1);
        assert_eq!(cfg.seconds, 45.0);
        let mut changed = cfg.clone();
        changed.monitor = 1;
        assert!(cfg.needs_restart(&changed));
        let restored: Config =
            serde_json::from_str(&serde_json::to_string(&changed).unwrap()).unwrap();
        assert_eq!(restored, changed);
    }
    #[test]
    fn default_encoder_matches_platform() {
        assert_eq!(
            Config::default().encoder,
            if cfg!(windows) {
                Encoder::X264enc
            } else {
                Encoder::Vah264enc
            }
        );
    }
}
