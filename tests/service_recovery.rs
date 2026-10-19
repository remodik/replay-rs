//! Регрессия: после ошибки конвейера поток захвата завершался.
//!
//! Дальше «Применить» с исправленными настройками отправляло Restart в
//! закрытый канал, ошибка отправки молча глоталась, а окно сообщало, что
//! конвейер пересобирается. Запись оживала только после перезапуска
//! приложения.
//!
//! Нужен H.264-кодировщик: x264enc, а без него — VA-API.
//! Запуск: `cargo test --test service_recovery -- --ignored --nocapture`

use std::sync::Arc;
use std::time::{Duration, Instant};

use gstreamer as gst;

use replay_rs::config::{Config, Encoder};
use replay_rs::recorder::Recorder;
use replay_rs::service::{CaptureService, CaptureState, Mode};

fn wait_for(what: &str, timeout: Duration, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while !done() {
        assert!(Instant::now() < deadline, "не дождались: {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Автовыбор: x264enc, VA-API или openh264 — что есть на машине.
fn encoder() -> Encoder {
    Encoder::Auto
}

#[test]
#[ignore = "нужен H.264-кодировщик (x264enc или VA-API)"]
fn apply_after_pipeline_failure_restarts_capture() {
    gst::init().unwrap();
    let mut good = Config {
        encoder: encoder(),
        width: 320,
        height: 240,
        fps: 30,
        bitrate: 2000,
        ..Config::default()
    };
    good.audio.system = false;
    good.audio.mic = false;
    // Кадр нулевой ширины ни один преобразователь не принимает: конвейер
    // не собирается, поток захвата уходит в Failed.
    let broken = Config {
        width: 0,
        ..good.clone()
    };

    let rec = Arc::new(Recorder::new(broken, None));
    let mut service = CaptureService::start(rec.clone(), Mode::Test);
    wait_for(
        "ошибки конвейера",
        Duration::from_secs(10),
        || matches!(service.state(), CaptureState::Failed(_)),
    );
    println!("ожидаемая ошибка: {:?}", service.state());

    // Так поступает окно на «Применить».
    assert!(rec.update_config(good), "смена размера требует пересборки");
    // Несколько нажатий подряд — допустимо, достаточно одной пересборки.
    service.restart();
    service.restart();
    wait_for(
        "восстановления записи",
        Duration::from_secs(15),
        || service.state() == CaptureState::Running && rec.stats().frames > 10,
    );

    let started = Instant::now();
    service.stop();
    assert_eq!(service.state(), CaptureState::Stopped);
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "остановка заняла {:?}",
        started.elapsed()
    );
}

#[test]
#[ignore = "нужен H.264-кодировщик (x264enc или VA-API)"]
fn stop_while_failed_does_not_hang() {
    gst::init().unwrap();
    let mut cfg = Config {
        encoder: encoder(),
        width: 0,
        height: 240,
        fps: 30,
        ..Config::default()
    };
    cfg.audio.system = false;
    cfg.audio.mic = false;
    let rec = Arc::new(Recorder::new(cfg, None));
    let mut service = CaptureService::start(rec, Mode::Test);
    wait_for(
        "ошибки конвейера",
        Duration::from_secs(10),
        || matches!(service.state(), CaptureState::Failed(_)),
    );
    // Поток теперь ждёт команды; stop обязан его разбудить и дождаться.
    let started = Instant::now();
    service.stop();
    assert_eq!(service.state(), CaptureState::Stopped);
    assert!(started.elapsed() < Duration::from_secs(5));
}
