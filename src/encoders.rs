//! Кодировщики видео: какие есть в системе, какие из них реально работают
//! и какой выбрать.
//!
//! Наличие видеокарты определённого производителя ничего не гарантирует:
//! элемент может отсутствовать (плагин не установлен), быть
//! незарегистрированным (плагин есть, а драйвер или устройство не
//! подошли — VA и nvcodec проверяют это при загрузке) или не открыться на
//! деле. Поэтому каждый кандидат проходит три проверки: есть ли фабрика,
//! есть ли плагин, и кодирует ли он несколько кадров через ту же цепочку
//! преобразований, парсер и caps, что и настоящий конвейер.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use gstreamer as gst;
use gstreamer::prelude::*;

use crate::config::{Config, Encoder};

/// Видеокодек выхода.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, Default, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub enum VideoCodec {
    #[default]
    H264,
    H265,
    /// Конвейер и сохранение реализованы и проверены только с программными
    /// кодировщиками, слишком тяжёлыми для фоновой записи 1080p60. В окне
    /// не показывается; включается из командной строки как эксперимент.
    Av1,
}

impl VideoCodec {
    pub const ALL: [VideoCodec; 3] = [VideoCodec::H264, VideoCodec::H265, VideoCodec::Av1];

    pub fn label(self) -> &'static str {
        match self {
            VideoCodec::H264 => "H.264",
            VideoCodec::H265 => "HEVC (H.265)",
            VideoCodec::Av1 => "AV1",
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            VideoCodec::H264 => "h264",
            VideoCodec::H265 => "h265",
            VideoCodec::Av1 => "av1",
        }
    }

    /// Показывать ли кодек в окне настроек.
    pub fn in_ui(self) -> bool {
        !matches!(self, VideoCodec::Av1)
    }

    /// Парсер на выходе кодировщика в конвейере захвата.
    ///
    /// config-interval=-1 вставляет параметры потока перед каждым
    /// ключевым кадром: буфер режется по GOP, и любой GOP обязан
    /// декодироваться сам по себе. У av1parse такого свойства нет — там
    /// заголовок последовательности обязан идти в каждом ключевом TU от
    /// самого кодировщика (svtav1enc и av1enc так и делают).
    pub fn capture_parser(self) -> &'static str {
        match self {
            VideoCodec::H264 => "h264parse config-interval=-1",
            VideoCodec::H265 => "h265parse config-interval=-1",
            VideoCodec::Av1 => "av1parse",
        }
    }

    /// Caps между парсером и appsink: поток режется по access unit'ам.
    pub fn capture_caps(self, profile: Option<&str>) -> String {
        let base = match self {
            VideoCodec::H264 => "video/x-h264,stream-format=byte-stream,alignment=au",
            VideoCodec::H265 => "video/x-h265,stream-format=byte-stream,alignment=au",
            VideoCodec::Av1 => "video/x-av1,stream-format=obu-stream,alignment=tu",
        };
        match profile {
            Some(p) => format!("{base},profile={p}"),
            None => base.to_owned(),
        }
    }

    /// Caps для appsrc при сохранении: то же, что выдал захват.
    pub fn mux_caps(self, fps: u32) -> String {
        format!("{},framerate={fps}/1", self.capture_caps(None))
    }

    /// Парсер перед mp4mux: переупаковывает byte-stream в avc/hvc1.
    pub fn mux_parser(self) -> &'static str {
        match self {
            VideoCodec::H264 => "h264parse",
            VideoCodec::H265 => "h265parse",
            VideoCodec::Av1 => "av1parse",
        }
    }

    fn from_part(s: &str) -> Option<Self> {
        match s {
            "h264" => Some(VideoCodec::H264),
            "h265" => Some(VideoCodec::H265),
            "av1" => Some(VideoCodec::Av1),
            _ => None,
        }
    }
}

impl std::str::FromStr for VideoCodec {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "h264" | "h.264" | "avc" => Ok(VideoCodec::H264),
            "h265" | "h.265" | "hevc" => Ok(VideoCodec::H265),
            "av1" => Ok(VideoCodec::Av1),
            other => Err(format!("неизвестный видеокодек: {other}")),
        }
    }
}

/// Через что кодировщик работает с железом.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Backend {
    VaApi,
    /// Low-power вариант VA: на Intel обычно только постоянный квантователь.
    VaApiLowPower,
    Nvenc,
    QuickSync,
    Amf,
    MediaFoundation,
    D3d12,
    Software,
}

impl Backend {
    pub fn label(self) -> &'static str {
        match self {
            Backend::VaApi => "VA-API",
            Backend::VaApiLowPower => "VA-API low-power",
            Backend::Nvenc => "NVIDIA NVENC",
            Backend::QuickSync => "Intel Quick Sync",
            Backend::Amf => "AMD AMF",
            Backend::MediaFoundation => "Media Foundation",
            Backend::D3d12 => "Direct3D 12",
            Backend::Software => "программный",
        }
    }
}

/// Как кадры попадают в кодировщик.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Upload {
    /// vapostproc → VA-память. Принимает и DMABuf с портала, и системную
    /// память, так что кадр с экрана не покидает видеокарту.
    VaMemory,
    /// videoconvert → системная память в указанном формате.
    System(&'static str),
}

/// Один кандидат: элемент GStreamer и всё, что нужно, чтобы его включить.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncoderSpec {
    pub element: String,
    pub codec: VideoCodec,
    pub backend: Backend,
    pub hardware: bool,
    /// Плагин, в котором живёт элемент: по нему отличаем «не установлен»
    /// от «установлен, но устройство не подошло».
    plugin: &'static str,
    upload: Upload,
    /// Профиль, который просим в caps после парсера. Только там, где он
    /// проверен: openh264enc, например, умеет лишь constrained-baseline, и
    /// требование high роняло его в not-negotiated.
    profile: Option<&'static str>,
}

impl EncoderSpec {
    /// «VA-API · H.264 · аппаратный (vah264enc)».
    pub fn label(&self) -> String {
        let kind = if self.hardware {
            "аппаратный"
        } else {
            "программный"
        };
        if self.backend == Backend::Software {
            format!("{} · {kind} ({})", self.codec.label(), self.element)
        } else {
            format!(
                "{} · {} · {kind} ({})",
                self.backend.label(),
                self.codec.label(),
                self.element
            )
        }
    }

    /// Свойства кодировщика под настройки.
    ///
    /// Везде: битрейт, расстояние между ключевыми кадрами в кадрах и
    /// никаких B-кадров — буфер хранит только PTS, а сохранение ставит
    /// DTS = PTS, что верно лишь без переупорядочивания.
    pub fn properties(&self, cfg: &Config) -> String {
        let kbps = cfg.bitrate;
        let gop = cfg.key_int_max();
        let el = self.element.as_str();
        match (self.backend, self.codec) {
            (Backend::VaApi, VideoCodec::Av1) => {
                format!("rate-control=vbr bitrate={kbps} key-int-max={gop}")
            }
            (Backend::VaApi, _) => {
                format!("rate-control=vbr bitrate={kbps} key-int-max={gop} b-frames=0")
            }
            // У low-power режима на Intel доступен только CQP.
            (Backend::VaApiLowPower, VideoCodec::Av1) => format!("key-int-max={gop}"),
            (Backend::VaApiLowPower, _) => format!("qpi=25 qpp=27 key-int-max={gop} b-frames=0"),
            // repeat-sequence-header по умолчанию выключен: GOP из середины
            // буфера остался бы без SPS/PPS (парсер их тоже вставляет, но
            // лишняя страховка дешёвая).
            (Backend::Nvenc, VideoCodec::Av1) => {
                format!("rc-mode=vbr bitrate={kbps} gop-size={gop} bframes=0")
            }
            (Backend::Nvenc, _) => format!(
                "rc-mode=vbr bitrate={kbps} gop-size={gop} bframes=0 repeat-sequence-header=true"
            ),
            // idr-interval=0: каждый I-кадр — IDR, то есть точка разреза.
            (Backend::QuickSync, VideoCodec::Av1) => {
                format!("rate-control=vbr bitrate={kbps} gop-size={gop}")
            }
            (Backend::QuickSync, _) => {
                format!("rate-control=vbr bitrate={kbps} gop-size={gop} b-frames=0 idr-interval=0")
            }
            (Backend::Amf, VideoCodec::H264) => format!(
                "rate-control=vbr bitrate={kbps} gop-size={gop} b-frames=0 usage=low-latency"
            ),
            (Backend::Amf, _) => {
                format!("rate-control=vbr bitrate={kbps} gop-size={gop} usage=low-latency")
            }
            // gop-size по умолчанию -1, а 0 у части драйверов даёт один
            // ключевой кадр на весь поток — задаём явно.
            (Backend::MediaFoundation, _) => {
                format!("bitrate={kbps} gop-size={gop} bframes=0")
            }
            // VBR у d3d12h264enc ограничен max-bitrate (по умолчанию 4 Мбит/с).
            (Backend::D3d12, _) => format!("rate-control=cbr bitrate={kbps} gop-size={gop}"),
            (Backend::Software, _) => match el {
                "x264enc" => format!(
                    "tune=zerolatency speed-preset=ultrafast bitrate={kbps} \
                     key-int-max={gop} bframes=0"
                ),
                // openh264enc считает битрейт в битах в секунду.
                "openh264enc" => format!(
                    "rate-control=bitrate bitrate={} gop-size={gop}",
                    kbps as u64 * 1000
                ),
                // По умолчанию x265 пишет открытые GOP (CRA) без параметров
                // потока, и GOP из середины буфера не декодируется.
                "x265enc" => format!(
                    "tune=zerolatency speed-preset=ultrafast bitrate={kbps} key-int-max={gop} \
                     option-string=\"bframes=0:repeat-headers=1:open-gop=0\""
                ),
                "av1enc" => format!(
                    "end-usage=vbr target-bitrate={kbps} usage-profile=realtime cpu-used=10 \
                     lag-in-frames=0 keyframe-max-dist={gop}"
                ),
                // Без явного intra-period-length svtav1enc выдаёт один
                // ключевой кадр на весь поток.
                "svtav1enc" => format!(
                    "target-bitrate={kbps} preset=12 intra-period-length={}",
                    gop.saturating_sub(1)
                ),
                "rav1enc" => format!(
                    "bitrate={} low-latency=true max-key-frame-interval={gop}",
                    kbps as u64 * 1000
                ),
                _ => String::new(),
            },
        }
    }

    /// Цепочка от источника до кодировщика, включая приведение частоты.
    ///
    /// Порядок принципиален: преобразователь стоит ДО videorate.
    /// Портал отдаёт кадры как video/x-raw(memory:DMABuf), а фильтр вида
    /// video/x-raw,framerate=N/1 без указания памяти матчится только с
    /// системной: фичи caps должны совпадать точно. Фильтр сразу после
    /// источника ронял конвейер в not-negotiated. vapostproc принимает и
    /// DMABuf, и системную память, поэтому импорт делаем первым, а частоту
    /// приводим уже в VA-памяти. Так ещё и дешевле: преобразуются только
    /// реально пришедшие кадры, а дубликаты до нужной частоты добавляются после.
    pub fn upstream(&self, fps: u32) -> String {
        match self.upload {
            Upload::VaMemory => format!(
                "vapostproc name=vup ! video/x-raw(memory:VAMemory),format=NV12 \
                 ! videorate ! video/x-raw(memory:VAMemory),format=NV12,framerate={fps}/1"
            ),
            Upload::System(fmt) => format!(
                "videoconvert name=vup ! video/x-raw,format={fmt} \
                 ! videorate ! video/x-raw,format={fmt},framerate={fps}/1"
            ),
        }
    }

    /// Только преобразование формата под кодировщик, без приведения
    /// частоты: для перекодирования файла, где частота уже задана.
    pub fn convert(&self) -> String {
        match self.upload {
            Upload::VaMemory => {
                "vapostproc name=vup ! video/x-raw(memory:VAMemory),format=NV12".to_owned()
            }
            Upload::System(fmt) => format!("videoconvert name=vup ! video/x-raw,format={fmt}"),
        }
    }

    /// Кодировщик с настройками, под именем `venc`.
    pub fn encoder_desc(&self, cfg: &Config) -> String {
        format!("{} name=venc {}", self.element, self.properties(cfg))
            .trim_end()
            .to_owned()
    }

    /// Хвост после кодировщика до appsink: парсер и caps.
    pub fn downstream(&self) -> String {
        format!(
            "{} ! {}",
            self.codec.capture_parser(),
            self.codec.capture_caps(self.profile)
        )
    }

    /// Порядок при автовыборе: меньше — раньше. См. `rank_reason`.
    fn priority(&self) -> u32 {
        let backend = match self.backend {
            Backend::VaApi => 0,
            Backend::Nvenc => 1,
            Backend::QuickSync => 2,
            Backend::Amf => 3,
            Backend::MediaFoundation => 4,
            Backend::D3d12 => 5,
            Backend::VaApiLowPower => 6,
            Backend::Software => 10,
        };
        let element = match self.element.as_str() {
            "x264enc" | "x265enc" | "av1enc" => 0,
            "openh264enc" | "svtav1enc" => 1,
            _ => 0,
        };
        backend * 10 + element
    }

    /// Почему кандидат стоит на своём месте в автовыборе.
    pub fn rank_reason(&self) -> &'static str {
        match self.backend {
            Backend::VaApi => {
                "аппаратный; кадр с портала идёт в кодировщик без копирования через системную память"
            }
            Backend::Nvenc | Backend::QuickSync | Backend::Amf => {
                "аппаратный; управление битрейтом и интервалом ключевых кадров"
            }
            Backend::MediaFoundation => {
                "аппаратный или программный — решает драйвер (MFT); после драйверов производителей"
            }
            Backend::D3d12 => "аппаратный; требует Windows 11, в GStreamer пока с нулевым рангом",
            Backend::VaApiLowPower => {
                "аппаратный, но только постоянный квантователь: битрейт не управляется"
            }
            Backend::Software => "программный запасной вариант: нагружает процессор",
        }
    }
}

/// Все кандидаты, которых имеет смысл искать на этой платформе.
///
/// Имена VA-элементов зависят от устройства: при нескольких GPU или с
/// GST_VA_ALL_DRIVERS=1 появляются `varenderD129h264enc` и т. п., а
/// `vah264enc` может не существовать вовсе. Поэтому VA-кандидаты берутся
/// из реестра, а канонические имена добавляются для диагностики.
pub fn candidates() -> Vec<EncoderSpec> {
    let mut out = Vec::new();
    let spec = |element: &str,
                codec,
                backend,
                plugin: &'static str,
                upload,
                profile: Option<&'static str>| EncoderSpec {
        element: element.to_owned(),
        codec,
        backend,
        hardware: backend != Backend::Software,
        plugin,
        upload,
        profile,
    };
    use Backend::*;
    use VideoCodec::*;

    if cfg!(target_os = "linux") {
        let mut va = va_factories();
        if va.is_empty() {
            for name in [
                "vah264enc",
                "vah265enc",
                "vaav1enc",
                "vah264lpenc",
                "vah265lpenc",
            ] {
                if let Some(s) = va_spec(name) {
                    va.push(s);
                }
            }
        }
        out.extend(va);
        for (el, codec) in [("nvh264enc", H264), ("nvh265enc", H265), ("nvav1enc", Av1)] {
            out.push(spec(
                el,
                codec,
                Nvenc,
                "nvcodec",
                Upload::System("NV12"),
                None,
            ));
        }
        for (el, codec) in [
            ("qsvh264enc", H264),
            ("qsvh265enc", H265),
            ("qsvav1enc", Av1),
        ] {
            out.push(spec(el, codec, QuickSync, "qsv", Upload::VaMemory, None));
        }
    }
    if cfg!(windows) {
        // Источник на Windows отдаёт BGRA в системной памяти. Аппаратные
        // кодировщики принимают NV12 и оттуда; путь через D3D11-память без
        // копирования — следующий шаг, его нечем проверить без Windows.
        let sys = Upload::System("NV12");
        for (el, codec) in [("nvh264enc", H264), ("nvh265enc", H265), ("nvav1enc", Av1)] {
            out.push(spec(el, codec, Nvenc, "nvcodec", sys, None));
        }
        for (el, codec) in [
            ("qsvh264enc", H264),
            ("qsvh265enc", H265),
            ("qsvav1enc", Av1),
        ] {
            out.push(spec(el, codec, QuickSync, "qsv", sys, None));
        }
        for (el, codec) in [
            ("amfh264enc", H264),
            ("amfh265enc", H265),
            ("amfav1enc", Av1),
        ] {
            out.push(spec(el, codec, Amf, "amfcodec", sys, None));
        }
        for (el, codec) in [("mfh264enc", H264), ("mfh265enc", H265)] {
            out.push(spec(
                el,
                codec,
                MediaFoundation,
                "mediafoundation",
                sys,
                None,
            ));
        }
        out.push(spec("d3d12h264enc", H264, D3d12, "d3d12", sys, None));
    }
    let i420 = Upload::System("I420");
    out.push(spec("x264enc", H264, Software, "x264", i420, Some("high")));
    out.push(spec("openh264enc", H264, Software, "openh264", i420, None));
    out.push(spec("x265enc", H265, Software, "x265", i420, None));
    out.push(spec("av1enc", Av1, Software, "aom", i420, None));
    out.push(spec("svtav1enc", Av1, Software, "svtav1", i420, None));
    out
}

/// Разбирает имя VA-кодировщика: `va[renderDNNN](h264|h265|av1)[lp]enc`.
fn va_spec(name: &str) -> Option<EncoderSpec> {
    let rest = name.strip_prefix("va")?.strip_suffix("enc")?;
    let rest = match rest.strip_prefix("renderD") {
        Some(r) => r.trim_start_matches(|c: char| c.is_ascii_digit()),
        None => rest,
    };
    let (codec_part, lp) = match rest.strip_suffix("lp") {
        Some(c) => (c, true),
        None => (rest, false),
    };
    let codec = VideoCodec::from_part(codec_part)?;
    Some(EncoderSpec {
        element: name.to_owned(),
        codec,
        backend: if lp {
            Backend::VaApiLowPower
        } else {
            Backend::VaApi
        },
        hardware: true,
        plugin: "va",
        upload: Upload::VaMemory,
        // vah264enc сам выбирает main; high просили и раньше, профиль
        // проверен на живой машине.
        profile: (codec == VideoCodec::H264).then_some("high"),
    })
}

/// VA-кодировщики, которые зарегистрировал плагин на этой машине.
fn va_factories() -> Vec<EncoderSpec> {
    if gst::Registry::get().find_plugin("va").is_none() {
        return Vec::new();
    }
    let mut out: Vec<EncoderSpec> = gst::ElementFactory::factories_with_type(
        gst::ElementFactoryType::ENCODER | gst::ElementFactoryType::MEDIA_VIDEO,
        gst::Rank::NONE,
    )
    .into_iter()
    .filter_map(|f| va_spec(f.name().as_str()))
    .collect();
    out.sort_by(|a, b| a.element.cmp(&b.element));
    out
}

/// Чем закончилась проверка кандидата.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeOutcome {
    /// Кодирует: несколько кадров прошли через всю цепочку.
    Works { millis: u32 },
    /// Плагин не установлен.
    PluginMissing,
    /// Плагин есть, а элемента нет: при загрузке он не нашёл подходящее
    /// устройство или драйвер (VA и nvcodec проверяют это сами).
    NoDevice,
    /// Элемент есть, но не открылся: ошибка устройства, свойства или таймаут.
    InitFailed(String),
    /// Элемент есть, но не договорился о формате с цепочкой захвата.
    Incompatible(String),
}

impl ProbeOutcome {
    pub fn works(&self) -> bool {
        matches!(self, ProbeOutcome::Works { .. })
    }

    pub fn label(&self) -> String {
        match self {
            ProbeOutcome::Works { millis } => format!("работает (проверка {millis} мс)"),
            ProbeOutcome::PluginMissing => "плагин не установлен".into(),
            ProbeOutcome::NoDevice => "плагин есть, устройство или драйвер не подошли".into(),
            ProbeOutcome::InitFailed(e) => format!("не открылся: {e}"),
            ProbeOutcome::Incompatible(e) => format!("несовместимый формат: {e}"),
        }
    }
}

/// Кандидат и результат его проверки.
#[derive(Debug, Clone)]
pub struct Probed {
    pub spec: EncoderSpec,
    pub outcome: ProbeOutcome,
}

/// Итог обнаружения кодировщиков.
#[derive(Debug, Clone, Default)]
pub struct Discovery {
    pub probed: Vec<Probed>,
    /// Сколько заняло всё обнаружение.
    pub millis: u32,
}

impl Discovery {
    /// Рабочие кандидаты кодека в порядке автовыбора.
    pub fn working(&self, codec: VideoCodec) -> Vec<&EncoderSpec> {
        let mut out: Vec<&EncoderSpec> = self
            .probed
            .iter()
            .filter(|p| p.spec.codec == codec && p.outcome.works())
            .map(|p| &p.spec)
            .collect();
        out.sort_by_key(|s| s.priority());
        out
    }

    pub fn find(&self, element: &str) -> Option<&Probed> {
        self.probed.iter().find(|p| p.spec.element == element)
    }

    /// Кодеки, для которых нашёлся хотя бы один рабочий кодировщик.
    pub fn codecs(&self) -> Vec<VideoCodec> {
        VideoCodec::ALL
            .into_iter()
            .filter(|c| !self.working(*c).is_empty())
            .collect()
    }
}

/// Сколько ждём пробного кодирования. Таймаут обязателен: svtav1enc в
/// режиме низкой задержки не выдаёт ни кадра и не заканчивается никогда.
const PROBE_TIMEOUT: gst::ClockTime = gst::ClockTime::from_seconds(5);

/// Проверяет кандидатов указанных кодеков.
///
/// Требует инициализированного GStreamer. Пробное кодирование — несколько
/// кадров 320×240; на этой машине оно занимает от единиц до сотен
/// миллисекунд на кандидата, поэтому зовётся из фонового потока, а не из GUI.
pub fn discover(codecs: &[VideoCodec]) -> Discovery {
    let started = Instant::now();
    let probed = candidates()
        .into_iter()
        .filter(|s| codecs.contains(&s.codec))
        .map(|spec| {
            let outcome = probe(&spec);
            log::debug!("кодировщик {}: {}", spec.element, outcome.label());
            Probed { spec, outcome }
        })
        .collect();
    Discovery {
        probed,
        millis: started.elapsed().as_millis() as u32,
    }
}

/// Проверяет один кандидат.
pub fn probe(spec: &EncoderSpec) -> ProbeOutcome {
    if gst::ElementFactory::find(&spec.element).is_none() {
        return if gst::Registry::get().find_plugin(spec.plugin).is_some() {
            ProbeOutcome::NoDevice
        } else {
            ProbeOutcome::PluginMissing
        };
    }
    let cfg = Config {
        fps: 30,
        bitrate: 2000,
        gop: 1.0,
        ..Config::default()
    };
    let desc = format!(
        "videotestsrc num-buffers=5 ! video/x-raw,width=320,height=240,framerate=30/1 \
         ! {up} ! {enc} ! {down} ! fakesink name=probe_sink",
        up = spec.upstream(cfg.fps),
        enc = spec.encoder_desc(&cfg),
        down = spec.downstream(),
    );
    let started = Instant::now();
    let pipeline = match gst::parse::launch(&desc) {
        Ok(p) => p,
        // Обычно — неверное свойство или значение перечисления.
        Err(e) => return ProbeOutcome::InitFailed(e.to_string()),
    };
    let Ok(pipeline) = pipeline.downcast::<gst::Pipeline>() else {
        return ProbeOutcome::InitFailed("parse::launch вернул не Pipeline".into());
    };
    let frames = Arc::new(AtomicUsize::new(0));
    if let Some(pad) = pipeline
        .by_name("probe_sink")
        .and_then(|s| s.static_pad("sink"))
    {
        let f = frames.clone();
        pad.add_probe(gst::PadProbeType::BUFFER, move |_, _| {
            f.fetch_add(1, Ordering::Relaxed);
            gst::PadProbeReturn::Ok
        });
    }
    let outcome = run_probe(&pipeline, &frames);
    let _ = pipeline.set_state(gst::State::Null);
    match outcome {
        Ok(()) => ProbeOutcome::Works {
            millis: started.elapsed().as_millis() as u32,
        },
        Err(o) => o,
    }
}

fn run_probe(pipeline: &gst::Pipeline, frames: &AtomicUsize) -> Result<(), ProbeOutcome> {
    let bus = pipeline
        .bus()
        .ok_or_else(|| ProbeOutcome::InitFailed("нет шины".into()))?;
    let started = pipeline.set_state(gst::State::Playing);
    let msg = bus.timed_pop_filtered(
        if started.is_ok() {
            PROBE_TIMEOUT
        } else {
            gst::ClockTime::ZERO
        },
        &[gst::MessageType::Eos, gst::MessageType::Error],
    );
    match msg.as_ref().map(|m| m.view()) {
        Some(gst::MessageView::Eos(_)) if frames.load(Ordering::Relaxed) > 0 => Ok(()),
        Some(gst::MessageView::Eos(_)) => {
            Err(ProbeOutcome::InitFailed("не выдал ни одного кадра".into()))
        }
        Some(gst::MessageView::Error(e)) => Err(classify(&e.error(), e.debug().as_deref())),
        _ if started.is_err() => Err(ProbeOutcome::InitFailed(
            "конвейер проверки не запустился".into(),
        )),
        _ => Err(ProbeOutcome::InitFailed(format!(
            "не ответил за {} с",
            PROBE_TIMEOUT.seconds()
        ))),
    }
}

/// «not-negotiated» — формат, всё прочее — отказ открытия.
fn classify(err: &gst::glib::Error, debug: Option<&str>) -> ProbeOutcome {
    let text = err.to_string();
    let negotiation = err.matches(gst::CoreError::Negotiation)
        || debug.is_some_and(|d| d.contains("not-negotiated"))
        || text.contains("not-negotiated");
    if negotiation {
        ProbeOutcome::Incompatible(text)
    } else {
        ProbeOutcome::InitFailed(text)
    }
}

/// Какой кодировщик пробовать и почему.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection {
    /// Кандидаты по порядку: первый — основной, остальные — запасные.
    pub order: Vec<EncoderSpec>,
    /// Почему основной — не тот, что выбран вручную.
    pub note: Option<String>,
}

/// Выбирает кодировщик по настройке и итогам обнаружения.
///
/// Критерии автовыбора по порядку: кодек совпадает с запрошенным;
/// пробное кодирование прошло через ту же цепочку, что и захват;
/// аппаратный раньше программного; среди аппаратных — сначала путь без
/// копирования кадра через системную память (VA-API на Linux), затем
/// драйверы производителей с управлением битрейтом, затем универсальные
/// API; low-power VA — последним из аппаратных: у него нет управления
/// битрейтом. О скорости кодировщиков здесь не утверждается ничего.
///
/// `rejected` — кодировщики, которые прошли проверку, но упали в
/// настоящем конвейере: в этот раз их не предлагаем.
pub fn select(cfg: &Config, found: &Discovery, rejected: &[(String, String)]) -> Selection {
    let is_rejected = |el: &str| rejected.iter().any(|(r, _)| r == el);
    let mut note = None;
    let mut order: Vec<EncoderSpec> = Vec::new();
    let codec = match &cfg.encoder {
        Encoder::Element(name) => match found.find(name) {
            Some(p) if p.outcome.works() && !is_rejected(name) => {
                order.push(p.spec.clone());
                p.spec.codec
            }
            Some(p) => {
                let why = rejected
                    .iter()
                    .find(|(r, _)| r == name)
                    .map(|(_, why)| why.clone())
                    .unwrap_or_else(|| p.outcome.label());
                note = Some(format!("{name} недоступен ({why}) — выбран автоматически"));
                p.spec.codec
            }
            None => {
                // Элемент вне таблицы — его выбрали из командной строки.
                // Пробуем как есть, а при отказе вернёмся к автовыбору.
                if is_rejected(name) {
                    // Уже падал в этом запуске — без этой проверки поток
                    // захвата пересобирал бы конвейер с ним бесконечно.
                } else if gst::ElementFactory::find(name).is_some() {
                    if let Some(spec) = custom_spec(name, cfg.video_codec) {
                        order.push(spec);
                    }
                } else {
                    note = Some(format!("элемента {name} нет — выбран автоматически"));
                }
                cfg.video_codec
            }
        },
        Encoder::Auto => cfg.video_codec,
    };
    for spec in found.working(codec) {
        if !is_rejected(&spec.element) && !order.iter().any(|s| s.element == spec.element) {
            order.push(spec.clone());
        }
    }
    if note.is_none() && !rejected.is_empty() {
        if let Some((el, why)) = rejected.last() {
            note = Some(format!("{el} не запустился ({why})"));
        }
    }
    Selection { order, note }
}

/// Кодировщик, которого нет в таблице: программный путь через системную
/// память, без особых свойств.
fn custom_spec(element: &str, codec: VideoCodec) -> Option<EncoderSpec> {
    va_spec(element).or_else(|| {
        Some(EncoderSpec {
            element: element.to_owned(),
            codec,
            backend: Backend::Software,
            hardware: false,
            plugin: "",
            upload: Upload::System("I420"),
            profile: None,
        })
    })
}

/// Кодек, который нужен настройкам: у ручного выбора — кодек элемента.
pub fn codec_for(cfg: &Config) -> VideoCodec {
    match &cfg.encoder {
        Encoder::Element(name) => va_spec(name)
            .map(|s| s.codec)
            .or_else(|| {
                candidates()
                    .into_iter()
                    .find(|s| &s.element == name)
                    .map(|s| s.codec)
            })
            .unwrap_or(cfg.video_codec),
        Encoder::Auto => cfg.video_codec,
    }
}

/// Быстрый выбор без пробного кодирования: первый кандидат, чья фабрика
/// есть в реестре. Для тестов и разовых конвейеров, где подробная
/// проверка не нужна.
pub fn resolve_quick(cfg: &Config) -> Option<EncoderSpec> {
    let all = candidates();
    match &cfg.encoder {
        Encoder::Element(name) => all
            .into_iter()
            .find(|s| &s.element == name)
            .or_else(|| custom_spec(name, cfg.video_codec)),
        Encoder::Auto => {
            let mut ok: Vec<EncoderSpec> = all
                .into_iter()
                .filter(|s| s.codec == cfg.video_codec)
                .filter(|s| gst::ElementFactory::find(&s.element).is_some())
                .collect();
            ok.sort_by_key(|s| s.priority());
            ok.into_iter().next()
        }
    }
}

/// Таблица для `--list-encoders`.
pub fn report(found: &Discovery) -> String {
    let mut out = format!("Обнаружение кодировщиков заняло {} мс.\n", found.millis);
    for codec in VideoCodec::ALL {
        let rows: Vec<&Probed> = found
            .probed
            .iter()
            .filter(|p| p.spec.codec == codec)
            .collect();
        if rows.is_empty() {
            continue;
        }
        out.push_str(&format!("\n{}:\n", codec.label()));
        for p in rows {
            out.push_str(&format!(
                "  {:<22} {:<20} {}\n",
                p.spec.element,
                p.spec.backend.label(),
                p.outcome.label()
            ));
        }
        let working = found.working(codec);
        match working.first() {
            Some(s) => out.push_str(&format!(
                "  автовыбор: {} — {}\n",
                s.element,
                s.rank_reason()
            )),
            None => out.push_str("  автовыбор: нет рабочих кодировщиков\n"),
        }
    }
    out
}

/// Сколько ждать проверки при обнаружении в фоне — для логов и тестов.
pub fn probe_timeout() -> Duration {
    Duration::from_nanos(PROBE_TIMEOUT.nseconds())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(element: &str) -> EncoderSpec {
        gst::init().unwrap();
        candidates()
            .into_iter()
            .find(|s| s.element == element)
            .or_else(|| va_spec(element))
            .unwrap_or_else(|| panic!("нет кандидата {element}"))
    }

    fn found(rows: &[(&str, ProbeOutcome)]) -> Discovery {
        Discovery {
            probed: rows
                .iter()
                .map(|(el, o)| Probed {
                    spec: spec(el),
                    outcome: o.clone(),
                })
                .collect(),
            millis: 0,
        }
    }

    const OK: ProbeOutcome = ProbeOutcome::Works { millis: 1 };

    #[test]
    fn va_names_are_parsed_including_device_specific() {
        let s = va_spec("vah264enc").unwrap();
        assert_eq!((s.codec, s.backend), (VideoCodec::H264, Backend::VaApi));
        let s = va_spec("varenderD129h265lpenc").unwrap();
        assert_eq!(
            (s.codec, s.backend),
            (VideoCodec::H265, Backend::VaApiLowPower)
        );
        assert_eq!(va_spec("vaav1enc").unwrap().codec, VideoCodec::Av1);
        assert!(va_spec("vapostproc").is_none());
        assert!(va_spec("vah264dec").is_none());
        assert!(va_spec("vavp9enc").is_none());
    }

    /// Аппаратный раньше программного, обычный VA раньше low-power.
    #[test]
    fn auto_prefers_hardware_then_software() {
        let d = found(&[
            ("x264enc", OK),
            ("openh264enc", OK),
            ("vah264lpenc", OK),
            ("vah264enc", OK),
            ("x265enc", OK),
        ]);
        let cfg = Config::default();
        let sel = select(&cfg, &d, &[]);
        let names: Vec<&str> = sel.order.iter().map(|s| s.element.as_str()).collect();
        assert_eq!(
            names,
            ["vah264enc", "vah264lpenc", "x264enc", "openh264enc"]
        );
        assert_eq!(sel.note, None);
    }

    /// Не прошедший проверку кандидат в автовыбор не попадает, какой бы
    /// «правильный» производитель за ним ни стоял.
    #[test]
    fn failed_probes_are_skipped() {
        let d = found(&[
            (
                "vah264enc",
                ProbeOutcome::InitFailed("vaInitialize failed".into()),
            ),
            ("vah264lpenc", ProbeOutcome::NoDevice),
            ("openh264enc", OK),
            ("x264enc", ProbeOutcome::PluginMissing),
        ]);
        let sel = select(&Config::default(), &d, &[]);
        let names: Vec<&str> = sel.order.iter().map(|s| s.element.as_str()).collect();
        assert_eq!(names, ["openh264enc"]);
    }

    /// Ручной выбор ставится первым, остальное — запасные.
    #[test]
    fn manual_choice_comes_first_with_fallbacks() {
        let d = found(&[("vah264enc", OK), ("openh264enc", OK)]);
        let cfg = Config {
            encoder: Encoder::Element("openh264enc".into()),
            ..Config::default()
        };
        let sel = select(&cfg, &d, &[]);
        let names: Vec<&str> = sel.order.iter().map(|s| s.element.as_str()).collect();
        assert_eq!(names, ["openh264enc", "vah264enc"]);
        assert_eq!(sel.note, None);
    }

    /// Недоступный ручной выбор не меняет настройку, но честно сообщает,
    /// почему запись идёт другим кодировщиком.
    #[test]
    fn unavailable_manual_choice_falls_back_with_reason() {
        let d = found(&[
            (
                "vah264enc",
                ProbeOutcome::InitFailed("нет устройства".into()),
            ),
            ("openh264enc", OK),
        ]);
        let cfg = Config {
            encoder: Encoder::Element("vah264enc".into()),
            ..Config::default()
        };
        let sel = select(&cfg, &d, &[]);
        assert_eq!(sel.order[0].element, "openh264enc");
        let note = sel.note.unwrap();
        assert!(
            note.contains("vah264enc") && note.contains("нет устройства"),
            "{note}"
        );
    }

    /// Упавший в настоящем конвейере кандидат пропускается до следующего
    /// «Применить», и причина видна.
    #[test]
    fn runtime_rejection_moves_to_next_candidate() {
        let d = found(&[("vah264enc", OK), ("openh264enc", OK)]);
        let rejected = vec![("vah264enc".to_owned(), "GPU hang".to_owned())];
        let sel = select(&Config::default(), &d, &rejected);
        assert_eq!(sel.order.len(), 1);
        assert_eq!(sel.order[0].element, "openh264enc");
        assert!(sel.note.unwrap().contains("GPU hang"));
    }

    /// Регрессия: элемент вне таблицы, упавший в конвейере, предлагался
    /// снова и снова, и поток захвата пересобирал конвейер бесконечно.
    #[test]
    fn rejected_custom_element_is_not_retried() {
        let d = found(&[("openh264enc", OK)]);
        let cfg = Config {
            encoder: Encoder::Element("videoconvert".into()),
            ..Config::default()
        };
        let first = select(&cfg, &d, &[]);
        assert_eq!(first.order[0].element, "videoconvert");
        let rejected = vec![("videoconvert".to_owned(), "не подключить".to_owned())];
        let next = select(&cfg, &d, &rejected);
        let names: Vec<&str> = next.order.iter().map(|s| s.element.as_str()).collect();
        assert_eq!(names, ["openh264enc"]);
        assert!(next.note.unwrap().contains("videoconvert"));
    }

    /// Кодек берётся из настройки; кандидаты другого кодека не подмешиваются.
    #[test]
    fn codec_filter_is_strict() {
        let d = found(&[("vah264enc", OK), ("x265enc", OK)]);
        let cfg = Config {
            video_codec: VideoCodec::H265,
            ..Config::default()
        };
        let sel = select(&cfg, &d, &[]);
        let names: Vec<&str> = sel.order.iter().map(|s| s.element.as_str()).collect();
        assert_eq!(names, ["x265enc"]);
        assert_eq!(d.codecs(), [VideoCodec::H264, VideoCodec::H265]);
    }

    #[test]
    fn no_b_frames_anywhere() {
        let cfg = Config::default();
        for s in candidates() {
            let p = s.properties(&cfg);
            assert!(
                !p.contains("bframes=1") && !p.contains("b-frames=1"),
                "{}: {p}",
                s.element
            );
            // Интервал ключевых кадров задан у всех: иначе буфер нечем резать.
            assert!(
                p.contains("key-int-max=")
                    || p.contains("gop-size=")
                    || p.contains("keyframe-max-dist=")
                    || p.contains("intra-period-length=")
                    || p.contains("max-key-frame-interval="),
                "{}: {p}",
                s.element
            );
        }
    }

    #[test]
    fn missing_plugin_is_distinguished_from_missing_device() {
        gst::init().unwrap();
        let ghost = EncoderSpec {
            element: "replayrsnosuchenc".into(),
            plugin: "replayrsnosuchplugin",
            ..spec("x264enc")
        };
        assert_eq!(probe(&ghost), ProbeOutcome::PluginMissing);
        // Плагин coreelements есть, а такого элемента в нём нет.
        let no_device = EncoderSpec {
            element: "replayrsnosuchenc".into(),
            plugin: "coreelements",
            ..spec("x264enc")
        };
        assert_eq!(probe(&no_device), ProbeOutcome::NoDevice);
    }

    /// Неверное свойство — отказ открытия, а не «не установлен».
    #[test]
    fn bad_property_is_an_init_failure() {
        gst::init().unwrap();
        if gst::ElementFactory::find("openh264enc").is_none() {
            return;
        }
        let mut s = spec("openh264enc");
        s.backend = Backend::Nvenc; // свойства NVENC у openh264enc нет
        assert!(matches!(probe(&s), ProbeOutcome::InitFailed(_)));
    }

    /// Требование профиля, которого кодировщик не умеет, — несовместимость
    /// формата, а не отказ устройства.
    #[test]
    fn caps_mismatch_is_reported_as_incompatible() {
        gst::init().unwrap();
        if gst::ElementFactory::find("openh264enc").is_none() {
            return;
        }
        let s = EncoderSpec {
            profile: Some("high-4:4:4"),
            ..spec("openh264enc")
        };
        let o = probe(&s);
        assert!(matches!(o, ProbeOutcome::Incompatible(_)), "{o:?}");
    }
}
