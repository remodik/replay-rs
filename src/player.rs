//! Просмотр клипа: playbin → кадры RGBA для окна.
//!
//! Плеер живёт, только пока открыт: декодирование начинается при открытии
//! и останавливается в `Drop`. С конвейером захвата он ничего не делит —
//! свой playbin, свои часы, свой поток декодирования.
//!
//! Кадры уменьшаются до 1280×720 (с сохранением пропорций) ещё в GStreamer:
//! в окно идёт не больше 3,7 МБ на кадр, и копия в текстуру egui — одна на
//! показанный кадр. appsink держит только последний кадр (`max-buffers=1
//! drop=true`): если окно не успевает, промежуточные кадры не копируются.

use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;

use crate::media::MediaInfo;

/// Предел размера кадра для окна.
const PREVIEW_CAPS: &str =
    "video/x-raw,format=RGBA,pixel-aspect-ratio=1/1,width=[16,1280],height=[16,720]";

/// Кадр для показа: размер и пиксели RGBA без выравнивания строк.
pub struct Frame {
    pub width: usize,
    pub height: usize,
    pub rgba: Vec<u8>,
}

/// Что случилось с воспроизведением с прошлого опроса.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// Дошли до конца (файла или выбранного диапазона).
    Finished,
    Error(String),
}

pub struct Player {
    playbin: gst::Element,
    bus: gst::Bus,
    latest: Arc<Mutex<Option<gst::Sample>>>,
    pub info: MediaInfo,
    playing: bool,
    /// Выбранная дорожка. playbin применяет выбор, когда пойдут данные, и до
    /// этого `current-audio` показывает прежнюю — окну нужен выбор.
    selected_audio: i32,
}

impl Player {
    /// Открывает клип на паузе, на первом кадре. `on_frame` зовётся из
    /// потока GStreamer при каждом новом кадре — окну пора перерисоваться.
    ///
    /// Блокирует до первого кадра (обычно десятки миллисекунд, с аппаратным
    /// декодером при первом запуске — дольше): звать из фонового потока.
    pub fn open(path: &Path, on_frame: impl Fn() + Send + Sync + 'static) -> Result<Self> {
        // Пустой, недописанный или битый файл отсеиваем до playbin: у него
        // ошибки беднее, а нам нужно понятное сообщение.
        let info = crate::media::probe(path)?;
        info.video().context("в клипе нет видео")?;

        let uri = gst::glib::filename_to_uri(path, None)
            .map_err(|e| anyhow::anyhow!("не удалось построить URI: {e}"))?;
        let playbin = gst::ElementFactory::make("playbin")
            .property("uri", uri.as_str())
            .build()
            .context("нет playbin (gst-plugins-base)")?;

        let sink_bin = gst::parse::bin_from_description(
            &format!(
                "videoconvertscale ! {PREVIEW_CAPS} ! \
                 appsink name=frames sync=true max-buffers=1 drop=true"
            ),
            true,
        )
        .context("не собрать вывод кадров")?;
        let sink = sink_bin
            .by_name("frames")
            .context("нет appsink")?
            .downcast::<gst_app::AppSink>()
            .map_err(|_| anyhow::anyhow!("frames не appsink"))?;
        let latest: Arc<Mutex<Option<gst::Sample>>> = Arc::new(Mutex::new(None));
        let slot = latest.clone();
        let store = move |sample: gst::Sample| {
            // Храним sample, а не копию пикселей: копия — только при показе.
            if let Ok(mut s) = slot.lock() {
                *s = Some(sample);
            }
            on_frame();
        };
        let store = Arc::new(store);
        let preroll_store = store.clone();
        sink.set_callbacks(
            gst_app::AppSinkCallbacks::builder()
                .new_sample(move |s| {
                    let sample = s.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                    store(sample);
                    Ok(gst::FlowSuccess::Ok)
                })
                .new_preroll(move |s| {
                    // Кадр на паузе и после перехода — иначе перемотка на
                    // паузе не показывала бы, куда перемотали.
                    let sample = s.pull_preroll().map_err(|_| gst::FlowError::Eos)?;
                    preroll_store(sample);
                    Ok(gst::FlowSuccess::Ok)
                })
                .build(),
        );
        playbin.set_property("video-sink", &sink_bin);
        let bus = playbin.bus().context("у playbin нет шины")?;
        playbin
            .set_state(gst::State::Paused)
            .context("клип не открылся")?;
        // Ждём первый кадр: только после этого playbin знает число дорожек.
        // Поэтому `open` зовут не из потока окна.
        let (ready, _, _) = playbin.state(gst::ClockTime::from_seconds(5));
        if ready.is_err() {
            let reason = bus
                .pop_filtered(&[gst::MessageType::Error])
                .and_then(|m| match m.view() {
                    gst::MessageView::Error(e) => Some(e.error().to_string()),
                    _ => None,
                })
                .unwrap_or_else(|| "не удалось начать воспроизведение".into());
            let _ = playbin.set_state(gst::State::Null);
            anyhow::bail!("{reason}");
        }
        Ok(Self {
            playbin,
            bus,
            latest,
            info,
            playing: false,
            selected_audio: 0,
        })
    }

    pub fn is_playing(&self) -> bool {
        self.playing
    }

    pub fn play(&mut self) {
        if self.playbin.set_state(gst::State::Playing).is_ok() {
            self.playing = true;
        }
    }

    pub fn pause(&mut self) {
        let _ = self.playbin.set_state(gst::State::Paused);
        self.playing = false;
    }

    /// Переход. `accurate` — точно в кадр (медленнее); без него — к
    /// ближайшему ключевому кадру, для перетаскивания ползунка.
    pub fn seek(&self, t: gst::ClockTime, accurate: bool) {
        let flags = if accurate {
            gst::SeekFlags::FLUSH | gst::SeekFlags::ACCURATE
        } else {
            gst::SeekFlags::FLUSH | gst::SeekFlags::KEY_UNIT
        };
        if let Err(e) = self.playbin.seek_simple(flags, t) {
            log::debug!("переход не удался: {e}");
        }
    }

    /// Проигрывает `[start, end)` и останавливается — предпросмотр обрезки.
    pub fn play_range(&mut self, start: gst::ClockTime, end: gst::ClockTime) {
        let ok = self.playbin.seek(
            1.0,
            gst::SeekFlags::FLUSH | gst::SeekFlags::ACCURATE,
            gst::SeekType::Set,
            start,
            gst::SeekType::Set,
            end,
        );
        if let Err(e) = ok {
            log::debug!("переход к диапазону не удался: {e}");
        }
        self.play();
    }

    pub fn position(&self) -> Option<gst::ClockTime> {
        self.playbin.query_position::<gst::ClockTime>()
    }

    pub fn duration(&self) -> Option<gst::ClockTime> {
        self.playbin
            .query_duration::<gst::ClockTime>()
            .or(self.info.duration)
    }

    pub fn set_volume(&self, volume: f64) {
        self.playbin.set_property("volume", volume.clamp(0.0, 1.5));
    }

    /// Число звуковых дорожек, которые видит playbin.
    pub fn audio_tracks(&self) -> i32 {
        self.playbin.property::<i32>("n-audio")
    }

    /// Дорожка, которую сейчас играет playbin.
    pub fn current_audio(&self) -> i32 {
        self.playbin.property::<i32>("current-audio")
    }

    /// Дорожка, которую выбрал пользователь.
    pub fn selected_audio(&self) -> i32 {
        self.selected_audio
    }

    pub fn set_audio(&mut self, index: i32) {
        self.selected_audio = index;
        self.playbin.set_property("current-audio", index);
    }

    /// Последний кадр, если пришёл новый. Пиксели копируются один раз —
    /// здесь, для текстуры окна.
    pub fn take_frame(&self) -> Option<Frame> {
        let sample = self.latest.lock().ok()?.take()?;
        sample_to_frame(&sample)
    }

    /// Разбирает шину без ожидания.
    pub fn poll(&mut self) -> Vec<Event> {
        let mut out = Vec::new();
        while let Some(msg) = self.bus.pop() {
            match msg.view() {
                gst::MessageView::Eos(_) => {
                    // Конец файла или диапазона: встаём на паузу, следующий
                    // «Играть» начнёт с текущего места.
                    self.pause();
                    out.push(Event::Finished);
                }
                gst::MessageView::Error(e) => {
                    self.pause();
                    out.push(Event::Error(e.error().to_string()));
                }
                _ => {}
            }
        }
        out
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        // NULL освобождает декодер (в том числе VA-контекст) и звуковой вывод.
        let _ = self.playbin.set_state(gst::State::Null);
    }
}

/// RGBA-кадр без выравнивания строк.
fn sample_to_frame(sample: &gst::Sample) -> Option<Frame> {
    let caps = sample.caps()?;
    let s = caps.structure(0)?;
    let width = s.get::<i32>("width").ok()? as usize;
    let height = s.get::<i32>("height").ok()? as usize;
    let buf = sample.buffer()?;
    let map = buf.map_readable().ok()?;
    let data = map.as_slice();
    let row = width * 4;
    if height == 0 || data.len() < row * height {
        return None;
    }
    // Шаг строки RGBA у GStreamer кратен 4, то есть обычно равен ширине ×4,
    // но по размеру буфера проверяем честно.
    let stride = data.len() / height;
    let rgba = if stride == row {
        data[..row * height].to_vec()
    } else {
        let mut out = Vec::with_capacity(row * height);
        for y in 0..height {
            out.extend_from_slice(&data[y * stride..y * stride + row]);
        }
        out
    };
    Some(Frame {
        width,
        height,
        rgba,
    })
}
