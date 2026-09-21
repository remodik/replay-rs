//! Регрессия: сохранение большого клипа со звуком вставало намертво.
//!
//! appsrc по умолчанию держит в очереди 200 КБ. Если сложить в него всё видео
//! до того, как в mp4mux попадёт хоть один звуковой кадр, мультиплексор
//! перестаёт забирать видео (ему нечем выравнивать дорожки), очередь
//! упирается в лимит, и push_buffer блокируется навсегда.
//!
//! Тесты этого не ловили: клипы из videotestsrc pattern=ball выходили на
//! ~166 КБ и целиком помещались в очередь. Реальные 30 с при 15 Мбит/с —
//! это 56 МБ.
//!
//! Запуск: `cargo test --test large_clip_deadlock -- --ignored --nocapture`

use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use gstreamer as gst;
use gstreamer::prelude::*;

use replay_rs::audio::{self, AudioRingBuffer};
use replay_rs::config::Config;
use replay_rs::pipeline::SharedAudio;
use replay_rs::ringbuf::GopRingBuffer;
use replay_rs::{pipeline, saver};

/// Столько ждём сохранения, прежде чем счесть его зависшим.
const SAVE_TIMEOUT: Duration = Duration::from_secs(60);

#[test]
#[ignore = "нужен VA-API энкодер и звуковой сервер"]
fn large_clip_with_audio_does_not_deadlock() {
    gst::init().expect("GStreamer не инициализировался");
    let codec = audio::choose_codec(audio::CodecPreference::Auto)
        .codec()
        .expect("нет аудиокодека");

    let dir = std::env::temp_dir().join(format!("replay-big-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();

    let mut cfg = Config {
        fps: 60,
        gop: 1.0,
        // Высокий битрейт плюс шум — чтобы гарантированно перевалить за
        // 200 КБ очереди appsrc.
        bitrate: 40000,
        seconds: 20.0,
        width: 1920,
        height: 1080,
        output: dir.clone(),
        ..Config::default()
    };
    cfg.audio.system = true;

    let ring = Arc::new(Mutex::new(GopRingBuffer::new(cfg.seconds, cfg.max_bytes())));
    let sa = SharedAudio {
        buffer: Arc::new(Mutex::new(AudioRingBuffer::new(30.0, 16 << 20))),
        caps: Arc::new(Mutex::new(None)),
    };

    // pattern=snow почти не сжимается — быстро набираем объём.
    let source = format!(
        "videotestsrc is-live=true pattern=snow ! video/x-raw,width={},height={},framerate={}/1",
        cfg.width, cfg.height, cfg.fps
    );
    let abranch = pipeline::audio_branch(&cfg, codec).expect("ветка звука не собралась");
    let p = pipeline::build_pipeline_with_audio(&cfg, &source, Some(&abranch))
        .expect("конвейер не собрался");
    pipeline::attach_sink(&p, ring.clone()).unwrap();
    pipeline::attach_audio_sink(&p, sa.clone()).unwrap();

    p.set_state(gst::State::Playing).unwrap();
    std::thread::sleep(Duration::from_secs(8));
    p.set_state(gst::State::Null).unwrap();

    let vframes = ring.lock().unwrap().snapshot();
    let bytes: usize = vframes.iter().map(|f| f.data.len()).sum();
    println!("видео: {} кадров, {:.1} КБ", vframes.len(), bytes as f64 / 1e3);
    assert!(
        bytes > 200_000,
        "нужен клип больше очереди appsrc (200 КБ), получено {bytes} Б"
    );

    let aframes = sa
        .buffer
        .lock()
        .unwrap()
        .snapshot_range(vframes[0].pts, vframes[vframes.len() - 1].pts);
    let caps = sa.caps.lock().unwrap().clone().expect("нет caps звука");
    println!("звук: {} кадров", aframes.len());
    assert!(!aframes.is_empty(), "нужен звук, иначе тест не про то");

    let clip = dir.join("big.mp4");
    let (tx, rx) = mpsc::channel();
    let c = clip.clone();
    std::thread::spawn(move || {
        let r = saver::write_clip_with_audio(
            &vframes,
            60,
            Some(saver::AudioTrack { frames: &aframes, caps: &caps }),
            &c,
        );
        let _ = tx.send(r);
    });

    match rx.recv_timeout(SAVE_TIMEOUT) {
        Ok(Ok(())) => {
            let size = std::fs::metadata(&clip).expect("клипа нет").len();
            println!("сохранено: {:.1} КБ", size as f64 / 1e3);
            assert!(size > 200_000, "клип подозрительно мал: {size} Б");
        }
        Ok(Err(e)) => panic!("сохранение вернуло ошибку: {e:#}"),
        Err(_) => panic!(
            "сохранение зависло: за {} с оно не завершилось — \
             это и есть взаимная блокировка appsrc и mp4mux",
            SAVE_TIMEOUT.as_secs()
        ),
    }
    std::fs::remove_dir_all(&dir).ok();
}
