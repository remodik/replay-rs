//! Сохранение клипа: кадры из буфера → mp4 без перекодирования.
//!
//! Порт `Recorder._mux` из `reference/replay.py`. Кадры уже закодированы
//! в H.264, поэтому мы только ремьюксируем их через appsrc → mp4mux.

use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::{bail, Context, Result};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;

use crate::ringbuf::{Frame, NS};

/// Сколько ждём завершения мьюксинга.
const MUX_TIMEOUT: gst::ClockTime = gst::ClockTime::from_seconds(30);

/// Подбирает свободное имя вида `replay_2026-09-21_00-14-37.mp4`.
pub fn clip_path(dir: &Path, stamp: &str) -> PathBuf {
    let mut out = dir.join(format!("replay_{stamp}.mp4"));
    let mut n = 1;
    // Не затираем клипы, сохранённые в ту же секунду.
    while out.exists() {
        out = dir.join(format!("replay_{stamp}_{n}.mp4"));
        n += 1;
    }
    out
}

/// Звуковая дорожка клипа: кадры плюс caps, с которыми их отдал кодировщик.
pub struct AudioTrack<'a> {
    pub frames: &'a [Frame],
    pub caps: &'a str,
}

/// Ремьюксирует кадры в mp4. `frames` должен начинаться с keyframe.
pub fn write_clip(frames: &[Frame], fps: u32, out: &Path) -> Result<()> {
    write_clip_with_audio(frames, fps, None, out)
}

/// То же, но с дорожкой звука. Ни видео, ни звук не перекодируются.
pub fn write_clip_with_audio(
    frames: &[Frame],
    fps: u32,
    audio: Option<AudioTrack<'_>>,
    out: &Path,
) -> Result<()> {
    if frames.is_empty() {
        bail!("буфер пуст — нечего сохранять");
    }
    debug_assert!(frames[0].keyframe, "клип обязан начинаться с keyframe");

    // Звук без кадров — не ошибка: источник мог быть выключен или молчать.
    let audio = audio.filter(|a| !a.frames.is_empty());
    let audio_desc = match &audio {
        Some(_) => " appsrc name=asrc is-live=false format=time block=true ! mux.",
        None => "",
    };
    let desc = format!(
        "appsrc name=src is-live=false format=time block=true \
         caps=video/x-h264,stream-format=byte-stream,alignment=au,framerate={fps}/1 ! \
         h264parse ! mp4mux name=mux faststart=true ! \
         filesink name=output{audio_desc}",
    );
    let pipeline = gst::parse::launch(&desc)
        .context("не удалось собрать конвейер мьюксинга")?
        .downcast::<gst::Pipeline>()
        .map_err(|_| anyhow::anyhow!("parse::launch вернул не Pipeline"))?;

    crate::platform::set_file_location(&pipeline, "output", out)?;

    let src = pipeline
        .by_name("src")
        .context("в конвейере нет элемента с name=src")?
        .downcast::<gst_app::AppSrc>()
        .map_err(|_| anyhow::anyhow!("элемент src не является appsrc"))?;

    // Caps звука ставим объектом, а не строкой в описании: у Opus там
    // заголовок, который нельзя терять при склейке описания конвейера.
    let asrc = match &audio {
        Some(track) => {
            let asrc = pipeline
                .by_name("asrc")
                .context("в конвейере нет элемента с name=asrc")?
                .downcast::<gst_app::AppSrc>()
                .map_err(|_| anyhow::anyhow!("элемент asrc не является appsrc"))?;
            let caps = gst::Caps::from_str(track.caps)
                .with_context(|| format!("не разобрать caps звука: {}", track.caps))?;
            asrc.set_caps(Some(&caps));
            Some(asrc)
        }
        None => None,
    };

    pipeline
        .set_state(gst::State::Playing)
        .context("конвейер мьюксинга не запустился")?;

    // Дорожки заполняются параллельно, и это обязательно.
    //
    // appsrc держит в очереди 200 КБ и при block=true засыпает на
    // push_buffer, когда очередь полна. mp4mux выравнивает дорожки по
    // времени и не забирает видео вперёд звука. Если сложить всё видео
    // сначала, на клипе крупнее очереди push_buffer блокируется навсегда:
    // звук в мультиплексор так и не попадёт. Два потока дают каждой дорожке
    // свою обратную связь, и мультиплексор движется.
    let base = frames[0].pts;
    let result = std::thread::scope(|scope| {
        let video = scope.spawn(|| push_frames(&src, frames, fps));
        let sound = match (&asrc, &audio) {
            // Видео и звук режутся из одного конвейера, поэтому базу времени
            // берём общую — по первому видеокадру.
            (Some(asrc), Some(track)) => {
                Some(scope.spawn(move || push_audio(asrc, track.frames, base)))
            }
            _ => None,
        };
        let video = video
            .join()
            .map_err(|_| anyhow::anyhow!("поток видео упал"))?;
        let sound = match sound {
            Some(h) => h.join().map_err(|_| anyhow::anyhow!("поток звука упал"))?,
            None => Ok(()),
        };
        video.and(sound)
    })
    .and_then(|()| wait_for_eos(&pipeline));
    // Состояние сбрасываем в любом случае, иначе останется висеть filesink.
    let _ = pipeline.set_state(gst::State::Null);
    if let Err(e) = result {
        // mp4mux уже создал файл, но не дописал moov — такой огрызок не нужен.
        let _ = std::fs::remove_file(out);
        return Err(e);
    }
    Ok(())
}

fn push_frames(src: &gst_app::AppSrc, frames: &[Frame], fps: u32) -> Result<()> {
    let base = frames[0].pts;
    let frame_ns = NS / fps.max(1) as i64;

    for (i, f) in frames.iter().enumerate() {
        let mut gbuf = gst::Buffer::from_slice(f.data.clone());
        {
            let b = gbuf
                .get_mut()
                .expect("буфер только что создан, ссылка одна");
            let pts = gst::ClockTime::from_nseconds((f.pts - base) as u64);
            b.set_pts(pts);
            b.set_dts(pts);
            // Длительность — до следующего кадра, для последнего берём 1/fps.
            let next = frames.get(i + 1).map_or(f.pts + frame_ns, |n| n.pts);
            b.set_duration(gst::ClockTime::from_nseconds((next - f.pts).max(0) as u64));
            if !f.keyframe {
                b.set_flags(gst::BufferFlags::DELTA_UNIT);
            }
        }
        src.push_buffer(gbuf)
            .map_err(|e| anyhow::anyhow!("appsrc отверг кадр {i}: {e:?}"))?;
    }
    src.end_of_stream()
        .map_err(|e| anyhow::anyhow!("не удалось закрыть поток: {e:?}"))?;
    Ok(())
}

/// Кладёт звуковые кадры, выравнивая их по той же базе, что и видео.
fn push_audio(src: &gst_app::AppSrc, frames: &[Frame], base: i64) -> Result<()> {
    for (i, f) in frames.iter().enumerate() {
        let mut gbuf = gst::Buffer::from_slice(f.data.clone());
        {
            let b = gbuf
                .get_mut()
                .expect("буфер только что создан, ссылка одна");
            // Кадр мог начаться до начала клипа — прижимаем его к нулю,
            // иначе mp4mux получит отрицательный timestamp.
            let pts = gst::ClockTime::from_nseconds((f.pts - base).max(0) as u64);
            b.set_pts(pts);
            b.set_dts(pts);
            if let Some(next) = frames.get(i + 1) {
                b.set_duration(gst::ClockTime::from_nseconds(
                    (next.pts - f.pts).max(0) as u64
                ));
            }
        }
        src.push_buffer(gbuf)
            .map_err(|e| anyhow::anyhow!("appsrc отверг звуковой кадр {i}: {e:?}"))?;
    }
    src.end_of_stream()
        .map_err(|e| anyhow::anyhow!("не удалось закрыть звуковой поток: {e:?}"))?;
    Ok(())
}

/// Ждём EOS на шине — только после него mp4mux дописал moov.
///
/// Шину берём у конвейера, а не у элемента: у элементной шины не заведён
/// poll, и `timed_pop_filtered` на ней падает с GStreamer-CRITICAL.
fn wait_for_eos(pipeline: &gst::Pipeline) -> Result<()> {
    let bus = pipeline.bus().context("у конвейера мьюксинга нет шины")?;
    match bus.timed_pop_filtered(
        MUX_TIMEOUT,
        &[gst::MessageType::Eos, gst::MessageType::Error],
    ) {
        None => bail!("мьюксинг не завершился за {} с", MUX_TIMEOUT.seconds()),
        Some(msg) => match msg.view() {
            gst::MessageView::Eos(_) => Ok(()),
            gst::MessageView::Error(e) => {
                bail!("ошибка мьюксинга: {} ({:?})", e.error(), e.debug())
            }
            _ => unreachable!("шина отфильтрована на EOS и ERROR"),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clip_path_avoids_collisions() {
        let dir = std::env::temp_dir().join(format!("replay-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let first = clip_path(&dir, "2026-01-01_00-00-00");
        assert!(first.ends_with("replay_2026-01-01_00-00-00.mp4"));
        std::fs::write(&first, b"x").unwrap();
        let second = clip_path(&dir, "2026-01-01_00-00-00");
        assert!(
            second.ends_with("replay_2026-01-01_00-00-00_1.mp4"),
            "{second:?}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn empty_frames_rejected() {
        let e = write_clip(&[], 60, Path::new("/tmp/none.mp4")).unwrap_err();
        assert!(e.to_string().contains("буфер пуст"), "{e}");
    }
}
