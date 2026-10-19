//! Замок единственного экземпляра.
//!
//! Два рекордера одновременно — это два захвата экрана, две иконки в трее,
//! две регистрации хоткея и драка за pid-файл: чей pid там останется, тому и
//! уйдёт `--save`.

use std::fs::{File, OpenOptions, TryLockError};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

/// Держит блокировку, пока жив. Блокировка снимается вместе с процессом,
/// поэтому аварийное завершение не оставляет замок висеть.
pub struct InstanceLock {
    _file: File,
}

pub fn pidfile() -> PathBuf {
    #[cfg(windows)]
    let dir = crate::config::config_dir();
    #[cfg(not(windows))]
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(crate::config::cache_dir);
    dir.join("replay-rs.pid")
}

/// Забирает замок и записывает свой pid.
///
/// `Ok(None)` — рекордер уже запущен.
pub fn acquire() -> Result<Option<InstanceLock>> {
    acquire_at(&pidfile())
}

fn acquire_at(path: &Path) -> Result<Option<InstanceLock>> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("не удалось создать {}", parent.display()))?;
    }
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .with_context(|| format!("не удалось открыть {}", path.display()))?;

    // Блокировка файла, а не проверка «жив ли pid»: pid переиспользуются,
    // а замок ядро снимает само при смерти процесса.
    match file.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => return Ok(None),
        // Например, ФС без поддержки блокировок: это не «уже запущен».
        Err(TryLockError::Error(e)) => {
            return Err(e).with_context(|| format!("не удалось заблокировать {}", path.display()))
        }
    }

    file.set_len(0).context("не удалось очистить pid-файл")?;
    write!(file, "{}", std::process::id()).context("не удалось записать pid")?;
    file.flush().ok();
    Ok(Some(InstanceLock { _file: file }))
}

/// Читает pid работающего рекордера.
///
/// Pid-файл переживает аварийное завершение, а pid переиспользуются: без
/// проверки `--save` после падения слал бы SIGUSR1 (по умолчанию — смерть)
/// постороннему процессу. Поэтому верим файлу, только пока его держит замок.
pub fn running_pid() -> Result<i32> {
    running_pid_at(&pidfile())
}

fn running_pid_at(path: &Path) -> Result<i32> {
    let mut file = File::open(path)
        .with_context(|| format!("рекордер не запущен (нет {})", path.display()))?;
    match file.try_lock_shared() {
        Ok(()) => bail!("рекордер не запущен (устаревший {})", path.display()),
        Err(TryLockError::WouldBlock) => {}
        Err(TryLockError::Error(e)) => {
            return Err(e).with_context(|| format!("не удалось проверить {}", path.display()))
        }
    }
    let mut text = String::new();
    file.read_to_string(&mut text)
        .with_context(|| format!("не удалось прочитать {}", path.display()))?;
    text.trim().parse().context("повреждённый pid-файл")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_pidfile(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("replay-pid-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir.join("replay-rs.pid")
    }

    #[test]
    fn live_lock_reports_its_pid_and_blocks_second_instance() {
        let path = temp_pidfile("live");
        let lock = acquire_at(&path).unwrap().expect("замок свободен");
        // На Windows замок обязательный: чужой дескриптор файл не прочтёт.
        #[cfg(unix)]
        assert_eq!(running_pid_at(&path).unwrap(), std::process::id() as i32);
        assert!(
            acquire_at(&path).unwrap().is_none(),
            "второй экземпляр получил замок"
        );
        drop(lock);
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    /// Регрессия: после аварийного завершения pid-файл остаётся, и `--save`
    /// слал сигнал тому, кто получил этот pid следующим.
    #[test]
    fn stale_pidfile_is_not_trusted() {
        let path = temp_pidfile("stale");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        // Как после SIGKILL: pid записан, замка никто не держит.
        std::fs::write(&path, "1").unwrap();
        let err = running_pid_at(&path).unwrap_err();
        assert!(format!("{err:#}").contains("устаревший"), "{err:#}");
        // Проверка не должна мешать новому запуску.
        let lock = acquire_at(&path)
            .unwrap()
            .expect("устаревший файл запер запуск");
        #[cfg(unix)]
        assert_eq!(running_pid_at(&path).unwrap(), std::process::id() as i32);
        drop(lock);
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn missing_pidfile_means_not_running() {
        let path = temp_pidfile("none");
        let err = running_pid_at(&path).unwrap_err();
        assert!(format!("{err:#}").contains("не запущен"), "{err:#}");
    }
}
