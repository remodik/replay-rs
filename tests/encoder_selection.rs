//! Обнаружение и выбор кодировщика видео.
//!
//! Тесты проходят по кодировщикам, которые реально работают на машине:
//! на CI это x264enc/openh264enc/x265enc, на машине с VA-API — ещё и
//! vah264enc/vah265enc. Для каждого проверяется вся цепочка: захват →
//! кольцевой буфер → mp4 без перекодирования → полное декодирование, в том
//! числе клипа, вырезанного из середины потока (ключевые кадры обязаны быть
//! самостоятельными точками входа).
//!
//! Запуск: `cargo test --test encoder_selection -- --include-ignored --nocapture`

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use gstreamer as gst;
use gstreamer::prelude::*;

use common::{decode_video_frames, inspect_mp4, wait_for};
use replay_rs::config::{Config, Encoder};
use replay_rs::encoders::{self, EncoderSpec, ProbeOutcome, VideoCodec};
use replay_rs::recorder::{Recorder, SaveOutcome};
use replay_rs::ringbuf::{GopRingBuffer, NS};
use replay_rs::service::{CaptureService, CaptureState, Mode};
use replay_rs::{pipeline, saver};

fn temp_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("replay-enc-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Пишет 3 с тестового видео кодировщиком `spec` и сохраняет весь буфер
/// и последнюю секунду — она начинается с ключевого кадра из середины.
fn record_and_save(spec: &EncoderSpec, dir: &std::path::Path) {
    let cfg = Config {
        width: 320,
        height: 240,
        fps: 30,
        gop: 0.5,
        bitrate: 1500,
        ..Config::default()
    };
    let ring = Arc::new(Mutex::new(GopRingBuffer::new(10.0, cfg.max_bytes())));
    let p = pipeline::build_pipeline_for(&cfg, spec, &pipeline::test_source(&cfg), None)
        .unwrap_or_else(|e| panic!("{}: конвейер не собрался: {e:#}", spec.element));
    pipeline::attach_sink(&p, ring.clone()).unwrap();
    p.set_state(gst::State::Playing).unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while ring.lock().unwrap().stats().seconds() < 3.0 {
        assert!(
            std::time::Instant::now() < deadline,
            "{}: буфер не наполнился",
            spec.element
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    p.set_state(gst::State::Null).unwrap();
    let st = ring.lock().unwrap().stats();
    assert!(
        st.gops >= 4,
        "{}: ключевые кадры не распознаются — {} GOP за {:.1} с",
        spec.element,
        st.gops,
        st.seconds()
    );

    let media = match spec.codec {
        VideoCodec::H264 => "video/x-h264",
        VideoCodec::H265 => "video/x-h265",
        VideoCodec::Av1 => "video/x-av1",
    };
    // Срезы — отдельными операторами: временный guard из литерала массива
    // жил бы до конца цикла, и второй lock() ждал бы первый вечно.
    let all = ring.lock().unwrap().snapshot();
    let tail = ring.lock().unwrap().snapshot_last(1.0);
    for (name, frames) in [("all", all), ("tail", tail)] {
        assert!(frames[0].keyframe);
        let out = dir.join(format!("{}-{name}.mp4", spec.element));
        saver::write_clip_as(spec.codec, &frames, cfg.fps, &[], &out)
            .unwrap_or_else(|e| panic!("{} {name}: не сохранилось: {e:#}", spec.element));
        let streams = inspect_mp4(&out).unwrap();
        assert_eq!(streams.len(), 1, "{} {name}: {streams:#?}", spec.element);
        assert_eq!(streams[0].media, media, "{} {name}", spec.element);
        assert!(streams[0].starts_with_key, "{} {name}", spec.element);
        assert_eq!(streams[0].count, frames.len(), "{} {name}", spec.element);
        // Вырезанный из середины клип обязан декодироваться целиком: кадры
        // до первого самостоятельного ключевого декодер бы выбросил.
        let decoded =
            decode_video_frames(&out).unwrap_or_else(|e| panic!("{} {name}: {e:#}", spec.element));
        assert_eq!(
            decoded,
            frames.len(),
            "{} {name}: декодировано кадров",
            spec.element
        );
        println!(
            "{} {name}: {} кадров, {:.2} с — ок",
            spec.element,
            frames.len(),
            (frames[frames.len() - 1].pts - frames[0].pts) as f64 / NS as f64
        );
    }
}

#[test]
fn every_working_encoder_makes_decodable_clips() {
    gst::init().unwrap();
    let found = encoders::discover(&[VideoCodec::H264, VideoCodec::H265]);
    print!("{}", encoders::report(&found));
    let working: Vec<&EncoderSpec> = [VideoCodec::H264, VideoCodec::H265]
        .iter()
        .flat_map(|c| found.working(*c))
        .collect();
    if working.is_empty() {
        println!("пропускаю: ни одного рабочего кодировщика H.264/HEVC");
        return;
    }
    let dir = temp_dir("all");
    for spec in working {
        record_and_save(spec, &dir);
    }
    std::fs::remove_dir_all(&dir).ok();
}

/// AV1 — экспериментальный: в окне его нет, но сохранение обязано работать.
#[test]
#[ignore = "программный AV1 медленный; запускать явно"]
fn av1_software_encoder_makes_decodable_clips() {
    gst::init().unwrap();
    let found = encoders::discover(&[VideoCodec::Av1]);
    let Some(spec) = found.working(VideoCodec::Av1).first().map(|s| (*s).clone()) else {
        println!("пропускаю: нет рабочего кодировщика AV1");
        return;
    };
    let dir = temp_dir("av1");
    record_and_save(&spec, &dir);
    std::fs::remove_dir_all(&dir).ok();
}

/// Отсутствующий плагин, отсутствующее устройство и неработающий элемент
/// различаются и в автовыбор не попадают.
#[test]
fn unusable_candidates_are_classified_and_skipped() {
    gst::init().unwrap();
    let found = encoders::discover(&[VideoCodec::H264, VideoCodec::H265]);
    for p in &found.probed {
        let exists = gst::ElementFactory::find(&p.spec.element).is_some();
        match &p.outcome {
            ProbeOutcome::PluginMissing | ProbeOutcome::NoDevice => assert!(
                !exists,
                "{}: фабрика есть, а отчёт — {:?}",
                p.spec.element, p.outcome
            ),
            _ => assert!(exists, "{}: {:?}", p.spec.element, p.outcome),
        }
        if !p.outcome.works() {
            for codec in VideoCodec::ALL {
                assert!(
                    !found
                        .working(codec)
                        .iter()
                        .any(|s| s.element == p.spec.element),
                    "{} в автовыборе, хотя {:?}",
                    p.spec.element,
                    p.outcome
                );
            }
        }
    }
}

fn start_service(cfg: Config) -> (Arc<Recorder>, CaptureService) {
    let rec = Arc::new(Recorder::new(cfg, None));
    let service = CaptureService::start(rec.clone(), Mode::Test);
    wait_for(
        "запуска захвата",
        Duration::from_secs(30),
        || match service.state() {
            CaptureState::Running => true,
            CaptureState::Failed(e) => panic!("захват не запустился: {e}"),
            _ => false,
        },
    );
    (rec, service)
}

fn capture_config(dir: &std::path::Path) -> Config {
    let mut cfg = Config {
        width: 320,
        height: 240,
        fps: 30,
        bitrate: 1500,
        seconds: 10.0,
        save_seconds: 2.0,
        output: dir.to_path_buf(),
        ..Config::default()
    };
    cfg.audio.system = false;
    cfg.audio.mic = false;
    cfg
}

fn has_working(codec: VideoCodec) -> bool {
    let ok = !encoders::discover(&[codec]).working(codec).is_empty();
    if !ok {
        println!("пропускаю: нет рабочего {}", codec.label());
    }
    ok
}

/// Ручной выбор элемента, который не кодирует, не ломает запись: поток
/// захвата откатывается к автовыбору, настройка не меняется, причина видна.
#[test]
#[ignore = "нужен работающий кодировщик H.264"]
fn broken_manual_encoder_falls_back_and_says_why() {
    gst::init().unwrap();
    if !has_working(VideoCodec::H264) {
        return;
    }
    let dir = temp_dir("fallback");
    let cfg = Config {
        // videoconvert есть везде, но его выход в h264parse не подключить.
        encoder: Encoder::Element("videoconvert".into()),
        ..capture_config(&dir)
    };
    let (rec, mut service) = start_service(cfg.clone());
    let st = rec.encoder_status();
    let active = st.active.expect("нет активного кодировщика");
    assert_ne!(active.element, "videoconvert");
    let note = st.note.expect("нет объяснения отката");
    assert!(note.contains("videoconvert"), "{note}");
    assert_eq!(
        rec.config().encoder,
        cfg.encoder,
        "настройка изменилась сама"
    );
    wait_for(
        "наполнения буфера",
        Duration::from_secs(15),
        || rec.stats().seconds() >= 2.0,
    );
    assert!(matches!(rec.save(None).unwrap(), SaveOutcome::Saved(_)));
    println!("откат на {} — {note}", active.element);
    service.stop();
    std::fs::remove_dir_all(&dir).ok();
}

/// Ручной выбор кодировщика, которого на машине нет, тоже не ломает запись.
#[test]
#[ignore = "нужен работающий кодировщик H.264"]
fn missing_manual_encoder_falls_back() {
    gst::init().unwrap();
    if !has_working(VideoCodec::H264) {
        return;
    }
    let dir = temp_dir("missing");
    let cfg = Config {
        encoder: Encoder::Element("replayrsnosuchenc".into()),
        ..capture_config(&dir)
    };
    let (rec, mut service) = start_service(cfg);
    let st = rec.encoder_status();
    assert!(st.note.unwrap().contains("replayrsnosuchenc"));
    assert_eq!(st.active.unwrap().codec, VideoCodec::H264);
    service.stop();
    std::fs::remove_dir_all(&dir).ok();
}

/// HEVC через поток захвата: автовыбор, сохранение, декодирование.
#[test]
#[ignore = "нужен работающий кодировщик HEVC"]
fn hevc_capture_saves_hevc_clip() {
    gst::init().unwrap();
    if !has_working(VideoCodec::H265) {
        return;
    }
    let dir = temp_dir("hevc");
    let cfg = Config {
        video_codec: VideoCodec::H265,
        ..capture_config(&dir)
    };
    let (rec, mut service) = start_service(cfg);
    assert_eq!(rec.encoder_status().active.unwrap().codec, VideoCodec::H265);
    wait_for(
        "наполнения буфера",
        Duration::from_secs(15),
        || rec.stats().seconds() >= 2.5,
    );
    let SaveOutcome::Saved(clip) = rec.save(None).unwrap() else {
        panic!("не сохранилось")
    };
    let streams = inspect_mp4(&clip).unwrap();
    assert_eq!(streams[0].media, "video/x-h265");
    assert_eq!(decode_video_frames(&clip).unwrap(), streams[0].count);
    service.stop();
    std::fs::remove_dir_all(&dir).ok();
}
