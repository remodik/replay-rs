//! Превью клипов: первый кадр mp4 → JPEG в кеше.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;

/// Ширина превью в пикселях; высота считается по соотношению сторон.
const THUMB_WIDTH: u32 = 320;
/// Первый кадр клипа — всегда keyframe, декодировать дальше не нужно.
const PULL_TIMEOUT: gst::ClockTime = gst::ClockTime::from_seconds(10);

/// Путь превью для клипа. Имя берём от клипа, чтобы не городить индекс.
pub fn thumb_path(clip: &Path) -> PathBuf {
    let stem = clip
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();
    crate::config::cache_dir()
        .join("thumbs")
        .join(format!("{stem}.jpg"))
}

/// Возвращает путь к превью, создавая его при первом обращении.
pub fn ensure_thumb(clip: &Path) -> Result<PathBuf> {
    let out = thumb_path(clip);
    if out.exists() {
        return Ok(out);
    }
    if let Some(dir) = out.parent() {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("не удалось создать {}", dir.display()))?;
    }
    let jpeg = grab_first_frame(clip)?;
    std::fs::write(&out, jpeg).with_context(|| format!("не удалось записать {}", out.display()))?;
    Ok(out)
}

/// Декодирует первый кадр и кодирует его в JPEG.
fn grab_first_frame(clip: &Path) -> Result<Vec<u8>> {
    // decodebin сам выберет vah264dec. pixel-aspect-ratio=1/1 обязателен:
    // без него videoscale оставляет исходную высоту и «сжимает» кадр, унося
    // пропорции в метаданные, которых egui не видит. С PAR 1/1 высота
    // вычисляется из ширины и исходных пропорций.
    let desc = format!(
        "filesrc name=input ! decodebin ! videoconvert ! videoscale \
         ! video/x-raw,width={THUMB_WIDTH},pixel-aspect-ratio=1/1 ! jpegenc \
         ! appsink name=out max-buffers=1 drop=false sync=false"
    );
    let pipeline = gst::parse::launch(&desc)
        .context("не удалось собрать конвейер превью")?
        .downcast::<gst::Pipeline>()
        .map_err(|_| anyhow::anyhow!("parse::launch вернул не Pipeline"))?;

    crate::platform::set_file_location(&pipeline, "input", clip)?;

    let sink = pipeline
        .by_name("out")
        .context("в конвейере превью нет appsink")?
        .downcast::<gst_app::AppSink>()
        .map_err(|_| anyhow::anyhow!("элемент out не является appsink"))?;

    pipeline
        .set_state(gst::State::Playing)
        .context("конвейер превью не запустился")?;

    let pulled = sink.try_pull_sample(PULL_TIMEOUT);
    let _ = pipeline.set_state(gst::State::Null);

    let sample = pulled.context("не удалось получить кадр для превью")?;
    let buffer = sample.buffer().context("в кадре превью нет буфера")?;
    let map = buffer
        .map_readable()
        .map_err(|_| anyhow::anyhow!("не удалось прочитать кадр превью"))?;
    Ok(map.as_slice().to_vec())
}

/// Убирает превью вместе с клипом.
pub fn remove_thumb(clip: &Path) {
    let _ = std::fs::remove_file(thumb_path(clip));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thumb_path_follows_clip_name() {
        let p = thumb_path(Path::new("/x/replay_2026-01-01_00-00-00.mp4"));
        assert!(p.ends_with("replay_2026-01-01_00-00-00.jpg"), "{p:?}");
    }
}
