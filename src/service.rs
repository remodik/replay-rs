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

use crate::pipeline;
use crate::portal;
use crate::recorder::Recorder;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// videotestsrc — без экрана и портала.
    Test,
    /// Реальный захват через xdg-desktop-portal.
    Portal,
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
        let (tx, rx) = mpsc::channel();
        let state = Arc::new(Mutex::new(CaptureState::WaitingForPortal));
        let st = state.clone();
        let handle = std::thread::Builder::new()
            .name("capture".into())
            .spawn(move || capture_thread(rec, mode, rx, st))
            .expect("не удалось запустить поток захвата");
        Self { tx, state, handle: Some(handle) }
    }

    pub fn state(&self) -> CaptureState {
        self.state.lock().expect("состояние захвата отравлено").clone()
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
            let _ = h.join();
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
    rx: mpsc::Receiver<Command>,
    state: Arc<Mutex<CaptureState>>,
) {
    let set = |s: CaptureState| *state.lock().expect("состояние захвата отравлено") = s;

    // Сессию портала открываем один раз: при пересборке конвейера повторный
    // диалог не нужен, PipeWire-поток продолжает жить.
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
    loop {
        // Каждый новый конвейер отсчитывает running time заново, поэтому
        // накопленное до пересборки нужно выбросить: смешивать метки двух
        // конвейеров нельзя.
        rec.reset_buffers();
        let cfg = rec.config();
        let source = match &screencast {
            Some(s) => s.source_desc(),
            None => pipeline::test_source(&cfg),
        };

        let next = match run_once(&rec, &cfg, &source, &rx) {
            Ok(cmd) => {
                set(CaptureState::Running);
                cmd
            }
            Err(e) => {
                log::error!("конвейер захвата остановлен: {e:#}");
                set(CaptureState::Failed(format!("{e:#}")));
                return;
            }
        };
        match next {
            Some(Command::Restart) => {
                log::info!("пересобираю конвейер под новые настройки");
                continue;
            }
            Some(Command::Stop) | None => break,
        }
    }
    set(CaptureState::Stopped);
}

/// Строит и крутит конвейер до команды или ошибки.
/// Возвращает команду, которая его остановила.
fn run_once(
    rec: &Arc<Recorder>,
    cfg: &crate::config::Config,
    source: &str,
    rx: &Rc<mpsc::Receiver<Command>>,
) -> Result<Option<Command>> {
    // Ветку звука строим только если есть и настройка, и кодировщик.
    let audio_desc = rec
        .codec
        .and_then(|codec| pipeline::audio_branch(cfg, codec));
    let gst_pipeline =
        pipeline::build_pipeline_with_audio(cfg, source, audio_desc.as_deref())?;
    pipeline::attach_sink(&gst_pipeline, rec.ring.clone())?;
    if audio_desc.is_some() {
        if let Some(a) = &rec.audio {
            pipeline::attach_audio_sink(&gst_pipeline, a.clone())?;
        }
    }

    let main_loop = gst::glib::MainLoop::new(None, false);
    let pending: Rc<RefCell<Option<Command>>> = Rc::new(RefCell::new(None));
    let failure: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));

    // Опрашиваем канал команд изнутри MainLoop.
    {
        let l = main_loop.clone();
        let rx = rx.clone();
        let pending = pending.clone();
        gst::glib::timeout_add_local(Duration::from_millis(100), move || {
            match rx.try_recv() {
                Err(TryRecvError::Empty) => gst::glib::ControlFlow::Continue,
                Ok(cmd) => {
                    *pending.borrow_mut() = Some(cmd);
                    l.quit();
                    gst::glib::ControlFlow::Break
                }
                // GUI уронили — останавливаемся.
                Err(TryRecvError::Disconnected) => {
                    *pending.borrow_mut() = Some(Command::Stop);
                    l.quit();
                    gst::glib::ControlFlow::Break
                }
            }
        });
    }

    let bus = gst_pipeline.bus().context("у конвейера нет шины")?;
    let _guard = {
        let l = main_loop.clone();
        let failure = failure.clone();
        bus.add_watch_local(move |_, msg| {
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

    gst_pipeline
        .set_state(gst::State::Playing)
        .context("конвейер захвата не запустился")?;
    main_loop.run();
    let _ = gst_pipeline.set_state(gst::State::Null);

    if let Some(err) = failure.borrow_mut().take() {
        anyhow::bail!("{err}");
    }
    let cmd = pending.borrow_mut().take();
    Ok(cmd)
}

/// Открывает сеанс захвата, переиспользуя сохранённый restore_token.
///
/// С валидным токеном KDE не показывает диалог выбора экрана. Если токен
/// протух, забываем его и пробуем ещё раз — уже с диалогом.
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
