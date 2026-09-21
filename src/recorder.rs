//! Склейка буфера и сохранения: снимок последних N секунд → mp4.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use anyhow::{Context, Result};

use crate::audio::{AudioCodec, AudioRingBuffer};
use crate::config::Config;
use crate::pipeline::{SharedAudio, SharedBuffer};
use crate::ringbuf::{GopRingBuffer, Stats};
use crate::saver;

/// Запас звукового буфера сверх видеоокна.
///
/// Видео хранится целыми GOP'ами и потому тянется чуть дальше запрошенного
/// окна. Без запаса начало клипа осталось бы без звука.
fn audio_window(cfg: &Config) -> f64 {
    cfg.seconds + cfg.gop + 1.0
}

/// Лимит звукового буфера в байтах — с двукратным запасом к битрейту.
fn audio_bytes(cfg: &Config) -> usize {
    let per_sec = cfg.audio.bitrate as usize * 1000 / 8;
    (per_sec as f64 * audio_window(cfg) * 2.0) as usize
}

/// Чем закончилась попытка сохранения.
///
/// Раньше всё это было `Option<PathBuf>`, и «буфер пуст» было не отличить от
/// «предыдущее сохранение ещё идёт» — окно показывало про пустой буфер, когда
/// на самом деле сохранение залипло.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SaveOutcome {
    Saved(PathBuf),
    /// В кольцевом буфере нет кадров: захват не идёт или только начался.
    BufferEmpty,
    /// Предыдущее сохранение ещё не закончилось.
    AlreadySaving,
}

/// Сбрасывает флаг сохранения, чем бы ни кончилось — ошибкой, ранним
/// возвратом или паникой. Иначе одно зависшее сохранение навсегда запирает
/// все последующие.
struct SavingGuard<'a>(&'a AtomicBool);

impl Drop for SavingGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

pub struct Recorder {
    /// GUI меняет настройки на ходу, поток захвата их читает.
    cfg: RwLock<Config>,
    pub ring: SharedBuffer,
    /// `None`, если звук выключен или в системе нет подходящего кодировщика.
    pub audio: Option<SharedAudio>,
    pub codec: Option<AudioCodec>,
    /// Второе одновременное сохранение пропускаем, а не ставим в очередь:
    /// хоткей легко нажать дважды.
    saving: AtomicBool,
}

impl Recorder {
    /// `codec` определяет вызывающий код: для этого нужен инициализированный
    /// GStreamer, а конструктор не должен этого втихую требовать.
    pub fn new(cfg: Config, codec: Option<AudioCodec>) -> Self {
        let ring = Arc::new(Mutex::new(GopRingBuffer::new(cfg.seconds, cfg.max_bytes())));
        let audio = codec.map(|_| SharedAudio {
            buffer: Arc::new(Mutex::new(AudioRingBuffer::new(
                audio_window(&cfg),
                audio_bytes(&cfg),
            ))),
            caps: Arc::new(Mutex::new(None)),
        });
        Self { cfg: RwLock::new(cfg), ring, audio, codec, saving: AtomicBool::new(false) }
    }

    pub fn config(&self) -> Config {
        self.cfg.read().expect("конфиг отравлен").clone()
    }

    /// Применяет новые настройки. Возвращает `true`, если для них нужно
    /// пересобрать конвейер захвата (fps, битрейт, GOP, кодировщик, размер).
    pub fn update_config(&self, new: Config) -> bool {
        let mut guard = self.cfg.write().expect("конфиг отравлен");
        let restart = guard.needs_restart(&new);
        // Длина буфера и лимит по байтам применяются сразу, без перезапуска.
        if guard.seconds != new.seconds || guard.max_mb != new.max_mb {
            self.ring
                .lock()
                .expect("кольцевой буфер отравлен")
                .set_limits(new.seconds, new.max_bytes());
            if let Some(a) = &self.audio {
                a.buffer
                    .lock()
                    .expect("звуковой буфер отравлен")
                    .set_limits(audio_window(&new), audio_bytes(&new));
            }
        }
        *guard = new;
        restart
    }

    /// Сбрасывает буферы перед пересборкой конвейера.
    ///
    /// У нового конвейера running time начинается с нуля, поэтому старые
    /// кадры с новыми несовместимы: длительность буфера уходит в минус, а
    /// срез получается неупорядоченным и непригодным для мьюксинга.
    pub fn reset_buffers(&self) {
        self.ring.lock().expect("кольцевой буфер отравлен").clear();
        if let Some(a) = &self.audio {
            a.buffer.lock().expect("звуковой буфер отравлен").clear();
        }
    }

    pub fn stats(&self) -> Stats {
        self.ring.lock().expect("кольцевой буфер отравлен").stats()
    }

    /// Сохраняет последние `seconds` секунд (по умолчанию `save_seconds`).
    pub fn save(&self, seconds: Option<f64>) -> Result<SaveOutcome> {
        if self
            .saving
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            log::warn!("сохранение уже идёт");
            return Ok(SaveOutcome::AlreadySaving);
        }
        let _guard = SavingGuard(&self.saving);
        let cfg = self.config();
        self.save_inner(&cfg, seconds.unwrap_or(cfg.save_seconds))
    }

    fn save_inner(&self, cfg: &Config, seconds: f64) -> Result<SaveOutcome> {
        // Снимок берём под мьютексом, запись на диск — уже без него.
        let frames = self
            .ring
            .lock()
            .expect("кольцевой буфер отравлен")
            .snapshot_last(seconds);
        if frames.is_empty() {
            log::warn!("буфер пуст — нечего сохранять");
            return Ok(SaveOutcome::BufferEmpty);
        }

        std::fs::create_dir_all(&cfg.output)
            .with_context(|| format!("не удалось создать {}", cfg.output.display()))?;
        let stamp = chrono::Local::now().format("%Y-%m-%d_%H-%M-%S").to_string();
        let out = saver::clip_path(&cfg.output, &stamp);

        // Звук режем по границам видеоклипа: часы у них общие.
        let a_frames = self.audio.as_ref().map(|a| {
            a.buffer
                .lock()
                .expect("звуковой буфер отравлен")
                .snapshot_range(frames[0].pts, frames[frames.len() - 1].pts)
        });
        let a_caps = self
            .audio
            .as_ref()
            .and_then(|a| a.caps.lock().expect("caps звука отравлены").clone());
        let track = match (&a_frames, &a_caps) {
            (Some(f), Some(c)) => Some(saver::AudioTrack { frames: f, caps: c }),
            _ => None,
        };
        if track.is_none() && cfg.audio.any_enabled() {
            log::warn!("звук включён, но в буфере нет кадров — сохраняю без звука");
        }

        saver::write_clip_with_audio(&frames, cfg.fps, track, &out)?;

        let dur = (frames[frames.len() - 1].pts - frames[0].pts) as f64 / crate::ringbuf::NS as f64;
        let mb = std::fs::metadata(&out).map(|m| m.len()).unwrap_or(0) as f64 / 1e6;
        log::info!("сохранено: {} (~{dur:.1} с, {mb:.1} МБ)", out.display());
        Ok(SaveOutcome::Saved(out))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_buffer_is_distinguishable_from_busy() {
        // Именно смешение этих двух случаев показывало «буфер пуст», когда на
        // самом деле предыдущее сохранение залипло.
        assert_ne!(SaveOutcome::BufferEmpty, SaveOutcome::AlreadySaving);
    }

    #[test]
    fn saving_flag_resets_even_on_error() {
        let rec = Recorder::new(Config::default(), None);
        // Буфер пуст — save вернётся рано, но флаг обязан сброситься.
        assert_eq!(rec.save(None).unwrap(), SaveOutcome::BufferEmpty);
        assert_eq!(rec.save(None).unwrap(), SaveOutcome::BufferEmpty);
        assert!(
            !rec.saving.load(Ordering::Acquire),
            "флаг сохранения залип — следующие попытки будут молча отвергаться"
        );
    }

    #[test]
    fn live_settings_apply_without_restart() {
        let rec = Recorder::new(Config::default(), None);
        let mut cfg = rec.config();
        cfg.seconds = 5.0;
        cfg.output = PathBuf::from("/tmp/other");
        assert!(!rec.update_config(cfg), "длина буфера и папка меняются на лету");
    }

    #[test]
    fn encoder_settings_require_restart() {
        let rec = Recorder::new(Config::default(), None);
        let mut cfg = rec.config();
        cfg.fps = 30;
        assert!(rec.update_config(cfg), "смена fps требует пересборки конвейера");
    }

    #[test]
    fn shrinking_buffer_applies_to_ring_immediately() {
        let rec = Recorder::new(Config { seconds: 60.0, ..Config::default() }, None);
        let mut cfg = rec.config();
        cfg.seconds = 2.0;
        rec.update_config(cfg);
        // Лимит доехал до буфера, а не осел в конфиге.
        let mut ring = rec.ring.lock().unwrap();
        for i in 0..600i64 {
            ring.add(crate::ringbuf::Frame::new(
                vec![0u8; 10],
                i * (crate::ringbuf::NS / 60),
                i % 60 == 0,
            ));
        }
        let span = ring.stats().nanos;
        assert!(span <= 3 * crate::ringbuf::NS, "{span}");
    }
}
