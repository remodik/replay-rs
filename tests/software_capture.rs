//! Windows CI проверяет кодирование, безопасные пути, мьюксинг и превью
//! без захвата реального рабочего стола, звуковых устройств и GPU.
use gstreamer::{self as gst, prelude::*};
use replay_rs::{
    config::{Config, Encoder},
    pipeline,
    ringbuf::GopRingBuffer,
    saver, thumbs,
};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[test]
#[cfg_attr(
    not(windows),
    ignore = "requires x264enc; run explicitly with full GStreamer plugins"
)]
fn software_recording_saves_unicode_paths_and_previews() {
    gst::init().unwrap();
    let cfg = Config {
        encoder: Encoder::X264enc,
        width: 320,
        height: 180,
        fps: 30,
        ..Config::default()
    };
    let ring = Arc::new(Mutex::new(GopRingBuffer::new(5.0, cfg.max_bytes())));
    let capture = pipeline::build_pipeline(&cfg, &pipeline::test_source(&cfg)).unwrap();
    pipeline::attach_sink(&capture, ring.clone()).unwrap();
    capture.set_state(gst::State::Playing).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while ring.lock().unwrap().stats().seconds() < 2.0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    capture.set_state(gst::State::Null).unwrap();
    let frames = ring.lock().unwrap().snapshot_last(2.0);
    assert!(frames.len() > 30, "encoder failed to produce video");
    assert!(frames[0].keyframe);
    let dir = std::env::temp_dir().join(format!("replay Windows путь ! {}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let clip = dir.join(format!("clip пробел {}.mp4", std::process::id()));
    saver::write_clip(&frames, cfg.fps, &clip).unwrap();
    assert!(std::fs::metadata(&clip).unwrap().len() > 1000);
    let thumb = thumbs::ensure_thumb(&clip).unwrap();
    assert_eq!(image::image_dimensions(&thumb).unwrap(), (320, 180));
    thumbs::remove_thumb(&clip);
    std::fs::remove_dir_all(dir).unwrap();
}
