//! Обрезка клипа в отдельный файл. Оригинал не трогается.
//!
//! Оба режима устроены одинаково: qtdemux читает файл, пробы на его
//! выходах отбрасывают всё вне диапазона, а `set_offset` сдвигает шкалу
//! так, что результат начинается с нуля. Все дорожки — видео и каждая
//! звуковая — идут в mp4mux в исходном порядке; дорожку, которую не на чем
//! пересобрать, мы не выбрасываем молча, а отказываемся резать.
//!
//! * Быстрый режим — без перекодирования. Начало выравнивается по ключевому
//!   кадру не позже запрошенного: иначе первые кадры не декодировать.
//!   Фактические границы возвращаются вызывающему.
//! * Точный режим — видео и звук перекодируются. Декодирование начинается
//!   с того же ключевого кадра, а кадры до начала диапазона выбрасываются
//!   уже после декодера. Звук начинается с первого звукового кадра не
//!   раньше начала — расхождение меньше длительности кадра (~21 мс), и
//!   синхронность сохраняется: шкала у дорожек общая.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{bail, Context, Result};
use gstreamer as gst;
use gstreamer::prelude::*;

use crate::audio::AudioCodec;
use crate::config::Config;
use crate::encoders::{EncoderSpec, VideoCodec};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrimMode {
    /// Без перекодирования, начало — по ключевому кадру.
    Fast,
    /// С перекодированием, начало — по кадру.
    Precise,
}

#[derive(Debug, Clone)]
pub struct TrimRequest {
    pub input: PathBuf,
    pub output: PathBuf,
    pub start: gst::ClockTime,
    pub end: gst::ClockTime,
    pub mode: TrimMode,
    /// Битрейт видео при перекодировании, кбит/с.
    pub bitrate_kbps: u32,
    /// Кодировщик для точного режима. `None` — выбрать по кодеку исходника.
    pub encoder: Option<EncoderSpec>,
}

/// Фактический результат: границы могут отличаться от запрошенных.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrimOutcome {
    pub output: PathBuf,
    pub start: gst::ClockTime,
    pub end: gst::ClockTime,
    pub audio_tracks: usize,
}

/// Прогресс и отмена — разделяются между потоком экспорта и окном.
#[derive(Debug, Default)]
pub struct Progress {
    /// Сколько наносекунд диапазона уже обработано.
    pub done_ns: AtomicU64,
    pub cancel: AtomicBool,
}

/// Имя файла результата рядом с оригиналом.
///
/// Начинается с `replay_`, чтобы недописанный `*.mp4.part` после падения
/// убрала уборка при старте (`saver::remove_stale_parts`).
pub fn output_path(input: &Path, start: gst::ClockTime, end: gst::ClockTime) -> PathBuf {
    let stem = input
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();
    let stem = if stem.starts_with("replay_") {
        stem
    } else {
        format!("replay_{stem}")
    };
    let mark = |t: gst::ClockTime| {
        let ms = t.mseconds();
        format!(
            "{}m{:02}.{}s",
            ms / 60_000,
            (ms / 1000) % 60,
            (ms % 1000) / 100
        )
    };
    let dir = input.parent().unwrap_or(Path::new("."));
    let base = format!("{stem}_cut_{}-{}", mark(start), mark(end));
    let mut out = dir.join(format!("{base}.mp4"));
    let mut n = 1;
    while out.exists() {
        out = dir.join(format!("{base}_{n}.mp4"));
        n += 1;
    }
    out
}

/// Парсер перед mp4mux для потока с такими caps.
fn parser_for(media: &str) -> Option<&'static str> {
    match media {
        "video/x-h264" => Some("h264parse"),
        "video/x-h265" => Some("h265parse"),
        "video/x-av1" => Some("av1parse"),
        "audio/mpeg" => Some("aacparse"),
        // opusparse есть не во всех сборках, а mp4mux принимает выход
        // qtdemux напрямую.
        "audio/x-opus" => Some("identity"),
        _ => None,
    }
}

fn video_codec(media: &str) -> Option<VideoCodec> {
    match media {
        "video/x-h264" => Some(VideoCodec::H264),
        "video/x-h265" => Some(VideoCodec::H265),
        "video/x-av1" => Some(VideoCodec::Av1),
        _ => None,
    }
}

fn audio_codec(media: &str) -> Option<AudioCodec> {
    match media {
        "audio/mpeg" => Some(AudioCodec::Aac),
        "audio/x-opus" => Some(AudioCodec::Opus),
        _ => None,
    }
}

/// Сколько ждать между проверками отмены.
const POLL: gst::ClockTime = gst::ClockTime::from_mseconds(100);
/// Сколько можно не получать данных, прежде чем счесть экспорт зависшим.
const STALL: std::time::Duration = std::time::Duration::from_secs(30);

/// Режет клип. Блокирует поток — вызывать не из GUI.
pub fn trim(req: &TrimRequest, progress: &Arc<Progress>) -> Result<TrimOutcome> {
    if req.end <= req.start {
        bail!("конец диапазона раньше начала");
    }
    let info = crate::media::probe(&req.input)?;
    let video = info.video().context("в клипе нет видео")?.clone();
    for s in &info.streams {
        if parser_for(&s.media).is_none() {
            // Молча выбросить дорожку нельзя — лучше отказаться.
            bail!("не умею пересобрать поток {} ({})", s.pad, s.media);
        }
    }
    let end = match info.duration {
        Some(d) => req.end.min(d),
        None => req.end,
    };
    // С ключевого кадра начинается и быстрый режим, и декодирование точного.
    let key = crate::media::keyframe_at_or_before(&req.input, req.start)?;
    let start = match req.mode {
        TrimMode::Fast => key,
        TrimMode::Precise => req.start,
    };

    let venc = match req.mode {
        TrimMode::Fast => None,
        TrimMode::Precise => {
            let codec = video_codec(&video.media).context("неизвестный видеокодек")?;
            let spec = match &req.encoder {
                Some(s) if s.codec == codec => Some(s.clone()),
                _ => crate::encoders::resolve_quick(&Config {
                    video_codec: codec,
                    ..Config::default()
                }),
            };
            Some(spec.with_context(|| format!("нет кодировщика {}", codec.label()))?)
        }
    };
    if req.mode == TrimMode::Precise {
        for s in info.streams.iter().filter(|s| s.is_audio()) {
            let c = audio_codec(&s.media).context("неизвестный аудиокодек")?;
            let name = match c {
                AudioCodec::Aac => "avenc_aac",
                AudioCodec::Opus => "opusenc",
            };
            if gst::ElementFactory::find(name).is_none() {
                bail!("для точной обрезки нужен {name}");
            }
        }
    }

    let part = crate::saver::part_path(&req.output);
    let result = run(req, &info, key, start, end, venc.as_ref(), &part, progress);
    let result = result.and_then(|()| {
        // Проверяем результат, а не верим конвейеру: число дорожек то же.
        let got = crate::media::probe(&part).context("результат не читается")?;
        if got.streams.len() != info.streams.len() {
            bail!(
                "в результате {} потоков вместо {}",
                got.streams.len(),
                info.streams.len()
            );
        }
        std::fs::rename(&part, &req.output)
            .with_context(|| format!("не удалось записать {}", req.output.display()))
    });
    if let Err(e) = result {
        let _ = std::fs::remove_file(&part);
        return Err(e);
    }
    Ok(TrimOutcome {
        output: req.output.clone(),
        start,
        end,
        audio_tracks: info.audio_count(),
    })
}

/// Строит и крутит конвейер обрезки до EOS, ошибки или отмены.
#[allow(clippy::too_many_arguments)]
fn run(
    req: &TrimRequest,
    info: &crate::media::MediaInfo,
    key: gst::ClockTime,
    start: gst::ClockTime,
    end: gst::ClockTime,
    venc: Option<&EncoderSpec>,
    part: &Path,
    progress: &Arc<Progress>,
) -> Result<()> {
    let pipeline = gst::Pipeline::new();
    let src = gst::ElementFactory::make("filesrc").build()?;
    crate::platform::set_location(&src, &req.input)?;
    let demux = gst::ElementFactory::make("qtdemux").build()?;
    let mux = gst::ElementFactory::make("mp4mux")
        .property("faststart", true)
        .build()?;
    let sink = gst::ElementFactory::make("filesink").build()?;
    crate::platform::set_location(&sink, part)?;
    pipeline.add_many([&src, &demux, &mux, &sink])?;
    src.link(&demux)?;
    mux.link(&sink)?;

    let failure: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let linked = Arc::new(AtomicUsize::new(0));
    let fps = 60;
    let cfg = Config {
        bitrate: req.bitrate_kbps.max(500),
        fps,
        gop: 1.0,
        ..Config::default()
    };
    let ctx = Arc::new(StreamCtx {
        mode: req.mode,
        key: key.nseconds(),
        start: start.nseconds(),
        end: end.nseconds(),
        venc: venc.map(|s| (s.convert(), s.encoder_desc(&cfg))),
        audio_bitrate: 128,
    });

    let weak = pipeline.downgrade();
    let mux_ref = mux.clone();
    let (fail, link) = (failure.clone(), linked.clone());
    let prog = progress.clone();
    demux.connect_pad_added(move |_, pad| {
        let Some(pipeline) = weak.upgrade() else {
            return;
        };
        if let Err(e) = attach_stream(&pipeline, &mux_ref, pad, &ctx, &prog) {
            *fail.lock().expect("ошибка экспорта отравлена") = Some(format!("{e:#}"));
        } else {
            link.fetch_add(1, Ordering::AcqRel);
        }
    });

    pipeline
        .set_state(gst::State::Playing)
        .context("экспорт не запустился")?;
    let bus = pipeline.bus().context("нет шины")?;
    let mut last_progress = (0u64, std::time::Instant::now());
    let outcome = loop {
        if progress.cancel.load(Ordering::Acquire) {
            break Err(anyhow::anyhow!("отменено"));
        }
        if let Some(e) = failure.lock().expect("ошибка экспорта отравлена").take()
        {
            break Err(anyhow::anyhow!(e));
        }
        let done = progress.done_ns.load(Ordering::Relaxed);
        if done != last_progress.0 {
            last_progress = (done, std::time::Instant::now());
        } else if last_progress.1.elapsed() > STALL {
            break Err(anyhow::anyhow!(
                "экспорт завис: нет данных {} с",
                STALL.as_secs()
            ));
        }
        let Some(msg) =
            bus.timed_pop_filtered(POLL, &[gst::MessageType::Eos, gst::MessageType::Error])
        else {
            continue;
        };
        match msg.view() {
            gst::MessageView::Eos(_) => break Ok(()),
            gst::MessageView::Error(e) => {
                break Err(anyhow::anyhow!("{} ({:?})", e.error(), e.debug()))
            }
            _ => unreachable!("шина отфильтрована"),
        }
    };
    let _ = pipeline.set_state(gst::State::Null);
    outcome?;
    let n = linked.load(Ordering::Acquire);
    if n != info.streams.len() {
        bail!("подключено {n} потоков из {}", info.streams.len());
    }
    Ok(())
}

/// Параметры, общие для всех потоков одного экспорта.
struct StreamCtx {
    mode: TrimMode,
    key: u64,
    start: u64,
    end: u64,
    /// Преобразование и кодировщик видео для точного режима.
    venc: Option<(String, String)>,
    audio_bitrate: u32,
}

/// Подключает поток qtdemux к mp4mux и ставит пробы диапазона.
fn attach_stream(
    pipeline: &gst::Pipeline,
    mux: &gst::Element,
    pad: &gst::Pad,
    ctx: &Arc<StreamCtx>,
    progress: &Arc<Progress>,
) -> Result<()> {
    let media = pad
        .current_caps()
        .and_then(|c| c.structure(0).map(|s| s.name().to_string()))
        .context("у потока нет caps")?;
    let is_video = media.starts_with("video/");
    let parser = parser_for(&media).with_context(|| format!("неизвестный поток {media}"))?;

    let desc = match (ctx.mode, is_video) {
        (TrimMode::Fast, _) => format!("queue name=in ! {parser} name=out"),
        (TrimMode::Precise, true) => {
            let (convert, enc) = ctx.venc.as_ref().context("нет кодировщика видео")?;
            let codec = video_codec(&media).context("неизвестный видеокодек")?;
            format!(
                "queue name=in ! decodebin ! {convert} ! {enc} ! {} name=out",
                codec.mux_parser()
            )
        }
        (TrimMode::Precise, false) => {
            let codec = audio_codec(&media).context("неизвестный аудиокодек")?;
            format!(
                "queue name=in ! decodebin ! audioconvert name=vup ! audioresample \
                 ! audio/x-raw,channels=2,rate=48000 ! {}",
                match codec.parser() {
                    Some(p) => format!("{} ! {p} name=out", codec.encoder_desc(ctx.audio_bitrate)),
                    None => format!("{} name=out", codec.encoder_desc(ctx.audio_bitrate)),
                }
            )
        }
    };
    let bin = gst::parse::bin_from_description(&desc, false)
        .with_context(|| format!("не собрать ветку {media}: {desc}"))?;
    // Ghost-пэды вручную: при автоматических вход элемента за decodebin
    // (он подключится позже) тоже стал бы ghost-пэдом.
    let input = bin
        .by_name("in")
        .and_then(|q| q.static_pad("sink"))
        .context("нет входа ветки")?;
    bin.add_pad(&gst::GhostPad::with_target(&input)?)?;
    let output = bin
        .by_name("out")
        .and_then(|e| e.static_pad("src"))
        .context("нет выхода ветки")?;
    bin.add_pad(&gst::GhostPad::with_target(&output)?)?;

    pipeline.add(&bin)?;
    let template = if is_video { "video_%u" } else { "audio_%u" };
    let mux_pad = mux
        .request_pad_simple(template)
        .context("mp4mux не дал пэд")?;
    bin.static_pad("src")
        .context("нет выхода")?
        .link(&mux_pad)
        .context("ветка не подключилась к mp4mux")?;
    bin.sync_state_with_parent()?;

    // Проба на выходе qtdemux: диапазон по закодированным кадрам.
    let eos_sent = Arc::new(AtomicBool::new(false));
    let c = ctx.clone();
    let sent = eos_sent.clone();
    let progress = progress.clone();
    pad.add_probe(gst::PadProbeType::BUFFER, move |pad, info| {
        let Some(buf) = info.buffer() else {
            return gst::PadProbeReturn::Ok;
        };
        if sent.load(Ordering::Acquire) {
            return gst::PadProbeReturn::Drop;
        }
        let pts = buf.pts().map_or(0, |t| t.nseconds());
        // Конец: дальше этот поток не нужен. Звук в точном режиме — с
        // запасом, его хвост дорежет проба после декодера.
        let tail = if !is_video && c.mode == TrimMode::Precise {
            100_000_000
        } else {
            0
        };
        if pts >= c.end + tail {
            sent.store(true, Ordering::Release);
            if let Some(peer) = pad.peer() {
                peer.send_event(gst::event::Eos::new());
            }
            return gst::PadProbeReturn::Drop;
        }
        let keep = match (c.mode, is_video) {
            // Видео — с ключевого кадра: с него декодируется всё остальное.
            (_, true) => pts >= c.key,
            // Звук целиком внутри: кадр, начатый раньше, дал бы
            // отрицательное время в результате.
            (TrimMode::Fast, false) => pts >= c.key,
            // Немного раньше начала — декодеру AAC нужен разгон.
            (TrimMode::Precise, false) => pts + 300_000_000 >= c.start,
        };
        if !keep {
            return gst::PadProbeReturn::Drop;
        }
        if is_video {
            let base = match c.mode {
                TrimMode::Fast => c.key,
                TrimMode::Precise => c.start,
            };
            progress
                .done_ns
                .store(pts.saturating_sub(base), Ordering::Relaxed);
        }
        gst::PadProbeReturn::Ok
    });

    match ctx.mode {
        // Шкала результата начинается с ключевого кадра.
        TrimMode::Fast => pad.set_offset(-(ctx.key as i64)),
        // После декодера: точный диапазон и шкала от начала.
        TrimMode::Precise => {
            let conv = bin.by_name("vup").context("нет преобразователя")?;
            let sink = conv
                .static_pad("sink")
                .context("нет входа преобразователя")?;
            let (start, end) = (ctx.start, ctx.end);
            sink.add_probe(gst::PadProbeType::BUFFER, move |_, info| {
                let Some(buf) = info.buffer() else {
                    return gst::PadProbeReturn::Ok;
                };
                let pts = buf.pts().map_or(0, |t| t.nseconds());
                if pts < start || pts >= end {
                    gst::PadProbeReturn::Drop
                } else {
                    gst::PadProbeReturn::Ok
                }
            });
            sink.set_offset(-(start as i64));
        }
    }
    let target = bin.static_pad("sink").context("нет входа")?;
    pad.link(&target).context("поток не подключился")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_name_is_cleanable_and_unique() {
        let dir = std::env::temp_dir().join(format!("replay-trim-name-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let input = dir.join("clip.mp4");
        let a = output_path(
            &input,
            gst::ClockTime::from_mseconds(3500),
            gst::ClockTime::from_mseconds(72_100),
        );
        let name = a.file_name().unwrap().to_str().unwrap();
        assert_eq!(name, "replay_clip_cut_0m03.5s-1m12.1s.mp4");
        std::fs::write(&a, b"x").unwrap();
        let b = output_path(
            &input,
            gst::ClockTime::from_mseconds(3500),
            gst::ClockTime::from_mseconds(72_100),
        );
        assert_ne!(a, b, "существующий файл затёрся бы");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn unknown_streams_have_no_parser() {
        assert!(parser_for("video/x-h264").is_some());
        assert!(parser_for("audio/x-opus").is_some());
        assert!(parser_for("application/x-subtitle").is_none());
    }
}
