//! Фоновый поток захвата.
//!
//! egui занимает главный поток, поэтому конвейер и glib MainLoop живут здесь.
//! Сохранение клипа потокобезопасно и вызывается напрямую из GUI или из
//! обработчика хоткея — сюда ходят только за перезапуском и остановкой.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::mpsc::{self, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use gstreamer as gst;
use gstreamer::prelude::*;

use crate::audio::{self, AudioSource};
use crate::encoders::{self, Discovery, EncoderSpec, VideoCodec};
use crate::pipeline::{self, AudioSources};
#[cfg(target_os = "linux")]
use crate::portal;
use crate::recorder::Recorder;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// videotestsrc — без экрана и портала.
    Test,
    /// Реальный захват через xdg-desktop-portal.
    #[cfg(target_os = "linux")]
    Portal,
    #[cfg(windows)]
    Desktop,
}

#[derive(Debug, Clone, Copy)]
enum Command {
    /// Пересобрать конвейер под новые настройки.
    Restart,
    Stop,
}

/// Что показывать в статусе GUI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaptureState {
    /// Ждём выбора экрана в диалоге портала.
    WaitingForPortal,
    Running,
    Failed(String),
    Stopped,
}

pub struct CaptureService {
    tx: mpsc::Sender<Command>,
    state: Arc<Mutex<CaptureState>>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl CaptureService {
    pub fn start(rec: Arc<Recorder>, mode: Mode) -> Self {
        Self::start_with_audio(rec, mode, AudioSources::default())
    }

    /// То же, но со своими источниками звука — для тестов без звукового
    /// сервера и для имитации отказа микрофона.
    pub fn start_with_audio(rec: Arc<Recorder>, mode: Mode, sources: AudioSources) -> Self {
        let (tx, rx) = mpsc::channel();
        let state = Arc::new(Mutex::new(CaptureState::WaitingForPortal));
        let st = state.clone();
        let handle = std::thread::Builder::new()
            .name("capture".into())
            .spawn(move || capture_thread(rec, mode, sources, rx, st))
            .expect("не удалось запустить поток захвата");
        Self {
            tx,
            state,
            handle: Some(handle),
        }
    }

    pub fn state(&self) -> CaptureState {
        self.state
            .lock()
            .expect("состояние захвата отравлено")
            .clone()
    }

    /// Просит поток пересобрать конвейер (после смены fps/битрейта/кодировщика).
    pub fn restart(&self) {
        let _ = self.tx.send(Command::Restart);
    }

    /// Просит поток захвата остановиться, не дожидаясь его.
    ///
    /// Нужно из обработчика сигналов: там нельзя ни блокироваться, ни брать
    /// `&mut`, а рвать процесс на полуслове нельзя — VA-кодировщик ругается
    /// «Failed to encode the frame», если его убить посреди кадра.
    pub fn request_stop(&self) {
        let _ = self.tx.send(Command::Stop);
    }

    pub fn stop(&mut self) {
        let _ = self.tx.send(Command::Stop);
        if let Some(h) = self.handle.take() {
            if h.join().is_err() {
                log::error!("поток захвата завершился паникой");
            }
        }
    }
}

#[cfg(test)]
impl CaptureService {
    /// Заглушка для тестов раскладки окна: без потока и без захвата.
    pub(crate) fn inert() -> Self {
        let (tx, _rx) = mpsc::channel();
        Self {
            tx,
            state: Arc::new(Mutex::new(CaptureState::Running)),
            handle: None,
        }
    }
}

impl Drop for CaptureService {
    fn drop(&mut self) {
        self.stop();
    }
}

fn capture_thread(
    rec: Arc<Recorder>,
    mode: Mode,
    sources: AudioSources,
    rx: mpsc::Receiver<Command>,
    state: Arc<Mutex<CaptureState>>,
) {
    let set = |s: CaptureState| *state.lock().expect("состояние захвата отравлено") = s;

    // Сессию портала открываем один раз: при пересборке конвейера повторный
    // диалог не нужен, PipeWire-поток продолжает жить.
    #[cfg(target_os = "linux")]
    let screencast = match mode {
        Mode::Test => None,
        Mode::Portal => match open_screencast() {
            Ok(s) => Some(s),
            Err(e) => {
                log::error!("портал недоступен: {e:#}");
                set(CaptureState::Failed(format!("{e:#}")));
                return;
            }
        },
    };

    let rx = Rc::new(rx);
    // Источники звука, которые не открылись при сборке конвейера. Запись
    // идёт без них до следующего «Применить»: тогда пробуем снова.
    let mut unavailable: Vec<AudioSource> = Vec::new();
    // Кодировщики, прошедшие проверку, но упавшие в настоящем конвейере, —
    // с причиной. Тоже до следующего «Применить».
    let mut rejected: Vec<(String, String)> = Vec::new();
    let mut background_probe: Option<std::thread::JoinHandle<()>> = None;
    loop {
        // Каждый новый конвейер отсчитывает running time заново, поэтому
        // накопленное до пересборки нужно выбросить: смешивать метки двух
        // конвейеров нельзя.
        rec.reset_buffers();
        let cfg = rec.config();
        #[cfg(target_os = "linux")]
        let source = match &screencast {
            Some(s) => s.source_desc(),
            None => pipeline::test_source(&cfg),
        };

        #[cfg(windows)]
        let source = match mode {
            Mode::Test => pipeline::test_source(&cfg),
            Mode::Desktop => crate::platform::windows_source(cfg.monitor),
        };
        let found = ensure_discovery(&rec, &cfg);
        if background_probe.is_none() {
            background_probe = spawn_background_discovery(&rec);
        }
        let selection = encoders::select(&cfg, &found, &rejected);
        let attempt = selection.order.first().cloned().ok_or_else(|| {
            anyhow::anyhow!(
                "нет работающего кодировщика {}{}. Подробности: replay-rs --list-encoders",
                cfg.video_codec.label(),
                selection
                    .note
                    .as_deref()
                    .map(|n| format!(" ({n})"))
                    .unwrap_or_default()
            )
        });
        let result = attempt.and_then(|encoder| {
            if let Some(note) = &selection.note {
                log::warn!("{note}");
            }
            log::info!("видео: {}", encoder.label());
            rec.set_active_encoder(Some(encoder.clone()), selection.note.clone());
            run_once(&Attempt {
                rec: &rec,
                cfg: &cfg,
                source: &source,
                audio_sources: &sources,
                rx: &rx,
                state: &state,
                unavailable: &unavailable,
                encoder: &encoder,
            })
        });
        let next = match result {
            Ok(cmd) => cmd,
            // Кодировщик не открылся или упал — пробуем следующего кандидата.
            Err(e) if e.downcast_ref::<EncoderFailed>().is_some() => {
                let f = e.downcast_ref::<EncoderFailed>().expect("проверено выше");
                log::warn!("{f} — пробую следующий кодировщик");
                rejected.push((f.element.clone(), f.reason.clone()));
                continue;
            }
            // Не открылся источник звука — это не повод терять запись экрана.
            Err(e) => match e.downcast_ref::<SourceFailed>() {
                Some(f) if !unavailable.contains(&f.source) => {
                    log::warn!(
                        "{} недоступен ({}), продолжаю запись без него",
                        f.source.label(),
                        f.reason
                    );
                    rec.meters.get(f.source).fail(f.reason.clone());
                    unavailable.push(f.source);
                    continue;
                }
                _ => {
                    log::error!("конвейер захвата остановлен: {e:#}");
                    set(CaptureState::Failed(format!("{e:#}")));
                    rx.recv().ok()
                }
            },
        };
        match next.map(|cmd| coalesce(cmd, &rx)) {
            Some(Command::Restart) => {
                log::info!("пересобираю конвейер под новые настройки");
                unavailable.clear();
                rejected.clear();
                continue;
            }
            Some(Command::Stop) | None => break,
        }
    }
    if let Some(h) = background_probe {
        if h.join().is_err() {
            log::error!("фоновая проверка кодировщиков завершилась паникой");
        }
    }
    rec.set_active_encoder(None, None);
    set(CaptureState::Stopped);
}

/// Обнаруживает кодировщики нужного кодека, если это ещё не сделано.
///
/// Поток захвата проверяет только кодек текущих настроек — запись не
/// должна ждать проверки всего подряд. Остальные кодеки окна настроек
/// проверяются фоновым потоком (`spawn_background_discovery`). AV1 — только
/// когда его просят: программный svtav1enc на проверке занимает ~0,4–0,8 с
/// и сотни мегабайт.
fn ensure_discovery(rec: &Arc<Recorder>, cfg: &crate::config::Config) -> Arc<Discovery> {
    let codec = encoders::codec_for(cfg);
    if let Some(found) = rec.encoder_status().discovery {
        if found.probed.iter().any(|p| p.spec.codec == codec) {
            return found;
        }
    }
    let fresh = encoders::discover(&[codec]);
    log::info!(
        "проверка кодировщиков {}: {} мс, работают: {}",
        codec.label(),
        fresh.millis,
        fresh
            .probed
            .iter()
            .filter(|p| p.outcome.works())
            .map(|p| p.spec.element.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    );
    rec.merge_discovery(fresh)
}

/// Проверяет в фоне кодеки окна настроек, которых ещё нет в результатах.
///
/// Поток захвата дожидается его перед выходом: процесс, завершившийся
/// посреди пробного кодирования, падал с SIGSEGV внутри драйвера VA.
fn spawn_background_discovery(rec: &Arc<Recorder>) -> Option<std::thread::JoinHandle<()>> {
    let rec = rec.clone();
    let spawned = std::thread::Builder::new()
        .name("encoder-probe".into())
        .spawn(move || {
            let have: Vec<VideoCodec> = rec
                .encoder_status()
                .discovery
                .map(|d| d.probed.iter().map(|p| p.spec.codec).collect())
                .unwrap_or_default();
            let missing: Vec<VideoCodec> = VideoCodec::ALL
                .into_iter()
                .filter(|c| c.in_ui() && !have.contains(c))
                .collect();
            if !missing.is_empty() {
                rec.merge_discovery(encoders::discover(&missing));
            }
        });
    spawned
        .inspect_err(|e| log::warn!("не удалось запустить фоновую проверку кодировщиков: {e}"))
        .ok()
}

/// Сворачивает команды, накопившиеся, пока конвейер останавливался.
///
/// Несколько «Применить» подряд дают одну пересборку: настройки всё равно
/// читаются заново. Stop среди них главнее — выход не ждёт лишних пересборок.
fn coalesce(first: Command, rx: &mpsc::Receiver<Command>) -> Command {
    if let Command::Stop = first {
        return Command::Stop;
    }
    for next in rx.try_iter() {
        if let Command::Stop = next {
            return Command::Stop;
        }
    }
    Command::Restart
}

/// Кодировщик не открылся или упал в настоящем конвейере.
#[derive(Debug)]
struct EncoderFailed {
    element: String,
    reason: String,
}

impl std::fmt::Display for EncoderFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "кодировщик {} не работает: {}",
            self.element, self.reason
        )
    }
}

impl std::error::Error for EncoderFailed {}

/// Ошибка с шины, если её прислал кодировщик или цепочка перед ним.
///
/// Только эти элементы: not-negotiated от источника (скажем, нулевая
/// ширина кадра) — не повод перебирать кодировщики.
fn encoder_error(msg: &gst::MessageRef, element: &str) -> Option<EncoderFailed> {
    let gst::MessageView::Error(e) = msg.view() else {
        return None;
    };
    let mut cur = msg.src().cloned();
    while let Some(o) = cur {
        if matches!(o.name().as_str(), "venc" | "vup") {
            return Some(EncoderFailed {
                element: element.to_owned(),
                reason: e.error().to_string(),
            });
        }
        cur = o.parent();
    }
    None
}

/// Всё, что нужно одной сборке конвейера.
struct Attempt<'a> {
    rec: &'a Arc<Recorder>,
    cfg: &'a crate::config::Config,
    source: &'a str,
    audio_sources: &'a AudioSources,
    rx: &'a Rc<mpsc::Receiver<Command>>,
    state: &'a Arc<Mutex<CaptureState>>,
    unavailable: &'a [AudioSource],
    encoder: &'a EncoderSpec,
}

/// Источник звука не открылся при запуске конвейера.
///
/// Отдельный тип, чтобы поток захвата отличал его от прочих ошибок и
/// пересобирал конвейер без этого источника, а не останавливал запись.
#[derive(Debug)]
struct SourceFailed {
    source: AudioSource,
    reason: String,
}

impl std::fmt::Display for SourceFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} не открылся: {}", self.source.label(), self.reason)
    }
}

impl std::error::Error for SourceFailed {}

/// Ошибка с шины, если её прислал элемент ветки источника звука.
fn source_error(msg: &gst::MessageRef) -> Option<SourceFailed> {
    let gst::MessageView::Error(e) = msg.view() else {
        return None;
    };
    let source = AudioSource::owning(msg.src()?)?;
    Some(SourceFailed {
        source,
        reason: e.error().to_string(),
    })
}

/// Кодек звука под текущие настройки. Перевыбирается при каждой сборке:
/// пользователь мог включить звук или сменить предпочтение без перезапуска.
fn current_codec(cfg: &crate::config::Config) -> Option<audio::AudioCodec> {
    if !cfg.audio.any_enabled() {
        return None;
    }
    let choice = audio::choose_codec(cfg.audio.codec);
    match choice {
        audio::CodecChoice::Exact(c) => log::info!("звук: {}", c.as_str()),
        audio::CodecChoice::FellBack(c) => log::warn!(
            "запрошен AAC, но avenc_aac недоступен (нужен плагин libav) — пишу в {}",
            c.as_str()
        ),
        audio::CodecChoice::None => log::warn!(
            "звук включён, но нет ни avenc_aac (нужен плагин libav), ни opusenc — пишу без звука"
        ),
    }
    choice.codec()
}

/// Строит и крутит конвейер до команды или ошибки.
/// Возвращает команду, которая его остановила.
fn run_once(a: &Attempt<'_>) -> Result<Option<Command>> {
    let Attempt {
        rec,
        cfg,
        source,
        audio_sources,
        rx,
        state,
        unavailable,
        encoder,
    } = *a;
    // Ветку звука строим только если есть и настройка, и кодировщик.
    let codec = current_codec(cfg);
    let tracks = rec.prepare_audio(cfg, codec, unavailable);
    let plan = cfg.audio.track_plan(unavailable);
    let audio_desc =
        codec.and_then(|codec| pipeline::audio_description(&plan, cfg, codec, audio_sources));
    let gst_pipeline =
        match pipeline::build_pipeline_for(cfg, encoder, source, audio_desc.as_deref()) {
            Ok(p) => p,
            // Проверка прошла, а здесь свойство не принялось — значит, дело
            // в настройках этого кодировщика, пробуем следующий.
            Err(e) if format!("{e:#}").contains("venc") => {
                return Err(EncoderFailed {
                    element: encoder.element.clone(),
                    reason: format!("{e:#}"),
                }
                .into())
            }
            Err(e) => return Err(e),
        };
    pipeline::attach_sink(&gst_pipeline, rec.ring.clone())?;
    if audio_desc.is_some() {
        pipeline::attach_audio_sinks(&gst_pipeline, &tracks)?;
    }

    let main_loop = gst::glib::MainLoop::new(None, false);
    let pending: Rc<RefCell<Option<Command>>> = Rc::new(RefCell::new(None));
    let failure: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
    let encoder_failure: Rc<RefCell<Option<EncoderFailed>>> = Rc::new(RefCell::new(None));
    // Источник звука, упавший до перехода конвейера в PLAYING.
    let early_source: Rc<RefCell<Option<SourceFailed>>> = Rc::new(RefCell::new(None));
    let playing = Rc::new(std::cell::Cell::new(false));

    let bus = gst_pipeline.bus().context("у конвейера нет шины")?;
    let _guard = {
        let l = main_loop.clone();
        let failure = failure.clone();
        let encoder_failure = encoder_failure.clone();
        let element = encoder.element.clone();
        let rec = rec.clone();
        let early_source = early_source.clone();
        let playing = playing.clone();
        let pipe = gst_pipeline.downgrade();
        bus.add_watch_local(move |_, msg| {
            if let gst::MessageView::StateChanged(sc) = msg.view() {
                let from_pipeline = pipe
                    .upgrade()
                    .is_some_and(|p| msg.src() == Some(p.upcast_ref::<gst::Object>()));
                if from_pipeline && sc.current() == gst::State::Playing {
                    playing.set(true);
                }
            }
            if let Some(f) = encoder_error(msg, &element) {
                *encoder_failure.borrow_mut() = Some(f);
                l.quit();
                return gst::glib::ControlFlow::Continue;
            }
            if let Some((source, peak)) = pipeline::level_peak(msg) {
                rec.meters.get(source).record_peak(peak);
                return gst::glib::ControlFlow::Continue;
            }
            // Отвалившийся посреди записи источник звука не роняет захват:
            // basesrc после ошибки шлёт EOS, микшер дорожки его отпускает и
            // продолжает на тишине. Видео и остальные дорожки пишутся дальше.
            if let Some(f) = source_error(msg) {
                // До PLAYING ошибка источника отменяет переход состояния:
                // конвейер навсегда остаётся в PAUSED, и ничего не пишется.
                // Тогда — пересборка без источника, как при отказе на старте.
                if !playing.get() {
                    early_source.borrow_mut().get_or_insert(f);
                    l.quit();
                    return gst::glib::ControlFlow::Continue;
                }
                log::warn!("{f} — дорожка продолжится тишиной");
                rec.meters.get(f.source).fail(f.reason);
                return gst::glib::ControlFlow::Continue;
            }
            match msg.view() {
                gst::MessageView::Error(e) => {
                    *failure.borrow_mut() = Some(format!("{} ({:?})", e.error(), e.debug()));
                    l.quit();
                }
                gst::MessageView::Eos(_) => {
                    *failure.borrow_mut() = Some("поток неожиданно закончился".into());
                    l.quit();
                }
                _ => {}
            }
            gst::glib::ControlFlow::Continue
        })
        .context("не удалось подписаться на шину")?
    };

    let poll = {
        let l = main_loop.clone();
        let rx = rx.clone();
        let pending = pending.clone();
        gst::glib::timeout_add_local(Duration::from_millis(100), move || {
            if pending.borrow().is_none() {
                match rx.try_recv() {
                    Err(TryRecvError::Empty) => {}
                    Ok(cmd) => {
                        *pending.borrow_mut() = Some(cmd);
                        l.quit();
                    }
                    // GUI уронили — останавливаемся.
                    Err(TryRecvError::Disconnected) => {
                        *pending.borrow_mut() = Some(Command::Stop);
                        l.quit();
                    }
                }
            }
            gst::glib::ControlFlow::Continue
        })
    };

    let started = gst_pipeline.set_state(gst::State::Playing);
    if started.is_ok() {
        *state.lock().expect("состояние захвата отравлено") = CaptureState::Running;
        main_loop.run();
    }
    poll.remove();
    // Причину неудачного запуска читаем до NULL: переход в NULL сбрасывает
    // шину вместе с сообщениями об ошибках.
    let mut failed_source = early_source.borrow_mut().take();
    if started.is_err() {
        for msg in std::iter::from_fn(|| bus.pop_filtered(&[gst::MessageType::Error])) {
            if let Some(f) = encoder_error(&msg, &encoder.element) {
                encoder_failure.borrow_mut().get_or_insert(f);
            } else if let Some(f) = source_error(&msg) {
                failed_source.get_or_insert(f);
            }
        }
    }
    // NULL обязателен и после неудачного запуска: частично поднятый
    // конвейер держит VA-контекст и поток PipeWire.
    if let Err(e) = gst_pipeline.set_state(gst::State::Null) {
        log::warn!("конвейер захвата не перешёл в NULL: {e}");
    }
    let cmd_taken = pending.borrow().is_some();
    if let Some(f) = encoder_failure.borrow_mut().take() {
        // Команду из канала терять нельзя: при Stop пересборка не нужна.
        if !cmd_taken {
            return Err(f.into());
        }
    }
    if let Some(f) = failed_source.filter(|_| !cmd_taken) {
        // Запуск сорвал источник звука — пусть поток пересоберёт без него.
        return Err(f.into());
    }
    started.context("конвейер захвата не запустился")?;

    let cmd = pending.borrow_mut().take();
    if let Some(err) = failure.borrow_mut().take() {
        // Команду, которую уже забрали из канала, терять нельзя: иначе
        // Stop, пришедший вместе с ошибкой, никто бы больше не прислал.
        if cmd.is_none() {
            anyhow::bail!("{err}");
        }
        log::warn!("конвейер захвата упал при остановке: {err}");
    }
    Ok(cmd)
}

/// Открывает сеанс захвата, переиспользуя сохранённый restore_token.
///
/// С валидным токеном KDE не показывает диалог выбора экрана. Если токен
/// протух, забываем его и пробуем ещё раз — уже с диалогом.
#[cfg(target_os = "linux")]
fn open_screencast() -> Result<portal::ScreenCastSession> {
    let token = portal::load_token();
    if token.is_some() {
        log::info!("восстанавливаю сеанс по сохранённому токену");
    }
    let session = match portal::start_blocking(token.as_deref()) {
        Ok(s) => s,
        Err(e) if token.is_some() => {
            log::warn!("сохранённый токен не подошёл ({e:#}), спрашиваю заново");
            portal::forget_token();
            portal::start_blocking(None)?
        }
        Err(e) => return Err(e),
    };
    if let Some(t) = &session.restore_token {
        if let Err(e) = portal::save_token(t) {
            log::warn!("не удалось сохранить токен: {e:#}");
        }
    }
    log::info!("портал: node_id={}", session.node_id());
    Ok(session)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queued_restarts_collapse_into_one() {
        let (tx, rx) = mpsc::channel();
        for _ in 0..5 {
            tx.send(Command::Restart).unwrap();
        }
        assert!(matches!(coalesce(Command::Restart, &rx), Command::Restart));
        assert!(
            rx.try_recv().is_err(),
            "лишние пересборки остались в очереди"
        );
    }

    #[test]
    fn stop_wins_over_queued_restarts() {
        let (tx, rx) = mpsc::channel();
        tx.send(Command::Restart).unwrap();
        tx.send(Command::Stop).unwrap();
        tx.send(Command::Restart).unwrap();
        assert!(matches!(coalesce(Command::Restart, &rx), Command::Stop));
        let (_tx, rx) = mpsc::channel();
        assert!(matches!(coalesce(Command::Stop, &rx), Command::Stop));
    }
}
