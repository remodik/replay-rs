//! Общие помощники интеграционных тестов: разбор готового mp4 и
//! закодированный звук без звукового сервера.

#![allow(dead_code)]

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;

use replay_rs::audio::AudioCodec;
use replay_rs::ringbuf::{Frame, NS};

/// Один поток готового mp4, как его видит qtdemux.
#[derive(Debug, Clone)]
pub struct Stream {
    /// Имя структуры caps: `video/x-h264`, `audio/mpeg`, `audio/x-opus`.
    pub media: String,
    pub first_pts: i64,
    /// Конец последнего кадра: его PTS плюс длительность.
    pub end: i64,
    pub count: usize,
    /// Первый кадр — опорный (у звука всегда так).
    pub starts_with_key: bool,
}

impl Stream {
    pub fn is_video(&self) -> bool {
        self.media.starts_with("video/")
    }
    pub fn is_audio(&self) -> bool {
        self.media.starts_with("audio/")
    }
    pub fn duration_s(&self) -> f64 {
        (self.end - self.first_pts) as f64 / NS as f64
    }
}

/// Разбирает mp4 через qtdemux и возвращает потоки в порядке дорожек.
///
/// Читает все кадры, поэтому только для тестов: размер клипа ограничен.
pub fn inspect_mp4(path: &Path) -> anyhow::Result<Vec<Stream>> {
    let pipeline = gst::Pipeline::new();
    let src = gst::ElementFactory::make("filesrc")
        .property("location", path.to_str().unwrap())
        .build()?;
    let demux = gst::ElementFactory::make("qtdemux").build()?;
    pipeline.add_many([&src, &demux])?;
    src.link(&demux)?;

    let streams: Arc<Mutex<Vec<(String, Stream)>>> = Arc::new(Mutex::new(Vec::new()));
    let weak = pipeline.downgrade();
    let st = streams.clone();
    demux.connect_pad_added(move |_, pad| {
        let Some(pipeline) = weak.upgrade() else {
            return;
        };
        let pad_name = pad.name().to_string();
        let media = pad
            .current_caps()
            .and_then(|c| c.structure(0).map(|s| s.name().to_string()))
            .unwrap_or_default();
        st.lock().unwrap().push((
            pad_name.clone(),
            Stream {
                media,
                first_pts: -1,
                end: 0,
                count: 0,
                starts_with_key: false,
            },
        ));
        // async=false: иначе приёмник, добавленный на ходу, снова уводит
        // конвейер в preroll, а единственный поток qtdemux застревает на
        // первом же приёмнике, пока остальные ждут своего буфера.
        let sink = gst_app::AppSink::builder()
            .sync(false)
            .async_(false)
            .build();
        let st = st.clone();
        sink.set_callbacks(
            gst_app::AppSinkCallbacks::builder()
                .new_sample(move |sink| {
                    let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                    let buf = sample.buffer().ok_or(gst::FlowError::Error)?;
                    // Running time, а не сырой PTS: позднее начало дорожки
                    // mp4mux пишет пустой правкой (edit list), и qtdemux
                    // отражает его только в сегменте — буферы всё равно с нуля.
                    let pts = sample
                        .segment()
                        .and_then(|s| s.downcast_ref::<gst::ClockTime>().cloned())
                        .zip(buf.pts())
                        .and_then(|(seg, pts)| seg.to_running_time(pts))
                        .or(buf.pts())
                        .map(|t| t.nseconds() as i64)
                        .unwrap_or(0);
                    let dur = buf.duration().map(|t| t.nseconds() as i64).unwrap_or(0);
                    let mut all = st.lock().unwrap();
                    let (_, s) = all.iter_mut().find(|(n, _)| *n == pad_name).unwrap();
                    if s.count == 0 {
                        s.first_pts = pts;
                        s.starts_with_key = !buf.flags().contains(gst::BufferFlags::DELTA_UNIT);
                    }
                    s.count += 1;
                    s.end = s.end.max(pts + dur);
                    Ok(gst::FlowSuccess::Ok)
                })
                .build(),
        );
        pipeline.add(&sink).unwrap();
        sink.sync_state_with_parent().unwrap();
        pad.link(&sink.static_pad("sink").unwrap()).unwrap();
    });

    pipeline.set_state(gst::State::Playing)?;
    let bus = pipeline.bus().unwrap();
    let msg = bus.timed_pop_filtered(
        gst::ClockTime::from_seconds(60),
        &[gst::MessageType::Eos, gst::MessageType::Error],
    );
    pipeline.set_state(gst::State::Null)?;
    match msg.as_ref().map(|m| m.view()) {
        Some(gst::MessageView::Eos(_)) => {}
        Some(gst::MessageView::Error(e)) => {
            anyhow::bail!("qtdemux: {} ({:?})", e.error(), e.debug())
        }
        _ => anyhow::bail!("разбор {} не закончился за 60 с", path.display()),
    }
    let out = streams
        .lock()
        .unwrap()
        .iter()
        .map(|(_, s)| s.clone())
        .collect();
    Ok(out)
}

/// Кодирует `seconds` секунд тона в AAC или Opus — как это делает конвейер
/// захвата, но без звукового сервера и без живых часов.
pub fn encode_audio(codec: AudioCodec, seconds: f64, freq: f64) -> (Vec<Frame>, String) {
    let parser = codec
        .parser()
        .map(|p| format!("{p} ! "))
        .unwrap_or_default();
    let samples = (seconds * 48_000.0) as u64;
    let desc = format!(
        "audiotestsrc freq={freq} num-buffers={n} samplesperbuffer=1024 \
         ! audio/x-raw,channels=2,rate=48000 ! audioconvert ! {enc} ! {parser}\
         appsink name=out sync=false",
        n = samples.div_ceil(1024),
        enc = codec.encoder_desc(128),
    );
    let pipeline = gst::parse::launch(&desc)
        .unwrap()
        .downcast::<gst::Pipeline>()
        .unwrap();
    let sink = pipeline
        .by_name("out")
        .unwrap()
        .downcast::<gst_app::AppSink>()
        .unwrap();
    pipeline.set_state(gst::State::Playing).unwrap();
    let mut frames = Vec::new();
    let mut caps = None;
    while let Some(sample) = sink.try_pull_sample(gst::ClockTime::from_seconds(10)) {
        if caps.is_none() {
            caps = sample.caps().map(|c| c.to_string());
        }
        let buf = sample.buffer().unwrap();
        let map = buf.map_readable().unwrap();
        let pts = buf.pts().map(|t| t.nseconds() as i64).unwrap_or(0);
        frames.push(Frame::new(map.as_slice().to_vec(), pts, true));
    }
    pipeline.set_state(gst::State::Null).unwrap();
    (frames, caps.expect("кодировщик звука не отдал caps"))
}

/// Ждёт условия не дольше `timeout`.
pub fn wait_for(what: &str, timeout: Duration, mut done: impl FnMut() -> bool) {
    let deadline = std::time::Instant::now() + timeout;
    while !done() {
        assert!(std::time::Instant::now() < deadline, "не дождались: {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// SPS + PPS + IDR 64x64 из vah264enc, byte-stream, один access unit.
pub const IDR_64X64: &[u8] = &[
    0x00, 0x00, 0x00, 0x01, 0x09, 0xF0, 0x00, 0x00, 0x00, 0x01, 0x27, 0x64, 0x00, 0x0A, 0xAC, 0x56,
    0x41, 0x09, 0xBF, 0xF0, 0x00, 0x10, 0x00, 0x11, 0x00, 0x00, 0x03, 0x00, 0x01, 0x00, 0x00, 0x03,
    0x00, 0x3C, 0x9D, 0xA0, 0x80, 0x42, 0x58, 0x00, 0x00, 0x00, 0x01, 0x28, 0xEE, 0x3C, 0xB0, 0x00,
    0x00, 0x00, 0x01, 0x25, 0xB8, 0x10, 0x06, 0x42, 0x7F, 0xDE, 0x08, 0xE5, 0xF1, 0x03, 0xCD, 0xFF,
    0x9B, 0x80, 0x3A, 0xDC, 0x95, 0xBE, 0x3D, 0x1B, 0x16, 0x02, 0x62, 0xBA, 0xD8, 0x4C, 0xC5, 0xF0,
    0xCA, 0xC6, 0xAC, 0x0B, 0x7C,
];

/// Видео из одних IDR-кадров: кодировщик для него не нужен.
pub fn synthetic_video(seconds: f64, fps: u32) -> Vec<Frame> {
    let n = (seconds * fps as f64) as i64;
    (0..n)
        .map(|i| Frame::new(IDR_64X64, i * NS / fps as i64, true))
        .collect()
}

/// Отсчёты одной дорожки, по имени пэда qtdemux.
type Pcm = (String, Vec<f32>);

/// Энергия тонов 440 и 880 Гц в каждой звуковой дорожке mp4, по порядку.
///
/// Тесты подают системный звук тоном 440 Гц, а микрофон — 880 Гц: так по
/// содержимому файла видно, какой источник попал в какую дорожку.
pub fn audio_tones(path: &Path) -> anyhow::Result<Vec<(f64, f64)>> {
    let pipeline = gst::Pipeline::new();
    let src = gst::ElementFactory::make("filesrc")
        .property("location", path.to_str().unwrap())
        .build()?;
    let demux = gst::ElementFactory::make("qtdemux").build()?;
    pipeline.add_many([&src, &demux])?;
    src.link(&demux)?;
    let samples: Arc<Mutex<Vec<Pcm>>> = Arc::new(Mutex::new(Vec::new()));
    let weak = pipeline.downgrade();
    let st = samples.clone();
    demux.connect_pad_added(move |_, pad| {
        let Some(pipeline) = weak.upgrade() else {
            return;
        };
        let name = pad.name().to_string();
        if !name.starts_with("audio") {
            let sink = gst::ElementFactory::make("fakesink")
                .property("async", false)
                .build()
                .unwrap();
            pipeline.add(&sink).unwrap();
            sink.sync_state_with_parent().unwrap();
            pad.link(&sink.static_pad("sink").unwrap()).unwrap();
            return;
        }
        st.lock().unwrap().push((name.clone(), Vec::new()));
        // Без автоматических ghost-пэдов: у audioconvert за decodebin вход
        // тоже ещё не подключён, и его ghost-пэд перехватил бы имя «sink».
        let bin = gst::parse::bin_from_description(
            "queue name=in ! decodebin ! audioconvert ! audioresample \
             ! audio/x-raw,format=F32LE,channels=1,rate=48000 ! appsink name=pcm sync=false async=false",
            false,
        )
        .unwrap();
        let input = bin.by_name("in").unwrap().static_pad("sink").unwrap();
        bin.add_pad(&gst::GhostPad::with_target(&input).unwrap())
            .unwrap();
        let sink = bin
            .by_name("pcm")
            .unwrap()
            .downcast::<gst_app::AppSink>()
            .unwrap();
        let st = st.clone();
        sink.set_callbacks(
            gst_app::AppSinkCallbacks::builder()
                .new_sample(move |s| {
                    let sample = s.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                    let buf = sample.buffer().ok_or(gst::FlowError::Error)?;
                    let map = buf.map_readable().map_err(|_| gst::FlowError::Error)?;
                    let mut all = st.lock().unwrap();
                    let (_, v) = all.iter_mut().find(|(n, _)| *n == name).unwrap();
                    v.extend(
                        map.as_slice()
                            .as_chunks::<4>()
                            .0
                            .iter()
                            .map(|c| f32::from_le_bytes(*c)),
                    );
                    Ok(gst::FlowSuccess::Ok)
                })
                .build(),
        );
        pipeline.add(&bin).unwrap();
        bin.sync_state_with_parent().unwrap();
        pad.link(&bin.pads()[0]).unwrap();
    });
    pipeline.set_state(gst::State::Playing)?;
    let bus = pipeline.bus().unwrap();
    let msg = bus.timed_pop_filtered(
        gst::ClockTime::from_seconds(60),
        &[gst::MessageType::Eos, gst::MessageType::Error],
    );
    pipeline.set_state(gst::State::Null)?;
    match msg.as_ref().map(|m| m.view()) {
        Some(gst::MessageView::Eos(_)) => {}
        Some(gst::MessageView::Error(e)) => anyhow::bail!("{} ({:?})", e.error(), e.debug()),
        _ => anyhow::bail!("разбор звука {} не закончился", path.display()),
    }
    let all = samples.lock().unwrap();
    Ok(all
        .iter()
        .map(|(_, pcm)| (goertzel(pcm, 440.0), goertzel(pcm, 880.0)))
        .collect())
}

/// Средняя мощность частоты `freq` в сигнале 48 кГц (алгоритм Гёрцеля).
fn goertzel(pcm: &[f32], freq: f64) -> f64 {
    const BLOCK: usize = 4800;
    let k = 2.0 * (2.0 * std::f64::consts::PI * freq / 48_000.0).cos();
    let mut total = 0.0;
    let mut blocks = 0;
    for block in pcm.as_chunks::<BLOCK>().0 {
        let (mut s1, mut s2) = (0.0f64, 0.0f64);
        for &x in block {
            let s0 = x as f64 + k * s1 - s2;
            s2 = s1;
            s1 = s0;
        }
        total += (s1 * s1 + s2 * s2 - k * s1 * s2) / (BLOCK * BLOCK) as f64;
        blocks += 1;
    }
    if blocks == 0 {
        0.0
    } else {
        total / blocks as f64
    }
}

/// Какой тон в дорожке: «440», «880», «оба» или «тишина».
pub fn tone_label((a, b): (f64, f64)) -> &'static str {
    const FLOOR: f64 = 1e-4;
    match (a > FLOOR, b > FLOOR) {
        (true, true) if a > 20.0 * b => "440",
        (true, true) if b > 20.0 * a => "880",
        (true, true) => "оба",
        (true, false) => "440",
        (false, true) => "880",
        (false, false) => "тишина",
    }
}

/// Декодирует mp4 целиком и считает видеокадры. Ошибка декодера — ошибка.
pub fn decode_video_frames(path: &Path) -> anyhow::Result<usize> {
    let pipeline = gst::Pipeline::new();
    let src = gst::ElementFactory::make("filesrc")
        .property("location", path.to_str().unwrap())
        .build()?;
    let decode = gst::ElementFactory::make("decodebin").build()?;
    pipeline.add_many([&src, &decode])?;
    src.link(&decode)?;
    let frames = Arc::new(Mutex::new(0usize));
    let weak = pipeline.downgrade();
    let f = frames.clone();
    decode.connect_pad_added(move |_, pad| {
        let Some(pipeline) = weak.upgrade() else {
            return;
        };
        let is_video = pad
            .current_caps()
            .and_then(|c| c.structure(0).map(|s| s.name().starts_with("video/")))
            .unwrap_or(false);
        let sink = gst_app::AppSink::builder()
            .sync(false)
            .async_(false)
            .build();
        let f = f.clone();
        sink.set_callbacks(
            gst_app::AppSinkCallbacks::builder()
                .new_sample(move |s| {
                    let _ = s.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                    if is_video {
                        *f.lock().unwrap() += 1;
                    }
                    Ok(gst::FlowSuccess::Ok)
                })
                .build(),
        );
        pipeline.add(&sink).unwrap();
        sink.sync_state_with_parent().unwrap();
        pad.link(&sink.static_pad("sink").unwrap()).unwrap();
    });
    pipeline.set_state(gst::State::Playing)?;
    let bus = pipeline.bus().unwrap();
    let msg = bus.timed_pop_filtered(
        gst::ClockTime::from_seconds(60),
        &[gst::MessageType::Eos, gst::MessageType::Error],
    );
    pipeline.set_state(gst::State::Null)?;
    match msg.as_ref().map(|m| m.view()) {
        Some(gst::MessageView::Eos(_)) => Ok(*frames.lock().unwrap()),
        Some(gst::MessageView::Error(e)) => {
            anyhow::bail!("декодирование: {} ({:?})", e.error(), e.debug())
        }
        _ => anyhow::bail!("декодирование {} не закончилось за 60 с", path.display()),
    }
}

/// Готовый клип как из replay-rs: H.264 (автовыбор кодировщика), GOP 1 с,
/// 30 к/с, и по звуковой дорожке AAC/Opus на каждый тон из `tones`.
pub fn make_clip(path: &Path, seconds: u32, tones: &[f64]) -> anyhow::Result<()> {
    use replay_rs::config::Config;
    let cfg = Config {
        fps: 30,
        gop: 1.0,
        bitrate: 1500,
        ..Config::default()
    };
    let enc = replay_rs::encoders::resolve_quick(&cfg)
        .ok_or_else(|| anyhow::anyhow!("нет кодировщика H.264"))?;
    let codec = replay_rs::audio::choose_codec(replay_rs::audio::CodecPreference::Auto)
        .codec()
        .ok_or_else(|| anyhow::anyhow!("нет кодировщика звука"))?;
    let frames = seconds * 30;
    let mut desc = format!(
        "videotestsrc num-buffers={frames} pattern=ball ! video/x-raw,width=320,height=240,framerate=30/1 \
         ! {conv} ! {enc} ! h264parse ! mp4mux name=mux faststart=true ! filesink name=out",
        conv = enc.convert(),
        enc = enc.encoder_desc(&cfg),
    );
    for f in tones {
        let parser = codec
            .parser()
            .map(|p| format!("{p} ! "))
            .unwrap_or_default();
        desc.push_str(&format!(
            " audiotestsrc freq={f} num-buffers={n} samplesperbuffer=1024 \
             ! audio/x-raw,rate=48000,channels=2 ! audioconvert ! {e} ! {parser}mux.",
            n = (seconds as u64 * 48_000).div_ceil(1024),
            e = codec.encoder_desc(128),
        ));
    }
    let pipeline = gst::parse::launch(&desc)?
        .downcast::<gst::Pipeline>()
        .map_err(|_| anyhow::anyhow!("не Pipeline"))?;
    replay_rs::platform::set_file_location(&pipeline, "out", path)?;
    pipeline.set_state(gst::State::Playing)?;
    let bus = pipeline.bus().unwrap();
    let msg = bus.timed_pop_filtered(
        gst::ClockTime::from_seconds(120),
        &[gst::MessageType::Eos, gst::MessageType::Error],
    );
    pipeline.set_state(gst::State::Null)?;
    match msg.as_ref().map(|m| m.view()) {
        Some(gst::MessageView::Eos(_)) => Ok(()),
        Some(gst::MessageView::Error(e)) => anyhow::bail!("{} ({:?})", e.error(), e.debug()),
        _ => anyhow::bail!("клип не записался за 120 с"),
    }
}
