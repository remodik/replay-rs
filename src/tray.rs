//! Иконка в трее через StatusNotifierItem.
//!
//! На KDE за SNI отвечает `org.kde.StatusNotifierWatcher` (kded6), поэтому
//! берём ksni напрямую, без GTK-прослойки, которую тянет tray-icon.

use std::sync::Arc;

use anyhow::{Context, Result};
use ksni::blocking::TrayMethods;
use ksni::menu::{MenuItem, StandardItem};

use crate::recorder::Recorder;

pub struct ReplayTray {
    rec: Arc<Recorder>,
}

impl ReplayTray {
    fn save(&self) {
        let rec = self.rec.clone();
        // Сохранение не должно блокировать поток трея.
        std::thread::spawn(move || {
            if let Err(e) = rec.save(None) {
                log::error!("сохранение из трея не удалось: {e:#}");
            }
        });
    }

    fn open_folder(&self) {
        crate::platform::open_or_warn(&self.rec.config().output);
    }
}

impl ksni::Tray for ReplayTray {
    fn id(&self) -> String {
        "replay-rs".into()
    }

    fn title(&self) -> String {
        "replay-rs".into()
    }

    fn icon_name(&self) -> String {
        // Иконка из темы: своей пока нет, эта есть в Breeze.
        "media-record".into()
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        let s = self.rec.stats();
        let cfg = self.rec.config();
        let mut lines = vec![format!(
            "буфер {:.0} / {:.0} с · {:.0} МБ",
            s.seconds(),
            cfg.seconds,
            s.bytes as f64 / 1e6
        )];
        if let Some(enc) = self.rec.encoder_status().active {
            lines.push(enc.label());
        }
        if cfg.audio.mic {
            lines.push(format!(
                "микрофон: {}",
                self.rec
                    .meters
                    .get(crate::audio::AudioSource::Mic)
                    .status()
                    .label()
            ));
        }
        if let Some(r) = self.rec.last_save() {
            let at = crate::overlay::format_saved_at(r.at);
            lines.push(match r.result {
                Ok(p) => format!(
                    "{at} сохранён {}",
                    p.file_name().unwrap_or_default().to_string_lossy()
                ),
                Err(e) => format!("{at} не сохранено: {e}"),
            });
        }
        ksni::ToolTip {
            title: "replay-rs".into(),
            description: lines.join("\n"),
            icon_name: self.icon_name(),
            icon_pixmap: Vec::new(),
        }
    }

    // activate (левый клик) намеренно не переопределяем: поднять окно с
    // Wayland нельзя (focus_window у winit — пустая заглушка), а открывать
    // вместо этого что-то своё неожиданно. Пусть KDE показывает меню.

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let save_seconds = self.rec.config().save_seconds;
        vec![
            StandardItem {
                label: format!("Сохранить последние {save_seconds:.0} с"),
                icon_name: "media-record".into(),
                activate: Box::new(|t: &mut Self| t.save()),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: "Папка с клипами".into(),
                icon_name: "folder-videos".into(),
                activate: Box::new(|t: &mut Self| t.open_folder()),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: "Выход".into(),
                icon_name: "application-exit".into(),
                activate: Box::new(|_: &mut Self| {
                    // SAFETY: raise лишь ставит сигнал нашему же процессу.
                    if unsafe { libc::raise(libc::SIGTERM) } != 0 {
                        std::process::exit(0);
                    }
                }),
                ..Default::default()
            }
            .into(),
        ]
    }
}

/// Поднимает иконку в трее. Возвращает handle — его нужно держать живым.
pub fn spawn(rec: Arc<Recorder>) -> Result<ksni::blocking::Handle<ReplayTray>> {
    ReplayTray { rec }
        .spawn()
        .context("не удалось зарегистрировать иконку в трее")
}
