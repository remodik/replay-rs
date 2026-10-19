//! Конвейер захвата: источник → VA-кодирование → appsink → кольцевой буфер.
//!
//! Порт `build_pipeline`/`Recorder._on_sample` из `reference/replay.py`.

use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;

use crate::audio::{AudioCodec, AudioRingBuffer, AudioSource, TrackKind, TrackPlan};
use crate::config::Config;
use crate::encoders::EncoderSpec;
use crate::ringbuf::{Frame, GopRingBuffer};

pub type SharedBuffer = Arc<Mutex<GopRingBuffer>>;

/// Звуковая дорожка рекордера: буфер плюс caps, с которыми его потом мьюксить.
///
/// У каждой дорожки свой буфер: системный звук и микрофон в раздельном
/// режиме приходят из разных кодировщиков и режутся независимо, хотя и по
/// общей шкале времени конвейера.
#[derive(Clone)]
pub struct SharedAudio {
    pub kind: TrackKind,
    pub buffer: Arc<Mutex<AudioRingBuffer>>,
    /// Caps с выхода кодировщика. Нужны, чтобы appsrc при сохранении отдал
    /// mp4mux ровно то, что пришло, — особенно для Opus с его заголовком.
    pub caps: Arc<Mutex<Option<String>>>,
}

impl SharedAudio {
    pub fn new(kind: TrackKind, seconds: f64, max_bytes: usize) -> Self {
        Self {
            kind,
            buffer: Arc::new(Mutex::new(AudioRingBuffer::new(seconds, max_bytes))),
            caps: Arc::new(Mutex::new(None)),
        }
    }
}

/// Описания источников звука для `gst::parse::launch`.
///
/// Отдельной структурой, чтобы тесты подставляли `audiotestsrc` вместо
/// звукового сервера, а сбой источника можно было изобразить без железа.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioSources {
    pub system: String,
    pub mic: String,
}

impl Default for AudioSources {
    fn default() -> Self {
        Self {
            // @DEFAULT_MONITOR@ следует за устройством вывода по умолчанию,
            // поэтому смена колонок на наушники не требует перезапуска.
            system: if cfg!(windows) {
                "wasapi2src loopback=true".into()
            } else {
                "pulsesrc device=@DEFAULT_MONITOR@".into()
            },
            // Без device pulsesrc берёт источник записи по умолчанию.
            mic: if cfg!(windows) {
                "wasapi2src".into()
            } else {
                "pulsesrc".into()
            },
        }
    }
}

impl AudioSources {
    pub fn get(&self, source: AudioSource) -> &str {
        match source {
            AudioSource::System => &self.system,
            AudioSource::Mic => &self.mic,
        }
    }
}

/// Общий формат до кодировщика: микшер требует одинаковых caps на всех входах.
const AUDIO_CAPS: &str = "audio/x-raw,channels=2,rate=48000";

/// Источник для тестового режима — без экрана и без портала.
pub fn test_source(cfg: &Config) -> String {
    format!(
        "videotestsrc is-live=true pattern=ball ! \
         video/x-raw,width={},height={},framerate={}/1",
        cfg.width, cfg.height, cfg.fps
    )
}

/// Ветка звука по настройкам: системный звук и/или микрофон → appsink.
///
/// `None`, если звук выключен целиком.
pub fn audio_branch(cfg: &Config, codec: AudioCodec) -> Option<String> {
    audio_description(
        &cfg.audio.track_plan(&[]),
        cfg,
        codec,
        &AudioSources::default(),
    )
}

/// Описание всех звуковых дорожек: у каждой свой микшер, кодировщик и appsink.
///
/// В раздельном режиме системный звук и микрофон не встречаются ни в одном
/// элементе: независимые дорожки сводить нельзя. Микшер у дорожки всё равно
/// есть, но смешивает он источник только с тишиной (см. ниже).
///
/// `None`, если дорожек нет.
pub fn audio_description(
    plan: &[TrackPlan],
    cfg: &Config,
    codec: AudioCodec,
    sources: &AudioSources,
) -> Option<String> {
    if plan.is_empty() {
        return None;
    }
    let parser = codec
        .parser()
        .map(|p| format!("{p} ! "))
        .unwrap_or_default();
    let mut parts = Vec::new();
    for track in plan {
        let mix = format!("amix_{}", track.kind.tag());
        // ignore-inactive-pads обязателен: приостановленный микрофон иначе
        // подвешивает весь микшер, ожидая от него буферы.
        parts.push(format!(
            "audiomixer name={mix} ignore-inactive-pads=true ! audioconvert ! audioresample \
             ! {AUDIO_CAPS} ! {enc} ! {parser}\
             appsink name={sink} emit-signals=true sync=false max-buffers=64 drop=false",
            enc = codec.encoder_desc(cfg.audio.bitrate),
            sink = track.kind.sink_name(),
        ));
        // Всегда активный источник тишины — у каждой дорожки свой.
        //
        // Монитор устройства вывода молчит, пока в системе ничего не играет, а
        // приостановленный микрофон не отдаёт буферов вовсе. Без гарантированно
        // живого входа микшер в такие моменты не выдаёт ничего, и у клипа просто
        // не оказывается звуковой дорожки. Тишина в Opus занимает считаные байты.
        // Он же держит дорожку, если источник отвалился посреди записи.
        parts.push(format!(
            "audiotestsrc wave=silence is-live=true ! audioconvert ! audioresample \
             ! {AUDIO_CAPS} ! {mix}."
        ));
        for &source in &track.sources {
            // Источник — в именованном bin, чтобы ошибку любого его элемента
            // можно было отнести к нему и не ронять из-за отвалившегося
            // микрофона всю запись. Тип bin указан явно: голая скобка сразу
            // после ссылки `amix_sys.` читается как bin типа `amix_sys`.
            // `level` после громкости показывает то, что реально попадёт в клип.
            parts.push(format!(
                "bin.( name={name} {desc} ) ! audioconvert ! audioresample \
                 ! {AUDIO_CAPS} ! volume volume={volume} \
                 ! level name={level} interval={interval} post-messages=true ! {mix}.",
                desc = sources.get(source),
                name = source.element_name(),
                volume = cfg.audio.volume(source),
                level = source.level_name(),
                interval = crate::audio::LEVEL_INTERVAL_NS,
            ));
        }
    }
    Some(parts.join(" "))
}

/// Собирает конвейер захвата поверх готового описания источника.
///
/// Кодировщик выбирается быстро, без пробного кодирования
/// (`encoders::resolve_quick`); поток захвата пользуется
/// `build_pipeline_for` с результатом полноценного обнаружения.
pub fn build_pipeline(cfg: &Config, source_desc: &str) -> Result<gst::Pipeline> {
    build_pipeline_with_audio(cfg, source_desc, None)
}

/// То же, но с дополнительной веткой звука в том же конвейере.
///
/// Звук обязан жить в одном конвейере с видео: только так у них общие часы
/// и сравнимые PTS, по которым потом режется клип.
pub fn build_pipeline_with_audio(
    cfg: &Config,
    source_desc: &str,
    audio: Option<&str>,
) -> Result<gst::Pipeline> {
    let enc = crate::encoders::resolve_quick(cfg).with_context(|| {
        format!(
            "нет кодировщика {} для «{}»",
            cfg.video_codec.label(),
            cfg.encoder.as_str()
        )
    })?;
    build_pipeline_for(cfg, &enc, source_desc, audio)
}

/// Строит описание конвейера. Чистая функция — тестируется без GStreamer.
///
/// Цепочку до кодировщика (`EncoderSpec::upstream`) задаёт кодировщик:
/// VA-API получает кадры в VA-памяти без копирования, программные — в
/// системной. Её порядок — преобразователь до videorate — объяснён там же.
pub fn build_description(
    cfg: &Config,
    enc: &EncoderSpec,
    source_desc: &str,
    audio: Option<&str>,
) -> String {
    let desc = format!(
        "{source_desc} ! {up} ! {encoder} ! {down} ! \
         appsink name=sink emit-signals=true sync=false max-buffers=8 drop=false",
        up = enc.upstream(cfg.fps),
        encoder = enc.encoder_desc(cfg),
        down = enc.downstream(),
    );
    match audio {
        Some(a) => format!("{desc} {a}"),
        None => desc,
    }
}

/// Собирает конвейер захвата с указанным кодировщиком.
pub fn build_pipeline_for(
    cfg: &Config,
    enc: &EncoderSpec,
    source_desc: &str,
    audio: Option<&str>,
) -> Result<gst::Pipeline> {
    let desc = build_description(cfg, enc, source_desc, audio);
    log::debug!("pipeline: {desc}");
    let element = gst::parse::launch(&desc).context("не удалось собрать конвейер захвата")?;
    element
        .downcast::<gst::Pipeline>()
        .map_err(|_| anyhow::anyhow!("parse::launch вернул не Pipeline"))
}

/// Подключает appsink конвейера к кольцевому буферу.
pub fn attach_sink(pipeline: &gst::Pipeline, ring: SharedBuffer) -> Result<()> {
    let sink = pipeline
        .by_name("sink")
        .context("в конвейере нет элемента с name=sink")?
        .downcast::<gst_app::AppSink>()
        .map_err(|_| anyhow::anyhow!("элемент sink не является appsink"))?;

    let first = std::sync::atomic::AtomicBool::new(true);
    sink.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample(move |sink| {
                let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                if first.swap(false, std::sync::atomic::Ordering::Relaxed) {
                    if let Some(caps) = sample.caps() {
                        log::debug!("первый видеокадр, caps: {caps}");
                    }
                }
                let gbuf = sample.buffer().ok_or(gst::FlowError::Error)?;
                let map = gbuf.map_readable().map_err(|_| gst::FlowError::Error)?;

                let pts = running_time(&sample, gbuf);
                // Кадр без флага DELTA_UNIT — опорный, с него можно начинать клип.
                let keyframe = !gbuf.flags().contains(gst::BufferFlags::DELTA_UNIT);

                let frame = Frame::new(map.as_slice().to_vec(), pts, keyframe);
                // Мьютекс держим только на время вставки; при панике другого
                // потока продолжать запись всё равно нельзя.
                ring.lock().map_err(|_| gst::FlowError::Error)?.add(frame);
                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );
    Ok(())
}

/// Подключает звуковые appsink всех дорожек к их буферам.
pub fn attach_audio_sinks(pipeline: &gst::Pipeline, tracks: &[SharedAudio]) -> Result<()> {
    for track in tracks {
        attach_audio_sink(pipeline, track.clone())?;
    }
    Ok(())
}

/// Подключает звуковой appsink дорожки к её буферу.
pub fn attach_audio_sink(pipeline: &gst::Pipeline, audio: SharedAudio) -> Result<()> {
    let name = audio.kind.sink_name();
    log::debug!("подключаю звуковой appsink {name}");
    let sink = pipeline
        .by_name(&name)
        .with_context(|| format!("в конвейере нет элемента с name={name}"))?
        .downcast::<gst_app::AppSink>()
        .map_err(|_| anyhow::anyhow!("элемент {name} не является appsink"))?;

    sink.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample(move |sink| {
                let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                // Caps запоминаем с первого же сэмпла и больше не трогаем.
                if let Some(caps) = sample.caps() {
                    let mut slot = audio.caps.lock().map_err(|_| gst::FlowError::Error)?;
                    if slot.is_none() {
                        log::debug!("первый звуковой кадр {:?}, caps: {caps}", audio.kind);
                        *slot = Some(caps.to_string());
                    }
                }
                let gbuf = sample.buffer().ok_or(gst::FlowError::Error)?;
                let map = gbuf.map_readable().map_err(|_| gst::FlowError::Error)?;
                let pts = running_time(&sample, gbuf);
                // Каждый аудиокадр самодостаточен, поэтому keyframe=true.
                audio
                    .buffer
                    .lock()
                    .map_err(|_| gst::FlowError::Error)?
                    .add(Frame::new(map.as_slice().to_vec(), pts, true));
                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );
    Ok(())
}

/// Пиковый уровень из сообщения элемента `level`: какой источник и сколько дБ.
///
/// Берётся максимум по каналам: индикатору важно «есть ли звук», а не баланс.
pub fn level_peak(msg: &gst::MessageRef) -> Option<(AudioSource, f32)> {
    let gst::MessageView::Element(_) = msg.view() else {
        return None;
    };
    let s = msg.structure()?;
    if s.name() != "level" {
        return None;
    }
    let source = AudioSource::from_element_name(msg.src()?.name().as_str())?;
    let peaks = s.get::<gst::glib::ValueArray>("peak").ok()?;
    let peak = peaks
        .iter()
        .filter_map(|v| v.get::<f64>().ok())
        .fold(f64::NEG_INFINITY, f64::max);
    Some((source, peak as f32))
}

/// Время кадра на общей шкале конвейера.
///
/// Брать сырой PTS нельзя: GstVideoEncoder сдвигает выход vah264enc на
/// 1000 часов вперёд (min-pts), чтобы DTS не уходил в минус при переупорядочении
/// кадров. Звуковые кодировщики так не делают, и сырые PTS двух веток
/// оказываются в разных системах отсчёта. Сегмент этот сдвиг знает, поэтому
/// running time у видео и звука совпадает.
fn running_time(sample: &gst::Sample, buffer: &gst::BufferRef) -> i64 {
    sample
        .segment()
        .and_then(|seg| seg.downcast_ref::<gst::ClockTime>().cloned())
        .and_then(|seg| buffer.pts().and_then(|pts| seg.to_running_time(pts)))
        .map(|t| t.nseconds() as i64)
        .unwrap_or_else(monotonic_ns)
}

fn monotonic_ns() -> i64 {
    gst::glib::monotonic_time() * 1000
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(element: &str) -> EncoderSpec {
        gst::init().unwrap();
        crate::encoders::resolve_quick(&Config {
            encoder: crate::config::Encoder::Element(element.into()),
            ..Config::default()
        })
        .unwrap()
    }

    #[test]
    fn vah264enc_uses_vbr_and_gop_in_frames() {
        let cfg = Config {
            fps: 60,
            gop: 1.0,
            bitrate: 15000,
            ..Config::default()
        };
        let d = spec("vah264enc").encoder_desc(&cfg);
        assert!(d.contains("rate-control=vbr"), "{d}");
        assert!(d.contains("key-int-max=60"), "{d}");
        assert!(d.contains("bitrate=15000"), "{d}");
    }

    #[test]
    fn lpenc_falls_back_to_cqp() {
        // vah264lpenc на этом железе не умеет VBR — только постоянный квантователь.
        let d = spec("vah264lpenc").encoder_desc(&Config::default());
        assert!(!d.contains("rate-control"), "{d}");
        assert!(d.contains("qpi="), "{d}");
    }

    /// Регрессия: фильтр частоты кадров сразу после источника матчился только
    /// с системной памятью, а портал отдаёт DMABuf, и конвейер падал в
    /// not-negotiated. Преобразователь обязан стоять до videorate — у любого
    /// кодировщика.
    #[test]
    fn converter_comes_before_videorate() {
        for el in [
            "vah264enc",
            "x264enc",
            "openh264enc",
            "vah265enc",
            "x265enc",
        ] {
            let desc = build_description(&Config::default(), &spec(el), "fakesrc", None);
            let post = desc.find("name=vup").expect("нет преобразователя");
            let rate = desc.find("videorate").expect("нет videorate");
            assert!(
                post < rate,
                "{el}: преобразователь обязан стоять до videorate:\n{desc}"
            );
        }
    }

    /// Любой фильтр с частотой кадров на VA-пути должен нести признак памяти,
    /// иначе он молча исключает DMABuf.
    #[test]
    fn framerate_filter_keeps_memory_feature() {
        for el in ["vah264enc", "vah265enc", "vah264lpenc"] {
            let desc = build_description(&Config::default(), &spec(el), "fakesrc", None);
            for part in desc.split('!').map(str::trim) {
                if part.starts_with("video/x-raw") && part.contains("framerate=") {
                    assert!(
                        part.contains("(memory:"),
                        "{el}: фильтр частоты без признака памяти отсекает DMABuf: {part}"
                    );
                }
            }
        }
    }

    /// Парсер и caps после кодировщика — от его кодека, а профиль high
    /// требуется только там, где он проверен.
    #[test]
    fn downstream_follows_codec() {
        let d = build_description(&Config::default(), &spec("vah265enc"), "fakesrc", None);
        assert!(d.contains("h265parse config-interval=-1"), "{d}");
        assert!(
            d.contains("video/x-h265,stream-format=byte-stream,alignment=au"),
            "{d}"
        );
        let d = build_description(&Config::default(), &spec("openh264enc"), "fakesrc", None);
        assert!(!d.contains("profile=high"), "{d}");
        let d = build_description(&Config::default(), &spec("vah264enc"), "fakesrc", None);
        assert!(d.contains("profile=high"), "{d}");
    }

    #[test]
    fn key_int_never_zero() {
        let cfg = Config {
            fps: 30,
            gop: 0.01,
            ..Config::default()
        };
        assert_eq!(cfg.key_int_max(), 1);
    }
}

#[cfg(test)]
mod audio_desc_tests {
    use super::*;
    use crate::config::AudioMode;

    fn sources() -> AudioSources {
        AudioSources {
            system: "audiotestsrc is-live=true freq=440".into(),
            mic: "audiotestsrc is-live=true freq=880".into(),
        }
    }

    /// Описание звука собирается в конвейер во всех режимах: ссылки на
    /// микшеры, bin источников и имена appsink согласованы.
    #[test]
    fn audio_description_parses_in_every_mode() {
        gst::init().unwrap();
        let Some(codec) = crate::audio::choose_codec(crate::audio::CodecPreference::Auto).codec()
        else {
            return;
        };
        for mode in AudioMode::ALL {
            let mut cfg = Config::default();
            cfg.audio.set_mode(mode);
            let plan = cfg.audio.track_plan(&[]);
            let Some(desc) = audio_description(&plan, &cfg, codec, &sources()) else {
                assert_eq!(mode, AudioMode::Off);
                continue;
            };
            let p = gst::parse::launch(&desc)
                .unwrap_or_else(|e| panic!("{mode:?}: {e}\n{desc}"))
                .downcast::<gst::Pipeline>()
                .unwrap();
            for t in &plan {
                assert!(p.by_name(&t.kind.sink_name()).is_some(), "{mode:?}");
                for s in &t.sources {
                    let bin = p.by_name(&s.element_name()).expect("нет bin источника");
                    assert_eq!(AudioSource::owning(bin.upcast_ref()), Some(*s), "{mode:?}");
                }
            }
        }
    }

    /// Независимые дорожки не встречаются ни в одном микшере.
    #[test]
    fn separate_tracks_never_share_a_mixer() {
        let mut cfg = Config::default();
        cfg.audio.set_mode(AudioMode::Separate);
        let desc = audio_description(
            &cfg.audio.track_plan(&[]),
            &cfg,
            crate::audio::AudioCodec::Opus,
            &sources(),
        )
        .unwrap();
        // Каждый источник ведёт в микшер своей дорожки.
        let sys = desc.find("name=src_sys").unwrap();
        let mic = desc.find("name=src_mic").unwrap();
        let sys_tail = &desc[sys..];
        let mic_tail = &desc[mic..];
        assert!(
            sys_tail[..sys_tail.find(" bin.( ").unwrap_or(sys_tail.len())].contains("amix_sys.")
        );
        assert!(mic_tail.contains("amix_mic."));
        assert!(!sys_tail[..sys_tail.find("amix_sys.").unwrap()].contains("amix_mic"));
        assert_eq!(desc.matches("appsink").count(), 2);
    }
}
