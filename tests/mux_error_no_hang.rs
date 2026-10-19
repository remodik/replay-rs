//! Регрессия: ошибка мультиплексора посреди сохранения вешала его навсегда.
//!
//! Дорожки заливаются в appsrc с block=true. Когда mp4mux падает (здесь —
//! не принимает caps звука; в жизни — не смог создать временный файл
//! faststart), поток appsrc встаёт, очередь больше не разбирается, и
//! push_buffer засыпает. Ошибка лежит на шине, но шину читали только после
//! того, как оба потока закончат, — то есть никогда. Флаг сохранения при этом
//! оставался поднятым, и все следующие сохранения отвечали «уже идёт».
//!
//! Тесту не нужен кодировщик: видео собрано из одного настоящего IDR-кадра.
//! Нужны только appsrc, h264parse и mp4mux.

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use gstreamer as gst;

use replay_rs::ringbuf::{Frame, NS};
use replay_rs::saver;

/// Сколько ждём, прежде чем счесть сохранение зависшим.
const SAVE_TIMEOUT: Duration = Duration::from_secs(20);

const FPS: u32 = 60;

/// SPS + PPS + IDR 64x64 из vah264enc, byte-stream, один access unit.
const IDR_64X64: &[u8] = &[
    0x00, 0x00, 0x00, 0x01, 0x09, 0xF0, 0x00, 0x00, 0x00, 0x01, 0x27, 0x64, 0x00, 0x0A, 0xAC, 0x56,
    0x41, 0x09, 0xBF, 0xF0, 0x00, 0x10, 0x00, 0x11, 0x00, 0x00, 0x03, 0x00, 0x01, 0x00, 0x00, 0x03,
    0x00, 0x3C, 0x9D, 0xA0, 0x80, 0x42, 0x58, 0x00, 0x00, 0x00, 0x01, 0x28, 0xEE, 0x3C, 0xB0, 0x00,
    0x00, 0x00, 0x01, 0x25, 0xB8, 0x10, 0x06, 0x42, 0x7F, 0xDE, 0x08, 0xE5, 0xF1, 0x03, 0xCD, 0xFF,
    0x9B, 0x80, 0x3A, 0xDC, 0x95, 0xBE, 0x3D, 0x1B, 0x16, 0x02, 0x62, 0xBA, 0xD8, 0x4C, 0xC5, 0xF0,
    0xCA, 0xC6, 0xAC, 0x0B, 0x7C,
];

/// Видео заметно больше очереди appsrc (200 КБ): иначе push_buffer не
/// блокируется и тест ничего не проверяет.
fn video_frames() -> Vec<Frame> {
    let n = 6000;
    let frames: Vec<Frame> = (0..n)
        .map(|i| Frame::new(IDR_64X64, i * NS / FPS as i64, true))
        .collect();
    let bytes: usize = frames.iter().map(|f| f.data.len()).sum();
    assert!(bytes > 400_000, "клип меньше очереди appsrc: {bytes} Б");
    frames
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("replay-mux-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Запускает сохранение в отдельном потоке и ждёт его не дольше таймаута.
fn save_with_timeout(
    video: Vec<Frame>,
    audio: Option<(Vec<Frame>, &'static str)>,
    out: &Path,
) -> anyhow::Result<()> {
    let (tx, rx) = mpsc::channel();
    let out = out.to_path_buf();
    std::thread::spawn(move || {
        let track = audio
            .as_ref()
            .map(|(frames, caps)| saver::AudioTrack { frames, caps });
        let _ = tx.send(saver::write_clip_with_audio(&video, FPS, track, &out));
    });
    rx.recv_timeout(SAVE_TIMEOUT).unwrap_or_else(|_| {
        panic!(
            "сохранение зависло: за {} с оно не вернулось",
            SAVE_TIMEOUT.as_secs()
        )
    })
}

#[test]
fn video_only_clip_from_synthetic_frames_is_written() {
    gst::init().unwrap();
    let dir = temp_dir("ok");
    let out = dir.join("clip.mp4");
    save_with_timeout(video_frames(), None, &out).expect("клип без звука не записался");
    let size = std::fs::metadata(&out).expect("клипа нет").len();
    assert!(size > 400_000, "клип подозрительно мал: {size} Б");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn mux_error_mid_stream_fails_instead_of_hanging() {
    gst::init().unwrap();
    let dir = temp_dir("err");
    let out = dir.join("clip.mp4");
    let base = 0;
    let audio: Vec<Frame> = (0..100)
        .map(|i| Frame::new(vec![0u8; 50], base + i * NS / 50, true))
        .collect();

    // mp4mux такие caps не принимает: дорожка звука встаёт с not-negotiated,
    // а мультиплексор ждёт её и перестаёт забирать видео.
    let err = save_with_timeout(
        video_frames(),
        Some((audio, "audio/x-replay-unsupported")),
        &out,
    )
    .expect_err("сохранение с неподходящим звуком обязано завершиться ошибкой");
    println!("ошибка: {err:#}");
    assert!(
        format!("{err:#}").contains("мьюксинг"),
        "ожидалась ошибка мультиплексора: {err:#}"
    );
    assert!(!out.exists(), "недописанный клип остался на диске");
    std::fs::remove_dir_all(&dir).ok();
}
