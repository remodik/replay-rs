//! Глобальный хоткей через xdg-desktop-portal GlobalShortcuts.
//!
//! На Wayland приложение не может перехватывать клавиши само — их раздаёт
//! композитор. Портал задаёт разделение ролей: приложение объявляет действие
//! и предлагает комбинацию, а назначает её пользователь. Поэтому «настроить
//! хоткей в приложении» означает показать текущую привязку и открыть
//! системный редактор, наведённый на наши действия (`ConfigureShortcuts`),
//! а не записать комбинацию самим.
//!
//! Запасной путь, если портала нет: `replay-rs --save` шлёт SIGUSR1.

use std::sync::mpsc::{self, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use ashpd::desktop::global_shortcuts::{GlobalShortcuts, NewShortcut};
use futures_util::future::{select, Either};
use futures_util::StreamExt;

use crate::recorder::Recorder;

/// Идентификатор действия. KDE хранит привязку по нему.
pub const SHORTCUT_ID: &str = "save-replay";
/// Что предлагаем композитору. Назначает всё равно пользователь.
pub const PREFERRED_TRIGGER: &str = "CTRL+ALT+S";
/// Как часто заглядываем в очередь команд от GUI.
const POLL: Duration = Duration::from_millis(150);

/// Что показывать в окне.
#[derive(Debug, Clone, Default)]
pub struct ShortcutState {
    /// Привязка словами композитора. `None` — действие есть, но клавиши нет.
    pub trigger: Option<String>,
    /// Портал недоступен: остаётся только `replay-rs --save`.
    pub error: Option<String>,
    /// Портал ответил и действие зарегистрировано.
    pub registered: bool,
}

impl ShortcutState {
    /// Текст для окна.
    pub fn summary(&self) -> String {
        match (&self.error, &self.trigger, self.registered) {
            (Some(e), _, _) => format!("портал недоступен: {e}"),
            (None, Some(t), _) => t.clone(),
            (None, None, true) => "не назначена".into(),
            (None, None, false) => "регистрируется…".into(),
        }
    }
}

enum Cmd {
    /// Открыть системный редактор комбинаций.
    Configure,
}

/// Ручка для GUI.
pub struct Shortcuts {
    tx: mpsc::Sender<Cmd>,
    state: Arc<Mutex<ShortcutState>>,
}

#[cfg(test)]
impl Shortcuts {
    /// Заглушка для тестов раскладки окна: не ходит в портал и не
    /// регистрирует хоткей в настройках KDE.
    pub(crate) fn inert(state: ShortcutState) -> Self {
        let (tx, _rx) = mpsc::channel();
        Self { tx, state: Arc::new(Mutex::new(state)) }
    }
}

impl Shortcuts {
    pub fn spawn(rec: Arc<Recorder>) -> Self {
        let (tx, rx) = mpsc::channel();
        let state = Arc::new(Mutex::new(ShortcutState::default()));
        let st = state.clone();
        std::thread::Builder::new()
            .name("shortcuts".into())
            .spawn(move || {
                if let Err(e) = async_io::block_on(listen(rec, rx, st.clone())) {
                    log::warn!("глобальный хоткей недоступен ({e:#}); остаётся replay-rs --save");
                    st.lock().expect("состояние хоткея отравлено").error = Some(format!("{e:#}"));
                }
            })
            .expect("не удалось запустить поток хоткеев");
        Self { tx, state }
    }

    pub fn state(&self) -> ShortcutState {
        self.state.lock().expect("состояние хоткея отравлено").clone()
    }

    /// Просит портал открыть системный редактор комбинаций для наших действий.
    pub fn open_settings(&self) {
        let _ = self.tx.send(Cmd::Configure);
    }
}

/// События, которые нас будят.
enum Event {
    Activated,
    Changed(Vec<(String, String)>),
}

async fn listen(
    rec: Arc<Recorder>,
    rx: mpsc::Receiver<Cmd>,
    state: Arc<Mutex<ShortcutState>>,
) -> Result<()> {
    let proxy = GlobalShortcuts::new()
        .await
        .context("портал GlobalShortcuts недоступен")?;
    let session = proxy
        .create_session(Default::default())
        .await
        .context("не удалось создать сеанс хоткеев")?;

    // Подписываемся до BindShortcuts, иначе можно проспать первое нажатие.
    let activated = proxy
        .receive_activated()
        .await
        .context("не удалось подписаться на нажатия")?;
    let changed = proxy
        .receive_shortcuts_changed()
        .await
        .context("не удалось подписаться на смену привязок")?;

    let bound = proxy
        .bind_shortcuts(
            &session,
            &[NewShortcut::new(SHORTCUT_ID, "Сохранить реплей")
                .preferred_trigger(PREFERRED_TRIGGER)],
            None,
            Default::default(),
        )
        .await
        .context("не удалось зарегистрировать хоткей")?
        .response()
        .context("привязка хоткея отклонена")?;

    // Первичное состояние берём из ответа BindShortcuts.
    {
        let mut s = state.lock().expect("состояние хоткея отравлено");
        s.registered = true;
        s.trigger = bound
            .shortcuts()
            .iter()
            .find(|s| s.id() == SHORTCUT_ID)
            .map(|s| s.trigger_description().to_string())
            .filter(|t| !t.is_empty());
        log::info!("хоткей зарегистрирован, привязка: {}", s.summary());
    }

    let mut events = futures_util::stream::select(
        activated.map(|_| Event::Activated),
        changed.map(|c| {
            Event::Changed(
                c.shortcuts()
                    .iter()
                    .map(|s| (s.id().to_string(), s.trigger_description().to_string()))
                    .collect(),
            )
        }),
    );

    loop {
        // Команды от GUI приходят по обычному каналу, поэтому будим себя
        // таймером и смотрим очередь между событиями портала.
        let next = events.next();
        let tick = async_io::Timer::after(POLL);
        futures_util::pin_mut!(next, tick);

        match select(next, tick).await {
            Either::Left((Some(Event::Activated), _)) => {
                log::info!("хоткей нажат");
                let rec = rec.clone();
                // Сохранение не должно задерживать приём следующих нажатий.
                std::thread::spawn(move || {
                    if let Err(e) = rec.save(None) {
                        log::error!("сохранение по хоткею не удалось: {e:#}");
                    }
                });
            }
            Either::Left((Some(Event::Changed(list)), _)) => {
                let mut s = state.lock().expect("состояние хоткея отравлено");
                s.trigger = list
                    .iter()
                    .find(|(id, _)| id == SHORTCUT_ID)
                    .map(|(_, t)| t.clone())
                    .filter(|t| !t.is_empty());
                log::info!("привязка изменилась: {}", s.summary());
            }
            // Портал закрыл сессию — дальше слушать нечего.
            Either::Left((None, _)) => {
                anyhow::bail!("сеанс хоткеев закрыт порталом");
            }
            Either::Right((_instant, _)) => match rx.try_recv() {
                Ok(Cmd::Configure) => {
                    if let Err(e) = proxy
                        .configure_shortcuts(&session, None, Default::default())
                        .await
                    {
                        log::warn!("не удалось открыть редактор комбинаций: {e}");
                    }
                }
                Err(TryRecvError::Empty) => {}
                // GUI уронили.
                Err(TryRecvError::Disconnected) => return Ok(()),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_distinguishes_unbound_from_unavailable() {
        // «Не назначена» и «портала нет» — разные беды с разным лечением.
        let unbound = ShortcutState { registered: true, ..Default::default() };
        assert_eq!(unbound.summary(), "не назначена");

        let pending = ShortcutState::default();
        assert_eq!(pending.summary(), "регистрируется…");

        let broken = ShortcutState { error: Some("нет портала".into()), ..Default::default() };
        assert!(broken.summary().contains("нет портала"));

        let ok = ShortcutState {
            registered: true,
            trigger: Some("Ctrl+Alt+S".into()),
            ..Default::default()
        };
        assert_eq!(ok.summary(), "Ctrl+Alt+S");
    }

    #[test]
    fn empty_trigger_is_treated_as_unbound() {
        // Композитор отдаёт пустую строку, когда клавиша не назначена;
        // показывать пустоту как привязку нельзя.
        let s = ShortcutState {
            registered: true,
            trigger: Some(String::new()).filter(|t| !t.is_empty()),
            ..Default::default()
        };
        assert_eq!(s.summary(), "не назначена");
    }
}
