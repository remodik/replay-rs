//! replay-rs — replay buffer для Wayland/KDE.
//!
//! Постоянно кодирует экран в кольцевой буфер и по команде сохраняет
//! последние N секунд без перекодирования.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use clap::Parser;
use gstreamer as gst;

use replay_rs::audio;
use replay_rs::config::{Config, Encoder};
use replay_rs::instance;
use replay_rs::recorder::Recorder;
use replay_rs::service::{CaptureService, Mode};
use replay_rs::shortcuts;
#[cfg(target_os = "linux")]
use replay_rs::tray;

#[derive(Parser, Debug)]
#[command(
    name = "replay-rs",
    about = "Replay buffer screen recorder (Linux / Windows)"
)]
struct Args {
    /// auto — захват экрана; test — тестовый источник; portal — Linux; desktop — Windows
    #[arg(long, default_value = "auto")]
    mode: String,

    /// Работать без окна, трей и хоткей
    #[arg(long)]
    headless: bool,

    /// Послать сигнал сохранения работающему рекордеру и выйти
    #[arg(long)]
    save: bool,

    /// Открыть системный редактор комбинаций у работающего рекордера и выйти
    #[arg(long)]
    configure_shortcut: bool,

    /// Остановить работающий рекордер и выйти
    #[arg(long)]
    quit: bool,

    /// Проверить кодировщики видео, напечатать таблицу и выйти
    #[arg(long)]
    list_encoders: bool,

    // Ниже — разовые переопределения сохранённых настроек.
    /// auto — лучший работающий; иначе имя элемента GStreamer (vah264enc, x264enc…)
    #[arg(long)]
    encoder: Option<String>,
    /// Видеокодек для автовыбора: h264, h265; av1 — экспериментально
    #[arg(long)]
    video_codec: Option<String>,
    /// Windows: индекс монитора DXGI, -1 — основной монитор
    #[arg(long, allow_hyphen_values = true, value_parser = clap::value_parser!(i32).range(-1..))]
    monitor: Option<i32>,
    /// Длина буфера, с
    #[arg(long)]
    seconds: Option<f64>,
    /// Сколько сохранять по хоткею, с
    #[arg(long)]
    save_seconds: Option<f64>,
    /// кбит/с
    #[arg(long)]
    bitrate: Option<u32>,
    #[arg(long)]
    fps: Option<u32>,
    /// Секунд между keyframe
    #[arg(long)]
    gop: Option<f64>,
    /// Лимит буфера, МБ
    #[arg(long)]
    max_mb: Option<usize>,
    #[arg(long)]
    output: Option<PathBuf>,

    /// (для тестов) остановиться через N секунд
    #[arg(long, default_value_t = 0.0)]
    run_for: f64,
}

/// Сколько ждём остановки конвейера перед выходом.
#[cfg(target_os = "linux")]
const SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_millis(500);

#[cfg(target_os = "linux")]
const SAVE_GRACE: std::time::Duration = std::time::Duration::from_secs(15);

/// Будим работающий процесс сигналом.
///
/// SIGUSR1 — сохранить клип (запасной вариант хоткея), SIGUSR2 — открыть
/// системный редактор комбинаций. Второе нужно в headless-режиме, где окна
/// с кнопкой просто нет.
/// Уведомление на рабочий стол. Необязательное: нет notify-send — не беда.
#[cfg(target_os = "linux")]
fn notify(summary: &str, body: &str) {
    match std::process::Command::new("notify-send")
        .args(["--app-name=replay-rs", "--icon=media-record", summary, body])
        .spawn()
    {
        // Ждём в фоне: иначе каждое уведомление оставляло бы зомби.
        Ok(child) => replay_rs::platform::reap(child),
        Err(e) => log::debug!("notify-send недоступен: {e}"),
    }
}

#[cfg(target_os = "linux")]
fn signal_running(sig: i32, what: &str) -> Result<()> {
    let pid = instance::running_pid()?;
    // SAFETY: kill с валидным pid — обычный системный вызов без побочных
    // эффектов для нашего адресного пространства.
    if unsafe { libc::kill(pid, sig) } != 0 {
        bail!("рекордер не запущен (процесс {pid} не отвечает)");
    }
    println!("{what} отправлено процессу {pid}");
    Ok(())
}

/// Сохранённые настройки плюс разовые правки из командной строки.
fn build_config(args: &Args) -> Result<Config> {
    let mut cfg = Config::load();
    if let Some(monitor) = args.monitor {
        cfg.monitor = monitor;
    }
    if let Some(e) = &args.encoder {
        cfg.encoder = e.parse::<Encoder>().map_err(anyhow::Error::msg)?;
    }
    if let Some(c) = &args.video_codec {
        cfg.video_codec = c
            .parse::<replay_rs::encoders::VideoCodec>()
            .map_err(anyhow::Error::msg)?;
    }
    if let Some(v) = args.seconds {
        cfg.seconds = v;
        // save_seconds по умолчанию тянется за длиной буфера.
        cfg.save_seconds = args.save_seconds.unwrap_or(v);
    }
    if let Some(v) = args.save_seconds {
        cfg.save_seconds = v;
    }
    if let Some(v) = args.bitrate {
        cfg.bitrate = v;
    }
    if let Some(v) = args.fps {
        cfg.fps = v;
    }
    if let Some(v) = args.gop {
        cfg.gop = v;
    }
    if let Some(v) = args.max_mb {
        cfg.max_mb = v;
    }
    if let Some(v) = &args.output {
        cfg.output = v.clone();
    }
    Ok(cfg)
}

fn main() -> Result<()> {
    // zbus логирует каждый вызов на INFO, а его property-кеш шумит WARN-ами
    // про несуществующие объекты портала — это нормально и топит наши строки.
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        // Глушим по модулям, а не только в фильтре по умолчанию: иначе
        // RUST_LOG=info возвращает поток служебных сообщений zbus.
        .filter_module("zbus", log::LevelFilter::Error)
        .filter_module("tracing", log::LevelFilter::Warn)
        .init();
    let args = Args::parse();

    #[cfg(windows)]
    if args.save || args.quit || args.configure_shortcut {
        return shortcuts::send_command(if args.save {
            shortcuts::SAVE
        } else if args.quit {
            shortcuts::QUIT
        } else {
            shortcuts::CONFIGURE
        });
    }
    #[cfg(target_os = "linux")]
    if args.save {
        return signal_running(libc::SIGUSR1, "сохранение");
    }
    #[cfg(target_os = "linux")]
    if args.configure_shortcut {
        return signal_running(libc::SIGUSR2, "открытие редактора комбинаций");
    }
    #[cfg(target_os = "linux")]
    if args.quit {
        return signal_running(libc::SIGTERM, "остановка");
    }

    if args.list_encoders {
        gst::init().context("не удалось инициализировать GStreamer")?;
        let found = replay_rs::encoders::discover(&replay_rs::encoders::VideoCodec::ALL);
        print!("{}", replay_rs::encoders::report(&found));
        return Ok(());
    }

    let mode = match args.mode.as_str() {
        #[cfg(target_os = "linux")]
        "auto" | "portal" => Mode::Portal,
        #[cfg(windows)]
        "auto" | "desktop" => Mode::Desktop,
        "test" => Mode::Test,
        other => bail!("неизвестный режим: {other}"),
    };

    // Замок берём до тяжёлой инициализации: незачем поднимать второй
    // захват экрана, чтобы тут же его свернуть.
    let Some(_lock) = instance::acquire()? else {
        let pid = instance::running_pid()
            .map(|p| p.to_string())
            .unwrap_or_default();
        // При запуске из меню stderr никто не видит (Terminal=false), поэтому
        // говорим ещё и уведомлением — иначе клик по значку выглядит так,
        // будто ничего не произошло.
        notify(
            "replay-rs уже запущен",
            "Окно разворачивается из панели задач",
        );
        bail!(
            "replay-rs уже запущен (процесс {pid}).\n\
             Окно разворачивается из панели задач, сохранение — replay-rs --save,\n\
             остановка — replay-rs --quit."
        );
    };

    let cfg = build_config(&args)?;
    // Замок уже наш, так что *.mp4.part в папке — огрызки прошлого запуска,
    // а не чужая запись.
    replay_rs::saver::remove_stale_parts(&cfg.output);
    #[cfg(windows)]
    anyhow::ensure!(
        !cfg.encoder.is_va(),
        "VA-API доступен только на Linux; используйте --encoder x264enc"
    );
    gst::init().context("не удалось инициализировать GStreamer")?;

    // Кодек выбираем после gst::init(): без неё реестр элементов недоступен.
    // Поток захвата перевыбирает его при каждой сборке конвейера и пишет в
    // лог, что получилось; здесь — только начальное значение для окна.
    let codec = cfg
        .audio
        .any_enabled()
        .then(|| audio::choose_codec(cfg.audio.codec).codec())
        .flatten();
    let rec = Arc::new(Recorder::new(cfg, codec));
    let service = Arc::new(CaptureService::start(rec.clone(), mode));

    log::info!(
        "replay-rs запущен (pid {}). Сохранить: replay-rs --save",
        std::process::id()
    );

    // Уведомление после сохранения — по настройке, проверяемой в момент
    // сохранения. Weak: подписка живёт в самом Recorder.
    let weak = Arc::downgrade(&rec);
    rec.on_saved(move |r| {
        let Some(rec) = weak.upgrade() else { return };
        if !rec.config().overlay.notify_on_save {
            return;
        }
        match &r.result {
            Ok(p) => notify(
                "Клип сохранён",
                &p.file_name().unwrap_or_default().to_string_lossy(),
            ),
            Err(e) => notify("Клип не сохранён", e),
        }
    });
    // Оверлей сам следит за настройками: выключенный — спит.
    let overlay = Arc::new(replay_rs::overlay::Overlay::spawn(
        rec.clone(),
        service.clone(),
    ));

    let shortcuts = Arc::new(shortcuts::Shortcuts::spawn(rec.clone()));
    #[cfg(target_os = "linux")]
    install_signal_handlers(&rec, &shortcuts, &service)?;
    #[cfg(windows)]
    anyhow::ensure!(
        shortcuts.available(),
        "не удалось запустить интеграцию Windows: {}",
        shortcuts.state().summary()
    );

    // Handle держим до конца main: с его смертью иконка пропадает.
    #[cfg(target_os = "linux")]
    let _tray = match tray::spawn(rec.clone()) {
        Ok(h) => Some(h),
        Err(e) => {
            log::warn!("трей недоступен: {e:#}");
            None
        }
    };
    if args.run_for > 0.0 {
        let secs = args.run_for;
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_secs_f64(secs));
            log::info!("--run-for истёк, выхожу");
            std::process::exit(0);
        });
    }

    let result = if args.headless {
        run_headless(&shortcuts)
    } else {
        run_gui(
            rec.clone(),
            service.clone(),
            shortcuts.clone(),
            overlay.clone(),
        )
    };

    service.request_stop();
    drop(service);
    drop(shortcuts);
    #[cfg(target_os = "linux")]
    let _ = std::fs::remove_file(instance::pidfile());
    result
}

fn run_headless(_shortcuts: &shortcuts::Shortcuts) -> Result<()> {
    // Вся работа в фоновых потоках; главный просто ждёт сигнала.
    loop {
        #[cfg(windows)]
        {
            if _shortcuts.should_quit() {
                return Ok(());
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        #[cfg(target_os = "linux")]
        std::thread::park();
    }
}

/// Windows: закрытие окна завершает приложение; Linux перехватывает крестик
/// в GUI и сворачивает окно, сохраняя запись.
fn run_gui(
    rec: Arc<Recorder>,
    service: Arc<CaptureService>,
    shortcuts: Arc<shortcuts::Shortcuts>,
    overlay: Arc<replay_rs::overlay::Overlay>,
) -> Result<()> {
    #[cfg(target_os = "linux")]
    let background_shortcuts = shortcuts.clone();
    let options = eframe::NativeOptions {
        // Задаём явно: на этом держится «закрыл окно — запись идёт дальше».
        // При false eframe вызывает process::exit(0) на закрытии окна, и всё
        // после run_native становится мёртвым кодом. Значение по умолчанию
        // сейчас true, но в документации оно помечено как обратимое.
        run_and_return: true,
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([1120.0, 780.0])
            .with_min_inner_size([760.0, 480.0])
            .with_title("replay-rs")
            // Связывает окно с packaging/replay-rs.desktop: иконка в панели
            // задач и собственная идентичность вместо родительского приложения.
            .with_app_id("replay-rs"),
        ..Default::default()
    };
    eframe::run_native(
        "replay-rs",
        options,
        Box::new(move |cc| {
            Ok(Box::new(replay_rs::gui::App::new(
                cc, rec, service, shortcuts, overlay,
            )))
        }),
    )
    .map_err(|e| anyhow::anyhow!("не удалось открыть окно: {e}"))?;

    #[cfg(target_os = "linux")]
    return run_headless(&background_shortcuts);
    #[cfg(windows)]
    Ok(())
}

/// SIGUSR1 — сохранить клип, SIGINT/SIGTERM — выйти.
///
/// glib 0.22 больше не биндит `unix_signal_add`, поэтому сигналы слушает
/// отдельный поток.
#[cfg(target_os = "linux")]
fn install_signal_handlers(
    rec: &Arc<Recorder>,
    shortcuts: &Arc<shortcuts::Shortcuts>,
    service: &Arc<CaptureService>,
) -> Result<()> {
    use signal_hook::consts::{SIGINT, SIGTERM, SIGUSR1, SIGUSR2};
    use signal_hook::iterator::Signals;

    let mut signals = Signals::new([SIGUSR1, SIGUSR2, SIGINT, SIGTERM])
        .context("не удалось подписаться на сигналы")?;
    let rec = rec.clone();
    let shortcuts = shortcuts.clone();
    let service = service.clone();
    std::thread::Builder::new()
        .name("signals".into())
        .spawn(move || {
            for sig in signals.forever() {
                match sig {
                    SIGUSR1 => {
                        // Сохранение в отдельном потоке: не задерживаем
                        // обработку следующих сигналов.
                        let r = rec.clone();
                        std::thread::spawn(move || {
                            if let Err(e) = r.save(None) {
                                log::error!("сохранение не удалось: {e:#}");
                            }
                        });
                    }
                    SIGUSR2 => shortcuts.open_settings(),
                    _ => {
                        // Даём конвейеру дойти до NULL: иначе VA-кодировщик
                        // падает с «Failed to encode the frame» посреди кадра.
                        service.request_stop();
                        let deadline = std::time::Instant::now() + SAVE_GRACE;
                        while rec.is_saving() && std::time::Instant::now() < deadline {
                            std::thread::sleep(std::time::Duration::from_millis(50));
                        }
                        if rec.is_saving() {
                            log::warn!("сохранение не успело завершиться до выхода");
                        }
                        std::thread::sleep(SHUTDOWN_GRACE);
                        let _ = std::fs::remove_file(instance::pidfile());
                        std::process::exit(0);
                    }
                }
            }
        })
        .context("не удалось запустить поток сигналов")?;
    Ok(())
}

#[cfg(windows)]
fn notify(summary: &str, body: &str) {
    shortcuts::notify(summary, body);
}
