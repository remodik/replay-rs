//! Сохранение клипа: кадры из буфера → mp4 без перекодирования.
//!
//! Порт `Recorder._mux` из `reference/replay.py`. Кадры уже закодированы
//! (H.264, HEVC или AV1), поэтому мы только ремьюксируем их через
//! appsrc → парсер → mp4mux.

use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::{bail, Context, Result};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;

use crate::encoders::VideoCodec;
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
    write_clip_with_tracks(frames, fps, &[], out)
}

/// То же, но с одной дорожкой звука. Ни видео, ни звук не перекодируются.
pub fn write_clip_with_audio(
    frames: &[Frame],
    fps: u32,
    audio: Option<AudioTrack<'_>>,
    out: &Path,
) -> Result<()> {
    let tracks: Vec<AudioTrack<'_>> = audio.into_iter().collect();
    write_clip_with_tracks(frames, fps, &tracks, out)
}

/// Временное имя, под которым клип пишется до успешного завершения.
///
/// mp4mux дописывает moov только в самом конце. Пока запись идёт под этим
/// именем, библиотека клипов её не видит (расширение не .mp4), а падение
/// процесса посреди записи не оставляет в папке битый клип.
pub fn part_path(out: &Path) -> PathBuf {
    let mut name = out.file_name().unwrap_or_default().to_os_string();
    name.push(".part");
    out.with_file_name(name)
}

/// Наш ли это недописанный файл: `replay_*.mp4.part` — клип или экспорт.
///
/// Папку клипов выбирает пользователь, и там могут лежать чужие `.part`
/// (недокачанные файлы браузера, yt-dlp) — их трогать нельзя.
fn is_own_part(name: &str) -> bool {
    name.starts_with("replay_") && name.ends_with(".mp4.part")
}

/// Убирает огрызки `replay_*.mp4.part`, оставшиеся от прошлого запуска.
///
/// Вызывать до начала записи и экспорта: экземпляр у нас один, так что
/// нашей незаконченной записи от живого процесса здесь быть не может.
pub fn remove_stale_parts(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let is_part = path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(is_own_part);
        if is_part && entry.file_type().is_ok_and(|t| t.is_file()) {
            match std::fs::remove_file(&path) {
                Ok(()) => log::info!("убран недописанный файл {}", path.display()),
                Err(e) => log::warn!("не удалось убрать {}: {e}", path.display()),
            }
        }
    }
}

/// Ремьюксирует H.264 и любое число звуковых дорожек в один mp4.
pub fn write_clip_with_tracks(
    frames: &[Frame],
    fps: u32,
    audio: &[AudioTrack<'_>],
    out: &Path,
) -> Result<()> {
    write_clip_as(VideoCodec::H264, frames, fps, audio, out)
}

/// Ремьюксирует видео кодека `codec` и любое число звуковых дорожек в mp4.
///
/// Дорожки идут в файл в порядке `audio`, каждая — отдельным потоком mp4
/// со своими caps. Ни видео, ни звук не перекодируются.
pub fn write_clip_as(
    codec: VideoCodec,
    frames: &[Frame],
    fps: u32,
    audio: &[AudioTrack<'_>],
    out: &Path,
) -> Result<()> {
    if frames.is_empty() {
        bail!("буфер пуст — нечего сохранять");
    }
    debug_assert!(frames[0].keyframe, "клип обязан начинаться с keyframe");

    // Звук без кадров — не ошибка: источник мог быть выключен или молчать.
    let audio: Vec<&AudioTrack<'_>> = audio.iter().filter(|a| !a.frames.is_empty()).collect();
    let audio_desc: String = (0..audio.len())
        .map(|i| format!(" appsrc name=asrc{i} is-live=false format=time block=true ! mux."))
        .collect();
    let desc = format!(
        "appsrc name=src is-live=false format=time block=true caps={caps} ! \
         {parser} ! mp4mux name=mux faststart=true ! \
         filesink name=output{audio_desc}",
        caps = codec.mux_caps(fps),
        parser = codec.mux_parser(),
    );
    let pipeline = gst::parse::launch(&desc)
        .context("не удалось собрать конвейер мьюксинга")?
        .downcast::<gst::Pipeline>()
        .map_err(|_| anyhow::anyhow!("parse::launch вернул не Pipeline"))?;

    let part = part_path(out);
    crate::platform::set_file_location(&pipeline, "output", &part)?;

    let src = pipeline
        .by_name("src")
        .context("в конвейере нет элемента с name=src")?
        .downcast::<gst_app::AppSrc>()
        .map_err(|_| anyhow::anyhow!("элемент src не является appsrc"))?;

    // Caps звука ставим объектом, а не строкой в описании: у Opus там
    // заголовок, который нельзя терять при склейке описания конвейера.
    let mut asrcs = Vec::with_capacity(audio.len());
    for (i, track) in audio.iter().enumerate() {
        let name = format!("asrc{i}");
        let asrc = pipeline
            .by_name(&name)
            .with_context(|| format!("в конвейере нет элемента с name={name}"))?
            .downcast::<gst_app::AppSrc>()
            .map_err(|_| anyhow::anyhow!("элемент {name} не является appsrc"))?;
        let caps = gst::Caps::from_str(track.caps)
            .with_context(|| format!("не разобрать caps звука: {}", track.caps))?;
        asrc.set_caps(Some(&caps));
        asrcs.push(asrc);
    }

    pipeline
        .set_state(gst::State::Playing)
        .context("конвейер мьюксинга не запустился")?;

    // Дорожки заполняются параллельно, и это обязательно.
    //
    // appsrc держит в очереди 200 КБ и при block=true засыпает на
    // push_buffer, когда очередь полна. mp4mux выравнивает дорожки по
    // времени и не забирает видео вперёд звука. Если сложить всё видео
    // сначала, на клипе крупнее очереди push_buffer блокируется навсегда:
    // звук в мультиплексор так и не попадёт. У каждой дорожки — своя очередь
    // и свой поток, поэтому мультиплексор движется при любом их числе.
    let base = frames[0].pts;
    let bus = pipeline.bus().context("у конвейера мьюксинга нет шины")?;
    let result = std::thread::scope(|scope| {
        let video = scope.spawn(|| push_frames(&src, frames, fps));
        // Видео и звук режутся из одного конвейера, поэтому базу времени
        // берём общую — по первому видеокадру.
        let sounds: Vec<_> = asrcs
            .iter()
            .zip(&audio)
            .map(|(asrc, track)| scope.spawn(move || push_audio(asrc, track.frames, base)))
            .collect();
        // Пока дорожки заливаются, шину нужно слушать.
        //
        // Если mp4mux падает посреди записи, поток appsrc встаёт, очередь
        // больше не разбирается, и push_buffer с block=true засыпает навсегда.
        // Ошибка при этом уже лежит на шине; join без её чтения не вернётся.
        // Перевод конвейера в NULL будит заблокированные push_buffer.
        let event = watch_bus(&bus, || {
            video.is_finished() && sounds.iter().all(|h| h.is_finished())
        });
        if let BusEvent::Error(_) = event {
            let _ = pipeline.set_state(gst::State::Null);
        }
        let video = video
            .join()
            .map_err(|_| anyhow::anyhow!("поток видео упал"))?;
        let mut sound = Ok(());
        for h in sounds {
            let r = h.join().map_err(|_| anyhow::anyhow!("поток звука упал"))?;
            sound = sound.and(r);
        }
        // Ошибка шины первична: потоки после NULL сообщают лишь «flushing».
        match event {
            BusEvent::Error(e) => Err(e),
            BusEvent::Eos => video.and(sound),
            BusEvent::Nothing => video.and(sound).and_then(|()| wait_for_eos(&pipeline)),
        }
    });
    // Состояние сбрасываем в любом случае, иначе останется висеть filesink.
    let _ = pipeline.set_state(gst::State::Null);
    let result = result.and_then(|()| {
        std::fs::rename(&part, out).with_context(|| {
            format!(
                "не удалось переименовать {} в {}",
                part.display(),
                out.display()
            )
        })
    });
    if let Err(e) = result {
        // mp4mux уже создал файл, но не дописал moov — такой огрызок не нужен.
        match std::fs::remove_file(&part) {
            Ok(()) => {}
            Err(rm) if rm.kind() == std::io::ErrorKind::NotFound => {}
            Err(rm) => log::warn!("не удалось удалить недописанный {}: {rm}", part.display()),
        }
        return Err(e);
    }
    Ok(())
}

/// Что пришло на шину, пока заливались дорожки.
enum BusEvent {
    Nothing,
    /// Мультиплексор успел закончить раньше, чем мы заметили конец заливки.
    Eos,
    Error(anyhow::Error),
}

/// Слушает шину, пока `done()` не скажет, что заливка закончена.
///
/// EOS тоже ловим здесь: `timed_pop_filtered` выбрасывает сообщения, не
/// попавшие в фильтр, и потерянный EOS превратился бы в ложный таймаут.
fn watch_bus(bus: &gst::Bus, done: impl Fn() -> bool) -> BusEvent {
    while !done() {
        let Some(msg) = bus.timed_pop_filtered(
            gst::ClockTime::from_mseconds(50),
            &[gst::MessageType::Eos, gst::MessageType::Error],
        ) else {
            continue;
        };
        return match msg.view() {
            gst::MessageView::Eos(_) => BusEvent::Eos,
            gst::MessageView::Error(e) => BusEvent::Error(anyhow::anyhow!(
                "ошибка мьюксинга: {} ({:?})",
                e.error(),
                e.debug()
            )),
            _ => unreachable!("шина отфильтрована на EOS и ERROR"),
        };
    }
    BusEvent::Nothing
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
    fn part_path_hides_clip_from_library() {
        let part = part_path(Path::new("/x/replay_2026-01-01_00-00-00.mp4"));
        assert_eq!(part, Path::new("/x/replay_2026-01-01_00-00-00.mp4.part"));
        assert_ne!(part.extension().and_then(|e| e.to_str()), Some("mp4"));
    }

    #[test]
    fn stale_parts_are_removed_and_clips_kept() {
        let dir = std::env::temp_dir().join(format!("replay-parts-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let clip = dir.join("replay_a.mp4");
        let part = dir.join("replay_b.mp4.part");
        let other = dir.join("notes.part");
        let foreign = dir.join("talk.mp4.part");
        for p in [&clip, &part, &other, &foreign] {
            std::fs::write(p, b"x").unwrap();
        }
        remove_stale_parts(&dir);
        assert!(clip.exists(), "готовый клип удалён");
        assert!(!part.exists(), "огрызок остался");
        assert!(other.exists(), "чужой файл удалён");
        assert!(foreign.exists(), "чужая недокачка удалена");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn empty_frames_rejected() {
        let e = write_clip(&[], 60, Path::new("/tmp/none.mp4")).unwrap_err();
        assert!(e.to_string().contains("буфер пуст"), "{e}");
    }
}
