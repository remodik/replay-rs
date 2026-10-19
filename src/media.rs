//! Что лежит в mp4: потоки, длительность, ключевые кадры.
//!
//! Всё через qtdemux без декодирования: разбор занимает миллисекунды и не
//! зависит от длины клипа (индекс читается из moov).

use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{bail, Context, Result};
use gstreamer as gst;
use gstreamer::prelude::*;

/// Сколько ждём разбора файла.
const PROBE_TIMEOUT: gst::ClockTime = gst::ClockTime::from_seconds(10);

/// Один поток клипа.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamInfo {
    /// Имя пэда qtdemux: `video_0`, `audio_0`, `audio_1`.
    pub pad: String,
    /// Имя структуры caps: `video/x-h264`, `audio/mpeg`, `audio/x-opus`.
    pub media: String,
}

impl StreamInfo {
    pub fn is_video(&self) -> bool {
        self.media.starts_with("video/")
    }
    pub fn is_audio(&self) -> bool {
        self.media.starts_with("audio/")
    }
}

/// Сводка о клипе.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaInfo {
    pub duration: Option<gst::ClockTime>,
    pub streams: Vec<StreamInfo>,
}

impl MediaInfo {
    pub fn audio_count(&self) -> usize {
        self.streams.iter().filter(|s| s.is_audio()).count()
    }
    pub fn video(&self) -> Option<&StreamInfo> {
        self.streams.iter().find(|s| s.is_video())
    }
}

/// Разбирает mp4: потоки и длительность. Ошибка — для пустого, битого или
/// недописанного файла (без moov), а не зависание.
pub fn probe(path: &Path) -> Result<MediaInfo> {
    let meta = std::fs::metadata(path)
        .with_context(|| format!("не удалось прочитать {}", path.display()))?;
    if meta.len() == 0 {
        bail!("файл пуст");
    }
    let pipeline = gst::Pipeline::new();
    let src = gst::ElementFactory::make("filesrc")
        .build()
        .context("нет filesrc")?;
    crate::platform::set_location(&src, path)?;
    let demux = gst::ElementFactory::make("qtdemux")
        .build()
        .context("нет qtdemux (gst-plugins-good)")?;
    pipeline.add_many([&src, &demux])?;
    src.link(&demux)?;

    let streams: Arc<Mutex<Vec<StreamInfo>>> = Arc::new(Mutex::new(Vec::new()));
    let weak = pipeline.downgrade();
    let st = streams.clone();
    demux.connect_pad_added(move |_, pad| {
        let Some(pipeline) = weak.upgrade() else {
            return;
        };
        let media = pad
            .current_caps()
            .and_then(|c| c.structure(0).map(|s| s.name().to_string()))
            .unwrap_or_default();
        st.lock()
            .expect("список потоков отравлен")
            .push(StreamInfo {
                pad: pad.name().to_string(),
                media,
            });
        attach_fakesink(&pipeline, pad);
    });

    let result = (|| -> Result<Option<gst::ClockTime>> {
        pipeline
            .set_state(gst::State::Paused)
            .context("файл не открылся")?;
        wait_async_done(&pipeline)?;
        Ok(pipeline.query_duration::<gst::ClockTime>())
    })();
    let _ = pipeline.set_state(gst::State::Null);
    let duration = result?;
    let streams = std::mem::take(&mut *streams.lock().expect("список потоков отравлен"));
    if streams.is_empty() {
        bail!("в файле нет потоков");
    }
    Ok(MediaInfo { duration, streams })
}

/// Время ключевого кадра не позже `t`: с него начнётся быстрая обрезка.
///
/// Точный (ACCURATE) переход qtdemux начинает выдачу видео с ключевого
/// кадра перед `t`, поэтому первый видеобуфер после перехода — ответ.
pub fn keyframe_at_or_before(path: &Path, t: gst::ClockTime) -> Result<gst::ClockTime> {
    let pipeline = gst::Pipeline::new();
    let src = gst::ElementFactory::make("filesrc").build()?;
    crate::platform::set_location(&src, path)?;
    let demux = gst::ElementFactory::make("qtdemux").build()?;
    pipeline.add_many([&src, &demux])?;
    src.link(&demux)?;

    // Первый видеобуфер после сегмента перехода.
    // None — переход ещё не начат; Some(None) — переход сбросил поток,
    // ждём первый буфер; Some(Some(t)) — ответ.
    let found: Arc<Mutex<Option<Option<gst::ClockTime>>>> = Arc::new(Mutex::new(None));
    let armed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let weak = pipeline.downgrade();
    let (f, a) = (found.clone(), armed.clone());
    demux.connect_pad_added(move |_, pad| {
        let Some(pipeline) = weak.upgrade() else {
            return;
        };
        attach_fakesink(&pipeline, pad);
        if !pad.name().starts_with("video") {
            return;
        }
        let (f, a) = (f.clone(), a.clone());
        // Буферы, ушедшие до перехода, ответом не считаются: проба
        // взводится только после FLUSH_STOP самого перехода.
        pad.add_probe(
            gst::PadProbeType::BUFFER | gst::PadProbeType::EVENT_FLUSH,
            move |_, info| {
                if let Some(gst::PadProbeData::Event(e)) = &info.data {
                    if e.type_() == gst::EventType::FlushStop
                        && a.load(std::sync::atomic::Ordering::Acquire)
                    {
                        *f.lock().expect("ключевой кадр отравлен") = Some(None);
                    }
                    return gst::PadProbeReturn::Ok;
                }
                if let Some(buf) = info.buffer() {
                    let mut slot = f.lock().expect("ключевой кадр отравлен");
                    if let Some(None) = *slot {
                        *slot = Some(buf.pts());
                    }
                }
                gst::PadProbeReturn::Ok
            },
        );
    });

    let result = (|| -> Result<gst::ClockTime> {
        pipeline.set_state(gst::State::Paused)?;
        wait_async_done(&pipeline)?;
        armed.store(true, std::sync::atomic::Ordering::Release);
        pipeline
            .seek_simple(gst::SeekFlags::FLUSH | gst::SeekFlags::ACCURATE, t)
            .context("переход по файлу не удался")?;
        wait_async_done(&pipeline)?;
        let k = *found.lock().expect("ключевой кадр отравлен");
        k.flatten()
            .context("в файле нет видеокадров после перехода")
    })();
    let _ = pipeline.set_state(gst::State::Null);
    result
}

/// Подключает к пэду qtdemux очередь и fakesink — чтобы конвейер дошёл
/// до PAUSED.
///
/// Очередь обязательна: у qtdemux один поток, и fakesink без очереди
/// останавливал его на первом же буфере (preroll), а остальные приёмники
/// так и не получали своего — PAUSED не наступал никогда.
fn attach_fakesink(pipeline: &gst::Pipeline, pad: &gst::Pad) {
    let (Ok(queue), Ok(sink)) = (
        gst::ElementFactory::make("queue").build(),
        gst::ElementFactory::make("fakesink").build(),
    ) else {
        return;
    };
    if pipeline.add_many([&queue, &sink]).is_err() || queue.link(&sink).is_err() {
        return;
    }
    let _ = queue.sync_state_with_parent();
    let _ = sink.sync_state_with_parent();
    if let Some(sp) = queue.static_pad("sink") {
        let _ = pad.link(&sp);
    }
}

/// Ждёт ASYNC_DONE (конвейер дошёл до PAUSED) или ошибку.
fn wait_async_done(pipeline: &gst::Pipeline) -> Result<()> {
    let bus = pipeline.bus().context("нет шины")?;
    match bus.timed_pop_filtered(
        PROBE_TIMEOUT,
        &[gst::MessageType::AsyncDone, gst::MessageType::Error],
    ) {
        Some(msg) => match msg.view() {
            gst::MessageView::Error(e) => bail!("{}", e.error()),
            _ => Ok(()),
        },
        None => bail!("файл не разобрался за {} с", PROBE_TIMEOUT.seconds()),
    }
}
