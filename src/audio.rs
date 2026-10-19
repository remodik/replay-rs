//! Кольцевой буфер звука и выбор аудиокодека.
//!
//! В отличие от видео, кадры звука (AAC/Opus) декодируются независимо друг
//! от друга, поэтому GOP'ов здесь нет и резать можно по любому кадру.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

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

/// Откуда берётся звук.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AudioSource {
    /// Монитор устройства вывода (на Windows — loopback WASAPI).
    System,
    Mic,
}

impl AudioSource {
    pub const ALL: [AudioSource; 2] = [AudioSource::System, AudioSource::Mic];

    /// Короткое имя для элементов конвейера: `src_sys`, `level_mic`.
    pub fn tag(self) -> &'static str {
        match self {
            AudioSource::System => "sys",
            AudioSource::Mic => "mic",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            AudioSource::System => "Системный звук",
            AudioSource::Mic => "Микрофон",
        }
    }

    /// Имя элемента-источника в конвейере. По нему ошибку шины относят к
    /// конкретному источнику, а не ко всему захвату.
    pub fn element_name(self) -> String {
        format!("src_{}", self.tag())
    }

    pub fn level_name(self) -> String {
        format!("level_{}", self.tag())
    }

    /// Источник, которому принадлежит элемент с таким именем.
    pub fn from_element_name(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|s| name == s.element_name() || name == s.level_name())
    }

    /// Источник, внутри ветки которого живёт объект.
    ///
    /// Описание источника обёрнуто в bin с именем `src_*`, поэтому ошибку
    /// любого его элемента — самого pulsesrc или, скажем, `identity`, через
    /// который тест изображает отказ, — можно отнести к источнику по предкам.
    pub fn owning(obj: &gstreamer::Object) -> Option<Self> {
        use gstreamer::prelude::*;
        let mut cur = Some(obj.clone());
        while let Some(o) = cur {
            if let Some(s) = Self::from_element_name(o.name().as_str()) {
                return Some(s);
            }
            cur = o.parent();
        }
        None
    }
}

/// Что лежит в звуковой дорожке клипа.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TrackKind {
    System,
    Mic,
    /// Системный звук и микрофон, сведённые в одну дорожку.
    Mixed,
}

impl TrackKind {
    pub fn tag(self) -> &'static str {
        match self {
            TrackKind::System => "sys",
            TrackKind::Mic => "mic",
            TrackKind::Mixed => "mix",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            TrackKind::System => "Системный звук",
            TrackKind::Mic => "Микрофон",
            TrackKind::Mixed => "Системный звук + микрофон",
        }
    }

    /// Имя звукового appsink дорожки в конвейере захвата.
    pub fn sink_name(self) -> String {
        format!("asink_{}", self.tag())
    }
}

/// Одна дорожка, которую должен писать конвейер, и её источники.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackPlan {
    pub kind: TrackKind,
    pub sources: Vec<AudioSource>,
}

/// Порог, ниже которого пик считаем тишиной, дБFS.
pub const SILENCE_DB: f32 = -60.0;
/// Сколько можно не получать измерений, прежде чем сказать «нет данных».
/// `level` присылает их раз в `LEVEL_INTERVAL`, поэтому запас трёхкратный.
const STALE_AFTER_MS: u64 = 1500;
/// Как часто `level` присылает уровень. Чаще не нужно: это индикатор, а не
/// измерительный прибор, и каждое сообщение — пробуждение потока захвата.
pub const LEVEL_INTERVAL_NS: u64 = 500_000_000;

fn now_ms() -> u64 {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    // +1: ноль означает «измерений ещё не было».
    EPOCH.get_or_init(Instant::now).elapsed().as_millis() as u64 + 1
}

/// Живое состояние одного источника: открылся ли он и есть ли сигнал.
///
/// Пишет поток захвата (по сообщениям `level` и ошибкам шины), читают GUI,
/// трей и оверлей. Атомики вместо мьютекса: читатели дёргают его каждый кадр.
#[derive(Debug, Default)]
pub struct SourceMeter {
    /// Источник есть в текущем конвейере.
    active: AtomicBool,
    peak_db_bits: AtomicU32,
    updated_ms: AtomicU64,
    /// Почему источник не пишется: не открылся или отвалился посреди записи.
    failure: Mutex<Option<String>>,
}

/// Что показать пользователю про источник.
#[derive(Debug, Clone, PartialEq)]
pub enum SourceStatus {
    /// Выключен в настройках или конвейер ещё не собран.
    Off,
    /// Не открылся или отвалился; запись идёт без него.
    Failed(String),
    /// Источник в конвейере, но буферов от него нет (например, монитор
    /// вывода, пока ничего не играет).
    NoData,
    Silent,
    Signal {
        peak_db: f32,
    },
}

impl SourceStatus {
    pub fn label(&self) -> String {
        match self {
            SourceStatus::Off => "выключен".into(),
            SourceStatus::Failed(_) => "недоступен".into(),
            SourceStatus::NoData => "нет данных".into(),
            SourceStatus::Silent => "тишина".into(),
            SourceStatus::Signal { peak_db } => format!("сигнал {peak_db:.0} дБ"),
        }
    }
}

impl SourceMeter {
    /// Источник вошёл в новый конвейер: прошлые измерения и ошибки неактуальны.
    pub fn activate(&self) {
        self.active.store(true, Ordering::Release);
        self.updated_ms.store(0, Ordering::Release);
        *self.failure.lock().expect("состояние источника отравлено") = None;
    }

    pub fn deactivate(&self) {
        self.active.store(false, Ordering::Release);
        self.updated_ms.store(0, Ordering::Release);
    }

    pub fn fail(&self, reason: String) {
        self.active.store(false, Ordering::Release);
        *self.failure.lock().expect("состояние источника отравлено") = Some(reason);
    }

    /// Сбрасывает отказ: перед новой попыткой открыть источник.
    pub fn clear_failure(&self) {
        *self.failure.lock().expect("состояние источника отравлено") = None;
    }

    pub fn record_peak(&self, peak_db: f32) {
        self.peak_db_bits
            .store(peak_db.to_bits(), Ordering::Relaxed);
        self.updated_ms.store(now_ms(), Ordering::Release);
    }

    pub fn status(&self) -> SourceStatus {
        if let Some(reason) = self
            .failure
            .lock()
            .expect("состояние источника отравлено")
            .clone()
        {
            return SourceStatus::Failed(reason);
        }
        if !self.active.load(Ordering::Acquire) {
            return SourceStatus::Off;
        }
        let at = self.updated_ms.load(Ordering::Acquire);
        if at == 0 || now_ms().saturating_sub(at) > STALE_AFTER_MS {
            return SourceStatus::NoData;
        }
        let peak_db = f32::from_bits(self.peak_db_bits.load(Ordering::Relaxed));
        if peak_db < SILENCE_DB {
            SourceStatus::Silent
        } else {
            SourceStatus::Signal { peak_db }
        }
    }
}

/// Индикаторы обоих источников.
#[derive(Debug, Default)]
pub struct SourceMeters {
    pub system: SourceMeter,
    pub mic: SourceMeter,
}

impl SourceMeters {
    pub fn get(&self, source: AudioSource) -> &SourceMeter {
        match source {
            AudioSource::System => &self.system,
            AudioSource::Mic => &self.mic,
        }
    }
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

    /// Метка самого свежего кадра.
    pub fn last_pts(&self) -> Option<i64> {
        self.frames.back().map(|f| f.pts)
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
        assert_eq!(
            CodecChoice::FellBack(AudioCodec::Opus).codec(),
            Some(AudioCodec::Opus)
        );
        assert_eq!(CodecChoice::None.codec(), None);
        assert_eq!(CodecPreference::default(), CodecPreference::Auto);
    }

    #[test]
    fn meter_reports_failure_signal_and_silence() {
        let m = SourceMeter::default();
        assert_eq!(m.status(), SourceStatus::Off);
        m.activate();
        assert_eq!(m.status(), SourceStatus::NoData, "измерений ещё не было");
        m.record_peak(-12.0);
        assert_eq!(m.status(), SourceStatus::Signal { peak_db: -12.0 });
        m.record_peak(-90.0);
        assert_eq!(m.status(), SourceStatus::Silent);
        m.fail("нет устройства".into());
        assert_eq!(m.status(), SourceStatus::Failed("нет устройства".into()));
        // Новая попытка открыть источник стирает старый отказ.
        m.activate();
        assert_eq!(m.status(), SourceStatus::NoData);
    }

    #[test]
    fn errors_are_attributed_to_their_source_bin() {
        gstreamer::init().unwrap();
        use gstreamer::prelude::*;
        let p = gstreamer::parse::launch(
            "bin.( name=src_mic audiotestsrc name=inner ! identity ) ! fakesink",
        )
        .unwrap()
        .downcast::<gstreamer::Bin>()
        .unwrap();
        let inner = p.by_name("inner").unwrap();
        assert_eq!(
            AudioSource::owning(inner.upcast_ref()),
            Some(AudioSource::Mic)
        );
        let sink = p.iterate_sinks().next().unwrap().unwrap();
        assert_eq!(AudioSource::owning(sink.upcast_ref()), None);
        assert_eq!(
            AudioSource::from_element_name("level_sys"),
            Some(AudioSource::System)
        );
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
