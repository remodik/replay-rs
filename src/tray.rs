//! Иконка в трее через StatusNotifierItem.
//!
//! На KDE за SNI отвечает `org.kde.StatusNotifierWatcher` (kded6), поэтому
//! берём ksni напрямую, без GTK-прослойки, которую тянет tray-icon.

use std::sync::Arc;

use anyhow::{Context, Result};
use ksni::menu::{MenuItem, StandardItem};
use ksni::blocking::TrayMethods;

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
        let _ = std::process::Command::new("xdg-open")
            .arg(self.rec.config().output)
            .spawn();
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
        ksni::ToolTip {
            title: "replay-rs".into(),
            description: format!(
                "буфер {:.0} / {:.0} с · {:.0} МБ",
                s.seconds(),
                cfg.seconds,
                s.bytes as f64 / 1e6
            ),
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
                activate: Box::new(|_: &mut Self| std::process::exit(0)),
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
