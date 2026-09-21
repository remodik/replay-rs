//! Сквозной тест: кодирование → кольцевой буфер → mp4 → превью.
//!
//! Требует VA-API (Intel renderD129), поэтому помечен `#[ignore]`.
//! Запуск: `cargo test --test capture_to_clip -- --ignored --nocapture`

use std::sync::{Arc, Mutex};
use std::time::Duration;

use gstreamer as gst;
use gstreamer::prelude::*;

use replay_rs::audio::{self, AudioRingBuffer};
use replay_rs::config::Config;
use replay_rs::pipeline::SharedAudio;
use replay_rs::ringbuf::{GopRingBuffer, NS};
use replay_rs::{pipeline, saver, thumbs};

/// Сколько секунд реально писать.
const CAPTURE_SECS: u64 = 5;
/// Сколько секунд вырезать из буфера.
const CLIP_SECS: f64 = 3.0;

#[test]
#[ignore = "нужен VA-API энкодер"]
fn capture_encodes_saves_and_thumbnails() {
    gst::init().expect("GStreamer не инициализировался");

    let dir = std::env::temp_dir().join(format!("replay-it-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();

    let cfg = Config {
        fps: 60,
        gop: 1.0,
        bitrate: 8000,
        seconds: 10.0,
        width: 1280,
        height: 720,
        output: dir.clone(),
        ..Config::default()
    };

    let ring = Arc::new(Mutex::new(GopRingBuffer::new(cfg.seconds, cfg.max_bytes())));
    let source = pipeline::test_source(&cfg);
    let p = pipeline::build_pipeline(&cfg, &source).expect("конвейер не собрался");
    pipeline::attach_sink(&p, ring.clone()).expect("appsink не подключился");

    p.set_state(gst::State::Playing).expect("конвейер не запустился");
    std::thread::sleep(Duration::from_secs(CAPTURE_SECS));
    p.set_state(gst::State::Null).expect("конвейер не остановился");

    let stats = ring.lock().unwrap().stats();
    assert!(stats.frames > 0, "энкодер не отдал ни одного кадра");
    assert!(stats.gops >= 2, "ожидалось несколько GOP, получено {}", stats.gops);

    let frames = ring.lock().unwrap().snapshot_last(CLIP_SECS);
    assert!(frames[0].keyframe, "срез буфера обязан начинаться с keyframe");
    let span = (frames[frames.len() - 1].pts - frames[0].pts) as f64 / NS as f64;
    assert!(span >= CLIP_SECS, "срез короче запрошенного: {span}");

    let clip = dir.join("clip.mp4");
    saver::write_clip(&frames, cfg.fps, &clip).expect("клип не записался");
    let size = std::fs::metadata(&clip).expect("клипа нет").len();
    assert!(size > 10_000, "подозрительно маленький клип: {size} байт");

    let thumb = thumbs::ensure_thumb(&clip).expect("превью не собралось");
    let bytes = std::fs::read(&thumb).expect("превью не читается");
    // JPEG начинается с FF D8 FF.
    assert_eq!(&bytes[..3], &[0xFF, 0xD8, 0xFF], "превью не похоже на JPEG");

    // Пропорции превью должны совпадать с исходником. Без pixel-aspect-ratio=1/1
    // videoscale оставлял исходную высоту и выдавал сплющенный кадр.
    let (tw, th) = image::image_dimensions(&thumb).expect("размеры превью не читаются");
    let expected_h = tw * cfg.height / cfg.width;
    assert_eq!(
        th, expected_h,
        "превью {tw}x{th} не сохраняет пропорции {}x{}",
        cfg.width, cfg.height
    );

    println!(
        "клип: {} ({:.1} КБ, {:.2} с, {} кадров), превью: {} ({} Б)",
        clip.display(),
        size as f64 / 1e3,
        span,
        frames.len(),
        thumb.display(),
        bytes.len()
    );
    println!("оставляю файлы для ffprobe: {}", dir.display());
}

/// Видео плюс системный звук в одном конвейере: общие часы, общий срез.
#[test]
#[ignore = "нужен VA-API энкодер и звуковой сервер"]
fn capture_with_audio_produces_two_tracks() {
    gst::init().expect("GStreamer не инициализировался");

    // Auto: тест должен работать и на AAC, и на Opus.
    let codec = audio::choose_codec(audio::CodecPreference::Auto)
        .codec()
        .expect("нет ни avenc_aac, ни opusenc");
    println!("аудиокодек: {}", codec.as_str());

    let dir = std::env::temp_dir().join(format!("replay-av-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();

    let mut cfg = Config {
        fps: 60,
        gop: 1.0,
        bitrate: 8000,
        seconds: 10.0,
        width: 1280,
        height: 720,
        output: dir.clone(),
        ..Config::default()
    };
    cfg.audio.system = true;
    cfg.audio.mic = false;

    let ring = Arc::new(Mutex::new(GopRingBuffer::new(cfg.seconds, cfg.max_bytes())));
    let shared_audio = SharedAudio {
        buffer: Arc::new(Mutex::new(AudioRingBuffer::new(20.0, 8 << 20))),
        caps: Arc::new(Mutex::new(None)),
    };

    let source = pipeline::test_source(&cfg);
    let abranch = pipeline::audio_branch(&cfg, codec).expect("ветка звука не собралась");
    let p = pipeline::build_pipeline_with_audio(&cfg, &source, Some(&abranch))
        .expect("конвейер не собрался");
    pipeline::attach_sink(&p, ring.clone()).expect("видео-appsink не подключился");
    pipeline::attach_audio_sink(&p, shared_audio.clone()).expect("аудио-appsink не подключился");

    p.set_state(gst::State::Playing).expect("конвейер не запустился");
    std::thread::sleep(Duration::from_secs(CAPTURE_SECS));
    p.set_state(gst::State::Null).expect("конвейер не остановился");

    let vframes = ring.lock().unwrap().snapshot_last(CLIP_SECS);
    assert!(vframes[0].keyframe, "срез видео обязан начинаться с keyframe");

    let a_stats = shared_audio.buffer.lock().unwrap().stats();
    assert!(a_stats.frames > 0, "звуковых кадров не пришло");
    let aframes = shared_audio
        .buffer
        .lock()
        .unwrap()
        .snapshot_range(vframes[0].pts, vframes[vframes.len() - 1].pts);
    assert!(!aframes.is_empty(), "звук не покрывает окно клипа");

    // Регрессия: пока кадры метились сырым PTS, видео уезжало на 1000 часов
    // вперёд (min-pts у GstVideoEncoder), и в окно попадал ровно один
    // звуковой кадр. Звук обязан покрывать почти весь клип.
    let v_span = vframes[vframes.len() - 1].pts - vframes[0].pts;
    let a_span = aframes[aframes.len() - 1].pts - aframes[0].pts;
    assert!(
        a_span * 10 >= v_span * 9,
        "звук покрывает {:.2} с из {:.2} с видео",
        a_span as f64 / NS as f64,
        v_span as f64 / NS as f64
    );
    // Звук обязан начинаться не позже видео, иначе в начале будет тишина.
    assert!(
        aframes[0].pts <= vframes[0].pts,
        "звук начинается позже видео: {} > {}",
        aframes[0].pts,
        vframes[0].pts
    );

    let caps = shared_audio.caps.lock().unwrap().clone().expect("caps звука не заполнились");
    let clip = dir.join("clip_av.mp4");
    saver::write_clip_with_audio(
        &vframes,
        cfg.fps,
        Some(saver::AudioTrack { frames: &aframes, caps: &caps }),
        &clip,
    )
    .expect("клип со звуком не записался");

    let size = std::fs::metadata(&clip).expect("клипа нет").len();
    assert!(size > 10_000, "подозрительно маленький клип: {size} байт");
    println!(
        "клип со звуком: {} ({:.1} КБ), видео {} кадров, звук {} кадров",
        clip.display(),
        size as f64 / 1e3,
        vframes.len(),
        aframes.len()
    );
    println!("оставляю для ffprobe: {}", dir.display());
}
