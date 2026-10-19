//! Раздельные звуковые дорожки: системный звук и микрофон пишутся
//! независимо и попадают в mp4 отдельными потоками.
//!
//! Тесты без кодировщика видео (синтетические IDR-кадры) идут всегда и
//! проверяют мьюксинг: число дорожек, синхронность и отсутствие взаимной
//! блокировки на длинном клипе. Сквозные тесты через `CaptureService`
//! требуют H.264-кодировщика и помечены `#[ignore]`; звуковой сервер им не
//! нужен — источники подменены на `audiotestsrc`.
//!
//! Запуск всех: `cargo test --test audio_tracks -- --include-ignored --nocapture`

mod common;

use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;

use gstreamer as gst;

use common::{
    audio_tones, encode_audio, inspect_mp4, synthetic_video, tone_label, wait_for, Stream,
};
use replay_rs::audio::{self, AudioCodec, AudioSource, SourceStatus};
use replay_rs::config::{AudioMode, Config, Encoder};
use replay_rs::pipeline::AudioSources;
use replay_rs::recorder::{Recorder, SaveOutcome};
use replay_rs::ringbuf::NS;
use replay_rs::saver::{self, AudioTrack};
use replay_rs::service::{CaptureService, CaptureState, Mode};

const FPS: u32 = 60;

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("replay-tracks-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Кодек звука. Без кодировщика тесты обязаны падать, а не тихо «проходить»;
/// пропустить их можно только явно: REPLAY_SKIP_AUDIO_TESTS=1.
fn audio_codec() -> Option<AudioCodec> {
    let c = audio::choose_codec(audio::CodecPreference::Auto).codec();
    if c.is_none() {
        assert!(
            std::env::var_os("REPLAY_SKIP_AUDIO_TESTS").is_some(),
            "нет ни avenc_aac, ни opusenc (пропустить: REPLAY_SKIP_AUDIO_TESTS=1)"
        );
        println!("пропускаю: нет ни avenc_aac, ни opusenc");
    }
    c
}

/// Тоны дорожек файла по порядку: «440», «880», «оба», «тишина».
fn tones(path: &std::path::Path) -> Vec<&'static str> {
    audio_tones(path)
        .unwrap()
        .into_iter()
        .map(tone_label)
        .collect()
}

/// Видео и звуковые дорожки идут вровень: старт и конец каждой дорожки
/// отличаются от видео не больше чем на пару звуковых кадров.
fn assert_in_sync(streams: &[Stream]) {
    let video = streams.iter().find(|s| s.is_video()).expect("нет видео");
    // Последний видеокадр длится 1/fps, звук режется кадрами по ~21 мс, а
    // задержку кодировщика AAC (1024 отсчёта) mp4 описывает правкой. Итого
    // звук может кончиться раньше видео не больше чем на видеокадр плюс
    // звуковой кадр. Без ожидания догоняющего звука (Recorder::wait_for_audio)
    // на живой машине разрыв был ~110 мс — этот допуск его ловит.
    let frame = NS / 30 + 25_000_000;
    for (i, a) in streams.iter().filter(|s| s.is_audio()).enumerate() {
        let start = a.first_pts - video.first_pts;
        assert!(
            start.abs() <= 30_000_000,
            "дорожка {i} начинается на {:+.3} с от видео",
            start as f64 / NS as f64
        );
        let lag = video.end - a.end;
        assert!(
            lag <= frame && lag >= -30_000_000,
            "дорожка {i} кончается на {:+.3} с раньше видео ({:.3} с против {:.3} с)",
            lag as f64 / NS as f64,
            a.duration_s(),
            video.duration_s()
        );
    }
}

/// Длинный клип с двумя дорожками: каждая дорожка в своём потоке заливки,
/// иначе mp4mux, выравнивающий дорожки по времени, встал бы намертво.
#[test]
fn long_clip_with_two_tracks_does_not_deadlock() {
    gst::init().unwrap();
    let Some(codec) = audio_codec() else { return };
    let seconds = 120.0;
    let video = synthetic_video(seconds, FPS);
    let bytes: usize = video.iter().map(|f| f.data.len()).sum();
    assert!(bytes > 400_000, "видео меньше очереди appsrc: {bytes} Б");
    let (sys, sys_caps) = encode_audio(codec, seconds, 440.0);
    let (mic, mic_caps) = encode_audio(codec, seconds, 880.0);

    let dir = temp_dir("long");
    let out = dir.join("long.mp4");
    let (tx, rx) = mpsc::channel();
    let o = out.clone();
    std::thread::spawn(move || {
        let tracks = [
            AudioTrack {
                frames: &sys,
                caps: &sys_caps,
            },
            AudioTrack {
                frames: &mic,
                caps: &mic_caps,
            },
        ];
        let _ = tx.send(saver::write_clip_with_tracks(&video, FPS, &tracks, &o));
    });
    match rx.recv_timeout(Duration::from_secs(90)) {
        Ok(r) => r.expect("сохранение вернуло ошибку"),
        Err(_) => panic!("сохранение с двумя дорожками зависло"),
    }

    let streams = inspect_mp4(&out).unwrap();
    println!("{streams:#?}");
    assert_eq!(streams.iter().filter(|s| s.is_video()).count(), 1);
    assert_eq!(streams.iter().filter(|s| s.is_audio()).count(), 2);
    assert!(streams[0].starts_with_key);
    assert!(
        (streams[0].duration_s() - seconds).abs() < 0.1,
        "видео {:.2} с",
        streams[0].duration_s()
    );
    assert_in_sync(&streams);
    assert_eq!(
        tones(&out),
        ["440", "880"],
        "дорожки перепутаны или смешаны"
    );
    assert!(
        !saver::part_path(&out).exists(),
        "временный файл не переименован"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// Сколько дорожек дали — столько и в файле, в том же порядке.
#[test]
fn track_count_matches_input() {
    gst::init().unwrap();
    let Some(codec) = audio_codec() else { return };
    let video = synthetic_video(3.0, FPS);
    let (a, a_caps) = encode_audio(codec, 3.0, 440.0);
    let (b, b_caps) = encode_audio(codec, 3.0, 880.0);
    let dir = temp_dir("count");
    for n in 0..=2usize {
        let all = [
            AudioTrack {
                frames: &a,
                caps: &a_caps,
            },
            AudioTrack {
                frames: &b,
                caps: &b_caps,
            },
        ];
        let out = dir.join(format!("clip{n}.mp4"));
        saver::write_clip_with_tracks(&video, FPS, &all[..n], &out).unwrap();
        let streams = inspect_mp4(&out).unwrap();
        assert_eq!(
            streams.iter().filter(|s| s.is_audio()).count(),
            n,
            "{streams:#?}"
        );
        assert!(streams[0].is_video(), "видео обязано идти первой дорожкой");
        assert_in_sync(&streams);
        // Порядок дорожек в файле — порядок на входе.
        assert_eq!(tones(&out), ["440", "880"][..n].to_vec());
    }
    // Пустая дорожка — не ошибка: её просто нет в файле.
    let out = dir.join("empty-track.mp4");
    let empty = [
        AudioTrack {
            frames: &a,
            caps: &a_caps,
        },
        AudioTrack {
            frames: &[],
            caps: &b_caps,
        },
    ];
    saver::write_clip_with_tracks(&video, FPS, &empty, &out).unwrap();
    let streams = inspect_mp4(&out).unwrap();
    assert_eq!(streams.iter().filter(|s| s.is_audio()).count(), 1);
    std::fs::remove_dir_all(&dir).ok();
}

// ---------------------------------------------------------------------------
// Сквозные тесты через поток захвата.

/// Автовыбор: x264enc, VA-API или openh264 — что есть на машине.
fn h264_encoder() -> Encoder {
    Encoder::Auto
}

fn test_sources() -> AudioSources {
    AudioSources {
        system: "audiotestsrc is-live=true freq=440".into(),
        mic: "audiotestsrc is-live=true freq=880".into(),
    }
}

fn capture_config(dir: &std::path::Path, mode: AudioMode) -> Config {
    let mut cfg = Config {
        encoder: h264_encoder(),
        width: 320,
        height: 240,
        fps: 30,
        bitrate: 2000,
        seconds: 10.0,
        save_seconds: 3.0,
        output: dir.to_path_buf(),
        ..Config::default()
    };
    cfg.audio.set_mode(mode);
    cfg
}

fn start(cfg: Config, sources: AudioSources) -> (Arc<Recorder>, CaptureService) {
    let codec = audio::choose_codec(cfg.audio.codec).codec();
    let rec = Arc::new(Recorder::new(cfg, codec));
    let service = CaptureService::start_with_audio(rec.clone(), Mode::Test, sources);
    wait_for(
        "запуска захвата",
        Duration::from_secs(15),
        || match service.state() {
            CaptureState::Running => true,
            CaptureState::Failed(e) => panic!("захват не запустился: {e}"),
            _ => false,
        },
    );
    (rec, service)
}

/// Ждёт, пока в видеобуфере наберётся `seconds` секунд, и сохраняет клип.
fn save_after(rec: &Recorder, seconds: f64) -> PathBuf {
    wait_for(
        "наполнения буфера",
        Duration::from_secs(20),
        || rec.stats().seconds() >= seconds,
    );
    match rec.save(None).expect("сохранение упало") {
        SaveOutcome::Saved(p) => p,
        other => panic!("не сохранилось: {other:?}"),
    }
}

#[test]
#[ignore = "нужен H.264-кодировщик (x264enc или VA-API)"]
fn separate_mode_records_two_synced_tracks_and_meters() {
    gst::init().unwrap();
    let Some(_) = audio_codec() else { return };
    let dir = temp_dir("sep");
    let (rec, mut service) = start(capture_config(&dir, AudioMode::Separate), test_sources());
    let clip = save_after(&rec, 4.0);
    let streams = inspect_mp4(&clip).unwrap();
    println!("{streams:#?}");
    assert_eq!(streams.iter().filter(|s| s.is_audio()).count(), 2);
    assert_in_sync(&streams);
    // В файле: 1 — системный звук (440 Гц), 2 — микрофон (880 Гц), без смешения.
    assert_eq!(tones(&clip), ["440", "880"]);
    // Индикаторы видят сигнал обоих источников: тон 440/880 Гц не тишина.
    for source in AudioSource::ALL {
        let st = rec.meters.get(source).status();
        assert!(
            matches!(st, SourceStatus::Signal { .. }),
            "{source:?}: {st:?}"
        );
    }
    let kinds: Vec<_> = rec.audio_stats().iter().map(|(k, _)| *k).collect();
    assert_eq!(
        kinds,
        [audio::TrackKind::System, audio::TrackKind::Mic],
        "порядок дорожек"
    );
    service.stop();
    std::fs::remove_dir_all(&dir).ok();
}

/// Смена режима на лету пересобирает конвейер, и число дорожек в клипе
/// следует за режимом — в том числе обратно к старому смешанному.
#[test]
#[ignore = "нужен H.264-кодировщик (x264enc или VA-API)"]
fn switching_modes_rebuilds_tracks() {
    gst::init().unwrap();
    let Some(_) = audio_codec() else { return };
    let dir = temp_dir("modes");
    let (rec, mut service) = start(capture_config(&dir, AudioMode::Mixed), test_sources());
    use audio::TrackKind as K;
    for (mode, kinds, expect_tones) in [
        (AudioMode::Mixed, vec![K::Mixed], vec!["оба"]),
        (
            AudioMode::Separate,
            vec![K::System, K::Mic],
            vec!["440", "880"],
        ),
        (AudioMode::SystemOnly, vec![K::System], vec!["440"]),
        (AudioMode::MicOnly, vec![K::Mic], vec!["880"]),
        (AudioMode::Off, vec![], vec![]),
        // Обратно к старому смешанному режиму: оба источника в одной дорожке.
        (AudioMode::Mixed, vec![K::Mixed], vec!["оба"]),
    ] {
        let mut cfg = rec.config();
        if cfg.audio.mode() != mode {
            cfg.audio.set_mode(mode);
            assert!(rec.update_config(cfg), "смена режима обязана пересобирать");
            service.restart();
            // Пересборка очищает буфер — ждём, пока он снова пуст и наполнится.
            wait_for("пересборки", Duration::from_secs(10), || {
                rec.audio_stats().len() == kinds.len() && rec.stats().seconds() < 1.0
            });
        }
        let got: Vec<_> = rec.audio_stats().iter().map(|(k, _)| *k).collect();
        assert_eq!(got, kinds, "{mode:?}");
        let clip = save_after(&rec, 2.5);
        let streams = inspect_mp4(&clip).unwrap();
        assert_eq!(
            streams.iter().filter(|s| s.is_audio()).count(),
            kinds.len(),
            "{mode:?}: {streams:#?}"
        );
        assert_in_sync(&streams);
        assert_eq!(tones(&clip), expect_tones, "{mode:?}");
        // Выключенный в режиме источник не показывается как живой.
        if mode == AudioMode::SystemOnly {
            assert_eq!(rec.meters.get(AudioSource::Mic).status(), SourceStatus::Off);
        }
        println!("{mode:?}: дорожки {expect_tones:?} — ок");
    }
    service.stop();
    std::fs::remove_dir_all(&dir).ok();
}

/// Микрофон отваливается посреди записи: захват не падает, дорожка
/// микрофона продолжается тишиной, индикатор показывает отказ.
#[test]
#[ignore = "нужен H.264-кодировщик (x264enc или VA-API)"]
fn mic_failure_mid_recording_keeps_capture_running() {
    gst::init().unwrap();
    let Some(_) = audio_codec() else { return };
    let dir = temp_dir("micfail");
    let sources = AudioSources {
        // audiotestsrc отдаёт буферы по 10 мс: через ~1,5 с — ошибка потока.
        mic: "audiotestsrc is-live=true freq=880 ! identity error-after=150".into(),
        ..test_sources()
    };
    let (rec, mut service) = start(capture_config(&dir, AudioMode::Separate), sources);
    wait_for(
        "отказа микрофона",
        Duration::from_secs(10),
        || {
            matches!(
                rec.meters.get(AudioSource::Mic).status(),
                SourceStatus::Failed(_)
            )
        },
    );
    assert_eq!(
        service.state(),
        CaptureState::Running,
        "отказ микрофона уронил захват"
    );
    let clip = save_after(&rec, 4.0);
    let streams = inspect_mp4(&clip).unwrap();
    assert_eq!(
        streams.iter().filter(|s| s.is_audio()).count(),
        2,
        "дорожка микрофона пропала вместо тишины: {streams:#?}"
    );
    assert_in_sync(&streams);
    assert_eq!(
        tones(&clip)[0],
        "440",
        "системный звук пострадал от отказа микрофона"
    );
    assert!(matches!(
        rec.meters.get(AudioSource::System).status(),
        SourceStatus::Signal { .. }
    ));
    service.stop();
    std::fs::remove_dir_all(&dir).ok();
}

/// Микрофона нет с самого начала: запись идёт без его дорожки.
#[test]
#[ignore = "нужен H.264-кодировщик (x264enc или VA-API)"]
fn missing_mic_at_start_records_without_it() {
    gst::init().unwrap();
    let Some(_) = audio_codec() else { return };
    let dir = temp_dir("nomic");
    let sources = AudioSources {
        mic: "filesrc location=/nonexistent/replay-rs-no-such-mic".into(),
        ..test_sources()
    };
    let (rec, mut service) = start(capture_config(&dir, AudioMode::Separate), sources);
    assert!(matches!(
        rec.meters.get(AudioSource::Mic).status(),
        SourceStatus::Failed(_)
    ));
    let clip = save_after(&rec, 3.0);
    let streams = inspect_mp4(&clip).unwrap();
    assert_eq!(streams.iter().filter(|s| s.is_audio()).count(), 1);
    assert_in_sync(&streams);
    service.stop();
    std::fs::remove_dir_all(&dir).ok();
}

/// Микрофон падает в своём потоке ещё до перехода конвейера в PLAYING.
///
/// Регрессия: такая ошибка отменяла переход состояния, конвейер навсегда
/// оставался в PAUSED, окно показывало «Запись идёт», а буфер не наполнялся.
#[test]
#[ignore = "нужен H.264-кодировщик (x264enc или VA-API)"]
fn mic_failure_before_playing_rebuilds_without_it() {
    gst::init().unwrap();
    let Some(_) = audio_codec() else { return };
    let dir = temp_dir("micearly");
    let sources = AudioSources {
        mic: "audiotestsrc is-live=true freq=880 ! identity error-after=1".into(),
        ..test_sources()
    };
    let (rec, mut service) = start(capture_config(&dir, AudioMode::Separate), sources);
    wait_for(
        "отказа микрофона",
        Duration::from_secs(10),
        || {
            matches!(
                rec.meters.get(AudioSource::Mic).status(),
                SourceStatus::Failed(_)
            )
        },
    );
    let clip = save_after(&rec, 3.0);
    let streams = inspect_mp4(&clip).unwrap();
    assert_eq!(streams.iter().filter(|s| s.is_audio()).count(), 1);
    assert_eq!(tones(&clip), ["440"]);
    assert_in_sync(&streams);
    service.stop();
    std::fs::remove_dir_all(&dir).ok();
}
