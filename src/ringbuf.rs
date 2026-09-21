//! Кольцевой буфер закодированных кадров с выбросом целыми GOP'ами.
//!
//! Инвариант: буфер всегда начинается с keyframe, поэтому любой сохранённый
//! клип можно декодировать с первого кадра.
//!
//! Порт `reference/ringbuf.py`. В отличие от прототипа буфер не содержит
//! внутреннего мьютекса: вызывающий код оборачивает его в `Arc<Mutex<_>>`
//! (кадры кладёт поток appsink, читают GUI и сохранение).

use std::collections::VecDeque;
use std::sync::Arc;

/// Наносекунд в секунде.
pub const NS: i64 = 1_000_000_000;

/// Закодированный кадр (access unit) в том виде, в каком его отдал энкодер.
#[derive(Clone, Debug)]
pub struct Frame {
    /// H.264 access unit. `Arc`, чтобы `snapshot` не копировал полезную нагрузку.
    pub data: Arc<[u8]>,
    /// Монотонное время захвата, нс.
    pub pts: i64,
    pub keyframe: bool,
}

impl Frame {
    pub fn new(data: impl Into<Arc<[u8]>>, pts: i64, keyframe: bool) -> Self {
        Self { data: data.into(), pts, keyframe }
    }
}

/// Группа кадров от keyframe до следующего keyframe.
///
/// Инвариант: `frames` никогда не пуст — GOP создаётся сразу с keyframe.
#[derive(Debug)]
struct Gop {
    frames: Vec<Frame>,
    bytes: usize,
}

impl Gop {
    fn start_pts(&self) -> i64 {
        self.frames[0].pts
    }
    fn end_pts(&self) -> i64 {
        self.frames[self.frames.len() - 1].pts
    }
}

/// Состояние буфера для статус-строки GUI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Stats {
    pub gops: usize,
    pub frames: usize,
    pub bytes: usize,
    /// Длительность буфера в наносекундах.
    pub nanos: i64,
}

impl Stats {
    pub fn seconds(&self) -> f64 {
        self.nanos as f64 / NS as f64
    }
}

#[derive(Debug)]
pub struct GopRingBuffer {
    max_ns: i64,
    max_bytes: usize,
    gops: VecDeque<Gop>,
    bytes: usize,
}

impl GopRingBuffer {
    pub fn new(seconds: f64, max_bytes: usize) -> Self {
        Self {
            max_ns: (seconds * NS as f64) as i64,
            max_bytes,
            gops: VecDeque::new(),
            bytes: 0,
        }
    }

    /// Меняет лимиты на лету (GUI двигает ползунки при работающей записи).
    /// Уменьшение лимита сразу подрезает буфер.
    pub fn set_limits(&mut self, seconds: f64, max_bytes: usize) {
        self.max_ns = (seconds * NS as f64) as i64;
        self.max_bytes = max_bytes;
        self.trim();
    }

    /// Выбрасывает всё.
    ///
    /// Нужно при пересборке конвейера: running time у нового конвейера идёт
    /// с нуля, и старые кадры с большими метками с новыми не совместимы —
    /// длительность буфера становится отрицательной, а срез выходит
    /// неупорядоченным.
    pub fn clear(&mut self) {
        self.gops.clear();
        self.bytes = 0;
    }

    pub fn add(&mut self, frame: Frame) {
        let len = frame.data.len();
        if frame.keyframe {
            self.gops.push_back(Gop { frames: vec![frame], bytes: len });
        } else if let Some(gop) = self.gops.back_mut() {
            gop.frames.push(frame);
            gop.bytes += len;
        } else {
            // Кадры до первого keyframe не сохраняем: без него их не декодировать.
            return;
        }
        self.bytes += len;
        self.trim();
    }

    /// Последний pts в буфере. Вызывать только когда буфер не пуст.
    fn last_pts(&self) -> i64 {
        self.gops[self.gops.len() - 1].end_pts()
    }

    /// Всегда оставляем минимум один GOP. Выбрасываем старейший GOP, только
    /// если после его удаления буфер остаётся достаточно длинным.
    fn trim(&mut self) {
        while self.gops.len() > 1 {
            let over_bytes = self.bytes > self.max_bytes;
            let over_time = self.last_pts() - self.gops[1].start_pts() >= self.max_ns;
            if !(over_bytes || over_time) {
                break;
            }
            if let Some(gop) = self.gops.pop_front() {
                self.bytes -= gop.bytes;
            }
        }
    }

    /// Все кадры буфера. Начинается с keyframe.
    pub fn snapshot(&self) -> Vec<Frame> {
        self.collect_from(0)
    }

    /// Последние `seconds` секунд, дополненные до границы GOP.
    /// Окно покрывается целиком, поэтому результат может быть длиннее
    /// запрошенного максимум на один GOP. Начинается с keyframe.
    pub fn snapshot_last(&self, seconds: f64) -> Vec<Frame> {
        if self.gops.is_empty() {
            return Vec::new();
        }
        let limit = self.last_pts() - (seconds * NS as f64) as i64;
        // Ищем последний GOP, начинающийся не позже limit, чтобы покрыть окно целиком.
        let mut start = 0;
        for (i, gop) in self.gops.iter().enumerate() {
            if gop.start_pts() <= limit {
                start = i;
            } else {
                break;
            }
        }
        self.collect_from(start)
    }

    fn collect_from(&self, start: usize) -> Vec<Frame> {
        self.gops
            .iter()
            .skip(start)
            .flat_map(|g| g.frames.iter().cloned())
            .collect()
    }

    pub fn stats(&self) -> Stats {
        if self.gops.is_empty() {
            return Stats::default();
        }
        Stats {
            gops: self.gops.len(),
            frames: self.gops.iter().map(|g| g.frames.len()).sum(),
            bytes: self.bytes,
            nanos: self.last_pts() - self.gops[0].start_pts(),
        }
    }
}

/// Порт `reference/test_ringbuf.py`.
///
/// Границы, которые прототип проверяет с допуском 1e-6 секунды, здесь
/// сравниваются точно в наносекундах.
#[cfg(test)]
mod tests {
    use super::*;

    const FPS: i64 = 60;
    /// keyframe раз в секунду
    const GOP: i64 = 60;
    const FRAME_NS: i64 = NS / FPS;

    /// Подаёт `seconds` секунд кадров, возвращает следующий свободный индекс.
    fn feed_at(
        buf: &mut GopRingBuffer,
        seconds: f64,
        frame_size: usize,
        start_idx: i64,
        gop: i64,
    ) -> i64 {
        let n = (seconds * FPS as f64) as i64;
        for i in start_idx..start_idx + n {
            buf.add(Frame::new(vec![b'x'; frame_size], i * FRAME_NS, i % gop == 0));
        }
        start_idx + n
    }

    fn feed(buf: &mut GopRingBuffer, seconds: f64) {
        feed_at(buf, seconds, 1000, 0, GOP);
    }

    #[test]
    fn starts_with_keyframe_after_trim() {
        let mut buf = GopRingBuffer::new(10.0, 1_000_000_000);
        feed(&mut buf, 35.0);
        let snap = buf.snapshot();
        assert!(snap[0].keyframe, "клип должен начинаться с keyframe");
    }

    #[test]
    fn time_limit_respected() {
        let mut buf = GopRingBuffer::new(10.0, 1_000_000_000);
        feed(&mut buf, 60.0);
        let span = buf.stats().nanos;
        // не короче запрошенного окна и не длиннее окна + один GOP
        assert!(span >= 10 * NS, "{span}");
        assert!(span <= 11 * NS, "{span}");
    }

    #[test]
    fn byte_limit_respected() {
        let mut buf = GopRingBuffer::new(1000.0, 500_000);
        feed_at(&mut buf, 60.0, 1000, 0, GOP); // 1000*60 = 60 КБ на GOP
        let st = buf.stats();
        // допуск на один свежий GOP
        assert!(st.bytes <= 500_000 + 60_000, "{}", st.bytes);
        assert!(buf.snapshot()[0].keyframe);
    }

    #[test]
    fn frames_before_first_keyframe_dropped() {
        let mut buf = GopRingBuffer::new(10.0, 1_000_000_000);
        // первые 5 кадров без keyframe (поток подключился посреди GOP)
        for i in 0..5 {
            buf.add(Frame::new(vec![b'x'], i * FRAME_NS, false));
        }
        assert_eq!(buf.stats().frames, 0);
        buf.add(Frame::new(vec![b'x'], 5 * FRAME_NS, true));
        assert_eq!(buf.stats().frames, 1);
    }

    #[test]
    fn snapshot_window_covers_requested_and_starts_on_keyframe() {
        let mut buf = GopRingBuffer::new(30.0, 1_000_000_000);
        feed(&mut buf, 45.0);
        let snap = buf.snapshot_last(10.0);
        assert!(snap[0].keyframe);
        let dur = snap[snap.len() - 1].pts - snap[0].pts;
        assert!(dur >= 10 * NS, "{dur}"); // покрывает запрошенные 10 с
        assert!(dur <= 11 * NS, "{dur}"); // и не больше чем +1 GOP
    }

    #[test]
    fn snapshot_window_larger_than_buffer_returns_all() {
        let mut buf = GopRingBuffer::new(10.0, 1_000_000_000);
        feed(&mut buf, 30.0);
        assert_eq!(buf.snapshot_last(999.0).len(), buf.snapshot().len());
    }

    #[test]
    fn empty_snapshot() {
        let buf = GopRingBuffer::new(10.0, 1_000_000_000);
        assert!(buf.snapshot().is_empty());
        assert!(buf.snapshot_last(5.0).is_empty());
    }

    #[test]
    fn single_huge_gop_not_dropped() {
        // если один GOP больше лимита, он остаётся (нельзя остаться без данных)
        let mut buf = GopRingBuffer::new(1.0, 10);
        feed_at(&mut buf, 2.0, 1000, 0, 10_000); // один keyframe на весь поток
        assert_eq!(buf.stats().gops, 1);
        assert!(buf.snapshot()[0].keyframe);
    }

    #[test]
    fn shrinking_limit_trims_immediately() {
        let mut buf = GopRingBuffer::new(30.0, 1_000_000_000);
        feed(&mut buf, 30.0);
        assert!(buf.stats().nanos > 20 * NS);
        buf.set_limits(5.0, 1_000_000_000);
        let span = buf.stats().nanos;
        assert!((5 * NS..=6 * NS).contains(&span), "{span}");
        assert!(buf.snapshot()[0].keyframe, "инвариант keyframe должен пережить подрезку");
    }

    /// Регрессия: после пересборки конвейера время идёт с нуля. Если не
    /// очистить буфер, старые кадры смешиваются с новыми: длительность
    /// становится отрицательной, а срез — неупорядоченным.
    #[test]
    fn restart_without_clear_would_corrupt_the_timeline() {
        let mut buf = GopRingBuffer::new(10.0, 1 << 30);
        feed_at(&mut buf, 5.0, 100, 6000, GOP); // «до перезапуска», метки далеко
        assert!(buf.stats().nanos > 0);

        buf.clear();
        assert_eq!(buf.stats().frames, 0);
        assert_eq!(buf.stats().bytes, 0);
        assert!(buf.snapshot().is_empty());

        feed_at(&mut buf, 2.0, 100, 0, GOP); // «после перезапуска», с нуля
        let st = buf.stats();
        assert!(st.nanos >= 0, "длительность буфера ушла в минус: {}", st.nanos);
        let pts: Vec<i64> = buf.snapshot().iter().map(|f| f.pts).collect();
        let mut sorted = pts.clone();
        sorted.sort_unstable();
        assert_eq!(pts, sorted, "метки в срезе неупорядочены");
    }

    #[test]
    fn irregular_gops() {
        let mut buf = GopRingBuffer::new(5.0, 1_000_000_000);
        let mut idx = 0i64;
        for gop_len in [30, 90, 45, 120, 60, 200, 15, 60] {
            for j in 0..gop_len {
                buf.add(Frame::new(vec![b'x'], idx * FRAME_NS, j == 0));
                idx += 1;
            }
        }
        let snap = buf.snapshot();
        assert!(snap[0].keyframe);
        let pts: Vec<i64> = snap.iter().map(|f| f.pts).collect();
        let mut sorted = pts.clone();
        sorted.sort_unstable();
        assert_eq!(pts, sorted, "порядок кадров нарушен");
    }
}
