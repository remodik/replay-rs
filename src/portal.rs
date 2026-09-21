//! Захват экрана через xdg-desktop-portal ScreenCast (Wayland).
//!
//! Порт `portal_start_screencast` из `reference/replay.py` на ashpd плюс то,
//! чего в прототипе не было: `persist_mode` + `restore_token`, чтобы диалог
//! выбора экрана появлялся только один раз.

use std::os::fd::{AsRawFd, OwnedFd};
use std::path::PathBuf;

use anyhow::{Context, Result};
use ashpd::desktop::screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType};
use ashpd::desktop::{PersistMode, Session};

/// Живой сеанс захвата.
///
/// Держит и fd, и сессию: если уронить любое из них, PipeWire-поток закроется.
pub struct ScreenCastSession {
    _proxy: Screencast,
    _session: Session<Screencast>,
    fd: OwnedFd,
    node_id: u32,
    /// Токен для следующего запуска — портал выдаёт новый на каждый сеанс.
    pub restore_token: Option<String>,
}

impl ScreenCastSession {
    /// Описание источника для `gst::parse::launch`.
    pub fn source_desc(&self) -> String {
        format!(
            "pipewiresrc fd={} path={} do-timestamp=true keepalive-time=1000",
            self.fd.as_raw_fd(),
            self.node_id
        )
    }

    pub fn node_id(&self) -> u32 {
        self.node_id
    }
}

/// Открывает сеанс ScreenCast. С валидным `restore_token` KDE не показывает
/// диалог выбора экрана.
pub async fn start(restore_token: Option<&str>) -> Result<ScreenCastSession> {
    log::debug!("портал: подключаюсь к ScreenCast");
    let proxy = Screencast::new()
        .await
        .context("портал ScreenCast недоступен")?;

    log::debug!("портал: CreateSession");
    let session = proxy
        .create_session(Default::default())
        .await
        .context("не удалось создать сеанс портала")?;

    log::debug!("портал: SelectSources");
    proxy
        .select_sources(
            &session,
            SelectSourcesOptions::default()
                .set_sources(Some(SourceType::Monitor.into()))
                .set_multiple(false)
                // Курсор рисуется внутри кадра — как в прототипе (cursor_mode=2).
                .set_cursor_mode(CursorMode::Embedded)
                // Разрешение действует до явного отзыва пользователем.
                .set_persist_mode(PersistMode::ExplicitlyRevoked)
                .set_restore_token(restore_token),
        )
        .await
        .context("SelectSources не удался")?;

    log::debug!("портал: Start (здесь появляется диалог)");
    let streams = proxy
        .start(&session, None, Default::default())
        .await
        .context("Start не удался")?
        .response()
        .context("выбор экрана отменён пользователем")?;

    let node_id = streams
        .streams()
        .first()
        .context("портал не вернул ни одного потока")?
        .pipe_wire_node_id();
    let restore_token = streams.restore_token().map(str::to_owned);

    log::debug!("портал: OpenPipeWireRemote, node_id={node_id}");
    let fd = proxy
        .open_pipe_wire_remote(&session, Default::default())
        .await
        .context("OpenPipeWireRemote не удался")?;

    Ok(ScreenCastSession {
        _proxy: proxy,
        _session: session,
        fd,
        node_id,
        restore_token,
    })
}

/// Синхронная обёртка: рекордер живёт в glib MainLoop, своего рантайма у него нет.
pub fn start_blocking(restore_token: Option<&str>) -> Result<ScreenCastSession> {
    async_io::block_on(start(restore_token))
}

// ------------------------------------------------------------- токен на диске

fn token_path() -> PathBuf {
    let dir = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_default()
                .join(".config")
        });
    dir.join("replay-rs").join("restore-token")
}

pub fn load_token() -> Option<String> {
    let t = std::fs::read_to_string(token_path()).ok()?;
    let t = t.trim().to_string();
    (!t.is_empty()).then_some(t)
}

pub fn save_token(token: &str) -> Result<()> {
    let path = token_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("не удалось создать {}", dir.display()))?;
    }
    std::fs::write(&path, token)
        .with_context(|| format!("не удалось записать {}", path.display()))
}

/// Токен протух (экран отключили, разрешение отозвали) — забываем его,
/// следующий запуск снова покажет диалог.
pub fn forget_token() {
    let _ = std::fs::remove_file(token_path());
}
