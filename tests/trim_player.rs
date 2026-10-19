//! Плеер и обрезка: разбор клипа, быстрая и точная обрезка, отмена,
//! устойчивость к битым файлам.
//!
//! Нужен кодировщик H.264 (на CI — x264enc) и звука (avenc_aac/opusenc).
//! Запуск: `cargo test --test trim_player -- --include-ignored --nocapture`

mod common;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use gstreamer as gst;

use common::{audio_tones, decode_video_frames, inspect_mp4, make_clip, tone_label};
use replay_rs::ringbuf::NS;
use replay_rs::trim::{self, Progress, TrimMode, TrimRequest};
use replay_rs::{media, player};

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("replay-trim-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn secs(s: f64) -> gst::ClockTime {
    gst::ClockTime::from_nseconds((s * NS as f64) as u64)
}

fn tones(path: &Path) -> Vec<&'static str> {
    audio_tones(path)
        .unwrap()
        .into_iter()
        .map(tone_label)
        .collect()
}

fn request(input: &Path, start: f64, end: f64, mode: TrimMode) -> TrimRequest {
    TrimRequest {
        input: input.to_path_buf(),
        output: trim::output_path(input, secs(start), secs(end)),
        start: secs(start),
        end: secs(end),
        mode,
        bitrate_kbps: 1500,
        encoder: None,
    }
}

/// Клип 10 с с двумя дорожками: 440 Гц (как системный звук) и 880 Гц.
fn two_track_clip(dir: &Path) -> PathBuf {
    let clip = dir.join("replay_src.mp4");
    make_clip(&clip, 10, &[440.0, 880.0]).expect("исходный клип не записался");
    clip
}

#[test]
#[ignore = "нужны кодировщики H.264 и звука"]
fn probe_reports_streams_and_keyframes() {
    gst::init().unwrap();
    let dir = temp_dir("probe");
    let clip = two_track_clip(&dir);
    let info = media::probe(&clip).unwrap();
    assert_eq!(info.audio_count(), 2);
    assert_eq!(info.video().unwrap().media, "video/x-h264");
    let d = info.duration.unwrap().nseconds() as f64 / NS as f64;
    assert!((d - 10.0).abs() < 0.1, "{d}");
    // GOP 1 с: ключевой кадр не позже 3,5 с — ровно 3 с.
    for (t, k) in [(3.5, 3.0), (3.0, 3.0), (0.2, 0.0), (9.99, 9.0)] {
        let got = media::keyframe_at_or_before(&clip, secs(t)).unwrap();
        let got = got.nseconds() as f64 / NS as f64;
        assert!((got - k).abs() < 0.02, "{t} → {got}, ожидалось {k}");
    }
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
#[ignore = "нужны кодировщики H.264 и звука"]
fn fast_trim_keeps_tracks_and_starts_on_keyframe() {
    gst::init().unwrap();
    let dir = temp_dir("fast");
    let clip = two_track_clip(&dir);
    let before = std::fs::read(&clip).unwrap();
    let req = request(&clip, 3.5, 7.2, TrimMode::Fast);
    let out = trim::trim(&req, &Arc::new(Progress::default())).expect("быстрая обрезка");
    // Честные границы: начало выровнено по ключевому кадру.
    assert_eq!(out.start, secs(3.0));
    assert_eq!(out.end, secs(7.2));
    assert_eq!(out.audio_tracks, 2);
    let streams = inspect_mp4(&out.output).unwrap();
    assert_eq!(streams.len(), 3, "{streams:#?}");
    assert!(streams[0].starts_with_key);
    let d = streams[0].duration_s();
    assert!((d - 4.2).abs() < 0.05, "видео {d:.3} с вместо 4,2");
    for a in streams.iter().filter(|s| s.is_audio()) {
        assert!(
            (a.first_pts - streams[0].first_pts).abs() < 30_000_000,
            "{a:?}"
        );
        assert!((a.end - streams[0].end).abs() < 60_000_000, "{a:?}");
    }
    assert_eq!(tones(&out.output), ["440", "880"], "дорожки перепутаны");
    assert_eq!(decode_video_frames(&out.output).unwrap(), streams[0].count);
    assert_eq!(std::fs::read(&clip).unwrap(), before, "оригинал изменился");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
#[ignore = "нужны кодировщики H.264 и звука"]
fn precise_trim_cuts_at_the_frame() {
    gst::init().unwrap();
    let dir = temp_dir("precise");
    let clip = two_track_clip(&dir);
    let req = request(&clip, 3.5, 7.2, TrimMode::Precise);
    let out = trim::trim(&req, &Arc::new(Progress::default())).expect("точная обрезка");
    assert_eq!(out.start, secs(3.5));
    let streams = inspect_mp4(&out.output).unwrap();
    assert_eq!(streams.len(), 3, "{streams:#?}");
    let video = &streams[0];
    assert!(video.starts_with_key);
    let d = video.duration_s();
    assert!((d - 3.7).abs() < 0.05, "видео {d:.3} с вместо 3,7");
    for a in streams.iter().filter(|s| s.is_audio()) {
        assert!((a.first_pts - video.first_pts).abs() < 30_000_000, "{a:?}");
        assert!(
            (a.end - video.end).abs() < 60_000_000,
            "{a:?} против {video:?}"
        );
    }
    assert_eq!(tones(&out.output), ["440", "880"]);
    assert_eq!(decode_video_frames(&out.output).unwrap(), video.count);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
#[ignore = "нужны кодировщики H.264 и звука"]
fn single_and_no_audio_clips_trim_too() {
    gst::init().unwrap();
    let dir = temp_dir("few");
    for tones_in in [&[][..], &[440.0][..]] {
        let clip = dir.join(format!("replay_{}.mp4", tones_in.len()));
        make_clip(&clip, 5, tones_in).unwrap();
        for mode in [TrimMode::Fast, TrimMode::Precise] {
            let out = trim::trim(
                &request(&clip, 1.2, 3.0, mode),
                &Arc::new(Progress::default()),
            )
            .unwrap_or_else(|e| panic!("{} дорожек, {mode:?}: {e:#}", tones_in.len()));
            let streams = inspect_mp4(&out.output).unwrap();
            assert_eq!(streams.len(), 1 + tones_in.len(), "{mode:?}");
        }
    }
    std::fs::remove_dir_all(&dir).ok();
}

/// Отмена оставляет папку как была: ни результата, ни огрызка.
#[test]
#[ignore = "нужны кодировщики H.264 и звука"]
fn cancelled_export_leaves_nothing() {
    gst::init().unwrap();
    let dir = temp_dir("cancel");
    let clip = dir.join("replay_long.mp4");
    make_clip(&clip, 30, &[440.0]).unwrap();
    let req = request(&clip, 1.0, 29.0, TrimMode::Precise);
    let progress = Arc::new(Progress::default());
    progress.cancel.store(true, Ordering::Release);
    let err = trim::trim(&req, &progress).unwrap_err();
    assert!(format!("{err:#}").contains("отменено"), "{err:#}");
    assert!(!req.output.exists());
    assert!(!replay_rs::saver::part_path(&req.output).exists());
    std::fs::remove_dir_all(&dir).ok();
}

/// Пустой, обрезанный, случайный и несуществующий файл — ошибка, а не
/// зависание, и у разбора, и у плеера, и у обрезки.
#[test]
fn broken_files_fail_fast() {
    gst::init().unwrap();
    let dir = temp_dir("broken");
    let empty = dir.join("empty.mp4");
    std::fs::write(&empty, b"").unwrap();
    let random = dir.join("random.mp4");
    std::fs::write(
        &random,
        (0..200_000u32)
            .map(|i| (i.wrapping_mul(2654435761) >> 13) as u8)
            .collect::<Vec<_>>(),
    )
    .unwrap();
    let missing = dir.join("missing.mp4");
    let mut cases = vec![empty, random, missing];
    // Недописанный клип (без moov) — если есть чем его сделать.
    let full = dir.join("full.mp4");
    if make_clip(&full, 3, &[]).is_ok() {
        let bytes = std::fs::read(&full).unwrap();
        // faststart: moov в начале, mdat обрезан.
        let cut = dir.join("cut.mp4");
        std::fs::write(&cut, &bytes[..bytes.len() / 2]).unwrap();
        // А без moov вовсе — только хвост mdat.
        let tail = dir.join("tail.mp4");
        std::fs::write(&tail, &bytes[bytes.len() / 2..]).unwrap();
        cases.push(tail);
        // Обрезанный посередине файл может открыться: проверяем только,
        // что разбор не виснет.
        let started = Instant::now();
        let _ = media::probe(&cut);
        assert!(started.elapsed() < Duration::from_secs(12));
    }
    for path in cases {
        let started = Instant::now();
        assert!(
            media::probe(&path).is_err(),
            "{} разобрался",
            path.display()
        );
        assert!(
            player::Player::open(&path, || {}).is_err(),
            "{} открылся",
            path.display()
        );
        let r = trim::trim(
            &request(&path, 0.5, 1.0, TrimMode::Fast),
            &Arc::new(Progress::default()),
        );
        assert!(r.is_err());
        assert!(
            started.elapsed() < Duration::from_secs(15),
            "{} разбирался {:?}",
            path.display(),
            started.elapsed()
        );
    }
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
#[ignore = "нужны кодировщики H.264 и звука"]
fn player_plays_seeks_switches_tracks_and_releases() {
    gst::init().unwrap();
    let dir = temp_dir("player");
    let clip = two_track_clip(&dir);
    // Открыть и закрыть несколько клипов подряд — ресурсы освобождаются.
    for _ in 0..5 {
        let p = player::Player::open(&clip, || {}).unwrap();
        drop(p);
    }
    let frames = Arc::new(AtomicUsize::new(0));
    let f = frames.clone();
    let mut p = player::Player::open(&clip, move || {
        f.fetch_add(1, Ordering::Relaxed);
    })
    .unwrap();
    // На паузе виден первый кадр.
    let deadline = Instant::now() + Duration::from_secs(5);
    let first = loop {
        if let Some(fr) = p.take_frame() {
            break fr;
        }
        assert!(Instant::now() < deadline, "нет первого кадра");
        std::thread::sleep(Duration::from_millis(20));
    };
    assert_eq!((first.width, first.height), (320, 240));
    assert_eq!(first.rgba.len(), 320 * 240 * 4);
    assert_eq!(p.audio_tracks(), 2);
    p.set_audio(1);
    assert_eq!(p.selected_audio(), 1);
    p.set_volume(0.0);
    p.play();
    std::thread::sleep(Duration::from_millis(1200));
    // Выбор применяется, когда пошли данные.
    assert_eq!(p.current_audio(), 1, "вторая дорожка не включилась");
    let pos = p.position().unwrap().nseconds() as f64 / NS as f64;
    assert!(pos > 0.5, "позиция {pos}");
    assert!(frames.load(Ordering::Relaxed) > 10, "кадры не идут");
    p.seek(secs(8.0), true);
    std::thread::sleep(Duration::from_millis(300));
    let pos = p.position().unwrap().nseconds() as f64 / NS as f64;
    assert!(pos >= 7.9, "после перехода {pos}");
    // Предпросмотр диапазона кончается сам.
    p.play_range(secs(2.0), secs(2.5));
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if p.poll().contains(&player::Event::Finished) {
            break;
        }
        assert!(Instant::now() < deadline, "диапазон не закончился");
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(!p.is_playing());
    // Файл удалили во время просмотра — плеер не виснет и закрывается.
    p.play();
    std::fs::remove_file(&clip).unwrap();
    std::thread::sleep(Duration::from_millis(500));
    let _ = p.poll();
    let started = Instant::now();
    drop(p);
    assert!(started.elapsed() < Duration::from_secs(3));
    std::fs::remove_dir_all(&dir).ok();
}

/// Длинный клип: быстрая обрезка конца не перекодирует и укладывается
/// в секунды.
#[test]
#[ignore = "нужны кодировщики H.264 и звука"]
fn fast_trim_of_long_clip_is_quick() {
    gst::init().unwrap();
    let dir = temp_dir("longcut");
    let clip = dir.join("replay_long.mp4");
    make_clip(&clip, 300, &[440.0, 880.0]).unwrap();
    let started = Instant::now();
    let out = trim::trim(
        &request(&clip, 280.0, 295.0, TrimMode::Fast),
        &Arc::new(Progress::default()),
    )
    .unwrap();
    println!("обрезка 5-минутного клипа: {:?}", started.elapsed());
    assert!(started.elapsed() < Duration::from_secs(20));
    let streams = inspect_mp4(&out.output).unwrap();
    assert_eq!(streams.len(), 3);
    assert!((streams[0].duration_s() - 15.0).abs() < 0.1);
    std::fs::remove_dir_all(&dir).ok();
}
