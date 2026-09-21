//! Регрессия: пересборка конвейера обнуляет running time.
//!
//! Без очистки буфера старые кадры смешиваются с новыми: длительность
//! становится отрицательной, срез — неупорядоченным, а клип из него
//! непригодным.
//!
//! Запуск: `cargo test --test restart_timeline -- --ignored --nocapture`

use std::sync::Arc;
use std::time::Duration;

use gstreamer as gst;

use replay_rs::config::Config;
use replay_rs::recorder::{Recorder, SaveOutcome};
use replay_rs::service::{CaptureService, Mode};

#[test]
#[ignore = "нужен VA-API энкодер"]
fn restart_keeps_the_timeline_usable() {
    gst::init().expect("GStreamer не инициализировался");

    let dir = std::env::temp_dir().join(format!("replay-rst-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();

    let mut cfg = Config {
        fps: 60,
        gop: 1.0,
        bitrate: 8000,
        seconds: 20.0,
        save_seconds: 5.0,
        width: 1280,
        height: 720,
        output: dir.clone(),
        ..Config::default()
    };
    // Без звука: проверяем именно шкалу времени видео.
    cfg.audio.system = false;
    cfg.audio.mic = false;

    let rec = Arc::new(Recorder::new(cfg, None));
    let mut service = CaptureService::start(rec.clone(), Mode::Test);

    std::thread::sleep(Duration::from_secs(6));
    let before = rec.stats();
    println!("до пересборки: {:.1} с, {} GOP", before.seconds(), before.gops);
    assert!(before.nanos > 0, "буфер не наполнился");

    service.restart();
    std::thread::sleep(Duration::from_secs(6));

    let after = rec.stats();
    println!("после пересборки: {:.1} с, {} GOP", after.seconds(), after.gops);
    assert!(
        after.nanos >= 0,
        "длительность буфера ушла в минус ({} нс) — старые кадры смешались с новыми",
        after.nanos
    );

    // Главный инвариант: в срезе не должно быть кадров из двух шкал времени.
    // Проверять только длительность мало — она остаётся положительной, если
    // старейшим в буфере оказался кадр уже новой шкалы.
    let snap = rec.ring.lock().unwrap().snapshot();
    let pts: Vec<i64> = snap.iter().map(|f| f.pts).collect();
    let mut sorted = pts.clone();
    sorted.sort_unstable();
    assert_eq!(
        pts, sorted,
        "метки в буфере неупорядочены: кадры до и после пересборки смешались"
    );
    // И столько же кадров, сколько успело прийти после пересборки, а не вдвое.
    assert!(
        after.gops <= before.gops + 2,
        "в буфере остались GOP'ы до пересборки: было {}, стало {}",
        before.gops,
        after.gops
    );

    // И главное — из него должен получаться валидный клип.
    match rec.save(None).expect("сохранение упало") {
        SaveOutcome::Saved(path) => {
            let size = std::fs::metadata(&path).unwrap().len();
            println!("клип после пересборки: {:.1} КБ", size as f64 / 1e3);
            assert!(size > 10_000, "клип подозрительно мал: {size} Б");
        }
        other => panic!("после пересборки не сохранилось: {other:?}"),
    }

    service.stop();
    std::fs::remove_dir_all(&dir).ok();
}
