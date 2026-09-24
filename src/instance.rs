//! Замок единственного экземпляра.
//!
//! Два рекордера одновременно — это два захвата экрана, две иконки в трее,
//! две регистрации хоткея и драка за pid-файл: чей pid там останется, тому и
//! уйдёт `--save`.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;

use anyhow::{Context, Result};

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
        .unwrap_or_else(std::env::temp_dir);
    dir.join("replay-rs.pid")
}

/// Забирает замок и записывает свой pid.
///
/// `Ok(None)` — рекордер уже запущен.
pub fn acquire() -> Result<Option<InstanceLock>> {
    let path = pidfile();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .with_context(|| format!("не удалось открыть {}", path.display()))?;

    // Блокировка файла, а не проверка «жив ли pid»: pid переиспользуются,
    // а замок ядро снимает само при смерти процесса.
    if file.try_lock().is_err() {
        return Ok(None);
    }

    file.set_len(0).context("не удалось очистить pid-файл")?;
    write!(file, "{}", std::process::id()).context("не удалось записать pid")?;
    file.flush().ok();
    Ok(Some(InstanceLock { _file: file }))
}

/// Читает pid работающего рекордера.
pub fn running_pid() -> Result<i32> {
    let path = pidfile();
    std::fs::read_to_string(&path)
        .with_context(|| format!("рекордер не запущен (нет {})", path.display()))?
        .trim()
        .parse()
        .context("повреждённый pid-файл")
}
