//! Настройки рекордера.

use std::path::{Path, PathBuf};
use std::str::FromStr;

/// Какой кодировщик видео использовать.
///
/// В конфиге — строка: `"auto"` или имя элемента GStreamer. Старые
/// значения `"Vah264enc"`, `"Vah264lpenc"`, `"X264enc"` читаются как
/// ручной выбор того же элемента: настройка пользователя не меняется сама.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Encoder {
    /// Лучший из реально работающих на этой машине (`encoders::select`).
    /// По умолчанию на Linux; на Windows по умолчанию — `x264enc`.
    #[default]
    Auto,
    /// Конкретный элемент, выбранный вручную.
    Element(String),
}

impl Encoder {
    pub fn as_str(&self) -> &str {
        match self {
            Encoder::Auto => "auto",
            Encoder::Element(e) => e,
        }
    }

    /// VA-API доступен только на Linux.
    pub fn is_va(&self) -> bool {
        matches!(self, Encoder::Element(e) if e.starts_with("va") && e.ends_with("enc"))
    }
}

impl FromStr for Encoder {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        match s {
            "" => Err("пустое имя кодировщика".into()),
            "auto" | "Auto" => Ok(Encoder::Auto),
            // Имена вариантов прошлых версий.
            "Vah264enc" => Ok(Encoder::Element("vah264enc".into())),
            "Vah264lpenc" => Ok(Encoder::Element("vah264lpenc".into())),
            "X264enc" => Ok(Encoder::Element("x264enc".into())),
            other if other.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') => {
                Ok(Encoder::Element(other.to_owned()))
            }
            other => Err(format!("неизвестный кодировщик: {other}")),
        }
    }
}

impl serde::Serialize for Encoder {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

impl<'de> serde::Deserialize<'de> for Encoder {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        s.parse().map_err(serde::de::Error::custom)
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
    /// кбит/с на каждую дорожку.
    pub bitrate: u32,
    /// Какой кодек предпочесть. Доступность проверяется при запуске.
    pub codec: crate::audio::CodecPreference,
    /// Системный звук и микрофон — отдельными дорожками, а не одним миксом.
    /// Имеет смысл, только когда включены оба источника. В старых конфигах
    /// поля нет, и они читаются как прежний смешанный режим.
    pub separate_tracks: bool,
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
            separate_tracks: false,
        }
    }
}

/// Режим записи звука так, как его выбирает пользователь.
///
/// Хранится не отдельным полем, а выводится из `system`, `mic` и
/// `separate_tracks`: так конфиги прошлых версий означают ровно то же,
/// что и раньше.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioMode {
    Off,
    SystemOnly,
    MicOnly,
    /// Оба источника в одной дорожке — поведение прежних версий.
    Mixed,
    /// Оба источника, каждый своей дорожкой.
    Separate,
}

impl AudioMode {
    pub const ALL: [AudioMode; 5] = [
        AudioMode::Off,
        AudioMode::SystemOnly,
        AudioMode::MicOnly,
        AudioMode::Mixed,
        AudioMode::Separate,
    ];

    pub fn label(self) -> &'static str {
        match self {
            AudioMode::Off => "Без звука",
            AudioMode::SystemOnly => "Только системный звук",
            AudioMode::MicOnly => "Только микрофон",
            AudioMode::Mixed => "Системный + микрофон, одна дорожка",
            AudioMode::Separate => "Системный + микрофон, раздельно",
        }
    }
}

impl AudioConfig {
    pub fn any_enabled(&self) -> bool {
        self.system || self.mic
    }

    pub fn mode(&self) -> AudioMode {
        match (self.system, self.mic) {
            (false, false) => AudioMode::Off,
            (true, false) => AudioMode::SystemOnly,
            (false, true) => AudioMode::MicOnly,
            (true, true) if self.separate_tracks => AudioMode::Separate,
            (true, true) => AudioMode::Mixed,
        }
    }

    pub fn set_mode(&mut self, mode: AudioMode) {
        let (system, mic) = match mode {
            AudioMode::Off => (false, false),
            AudioMode::SystemOnly => (true, false),
            AudioMode::MicOnly => (false, true),
            AudioMode::Mixed | AudioMode::Separate => (true, true),
        };
        self.system = system;
        self.mic = mic;
        self.separate_tracks = mode == AudioMode::Separate;
    }

    /// Дорожки, которые должен писать конвейер, по порядку их следования
    /// в mp4. Источники из `unavailable` выбрасываются: их не удалось открыть.
    pub fn track_plan(
        &self,
        unavailable: &[crate::audio::AudioSource],
    ) -> Vec<crate::audio::TrackPlan> {
        use crate::audio::{AudioSource, TrackKind, TrackPlan};
        let system = self.system && !unavailable.contains(&AudioSource::System);
        let mic = self.mic && !unavailable.contains(&AudioSource::Mic);
        let one = |kind, source| TrackPlan {
            kind,
            sources: vec![source],
        };
        match (system, mic) {
            (false, false) => Vec::new(),
            (true, false) => vec![one(TrackKind::System, AudioSource::System)],
            (false, true) => vec![one(TrackKind::Mic, AudioSource::Mic)],
            (true, true) if self.separate_tracks => vec![
                one(TrackKind::System, AudioSource::System),
                one(TrackKind::Mic, AudioSource::Mic),
            ],
            (true, true) => vec![TrackPlan {
                kind: TrackKind::Mixed,
                sources: vec![AudioSource::System, AudioSource::Mic],
            }],
        }
    }

    /// Громкость источника.
    pub fn volume(&self, source: crate::audio::AudioSource) -> f64 {
        match source {
            crate::audio::AudioSource::System => self.system_volume,
            crate::audio::AudioSource::Mic => self.mic_volume,
        }
    }
}

/// Угол экрана для оверлея.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum OverlayCorner {
    #[default]
    TopRight,
    TopLeft,
    BottomRight,
    BottomLeft,
}

impl OverlayCorner {
    pub const ALL: [OverlayCorner; 4] = [
        OverlayCorner::TopRight,
        OverlayCorner::TopLeft,
        OverlayCorner::BottomRight,
        OverlayCorner::BottomLeft,
    ];

    pub fn label(self) -> &'static str {
        match self {
            OverlayCorner::TopRight => "справа сверху",
            OverlayCorner::TopLeft => "слева сверху",
            OverlayCorner::BottomRight => "справа снизу",
            OverlayCorner::BottomLeft => "слева снизу",
        }
    }
}

/// Когда показывать оверлей.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum OverlayShow {
    /// Всё время, пока включён.
    #[default]
    Always,
    /// Несколько секунд после сохранения клипа.
    AfterSave,
}

/// Игровой оверлей и уведомления. Применяются без пересборки конвейера.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct OverlayConfig {
    pub enabled: bool,
    /// Одна строка вместо карточки.
    pub compact: bool,
    pub corner: OverlayCorner,
    /// Непрозрачность, 0,3–1.
    pub opacity: f32,
    pub show: OverlayShow,
    /// Принимает ли оверлей мышь. Выключено — клики проходят в игру насквозь,
    /// кнопки «Сохранить» нет.
    pub interactive: bool,
    /// Системное уведомление после каждого сохранения.
    pub notify_on_save: bool,
}

impl Default for OverlayConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            compact: false,
            corner: OverlayCorner::TopRight,
            opacity: 0.85,
            show: OverlayShow::Always,
            interactive: true,
            notify_on_save: false,
        }
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
    /// Видеокодек для автовыбора. При ручном выборе кодек — тот, что у
    /// выбранного элемента.
    pub video_codec: crate::encoders::VideoCodec,
    /// Windows: -1 = основной монитор, остальные — индексы DXGI.
    pub monitor: i32,
    pub width: u32,
    pub height: u32,
    pub audio: AudioConfig,
    pub overlay: OverlayConfig,
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
            // На Windows аппаратные кодировщики не проверены на оборудовании:
            // по умолчанию — проверенный программный x264enc, автовыбор и
            // аппаратные доступны в окне вручную.
            encoder: if cfg!(windows) {
                Encoder::Element("x264enc".into())
            } else {
                Encoder::Auto
            },
            video_codec: crate::encoders::VideoCodec::H264,
            monitor: -1,
            width: 1920,
            height: 1080,
            audio: AudioConfig::default(),
            overlay: OverlayConfig::default(),
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
            || self.video_codec != other.video_codec
            || self.width != other.width
            || self.height != other.height
            || self.audio != other.audio
    }

    pub fn path() -> PathBuf {
        config_dir().join("config.json")
    }

    pub fn load() -> Self {
        Self::load_from(&Self::path())
    }

    pub fn load_from(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_else(|e| {
                log::warn!("повреждённый конфиг, беру значения по умолчанию: {e}");
                Self::default()
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(e) => {
                log::warn!(
                    "не удалось прочитать {}, беру значения по умолчанию: {e}",
                    path.display()
                );
                Self::default()
            }
        }
    }

    pub fn store(&self) -> anyhow::Result<()> {
        self.store_to(&Self::path())
    }

    pub fn store_to(&self, path: &Path) -> anyhow::Result<()> {
        use anyhow::Context;
        let text = serde_json::to_string_pretty(self).context("не удалось сериализовать конфиг")?;
        write_atomic(path, text.as_bytes())
    }
}

/// Записывает файл целиком или не трогает вовсе.
///
/// `fs::write` усекает файл до записи: падение или ошибка посреди неё
/// оставляли обрезанный JSON, а `load` молча откатывал его к значениям по
/// умолчанию. Здесь данные сначала уходят во временный файл рядом с целевым
/// и сбрасываются на диск, а `rename` подменяет файл атомарно: после сбоя
/// на месте остаётся либо старая версия, либо новая. Запись идёт только по
/// кнопке «Применить», поэтому `sync_all` по цене незаметен.
fn write_atomic(path: &Path, data: &[u8]) -> anyhow::Result<()> {
    use anyhow::Context;
    use std::io::Write;

    let path = match std::fs::canonicalize(path) {
        Ok(real) => real,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => path.to_path_buf(),
        Err(e) => return Err(e).with_context(|| format!("не удалось открыть {}", path.display())),
    };
    let dir = path
        .parent()
        .with_context(|| format!("у {} нет каталога", path.display()))?;
    std::fs::create_dir_all(dir)
        .with_context(|| format!("не удалось создать {}", dir.display()))?;
    let mut tmp_name = path.file_name().unwrap_or_default().to_os_string();
    // Имя постоянное: огрызок после падения перезапишется следующим сохранением.
    tmp_name.push(".tmp");
    let tmp = dir.join(tmp_name);

    let written = (|| -> anyhow::Result<()> {
        let mut file = std::fs::File::create(&tmp)
            .with_context(|| format!("не удалось создать {}", tmp.display()))?;
        if let Ok(meta) = std::fs::metadata(&path) {
            file.set_permissions(meta.permissions())
                .with_context(|| format!("не удалось выставить права {}", tmp.display()))?;
        }
        file.write_all(data)
            .with_context(|| format!("не удалось записать {}", tmp.display()))?;
        file.sync_all()
            .with_context(|| format!("не удалось сбросить на диск {}", tmp.display()))?;
        drop(file);
        std::fs::rename(&tmp, &path).with_context(|| {
            format!(
                "не удалось заменить {} на {}",
                path.display(),
                tmp.display()
            )
        })
    })();
    if written.is_err() {
        if let Err(e) = std::fs::remove_file(&tmp) {
            if e.kind() != std::io::ErrorKind::NotFound {
                log::warn!("не удалось удалить {}: {e}", tmp.display());
            }
        }
    }
    written
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
    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("replay-cfg-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn sample() -> Config {
        let mut cfg = Config {
            seconds: 42.0,
            fps: 30,
            output: PathBuf::from("/tmp/клипы с пробелом"),
            ..Config::default()
        };
        cfg.audio.mic = true;
        cfg.audio.codec = crate::audio::CodecPreference::Opus;
        cfg
    }

    #[test]
    fn store_then_load_round_trips() {
        let dir = temp_dir("rt");
        // Каталога ещё нет — store обязан его создать.
        let path = dir.join("nested").join("config.json");
        sample().store_to(&path).unwrap();
        assert_eq!(Config::load_from(&path), sample());
        // Повторная запись поверх существующего файла.
        let mut next = sample();
        next.bitrate = 9000;
        next.store_to(&path).unwrap();
        assert_eq!(Config::load_from(&path), next);
        assert!(
            !path.with_file_name("config.json.tmp").exists(),
            "временный файл остался после успешной записи"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_or_corrupt_config_falls_back_to_defaults() {
        let dir = temp_dir("bad");
        let path = dir.join("config.json");
        assert_eq!(Config::load_from(&path), Config::default());
        std::fs::write(&path, r#"{"seconds": 45.0, "fps": 3"#).unwrap();
        assert_eq!(Config::load_from(&path), Config::default());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn failed_write_keeps_previous_config_intact() {
        let dir = temp_dir("keep");
        let path = dir.join("config.json");
        sample().store_to(&path).unwrap();
        let before = std::fs::read(&path).unwrap();

        // Временный файл создать нельзя: на его месте каталог.
        std::fs::create_dir(dir.join("config.json.tmp")).unwrap();
        let err = Config::default().store_to(&path).unwrap_err();
        assert!(format!("{err:#}").contains("config.json.tmp"), "{err:#}");
        assert_eq!(
            std::fs::read(&path).unwrap(),
            before,
            "старый конфиг испорчен"
        );
        assert_eq!(Config::load_from(&path), sample());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn failed_rename_removes_temp_file() {
        let dir = temp_dir("rename");
        // Целевой путь занят каталогом: временный файл запишется, rename — нет.
        let path = dir.join("config.json");
        std::fs::create_dir(&path).unwrap();
        std::fs::write(path.join("keep"), b"x").unwrap();
        assert!(sample().store_to(&path).is_err());
        assert!(
            !dir.join("config.json.tmp").exists(),
            "временный файл не убран после ошибки"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn store_follows_symlink_and_keeps_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_dir("link");
        let real = dir.join("dotfiles").join("config.json");
        std::fs::create_dir_all(real.parent().unwrap()).unwrap();
        sample().store_to(&real).unwrap();
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o600)).unwrap();
        let link = dir.join("config.json");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        Config::default().store_to(&link).unwrap();
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink(),
            "симлинк заменён обычным файлом"
        );
        assert_eq!(Config::load_from(&real), Config::default());
        let mode = std::fs::metadata(&real).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "права конфига потеряны");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Конфиг прошлой версии без `separate_tracks` обязан означать то же,
    /// что и раньше: оба источника в одной дорожке.
    #[test]
    fn old_audio_config_keeps_mixed_track() {
        let cfg: Config = serde_json::from_str(
            r#"{"audio":{"system":true,"mic":true,"system_volume":0.5,"mic_volume":1.5,"bitrate":96,"codec":"Opus"}}"#,
        )
        .unwrap();
        assert_eq!(cfg.audio.mode(), AudioMode::Mixed);
        assert!(!cfg.audio.separate_tracks);
        assert_eq!(cfg.audio.system_volume, 0.5);
        let plan = cfg.audio.track_plan(&[]);
        assert_eq!(
            plan,
            [crate::audio::TrackPlan {
                kind: crate::audio::TrackKind::Mixed,
                sources: vec![
                    crate::audio::AudioSource::System,
                    crate::audio::AudioSource::Mic
                ],
            }],
            "старый смешанный режим: оба источника в одной дорожке"
        );
    }

    #[test]
    fn audio_modes_round_trip_and_plan_tracks() {
        use crate::audio::{AudioSource, TrackKind};
        for mode in AudioMode::ALL {
            let mut a = AudioConfig::default();
            a.set_mode(mode);
            assert_eq!(a.mode(), mode, "{mode:?}");
            let kinds: Vec<TrackKind> = a.track_plan(&[]).iter().map(|t| t.kind).collect();
            let expected: &[TrackKind] = match mode {
                AudioMode::Off => &[],
                AudioMode::SystemOnly => &[TrackKind::System],
                AudioMode::MicOnly => &[TrackKind::Mic],
                AudioMode::Mixed => &[TrackKind::Mixed],
                AudioMode::Separate => &[TrackKind::System, TrackKind::Mic],
            };
            assert_eq!(kinds, expected, "{mode:?}");
            // Источники: у дорожки системного звука — только он, и наоборот.
            for t in a.track_plan(&[]) {
                let want: &[AudioSource] = match t.kind {
                    TrackKind::System => &[AudioSource::System],
                    TrackKind::Mic => &[AudioSource::Mic],
                    TrackKind::Mixed => &[AudioSource::System, AudioSource::Mic],
                };
                assert_eq!(t.sources, want, "{mode:?}");
            }
        }
        // Смена режима — это смена ветки звука: без пересборки не обойтись.
        let mut a = Config::default();
        let mut b = a.clone();
        a.audio.set_mode(AudioMode::Mixed);
        b.audio.set_mode(AudioMode::Separate);
        assert!(a.needs_restart(&b));

        // Микрофон не открылся — в раздельном режиме остаётся одна дорожка,
        // а не пустая вторая.
        let mut sep = AudioConfig::default();
        sep.set_mode(AudioMode::Separate);
        let plan = sep.track_plan(&[AudioSource::Mic]);
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].kind, TrackKind::System);
        assert!(sep
            .track_plan(&[AudioSource::Mic, AudioSource::System])
            .is_empty());
    }

    #[test]
    fn default_encoder_matches_platform() {
        assert_eq!(
            Config::default().encoder,
            if cfg!(windows) {
                Encoder::Element("x264enc".into())
            } else {
                Encoder::Auto
            }
        );
    }

    /// Ручной выбор прошлых версий остаётся ручным выбором того же элемента.
    #[test]
    fn legacy_encoder_values_keep_manual_choice() {
        for (old, element) in [
            ("Vah264enc", "vah264enc"),
            ("Vah264lpenc", "vah264lpenc"),
            ("X264enc", "x264enc"),
        ] {
            let cfg: Config = serde_json::from_str(&format!(r#"{{"encoder":"{old}"}}"#)).unwrap();
            assert_eq!(cfg.encoder, Encoder::Element(element.into()), "{old}");
            assert_eq!(cfg.video_codec, crate::encoders::VideoCodec::H264);
            // И после записи — тот же выбор.
            let back: Config = serde_json::from_str(&serde_json::to_string(&cfg).unwrap()).unwrap();
            assert_eq!(back.encoder, cfg.encoder);
        }
        let cfg: Config =
            serde_json::from_str(r#"{"encoder":"auto","video_codec":"h265"}"#).unwrap();
        assert_eq!(cfg.encoder, Encoder::Auto);
        assert_eq!(cfg.video_codec, crate::encoders::VideoCodec::H265);
        assert!("bad name!".parse::<Encoder>().is_err());
    }
}
