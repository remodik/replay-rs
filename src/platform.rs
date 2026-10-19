//! Платформенные мелочи, общие для захвата, GUI и файловых конвейеров.
use anyhow::{Context, Result};
use gstreamer::prelude::*;
use std::path::Path;

/// Явные caps в системной памяти — так программный кодировщик работает везде.
pub fn windows_source(monitor: i32) -> String {
    format!("d3d11screencapturesrc capture-api=dxgi monitor-index={monitor} show-cursor=true ! video/x-raw,format=BGRA")
}

/// Путь задаём свойством GObject, а не подставляем в описание конвейера.
/// Так работают и буквы дисков Windows, и обратные слеши, и пробелы в именах.
pub fn set_file_location(pipeline: &gstreamer::Pipeline, name: &str, path: &Path) -> Result<()> {
    let element = pipeline
        .by_name(name)
        .context("в конвейере нет файлового элемента")?;
    set_location(&element, path)
}

/// То же для отдельного элемента (filesrc/filesink).
pub fn set_location(element: &gstreamer::Element, path: &Path) -> Result<()> {
    let location = path.to_str().context("путь к файлу не в UTF-8")?;
    anyhow::ensure!(!location.contains('\0'), "путь содержит NUL");
    element.set_property("location", location);
    Ok(())
}

pub fn open_path(path: &Path) -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        let child = std::process::Command::new("xdg-open").arg(path).spawn()?;
        reap(child);
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::UI::{Shell::ShellExecuteW, WindowsAndMessaging::SW_SHOWNORMAL};
        let path: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        let verb: Vec<u16> = "open\0".encode_utf16().collect();
        // SAFETY: буферы UTF-16 завершены NUL и живут всё время вызова.
        let result = unsafe {
            ShellExecuteW(
                std::ptr::null_mut(),
                verb.as_ptr(),
                path.as_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                SW_SHOWNORMAL,
            )
        };
        anyhow::ensure!(
            result as isize > 32,
            "не удалось открыть файл (ShellExecuteW: {})",
            result as isize
        );
    }
    Ok(())
}

/// `open_path` для кнопок «открыть»: результата ждать некому, но отказ
/// хотя бы попадает в лог, а не пропадает.
pub fn open_or_warn(path: &Path) {
    if let Err(e) = open_path(path) {
        log::warn!("не удалось открыть {}: {e:#}", path.display());
    }
}

/// Дожидается дочернего процесса в фоне.
///
/// Брошенный `Child` никто не ждёт, и каждый открытый клип оставлял зомби
/// до выхода рекордера, а он работает сутками.
#[cfg(target_os = "linux")]
pub fn reap(mut child: std::process::Child) {
    let spawned = std::thread::Builder::new()
        .name("reap".into())
        .spawn(move || {
            if let Err(e) = child.wait() {
                log::debug!("не удалось дождаться дочернего процесса: {e}");
            }
        });
    if let Err(e) = spawned {
        log::debug!("не удалось запустить поток ожидания: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn file_properties_preserve_windows_and_special_paths() {
        gstreamer::init().unwrap();
        let pipeline = gstreamer::parse::launch("filesrc name=input ! filesink name=output")
            .unwrap()
            .downcast::<gstreamer::Pipeline>()
            .unwrap();
        for location in [
            r"C:\Users\Имя пользователя\Videos\clip ! 1.mp4",
            "/tmp/a \"quoted\" clip.mp4",
        ] {
            for element in ["input", "output"] {
                set_file_location(&pipeline, element, Path::new(location)).unwrap();
                assert_eq!(
                    pipeline
                        .by_name(element)
                        .unwrap()
                        .property::<String>("location"),
                    location
                );
            }
        }
        assert!(set_file_location(&pipeline, "input", Path::new("bad\0path")).is_err());
    }
}

/// egui_extras ждёт ведущий слеш перед буквой диска Windows, в отличие от Unix.
pub fn file_uri(path: &Path) -> String {
    if cfg!(windows) {
        windows_file_uri(&path.to_string_lossy())
    } else {
        format!("file://{}", path.display())
    }
}

fn windows_file_uri(path: &str) -> String {
    let path = path.replace('\\', "/");
    if path.starts_with("//") {
        format!("file:{path}")
    } else {
        format!("file:///{path}")
    }
}

#[cfg(test)]
mod uri_tests {
    #[test]
    fn windows_drive_and_network_uris() {
        assert_eq!(
            super::windows_file_uri(r"C:\Users\User Name\preview.jpg"),
            "file:///C:/Users/User Name/preview.jpg"
        );
        assert_eq!(
            super::windows_file_uri(r"\\server\share\preview.jpg"),
            "file://server/share/preview.jpg"
        );
    }
}
