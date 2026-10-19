//! Склейка буфера и сохранения: снимок последних N секунд → mp4.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use anyhow::{Context, Result};

use crate::audio::{AudioCodec, AudioSource, SourceMeters, TrackKind};
use crate::config::Config;
use crate::encoders::{Discovery, EncoderSpec, VideoCodec};
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

/// Сколько сохранение ждёт, пока звук догонит видео.
const AUDIO_CATCH_UP: std::time::Duration = std::time::Duration::from_millis(400);

/// Лимит звукового буфера в байтах — с двукратным запасом к битрейту.
fn audio_bytes(cfg: &Config) -> usize {
    let per_sec = cfg.audio.bitrate as usize * 1000 / 8;
    (per_sec as f64 * audio_window(cfg) * 2.0) as usize
}

/// Что известно о кодировщике видео: итоги обнаружения и чем пишем сейчас.
#[derive(Debug, Clone, Default)]
pub struct EncoderStatus {
    /// `None`, пока поток захвата не закончил обнаружение.
    pub discovery: Option<Arc<Discovery>>,
    /// Кодировщик текущего конвейера.
    pub active: Option<EncoderSpec>,
    /// Почему пишем не тем, что выбрано, — причина отката.
    pub note: Option<String>,
}

/// Пустые буферы под дорожки плана.
fn make_tracks(cfg: &Config, plan: &[crate::audio::TrackPlan]) -> Vec<SharedAudio> {
    plan.iter()
        .map(|t| SharedAudio::new(t.kind, audio_window(cfg), audio_bytes(cfg)))
        .collect()
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
    /// Звуковые дорожки текущего конвейера, каждая со своим буфером.
    ///
    /// Состав зависит от режима звука и от того, какие источники открылись,
    /// поэтому поток захвата задаёт его заново при каждой сборке конвейера
    /// (`prepare_audio`). Пусто, если звук выключен или нет кодировщика.
    audio: RwLock<Vec<SharedAudio>>,
    /// Кодек звука последней сборки конвейера. `None` — кодировщика нет.
    codec: RwLock<Option<AudioCodec>>,
    /// Состояние источников звука для GUI, трея и оверлея.
    pub meters: SourceMeters,
    /// Итоги обнаружения кодировщиков и тот, которым пишем сейчас.
    encoder: RwLock<EncoderStatus>,
    /// Второе одновременное сохранение пропускаем, а не ставим в очередь:
    /// хоткей легко нажать дважды.
    saving: AtomicBool,
    last_save: Mutex<Option<SaveRecord>>,
    save_count: std::sync::atomic::AtomicU64,
    on_saved: Mutex<Vec<SaveHook>>,
}

type SaveHook = Box<dyn Fn(&SaveRecord) + Send + Sync>;

/// Чем кончилось завершённое сохранение (не «уже идёт»).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SaveRecord {
    pub at: std::time::SystemTime,
    pub result: Result<PathBuf, String>,
}

impl Recorder {
    /// `codec` определяет вызывающий код: для этого нужен инициализированный
    /// GStreamer, а конструктор не должен этого втихую требовать. Поток
    /// захвата перевыбирает кодек при каждой пересборке конвейера.
    pub fn new(cfg: Config, codec: Option<AudioCodec>) -> Self {
        let ring = Arc::new(Mutex::new(GopRingBuffer::new(cfg.seconds, cfg.max_bytes())));
        let audio = match codec {
            Some(_) => make_tracks(&cfg, &cfg.audio.track_plan(&[])),
            None => Vec::new(),
        };
        Self {
            cfg: RwLock::new(cfg),
            ring,
            audio: RwLock::new(audio),
            codec: RwLock::new(codec),
            meters: SourceMeters::default(),
            encoder: RwLock::new(EncoderStatus::default()),
            saving: AtomicBool::new(false),
            last_save: Mutex::new(None),
            save_count: std::sync::atomic::AtomicU64::new(0),
            on_saved: Mutex::new(Vec::new()),
        }
    }

    /// Что известно о кодировщике видео.
    pub fn encoder_status(&self) -> EncoderStatus {
        self.encoder
            .read()
            .expect("состояние кодировщика отравлено")
            .clone()
    }

    /// Добавляет результаты проверки к уже известным и возвращает итог.
    ///
    /// Проверки идут двумя потоками — нужный кодек сразу, остальные в
    /// фоне, — поэтому результаты сливаются, а не заменяют друг друга.
    pub fn merge_discovery(&self, fresh: Discovery) -> Arc<Discovery> {
        let mut st = self
            .encoder
            .write()
            .expect("состояние кодировщика отравлено");
        let mut merged = st.discovery.as_deref().cloned().unwrap_or_default();
        merged.millis += fresh.millis;
        for p in fresh.probed {
            match merged
                .probed
                .iter_mut()
                .find(|q| q.spec.element == p.spec.element)
            {
                Some(q) => *q = p,
                None => merged.probed.push(p),
            }
        }
        let merged = Arc::new(merged);
        st.discovery = Some(merged.clone());
        merged
    }

    /// Поток захвата сообщает, каким кодировщиком собран текущий конвейер.
    pub fn set_active_encoder(&self, spec: Option<EncoderSpec>, note: Option<String>) {
        let mut st = self
            .encoder
            .write()
            .expect("состояние кодировщика отравлено");
        st.active = spec;
        st.note = note;
    }

    /// Кодек видео в буфере: с ним кадры и ремьюксируются.
    fn video_codec(&self) -> VideoCodec {
        self.encoder
            .read()
            .expect("состояние кодировщика отравлено")
            .active
            .as_ref()
            .map_or(VideoCodec::H264, |s| s.codec)
    }

    pub fn config(&self) -> Config {
        self.cfg.read().expect("конфиг отравлен").clone()
    }

    /// Кодек звука, которым пишет текущий конвейер.
    pub fn codec(&self) -> Option<AudioCodec> {
        *self.codec.read().expect("кодек звука отравлен")
    }

    /// Дорожки текущего конвейера. Клонируются только `Arc`, не кадры.
    pub fn audio_tracks(&self) -> Vec<SharedAudio> {
        self.audio.read().expect("дорожки звука отравлены").clone()
    }

    /// Готовит буферы дорожек под новый конвейер и возвращает их.
    ///
    /// Вызывает поток захвата перед каждой сборкой: режим звука мог
    /// смениться, а источник — отвалиться. Старые буферы выбрасываются
    /// вместе с кадрами, как и в `reset_buffers`: метки времени у нового
    /// конвейера несовместимы со старыми.
    pub fn prepare_audio(
        &self,
        cfg: &Config,
        codec: Option<AudioCodec>,
        unavailable: &[AudioSource],
    ) -> Vec<SharedAudio> {
        let plan = match codec {
            Some(_) => cfg.audio.track_plan(unavailable),
            None => Vec::new(),
        };
        for source in AudioSource::ALL {
            let meter = self.meters.get(source);
            if plan.iter().any(|t| t.sources.contains(&source)) {
                meter.activate();
            } else if !unavailable.contains(&source) {
                // Отказ источника показываем, пока не попробуем открыть его снова.
                meter.deactivate();
                meter.clear_failure();
            }
        }
        // Лимиты — из текущего конфига под его блокировкой: «Длина буфера»,
        // применённая между чтением настроек потоком захвата и этой заменой,
        // иначе ушла бы старым дорожкам и потерялась.
        let live = self.cfg.read().expect("конфиг отравлен");
        let tracks = make_tracks(&live, &plan);
        *self.codec.write().expect("кодек звука отравлен") = codec;
        *self.audio.write().expect("дорожки звука отравлены") = tracks.clone();
        drop(live);
        tracks
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
            for a in self.audio.read().expect("дорожки звука отравлены").iter()
            {
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
        for a in self.audio.read().expect("дорожки звука отравлены").iter() {
            a.buffer.lock().expect("звуковой буфер отравлен").clear();
        }
    }

    /// Сколько звука накоплено в каждой дорожке.
    pub fn audio_stats(&self) -> Vec<(TrackKind, Stats)> {
        self.audio
            .read()
            .expect("дорожки звука отравлены")
            .iter()
            .map(|a| {
                (
                    a.kind,
                    a.buffer.lock().expect("звуковой буфер отравлен").stats(),
                )
            })
            .collect()
    }

    pub fn stats(&self) -> Stats {
        self.ring.lock().expect("кольцевой буфер отравлен").stats()
    }

    /// Идёт ли сейчас сохранение. Нужно при выходе: оборванный мьюксинг
    /// оставляет в папке клип без moov.
    pub fn is_saving(&self) -> bool {
        self.saving.load(Ordering::Acquire)
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
        let result = {
            let _guard = SavingGuard(&self.saving);
            let cfg = self.config();
            self.save_inner(&cfg, seconds.unwrap_or(cfg.save_seconds))
        };
        let record = SaveRecord {
            at: std::time::SystemTime::now(),
            result: match &result {
                Ok(SaveOutcome::Saved(p)) => Ok(p.clone()),
                Ok(SaveOutcome::BufferEmpty) => Err("буфер пуст — запись ещё не идёт".into()),
                Ok(SaveOutcome::AlreadySaving) => unreachable!("отсечено выше"),
                Err(e) => Err(format!("{e:#}")),
            },
        };
        *self
            .last_save
            .lock()
            .expect("последнее сохранение отравлено") = Some(record.clone());
        self.save_count.fetch_add(1, Ordering::AcqRel);
        // Подписчики — уведомления и т. п. — не должны задерживать ответ.
        for f in self.on_saved.lock().expect("подписчики отравлены").iter() {
            f(&record);
        }
        result
    }

    /// Итог последнего сохранения — для оверлея и трея.
    pub fn last_save(&self) -> Option<SaveRecord> {
        self.last_save
            .lock()
            .expect("последнее сохранение отравлено")
            .clone()
    }

    /// Сколько сохранений завершилось: по изменению видно новое.
    pub fn save_count(&self) -> u64 {
        self.save_count.load(Ordering::Acquire)
    }

    /// Вызывается после каждого завершённого сохранения, в потоке сохранения.
    pub fn on_saved(&self, f: impl Fn(&SaveRecord) + Send + Sync + 'static) {
        self.on_saved
            .lock()
            .expect("подписчики отравлены")
            .push(Box::new(f));
    }

    /// Даёт звуку догнать видео перед срезом.
    ///
    /// Звуковая ветка отстаёт от видео на задержку источника, микшера и
    /// кодировщика (на живой машине — около 0,1 с). Срез в момент нажатия
    /// оставлял конец клипа без последней доли секунды звука. Ждём, пока
    /// каждая дорожка дойдёт до последнего видеокадра, но недолго: молчащая
    /// дорожка не должна задерживать сохранение.
    fn wait_for_audio(&self, tracks: &[SharedAudio], video_end: i64) {
        let deadline = std::time::Instant::now() + AUDIO_CATCH_UP;
        loop {
            let behind = tracks.iter().any(|a| {
                a.buffer
                    .lock()
                    .expect("звуковой буфер отравлен")
                    .last_pts()
                    .is_none_or(|last| last < video_end)
            });
            if !behind || std::time::Instant::now() >= deadline {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
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

        // Звук режем по границам видеоклипа: часы у всех дорожек общие.
        // Каждую дорожку — из её собственного буфера.
        // Список дорожек — один раз вместе со срезом видео: пересборка во
        // время ожидания подменила бы их дорожками нового конвейера с
        // другой шкалой времени.
        let audio_tracks = self.audio_tracks();
        self.wait_for_audio(&audio_tracks, frames[frames.len() - 1].pts);
        let snapshots: Vec<(TrackKind, Vec<crate::ringbuf::Frame>, Option<String>)> = audio_tracks
            .iter()
            .map(|a| {
                let f = a
                    .buffer
                    .lock()
                    .expect("звуковой буфер отравлен")
                    .snapshot_range(frames[0].pts, frames[frames.len() - 1].pts);
                let caps = a.caps.lock().expect("caps звука отравлены").clone();
                (a.kind, f, caps)
            })
            .collect();
        let mut tracks = Vec::with_capacity(snapshots.len());
        for (kind, f, caps) in &snapshots {
            match caps {
                Some(c) if !f.is_empty() => tracks.push(saver::AudioTrack { frames: f, caps: c }),
                _ => log::warn!("в дорожке «{}» нет кадров — сохраняю без неё", kind.label()),
            }
        }
        if tracks.is_empty() && cfg.audio.any_enabled() {
            log::warn!("звук включён, но в буферах нет кадров — сохраняю без звука");
        }

        saver::write_clip_as(self.video_codec(), &frames, cfg.fps, &tracks, &out)?;

        let dur = (frames[frames.len() - 1].pts - frames[0].pts) as f64 / crate::ringbuf::NS as f64;
        let mb = std::fs::metadata(&out).map(|m| m.len()).unwrap_or(0) as f64 / 1e6;
        log::info!(
            "сохранено: {} (~{dur:.1} с, {mb:.1} МБ, звуковых дорожек: {})",
            out.display(),
            tracks.len()
        );
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
        // Итог виден оверлею и трею, подсчёт — по каждому сохранению.
        assert_eq!(rec.save_count(), 2);
        assert!(rec.last_save().unwrap().result.is_err());
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
        assert!(
            !rec.update_config(cfg),
            "длина буфера и папка меняются на лету"
        );
    }

    #[test]
    fn encoder_settings_require_restart() {
        let rec = Recorder::new(Config::default(), None);
        let mut cfg = rec.config();
        cfg.fps = 30;
        assert!(
            rec.update_config(cfg),
            "смена fps требует пересборки конвейера"
        );
    }

    #[test]
    fn shrinking_buffer_applies_to_ring_immediately() {
        let rec = Recorder::new(
            Config {
                seconds: 60.0,
                ..Config::default()
            },
            None,
        );
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
