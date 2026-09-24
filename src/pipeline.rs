//! Конвейер захвата: источник → VA-кодирование → appsink → кольцевой буфер.
//!
//! Порт `build_pipeline`/`Recorder._on_sample` из `reference/replay.py`.

use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;

use crate::audio::{AudioCodec, AudioRingBuffer};
use crate::config::{Config, Encoder};
use crate::ringbuf::{Frame, GopRingBuffer};

pub type SharedBuffer = Arc<Mutex<GopRingBuffer>>;

/// Звуковая часть рекордера: буфер плюс caps, с которыми его потом мьюксить.
#[derive(Clone)]
pub struct SharedAudio {
    pub buffer: Arc<Mutex<AudioRingBuffer>>,
    /// Caps с выхода кодировщика. Нужны, чтобы appsrc при сохранении отдал
    /// mp4mux ровно то, что пришло, — особенно для Opus с его заголовком.
    pub caps: Arc<Mutex<Option<String>>>,
}

/// Общий формат до кодировщика: микшер требует одинаковых caps на всех входах.
const AUDIO_CAPS: &str = "audio/x-raw,channels=2,rate=48000";

/// Описание элемента-кодировщика для `gst::parse::launch`.
pub fn encoder_desc(cfg: &Config) -> String {
    let key_int = cfg.key_int_max();
    match cfg.encoder {
        Encoder::Vah264enc => format!(
            "vah264enc rate-control=vbr bitrate={} key-int-max={key_int}",
            cfg.bitrate
        ),
        // У low-power режима на этом железе доступен только CQP.
        Encoder::Vah264lpenc => format!("vah264lpenc qpi=25 qpp=27 key-int-max={key_int}"),
        Encoder::X264enc => format!(
            "x264enc tune=zerolatency speed-preset=ultrafast bitrate={} \
             key-int-max={key_int} bframes=0",
            cfg.bitrate
        ),
    }
}

/// Источник для тестового режима — без экрана и без портала.
pub fn test_source(cfg: &Config) -> String {
    format!(
        "videotestsrc is-live=true pattern=ball ! \
         video/x-raw,width={},height={},framerate={}/1",
        cfg.width, cfg.height, cfg.fps
    )
}

/// Ветка звука: системный звук и/или микрофон → микшер → кодировщик → appsink.
///
/// `None`, если звук выключен целиком.
pub fn audio_branch(cfg: &Config, codec: AudioCodec) -> Option<String> {
    if !cfg.audio.any_enabled() {
        return None;
    }
    let parser = codec
        .parser()
        .map(|p| format!("{p} ! "))
        .unwrap_or_default();
    // ignore-inactive-pads обязателен: приостановленный микрофон иначе
    // подвешивает весь микшер, ожидая от него буферы.
    let mut parts = vec![format!(
        "audiomixer name=amix ignore-inactive-pads=true ! audioconvert ! audioresample \
         ! {AUDIO_CAPS} ! {enc} ! {parser}\
         appsink name=asink emit-signals=true sync=false max-buffers=64 drop=false",
        enc = codec.encoder_desc(cfg.audio.bitrate),
    )];
    // Всегда активный источник тишины.
    //
    // Монитор устройства вывода молчит, пока в системе ничего не играет, а
    // приостановленный микрофон не отдаёт буферов вовсе. Без гарантированно
    // живого входа микшер в такие моменты не выдаёт ничего, и у клипа просто
    // не оказывается звуковой дорожки. Тишина в Opus занимает считаные байты.
    parts.push(format!(
        "audiotestsrc wave=silence is-live=true ! audioconvert ! audioresample \
         ! {AUDIO_CAPS} ! amix."
    ));
    if cfg.audio.system {
        // @DEFAULT_MONITOR@ следует за устройством вывода по умолчанию,
        // поэтому смена колонок на наушники не требует перезапуска.
        parts.push(format!(
            "{source} ! audioconvert ! audioresample \
             ! {AUDIO_CAPS} ! volume volume={} ! amix.",
            cfg.audio.system_volume,
            source = if cfg!(windows) {
                "wasapi2src loopback=true"
            } else {
                "pulsesrc device=@DEFAULT_MONITOR@"
            }
        ));
    }
    if cfg.audio.mic {
        // Без device pulsesrc берёт источник записи по умолчанию.
        parts.push(format!(
            "{source} ! audioconvert ! audioresample \
             ! {AUDIO_CAPS} ! volume volume={} ! amix.",
            cfg.audio.mic_volume,
            source = if cfg!(windows) {
                "wasapi2src"
            } else {
                "pulsesrc"
            }
        ));
    }
    Some(parts.join(" "))
}

/// Собирает конвейер захвата поверх готового описания источника.
pub fn build_pipeline(cfg: &Config, source_desc: &str) -> Result<gst::Pipeline> {
    build_pipeline_with_audio(cfg, source_desc, None)
}

/// То же, но с дополнительной веткой звука в том же конвейере.
///
/// Звук обязан жить в одном конвейере с видео: только так у них общие часы
/// и сравнимые PTS, по которым потом режется клип.
/// Строит описание конвейера. Чистая функция — тестируется без GStreamer.
pub fn build_description(cfg: &Config, source_desc: &str, audio: Option<&str>) -> String {
    // Порядок принципиален: преобразователь стоит ДО videorate.
    //
    // Портал отдаёт кадры как video/x-raw(memory:DMABuf), а фильтр вида
    // video/x-raw,framerate=N/1 без указания памяти матчится только с
    // системной: фичи caps должны совпадать точно. Фильтр сразу после
    // источника ронял конвейер в not-negotiated. vapostproc принимает и
    // DMABuf, и системную память, поэтому импорт делаем первым, а частоту
    // приводим уже в VA-памяти.
    //
    // Так ещё и дешевле: преобразуются только реально пришедшие кадры,
    // а дубликаты до нужной частоты добавляются после.
    let convert = if cfg.encoder.is_va() {
        format!(
            "vapostproc ! video/x-raw(memory:VAMemory),format=NV12 \
             ! videorate ! video/x-raw(memory:VAMemory),format=NV12,framerate={fps}/1",
            fps = cfg.fps
        )
    } else {
        // Программный путь: videoconvert системную память не покидает.
        format!(
            "videoconvert ! video/x-raw,format=I420 \
             ! videorate ! video/x-raw,format=I420,framerate={fps}/1",
            fps = cfg.fps
        )
    };
    let desc = format!(
        "{source_desc} ! {convert} ! {enc} ! \
         h264parse config-interval=-1 ! \
         video/x-h264,stream-format=byte-stream,alignment=au,profile=high ! \
         appsink name=sink emit-signals=true sync=false max-buffers=8 drop=false",
        enc = encoder_desc(cfg),
    );
    match audio {
        Some(a) => format!("{desc} {a}"),
        None => desc,
    }
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
    let desc = build_description(cfg, source_desc, audio);
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

/// Подключает звуковой appsink к буферу.
pub fn attach_audio_sink(pipeline: &gst::Pipeline, audio: SharedAudio) -> Result<()> {
    log::debug!("подключаю звуковой appsink");
    let sink = pipeline
        .by_name("asink")
        .context("в конвейере нет элемента с name=asink")?
        .downcast::<gst_app::AppSink>()
        .map_err(|_| anyhow::anyhow!("элемент asink не является appsink"))?;

    sink.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample(move |sink| {
                let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                // Caps запоминаем с первого же сэмпла и больше не трогаем.
                if let Some(caps) = sample.caps() {
                    let mut slot = audio.caps.lock().map_err(|_| gst::FlowError::Error)?;
                    if slot.is_none() {
                        log::debug!("первый звуковой кадр, caps: {caps}");
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

    #[test]
    fn vah264enc_uses_vbr_and_gop_in_frames() {
        let cfg = Config {
            encoder: Encoder::Vah264enc,
            fps: 60,
            gop: 1.0,
            bitrate: 15000,
            ..Config::default()
        };
        let d = encoder_desc(&cfg);
        assert!(d.contains("rate-control=vbr"), "{d}");
        assert!(d.contains("key-int-max=60"), "{d}");
    }

    #[test]
    fn lpenc_falls_back_to_cqp() {
        // vah264lpenc на этом железе не умеет VBR — только постоянный квантователь.
        let cfg = Config {
            encoder: Encoder::Vah264lpenc,
            ..Config::default()
        };
        let d = encoder_desc(&cfg);
        assert!(!d.contains("rate-control"), "{d}");
        assert!(d.contains("qpi="), "{d}");
    }

    /// Регрессия: фильтр частоты кадров сразу после источника матчился только
    /// с системной памятью, а портал отдаёт DMABuf, и конвейер падал в
    /// not-negotiated. Преобразователь обязан стоять до videorate.
    #[test]
    fn converter_comes_before_videorate() {
        let cfg = Config {
            encoder: Encoder::Vah264enc,
            ..Config::default()
        };
        let desc = build_description(&cfg, "fakesrc", None);
        let post = desc.find("vapostproc").expect("нет vapostproc");
        let rate = desc.find("videorate").expect("нет videorate");
        assert!(
            post < rate,
            "vapostproc обязан стоять до videorate:\n{desc}"
        );
    }

    /// Любой фильтр с частотой кадров должен нести признак памяти, иначе он
    /// молча исключает DMABuf.
    #[test]
    fn framerate_filter_keeps_memory_feature() {
        let cfg = Config {
            encoder: Encoder::Vah264enc,
            ..Config::default()
        };
        let desc = build_description(&cfg, "fakesrc", None);
        for part in desc.split('!').map(str::trim) {
            if part.starts_with("video/x-raw") && part.contains("framerate=") {
                assert!(
                    part.contains("(memory:"),
                    "фильтр частоты без признака памяти отсекает DMABuf: {part}"
                );
            }
        }
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
