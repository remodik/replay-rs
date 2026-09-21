//! Кольцевой буфер звука и выбор аудиокодека.
//!
//! В отличие от видео, кадры звука (AAC/Opus) декодируются независимо друг
//! от друга, поэтому GOP'ов здесь нет и резать можно по любому кадру.

use std::collections::VecDeque;

use crate::ringbuf::{Frame, Stats, NS};

/// Аудиокодек, который умеет лежать в mp4.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum AudioCodec {
    /// Предпочтительный: понимают все плееры. Требует gst-libav.
    Aac,
    /// Запасной: mp4mux его принимает, gst-libav не нужен.
    Opus,
}

impl AudioCodec {
    /// Элемент-кодировщик для описания конвейера.
    pub fn encoder_desc(self, bitrate_kbps: u32) -> String {
        match self {
            // avenc_aac считает битрейт в битах, opusenc — тоже.
            AudioCodec::Aac => format!("avenc_aac bitrate={}", bitrate_kbps * 1000),
            AudioCodec::Opus => format!("opusenc bitrate={}", bitrate_kbps * 1000),
        }
    }

    /// Что поставить между кодировщиком и mp4mux.
    pub fn parser(self) -> Option<&'static str> {
        match self {
            AudioCodec::Aac => Some("aacparse"),
            // opusparse в этой сборке GStreamer отсутствует, а mp4mux
            // принимает выход opusenc напрямую.
            AudioCodec::Opus => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            AudioCodec::Aac => "AAC",
            AudioCodec::Opus => "Opus",
        }
    }
}

/// Чего хочет пользователь. Отдельно от того, что в системе есть.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum CodecPreference {
    /// AAC, если доступен, иначе Opus.
    #[default]
    Auto,
    Aac,
    Opus,
}

impl CodecPreference {
    pub fn label(self) -> &'static str {
        match self {
            CodecPreference::Auto => "авто (AAC, иначе Opus)",
            CodecPreference::Aac => "AAC",
            CodecPreference::Opus => "Opus",
        }
    }
}

/// Чем закончился выбор кодека.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodecChoice {
    /// Получили ровно то, что просили.
    Exact(AudioCodec),
    /// Просили AAC, но gst-libav не установлен — пишем Opus.
    FellBack(AudioCodec),
    /// Ни одного кодировщика нет.
    None,
}

impl CodecChoice {
    pub fn codec(self) -> Option<AudioCodec> {
        match self {
            CodecChoice::Exact(c) | CodecChoice::FellBack(c) => Some(c),
            CodecChoice::None => None,
        }
    }
}

/// Выбирает кодек с учётом пожелания и того, что реально есть в системе.
///
/// Требует инициализированного GStreamer: смотрит реестр элементов.
pub fn choose_codec(pref: CodecPreference) -> CodecChoice {
    let aac = has_element("avenc_aac");
    let opus = has_element("opusenc");
    match pref {
        CodecPreference::Aac if aac => CodecChoice::Exact(AudioCodec::Aac),
        // Молча подменять запрошенный кодек нельзя — об этом надо сказать.
        CodecPreference::Aac if opus => CodecChoice::FellBack(AudioCodec::Opus),
        CodecPreference::Opus if opus => CodecChoice::Exact(AudioCodec::Opus),
        CodecPreference::Auto if aac => CodecChoice::Exact(AudioCodec::Aac),
        CodecPreference::Auto if opus => CodecChoice::Exact(AudioCodec::Opus),
        _ => CodecChoice::None,
    }
}

fn has_element(name: &str) -> bool {
    gstreamer::ElementFactory::find(name).is_some()
}

/// Кольцевой буфер закодированного звука.
#[derive(Debug)]
pub struct AudioRingBuffer {
    max_ns: i64,
    max_bytes: usize,
    frames: VecDeque<Frame>,
    bytes: usize,
}

impl AudioRingBuffer {
    pub fn new(seconds: f64, max_bytes: usize) -> Self {
        Self {
            max_ns: (seconds * NS as f64) as i64,
            max_bytes,
            frames: VecDeque::new(),
            bytes: 0,
        }
    }

    pub fn set_limits(&mut self, seconds: f64, max_bytes: usize) {
        self.max_ns = (seconds * NS as f64) as i64;
        self.max_bytes = max_bytes;
        self.trim();
    }

    /// Выбрасывает всё — см. `GopRingBuffer::clear`.
    pub fn clear(&mut self) {
        self.frames.clear();
        self.bytes = 0;
    }

    pub fn add(&mut self, frame: Frame) {
        self.bytes += frame.data.len();
        self.frames.push_back(frame);
        self.trim();
    }

    /// Выбрасываем старейшие кадры, пока после выброса остаётся не меньше
    /// запрошенного окна. Минимум один кадр остаётся всегда.
    fn trim(&mut self) {
        while self.frames.len() > 1 {
            let last = self.frames[self.frames.len() - 1].pts;
            let over_time = last - self.frames[1].pts >= self.max_ns;
            if !(self.bytes > self.max_bytes || over_time) {
                break;
            }
            if let Some(f) = self.frames.pop_front() {
                self.bytes -= f.data.len();
            }
        }
    }

    /// Кадры, покрывающие окно `[from_pts, to_pts]`.
    ///
    /// Включает кадр, начавшийся до `from_pts`: у аудиокадра есть
    /// длительность, и без него начало клипа осталось бы без звука.
    pub fn snapshot_range(&self, from_pts: i64, to_pts: i64) -> Vec<Frame> {
        if self.frames.is_empty() {
            return Vec::new();
        }
        let mut start = 0;
        for (i, f) in self.frames.iter().enumerate() {
            if f.pts <= from_pts {
                start = i;
            } else {
                break;
            }
        }
        self.frames
            .iter()
            .skip(start)
            .take_while(|f| f.pts <= to_pts)
            .cloned()
            .collect()
    }

    pub fn stats(&self) -> Stats {
        if self.frames.is_empty() {
            return Stats::default();
        }
        Stats {
            gops: 0,
            frames: self.frames.len(),
            bytes: self.bytes,
            nanos: self.frames[self.frames.len() - 1].pts - self.frames[0].pts,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Кадр Opus на 20 мс — типичный размер пакета.
    const FRAME_NS: i64 = NS / 50;

    fn feed(buf: &mut AudioRingBuffer, seconds: f64, size: usize) {
        let n = (seconds * 50.0) as i64;
        for i in 0..n {
            buf.add(Frame::new(vec![0u8; size], i * FRAME_NS, true));
        }
    }

    #[test]
    fn preference_is_reported_honestly() {
        // Подмену запрошенного AAC на Opus обязаны отметить как FellBack,
        // иначе пользователь думает, что пишет AAC.
        assert_eq!(CodecChoice::FellBack(AudioCodec::Opus).codec(), Some(AudioCodec::Opus));
        assert_eq!(CodecChoice::None.codec(), None);
        assert_eq!(CodecPreference::default(), CodecPreference::Auto);
    }

    #[test]
    fn empty_buffer_yields_nothing() {
        let buf = AudioRingBuffer::new(10.0, 1 << 20);
        assert!(buf.snapshot_range(0, 10 * NS).is_empty());
        assert_eq!(buf.stats().frames, 0);
    }

    #[test]
    fn time_limit_respected() {
        let mut buf = AudioRingBuffer::new(10.0, 1 << 30);
        feed(&mut buf, 60.0, 100);
        let span = buf.stats().nanos;
        assert!(span >= 10 * NS, "{span}");
        // Лишку не больше одного кадра.
        assert!(span <= 10 * NS + FRAME_NS, "{span}");
    }

    #[test]
    fn byte_limit_respected() {
        let mut buf = AudioRingBuffer::new(1000.0, 50_000);
        feed(&mut buf, 60.0, 100);
        assert!(buf.stats().bytes <= 50_000 + 100, "{}", buf.stats().bytes);
    }

    #[test]
    fn range_covers_requested_window() {
        let mut buf = AudioRingBuffer::new(60.0, 1 << 30);
        feed(&mut buf, 30.0, 100);
        let from = 10 * NS;
        let to = 15 * NS;
        let got = buf.snapshot_range(from, to);
        assert!(!got.is_empty());
        // Первый кадр начинается не позже запрошенного начала — иначе
        // в начале клипа была бы дырка.
        assert!(got[0].pts <= from, "{} > {from}", got[0].pts);
        assert!(got[got.len() - 1].pts <= to);
        // И покрывает окно до конца.
        assert!(got[got.len() - 1].pts + FRAME_NS >= to);
    }

    #[test]
    fn range_beyond_buffer_is_clamped() {
        let mut buf = AudioRingBuffer::new(60.0, 1 << 30);
        feed(&mut buf, 5.0, 100);
        let got = buf.snapshot_range(-NS, 999 * NS);
        assert_eq!(got.len(), buf.stats().frames);
    }

    #[test]
    fn frames_stay_ordered() {
        let mut buf = AudioRingBuffer::new(5.0, 1 << 30);
        feed(&mut buf, 20.0, 100);
        let got = buf.snapshot_range(i64::MIN / 2, i64::MAX / 2);
        let pts: Vec<i64> = got.iter().map(|f| f.pts).collect();
        let mut sorted = pts.clone();
        sorted.sort_unstable();
        assert_eq!(pts, sorted);
    }

    #[test]
    fn clear_empties_the_buffer() {
        let mut buf = AudioRingBuffer::new(10.0, 1 << 30);
        feed(&mut buf, 5.0, 100);
        buf.clear();
        assert_eq!(buf.stats().frames, 0);
        assert_eq!(buf.stats().bytes, 0);
    }

    #[test]
    fn shrinking_limit_trims_immediately() {
        let mut buf = AudioRingBuffer::new(30.0, 1 << 30);
        feed(&mut buf, 30.0, 100);
        buf.set_limits(5.0, 1 << 30);
        let span = buf.stats().nanos;
        assert!(span <= 5 * NS + FRAME_NS, "{span}");
    }
}
