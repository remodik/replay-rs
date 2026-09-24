//! Окно настроек и список клипов на egui.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use eframe::egui;

use crate::config::Config;
use crate::recorder::{Recorder, SaveOutcome};
use crate::service::{CaptureService, CaptureState};
use crate::shortcuts::Shortcuts;
use crate::thumbs;

/// Как часто обновляем статус буфера.
const TICK: Duration = Duration::from_millis(500);
/// Сколько держим сообщение об успехе/ошибке.
const TOAST_LIFETIME: Duration = Duration::from_secs(6);
/// Как часто перечитываем папку: клип мог сохраниться по хоткею, мимо GUI.
const RESCAN_EVERY: Duration = Duration::from_secs(2);

#[derive(Debug, Clone)]
struct Clip {
    path: PathBuf,
    size: u64,
    modified: SystemTime,
    thumb: Option<PathBuf>,
}

/// Результаты фоновых задач (сохранение, превью).
enum Msg {
    Saved(anyhow::Result<SaveOutcome>),
    Thumb { clip: PathBuf, thumb: PathBuf },
}

pub struct App {
    rec: Arc<Recorder>,
    service: Arc<CaptureService>,
    shortcuts: Arc<Shortcuts>,
    /// Редактируемая копия; применяется кнопкой.
    draft: Config,
    clips: Vec<Clip>,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    toast: Option<(String, bool, Instant)>,
    /// Клип, для которого ждём подтверждения удаления.
    pending_delete: Option<PathBuf>,
    saving: bool,
    /// Превью уже считаются или не собрались — второй раз не запускаем.
    thumb_known: HashSet<PathBuf>,
    last_scan: Instant,
}

impl App {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        rec: Arc<Recorder>,
        service: Arc<CaptureService>,
        shortcuts: Arc<Shortcuts>,
    ) -> Self {
        egui_extras::install_image_loaders(&cc.egui_ctx);
        configure_style(&cc.egui_ctx);
        let (tx, rx) = mpsc::channel();
        let draft = rec.config();
        let mut app = Self {
            rec,
            service,
            shortcuts,
            draft,
            clips: Vec::new(),
            tx,
            rx,
            toast: None,
            pending_delete: None,
            saving: false,
            thumb_known: HashSet::new(),
            last_scan: Instant::now(),
        };
        app.refresh_clips();
        app
    }

    /// Перечитывает папку клипов и догоняет недостающие превью в фоне.
    fn refresh_clips(&mut self) {
        let dir = self.rec.config().output;
        let mut clips: Vec<Clip> = std::fs::read_dir(&dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| e.path().extension().is_some_and(|x| x == "mp4"))
            .filter_map(|e| {
                let meta = e.metadata().ok()?;
                let path = e.path();
                let thumb = thumbs::thumb_path(&path);
                Some(Clip {
                    thumb: thumb.exists().then_some(thumb),
                    path,
                    size: meta.len(),
                    modified: meta.modified().ok()?,
                })
            })
            .collect();
        // Свежие сверху.
        clips.sort_by_key(|c| std::cmp::Reverse(c.modified));

        for clip in clips.iter().filter(|c| c.thumb.is_none()) {
            // Один заход на клип: иначе пересканирование каждые 2 с плодило бы
            // потоки для клипа, превью которого не собирается в принципе.
            if !self.thumb_known.insert(clip.path.clone()) {
                continue;
            }
            let path = clip.path.clone();
            let tx = self.tx.clone();
            // Декодирование кадра не должно морозить кадр GUI.
            std::thread::spawn(move || match thumbs::ensure_thumb(&path) {
                Ok(thumb) => {
                    let _ = tx.send(Msg::Thumb { clip: path, thumb });
                }
                Err(e) => log::warn!("превью для {} не собралось: {e:#}", path.display()),
            });
        }
        self.clips = clips;
        self.last_scan = Instant::now();
    }

    /// Папка могла измениться мимо GUI — клип сохранён хоткеем или удалён извне.
    fn rescan_if_stale(&mut self) {
        if self.last_scan.elapsed() < RESCAN_EVERY {
            return;
        }
        let dir = self.rec.config().output;
        let current: Vec<(PathBuf, u64)> = std::fs::read_dir(&dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| e.path().extension().is_some_and(|x| x == "mp4"))
            .filter_map(|e| Some((e.path(), e.metadata().ok()?.len())))
            .collect();
        let known: Vec<(PathBuf, u64)> = self
            .clips
            .iter()
            .map(|c| (c.path.clone(), c.size))
            .collect();
        let changed = current.len() != known.len() || !current.iter().all(|x| known.contains(x));
        if changed {
            self.refresh_clips();
        } else {
            self.last_scan = Instant::now();
        }
    }

    fn drain_messages(&mut self) {
        let mut refresh = false;
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                Msg::Saved(result) => {
                    self.saving = false;
                    match result {
                        Ok(SaveOutcome::Saved(path)) => {
                            self.toast(format!("сохранено: {}", file_name(&path)), false);
                            refresh = true;
                        }
                        Ok(SaveOutcome::BufferEmpty) => {
                            self.toast("буфер пуст — запись ещё не идёт".into(), true)
                        }
                        Ok(SaveOutcome::AlreadySaving) => {
                            self.toast("предыдущее сохранение ещё идёт".into(), true)
                        }
                        Err(e) => self.toast(format!("не сохранилось: {e:#}"), true),
                    }
                }
                Msg::Thumb { clip, thumb } => {
                    if let Some(c) = self.clips.iter_mut().find(|c| c.path == clip) {
                        c.thumb = Some(thumb);
                    }
                }
            }
        }
        if refresh {
            self.refresh_clips();
        }
    }

    fn toast(&mut self, text: String, is_error: bool) {
        if is_error {
            log::warn!("{text}");
        }
        self.toast = Some((text, is_error, Instant::now()));
    }

    fn trigger_save(&mut self) {
        if self.saving {
            return;
        }
        self.saving = true;
        let rec = self.rec.clone();
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(Msg::Saved(rec.save(None)));
        });
    }

    fn apply_settings(&mut self) {
        let needs_restart = self.rec.update_config(self.draft.clone());
        if let Err(e) = self.draft.store() {
            self.toast(format!("настройки не сохранились: {e:#}"), true);
            return;
        }
        if needs_restart {
            self.service.restart();
            self.toast("настройки применены, конвейер пересобирается".into(), false);
        } else {
            self.toast("настройки применены".into(), false);
        }
        self.refresh_clips();
    }

    fn delete_clip(&mut self, path: &PathBuf) {
        match std::fs::remove_file(path) {
            Ok(()) => {
                thumbs::remove_thumb(path);
                self.thumb_known.remove(path);
                self.toast(format!("удалён: {}", file_name(path)), false);
                self.refresh_clips();
            }
            Err(e) => self.toast(format!("не удалось удалить: {e}"), true),
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.draw(ui);
    }
}

impl App {
    /// Всё окно. Вынесено из `eframe::App::ui`, чтобы раскладку можно было
    /// проверять в тестах без настоящего окна и `eframe::Frame`.
    fn draw(&mut self, ui: &mut egui::Ui) {
        self.drain_messages();
        self.rescan_if_stale();
        #[cfg(target_os = "linux")]
        self.minimize_instead_of_closing(ui.ctx());
        #[cfg(windows)]
        if self.shortcuts.should_quit() {
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
        }
        // Статус буфера живой, поэтому перерисовываемся по таймеру.
        let ctx = ui.ctx().clone();
        ctx.request_repaint_after(TICK);

        egui::Panel::top("status")
            .frame(egui::Frame::new().fill(SURFACE).inner_margin(20))
            .show(ui, |ui| self.status_bar(ui));
        // Ширина фиксирована: несжимаемая панель egui подстраивается под самую
        // широкую строку содержимого. Длинная ошибка портала в ряду хоткея
        // раздувала её на всё окно, и библиотеку сжимало в узкую полосу.
        egui::Panel::left("settings")
            .exact_size(SETTINGS_WIDTH)
            .frame(egui::Frame::new().fill(SURFACE).inner_margin(18))
            .show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .id_salt("settings_scroll")
                    .show(ui, |ui| self.settings_panel(ui));
            });
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(BACKGROUND).inner_margin(22))
            .show(ui, |ui| self.clips_panel(ui));

        self.confirm_delete_window(&ctx);
    }
}

impl App {
    /// Крестик сворачивает окно, а не закрывает.
    ///
    /// Закрытое окно на Wayland не вернуть: цикл событий завершается, а
    /// поднять его заново нельзя. Свёрнутое остаётся в панели задач, откуда
    /// пользователь его и разворачивает. Запись при этом идёт, выход — через
    /// меню в трее или `replay-rs --quit`.
    #[cfg(target_os = "linux")]
    fn minimize_instead_of_closing(&self, ctx: &egui::Context) {
        if ctx.input(|i| i.viewport().close_requested()) {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
        }
    }

    fn status_bar(&mut self, ui: &mut egui::Ui) {
        let stats = self.rec.stats();
        let cfg = self.rec.config();
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.label(egui::RichText::new("replay-rs").size(26.0).strong());
                ui.weak("Сохраняйте моменты, которые уже случились");
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let label = if self.saving {
                    "Сохранение…".to_owned()
                } else {
                    format!("Сохранить {:.0} с", cfg.save_seconds)
                };
                let btn = egui::Button::new(egui::RichText::new(label).strong().color(BACKGROUND))
                    .fill(ACCENT)
                    .min_size([174.0, 42.0].into());
                if ui.add_enabled(!self.saving, btn).clicked() {
                    self.trigger_save();
                }
            });
        });
        ui.add_space(16.0);
        ui.horizontal_wrapped(|ui| {
            let (label, color) = match self.service.state() {
                CaptureState::Running => ("● Запись идёт", ACCENT),
                CaptureState::WaitingForPortal => (
                    if cfg!(windows) {
                        "● Запуск захвата"
                    } else {
                        "● Выберите экран"
                    },
                    WARNING,
                ),
                CaptureState::Stopped => ("● Остановлено", MUTED),
                CaptureState::Failed(_) => ("● Ошибка записи", DANGER),
            };
            ui.colored_label(color, label);
            ui.separator();
            let fill = if cfg.seconds > 0.0 {
                (stats.seconds() / cfg.seconds).clamp(0.0, 1.0)
            } else {
                0.0
            };
            ui.add(
                // Без текста внутри: светлые цифры поверх бирюзовой заливки
                // не читались, заполненная часть их съедала.
                egui::ProgressBar::new(fill as f32)
                    .fill(ACCENT)
                    .desired_width(180.0),
            );
            ui.label(format!("{:.1} / {:.0} с", stats.seconds(), cfg.seconds));
            ui.separator();
            ui.weak(format!(
                "{:.1} / {} МиБ",
                stats.bytes as f64 / 1_048_576.0,
                cfg.max_mb
            ))
            .on_hover_text(format!("{} групп кадров в видеобуфере", stats.gops));
            if let Some(a) = &self.rec.audio {
                let st = a.buffer.lock().expect("звуковой буфер отравлен").stats();
                ui.separator();
                ui.weak(format!("Звук  {:.0} с", st.seconds()));
            }
        });

        if let CaptureState::Failed(err) = self.service.state() {
            ui.colored_label(egui::Color32::from_rgb(220, 100, 100), err);
        }
        if let Some((text, is_error, at)) = &self.toast {
            if at.elapsed() < TOAST_LIFETIME {
                let color = if *is_error {
                    egui::Color32::from_rgb(220, 140, 100)
                } else {
                    egui::Color32::from_rgb(140, 190, 140)
                };
                ui.colored_label(color, text);
            } else {
                self.toast = None;
            }
        }
        ui.add_space(6.0);
    }

    fn settings_panel(&mut self, ui: &mut egui::Ui) {
        ui.add_space(8.0);
        ui.heading("Параметры записи");
        ui.weak("Настройте свой следующий реплей");
        section_title(ui, "Буфер");
        ui.add_space(8.0);

        ui.label("Длина буфера");
        ui.add(egui::Slider::new(&mut self.draft.seconds, 5.0..=300.0).suffix(" с"));
        ui.label("Сохранять последние");
        ui.add(egui::Slider::new(&mut self.draft.save_seconds, 5.0..=300.0).suffix(" с"));
        ui.label("Лимит памяти");
        ui.add(egui::Slider::new(&mut self.draft.max_mb, 50..=4096).suffix(" МиБ"));

        ui.add_space(10.0);
        ui.separator();
        section_title(ui, "Качество видео");
        ui.label(
            egui::RichText::new("Изменения перезапустят запись")
                .small()
                .weak(),
        );
        #[cfg(windows)]
        {
            ui.label("Монитор (-1 — основной)");
            ui.add(egui::DragValue::new(&mut self.draft.monitor).range(-1..=64));
        }
        ui.label("Битрейт");
        ui.add(egui::Slider::new(&mut self.draft.bitrate, 1000..=100_000).suffix(" кбит/с"));
        ui.label("Кадров в секунду");
        ui.add(egui::Slider::new(&mut self.draft.fps, 24..=144));
        ui.label("Интервал ключевых кадров");
        ui.add(egui::Slider::new(&mut self.draft.gop, 0.5..=5.0).suffix(" с"));

        ui.add_space(10.0);
        ui.separator();
        section_title(ui, "Звук");
        match self.rec.codec {
            Some(c) => {
                ui.horizontal(|ui| {
                    ui.label("кодек");
                    egui::ComboBox::from_id_salt("audio_codec")
                        .selected_text(self.draft.audio.codec.label())
                        .show_ui(ui, |ui| {
                            for p in [
                                crate::audio::CodecPreference::Auto,
                                crate::audio::CodecPreference::Aac,
                                crate::audio::CodecPreference::Opus,
                            ] {
                                ui.selectable_value(&mut self.draft.audio.codec, p, p.label());
                            }
                        });
                });
                ui.label(
                    egui::RichText::new(format!("сейчас пишется: {}", c.as_str()))
                        .small()
                        .weak(),
                );
                // Запрошенный AAC мог подмениться на Opus — это видно только
                // здесь, в логе строка уже уехала.
                if self.draft.audio.codec == crate::audio::CodecPreference::Aac
                    && c != crate::audio::AudioCodec::Aac
                {
                    ui.colored_label(
                        egui::Color32::from_rgb(220, 180, 80),
                        if cfg!(windows) {
                            "AAC недоступен — установите GStreamer Complete"
                        } else {
                            "AAC недоступен — поставьте gst-libav"
                        },
                    );
                }
                ui.checkbox(&mut self.draft.audio.system, "Системный звук");
                ui.add_enabled(
                    self.draft.audio.system,
                    egui::Slider::new(&mut self.draft.audio.system_volume, 0.0..=2.0)
                        .text("громкость"),
                );
                ui.checkbox(&mut self.draft.audio.mic, "Микрофон");
                ui.add_enabled(
                    self.draft.audio.mic,
                    egui::Slider::new(&mut self.draft.audio.mic_volume, 0.0..=2.0)
                        .text("громкость"),
                );
                ui.add(
                    egui::Slider::new(&mut self.draft.audio.bitrate, 64..=320).suffix(" кбит/с"),
                );
            }
            None => {
                ui.colored_label(
                    egui::Color32::from_rgb(220, 180, 80),
                    "кодировщика звука нет",
                );
                ui.label(
                    egui::RichText::new(if cfg!(windows) {
                        "Установите GStreamer Complete с AAC или Opus"
                    } else {
                        "поставьте gst-libav (AAC) — opusenc тоже подойдёт"
                    })
                    .small()
                    .weak(),
                );
            }
        }

        ui.add_space(10.0);
        ui.separator();
        self.shortcut_section(ui);

        ui.add_space(10.0);
        ui.separator();
        ui.label("Папка для клипов");
        let mut dir = self.draft.output.to_string_lossy().to_string();
        if ui.text_edit_singleline(&mut dir).changed() {
            self.draft.output = PathBuf::from(dir);
        }
        if ui.button("Открыть папку").clicked() {
            let _ = crate::platform::open_path(&self.draft.output);
        }

        ui.add_space(12.0);
        let dirty = self.draft != self.rec.config();
        ui.horizontal(|ui| {
            if ui
                .add_enabled(
                    dirty,
                    egui::Button::new("Применить").min_size([130.0, 34.0].into()),
                )
                .clicked()
            {
                self.apply_settings();
            }
            if ui
                .add_enabled(dirty, egui::Button::new("Отменить"))
                .clicked()
            {
                self.draft = self.rec.config();
            }
        });
        if dirty {
            ui.label(
                egui::RichText::new("есть несохранённые изменения")
                    .small()
                    .weak(),
            );
        }
    }

    /// Хоткей назначает композитор, а не мы: на Wayland клавиши раздаёт он.
    /// Показываем текущую привязку и открываем системный редактор.
    fn shortcut_section(&mut self, ui: &mut egui::Ui) {
        let st = self.shortcuts.state();
        ui.label(egui::RichText::new("Хоткей сохранения").strong());
        // Отдельной строкой и с переносом: в горизонтальном ряду текст не
        // переносится, и полный текст ошибки D-Bus растягивал всю панель.
        match &st.error {
            Some(err) => {
                ui.add(
                    egui::Label::new(
                        egui::RichText::new("портал хоткеев недоступен")
                            .color(egui::Color32::from_rgb(220, 140, 100)),
                    )
                    .wrap(),
                )
                .on_hover_text(err);
                if err.contains("app id") {
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new(
                                "Запустите replay-rs из меню приложений: при запуске \
                                 из файлового менеджера портал не знает, чей это хоткей.",
                            )
                            .small()
                            .weak(),
                        )
                        .wrap(),
                    );
                }
            }
            None => {
                let color = if st.trigger.is_some() {
                    egui::Color32::from_rgb(140, 190, 140)
                } else {
                    egui::Color32::from_rgb(220, 180, 80)
                };
                ui.add(
                    egui::Label::new(
                        egui::RichText::new(format!("комбинация: {}", st.summary())).color(color),
                    )
                    .wrap(),
                );
            }
        }

        if ui
            .add_enabled(
                st.error.is_none(),
                egui::Button::new(if cfg!(windows) {
                    "О сочетании клавиш…"
                } else {
                    "Настроить комбинацию…"
                }),
            )
            .on_hover_text(if cfg!(windows) {
                "Информация о Ctrl+Alt+S"
            } else {
                "Откроет системный редактор KDE для действий replay-rs"
            })
            .clicked()
        {
            self.shortcuts.open_settings();
        }

        if cfg!(target_os = "linux") && st.trigger.is_none() && st.registered {
            ui.label(
                egui::RichText::new(
                    "Клавишу назначает композитор — нажмите «Настроить комбинацию…»",
                )
                .small()
                .weak(),
            );
        }

        ui.add_space(4.0);
        ui.label(
            egui::RichText::new("Работает всегда, без портала:")
                .small()
                .weak(),
        );
        ui.horizontal(|ui| {
            ui.code("replay-rs --save");
            if ui.small_button("копировать").clicked() {
                let exe = std::env::current_exe()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|_| "replay-rs".into());
                ui.ctx().copy_text(format!("{exe} --save"));
            }
        });
    }

    fn clips_panel(&mut self, ui: &mut egui::Ui) {
        // На узкой панели кнопки уходят на свою строку: right_to_left в
        // горизонтальном ряду не переносится и рисует их поверх заголовка.
        let narrow = ui.available_width() < 380.0;
        let mut refresh = false;
        let mut open_folder = false;
        let buttons = |ui: &mut egui::Ui, refresh: &mut bool, open_folder: &mut bool| {
            if ui.button("Обновить").clicked() {
                *refresh = true;
            }
            if ui.button("Папка").clicked() {
                *open_folder = true;
            }
        };
        ui.horizontal(|ui| {
            ui.heading("Библиотека");
            ui.weak(clips_count(self.clips.len()));
            if !narrow {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    buttons(ui, &mut refresh, &mut open_folder)
                });
            }
        });
        if narrow {
            ui.horizontal(|ui| buttons(ui, &mut refresh, &mut open_folder));
        }
        if refresh {
            self.refresh_clips();
        }
        if open_folder {
            let _ = crate::platform::open_path(&self.rec.config().output);
        }
        ui.weak("Последние сохранённые моменты · новые сверху");
        ui.add_space(18.0);

        if self.clips.is_empty() {
            card().show(ui, |ui| {
                ui.set_min_width((ui.available_width() - 2.0).max(0.0));
                ui.add_space(42.0);
                ui.vertical_centered(|ui| {
                    ui.label(
                        egui::RichText::new("Ваш первый реплей — впереди")
                            .size(21.0)
                            .strong(),
                    );
                    ui.add_space(8.0);
                    ui.weak("Дождитесь наполнения буфера и нажмите «Сохранить».");
                    ui.weak("Здесь появятся клипы с превью.");
                });
                ui.add_space(42.0);
            });
            return;
        }

        let mut delete_request = None;
        egui::ScrollArea::vertical()
            .id_salt("clips_scroll")
            .show(ui, |ui| {
                for clip in &self.clips {
                    clip_row(ui, clip, &mut delete_request);
                    ui.add_space(10.0);
                }
            });
        if let Some(p) = delete_request {
            self.pending_delete = Some(p);
        }
    }

    /// Удаление файла необратимо, поэтому спрашиваем подтверждение.
    fn confirm_delete_window(&mut self, ctx: &egui::Context) {
        let Some(path) = self.pending_delete.clone() else {
            return;
        };
        let mut open = true;
        egui::Window::new("Удалить клип?")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .open(&mut open)
            .show(ctx, |ui| {
                ui.label(file_name(&path));
                ui.label(
                    egui::RichText::new("Файл будет удалён безвозвратно.")
                        .small()
                        .weak(),
                );
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button("Удалить").clicked() {
                        self.delete_clip(&path);
                        self.pending_delete = None;
                    }
                    if ui.button("Отмена").clicked() {
                        self.pending_delete = None;
                    }
                });
            });
        if !open {
            self.pending_delete = None;
        }
    }
}

/// Ширина панели настроек.
const SETTINGS_WIDTH: f32 = 310.0;

/// Высота превью в списке клипов; ширина — под 16:9.
const THUMB_SIZE: egui::Vec2 = egui::vec2(160.0, 90.0);

/// Карточка одного клипа: превью, имя, метаданные, кнопки.
fn clip_row(ui: &mut egui::Ui, clip: &Clip, delete_request: &mut Option<PathBuf>) -> egui::Rect {
    let rect = card().show(ui, |ui| {
        ui.set_min_width((ui.available_width() - 2.0).max(0.0));
        // На узком окне метаданные переходят под превью.
        let compact = ui.available_width() < 460.0;
        if compact {
            ui.vertical(|ui| clip_row_contents(ui, clip, delete_request));
        } else {
            // Высоту ряда ограничиваем высотой превью. with_layout с
            // горизонтальной раскладкой внутри вертикальной панели забирает
            // всю оставшуюся высоту: карточка раздувалась на всё окно, а
            // превью центрировалось посреди этой пустоты, далеко под текстом.
            ui.allocate_ui_with_layout(
                egui::vec2(ui.available_width(), THUMB_SIZE.y),
                egui::Layout::left_to_right(egui::Align::Center),
                |ui| clip_row_contents(ui, clip, delete_request),
            );
        }
    })
    .response
    .rect;
    ui.add_space(10.0);
    rect
}

fn clip_row_contents(ui: &mut egui::Ui, clip: &Clip, delete_request: &mut Option<PathBuf>) {
    // На узкой карточке превью сжимается под её ширину с сохранением 16:9:
    // превью фиксированной ширины само вылезало за правый край.
    let thumb = if ui.available_width() < THUMB_SIZE.x {
        let w = ui.available_width().max(1.0);
        egui::vec2(w, w * THUMB_SIZE.y / THUMB_SIZE.x)
    } else {
        THUMB_SIZE
    };
    let (rect, _) = ui.allocate_exact_size(thumb, egui::Sense::hover());
    ui.painter().rect_filled(rect, 8.0, BACKGROUND);
    if let Some(t) = &clip.thumb {
        ui.put(
            rect,
            egui::Image::new(crate::platform::file_uri(t))
                .max_size(thumb)
                .corner_radius(8.0),
        );
    } else {
        ui.painter().text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            "Нет превью",
            egui::FontId::proportional(13.0),
            MUTED,
        );
    }
    ui.vertical(|ui| {
        ui.add(egui::Label::new(egui::RichText::new(file_name(&clip.path)).strong()).wrap());
        let date: chrono::DateTime<chrono::Local> = clip.modified.into();
        ui.weak(format!(
            "{} · {:.1} МБ · MP4",
            date.format("%d.%m.%Y  %H:%M"),
            clip.size as f64 / 1e6
        ));
        ui.add_space(6.0);
        // С переносом: на узкой карточке «Удалить» вылезал за правый край.
        ui.horizontal_wrapped(|ui| {
            if ui.button("Открыть клип").clicked() {
                let _ = crate::platform::open_path(&clip.path);
            }
            if ui
                .button(egui::RichText::new("Удалить").color(DANGER))
                .clicked()
            {
                *delete_request = Some(clip.path.clone());
            }
        });
    });
}

/// «1 клип», «3 клипа», «11 клипов».
fn clips_count(n: usize) -> String {
    let word = match (n % 10, n % 100) {
        (_, 11..=14) => "клипов",
        (1, _) => "клип",
        (2..=4, _) => "клипа",
        _ => "клипов",
    };
    format!("{n} {word}")
}

fn file_name(p: &std::path::Path) -> String {
    p.file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string()
}

const BACKGROUND: egui::Color32 = egui::Color32::from_rgb(15, 20, 29);
const SURFACE: egui::Color32 = egui::Color32::from_rgb(23, 30, 41);
const ACCENT: egui::Color32 = egui::Color32::from_rgb(99, 224, 193);
const MUTED: egui::Color32 = egui::Color32::from_rgb(159, 174, 192);
const WARNING: egui::Color32 = egui::Color32::from_rgb(244, 199, 112);
const DANGER: egui::Color32 = egui::Color32::from_rgb(255, 151, 155);

fn card() -> egui::Frame {
    egui::Frame::new()
        .fill(SURFACE)
        .corner_radius(12)
        .stroke(egui::Stroke::new(1.0, egui::Color32::from_rgb(43, 55, 71)))
        .inner_margin(14)
}

fn section_title(ui: &mut egui::Ui, title: &str) {
    ui.add_space(8.0);
    ui.label(egui::RichText::new(title).color(ACCENT).strong());
    ui.add_space(4.0);
}

fn configure_style(ctx: &egui::Context) {
    ctx.set_theme(egui::Theme::Dark);
    let mut style = (*ctx.style_of(egui::Theme::Dark)).clone();
    style.spacing.item_spacing = egui::vec2(10.0, 8.0);
    style.spacing.button_padding = egui::vec2(12.0, 7.0);
    style.spacing.interact_size.y = 30.0;
    style.spacing.slider_width = 160.0;
    style
        .text_styles
        .insert(egui::TextStyle::Body, egui::FontId::proportional(14.0));
    style
        .text_styles
        .insert(egui::TextStyle::Button, egui::FontId::proportional(14.0));
    style
        .text_styles
        .insert(egui::TextStyle::Heading, egui::FontId::proportional(22.0));
    let visuals = &mut style.visuals;
    visuals.panel_fill = BACKGROUND;
    visuals.window_fill = SURFACE;
    visuals.extreme_bg_color = BACKGROUND;
    visuals.weak_text_color = Some(MUTED);
    visuals.selection.bg_fill = egui::Color32::from_rgb(37, 94, 85);
    visuals.selection.stroke = egui::Stroke::new(1.0, ACCENT);
    visuals.hyperlink_color = ACCENT;
    for widget in [
        &mut visuals.widgets.inactive,
        &mut visuals.widgets.hovered,
        &mut visuals.widgets.active,
    ] {
        widget.corner_radius = egui::CornerRadius::same(7);
        widget.fg_stroke = egui::Stroke::new(1.0, egui::Color32::from_rgb(231, 238, 246));
    }
    visuals.widgets.inactive.bg_fill = egui::Color32::from_rgb(36, 47, 62);
    visuals.widgets.hovered.bg_fill = egui::Color32::from_rgb(49, 66, 82);
    visuals.widgets.active.bg_fill = egui::Color32::from_rgb(37, 94, 85);
    ctx.set_style_of(egui::Theme::Dark, style);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_app(shortcut: crate::shortcuts::ShortcutState, output: PathBuf) -> App {
        let cfg = Config { output, ..Config::default() };
        let (tx, rx) = mpsc::channel();
        App {
            draft: cfg.clone(),
            rec: Arc::new(Recorder::new(cfg, Some(crate::audio::AudioCodec::Aac))),
            service: Arc::new(CaptureService::inert()),
            shortcuts: Arc::new(Shortcuts::inert(shortcut)),
            clips: vec![Clip {
                path: PathBuf::from("/tmp/replay_2026-09-24_08-15-49.mp4"),
                size: 7_500_000,
                modified: std::time::SystemTime::UNIX_EPOCH,
                thumb: None,
            }],
            tx,
            rx,
            toast: None,
            pending_delete: None,
            saving: false,
            thumb_known: HashSet::new(),
            last_scan: Instant::now(),
        }
    }

    /// Прогоняет несколько кадров окна заданного размера и возвращает
    /// ширину панели настроек. Кадров несколько: несжимаемая панель egui
    /// подстраивается под содержимое прошлого кадра и может «расползаться».
    fn settings_width(app: &mut App, ctx: &egui::Context, size: [f32; 2]) -> f32 {
        for _ in 0..8 {
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size.into())),
                ..Default::default()
            };
            let mut out = ctx.run_ui(input, |ui| app.draw(ui));
            out.textures_delta.clear();
        }
        egui::containers::panel::PanelState::load(ctx, egui::Id::new("settings"))
            .map(|s| s.outer_rect.width())
            .unwrap_or(f32::NAN)
    }

    /// Регрессия: несжимаемая панель настроек раздувалась под самую широкую
    /// строку. На живой машине это была ошибка портала «An app id is
    /// required» в ряду хоткея — панель заняла ~900 px, библиотеку сжало.
    #[test]
    fn settings_panel_keeps_its_width() {
        use crate::shortcuts::ShortcutState;
        let tmp = std::env::temp_dir().join("replay-gui");
        let cases = [
            ("норма", ShortcutState { registered: true, ..Default::default() }, tmp.clone()),
            (
                "ошибка портала",
                ShortcutState {
                    error: Some(
                        "не удалось создать сеанс хоткеев: Portal request failed: \
                         org.freedesktop.portal.Error.NotAllowed: An app id is required"
                            .into(),
                    ),
                    ..Default::default()
                },
                tmp.clone(),
            ),
            (
                "длинная привязка",
                ShortcutState {
                    registered: true,
                    trigger: Some("Ctrl+Alt+Shift+Meta+S, Ctrl+Alt+Shift+Meta+R".into()),
                    ..Default::default()
                },
                tmp.clone(),
            ),
            (
                "длинный путь",
                ShortcutState { registered: true, ..Default::default() },
                PathBuf::from("/home/user/Videos/replays/очень/длинный/путь/к/папке/с/клипами"),
            ),
        ];
        for (name, st, out) in cases {
            for size in [[1130.0, 815.0], [760.0, 480.0], [1600.0, 900.0]] {
                let ctx = egui::Context::default();
                let mut app = test_app(st.clone(), out.clone());
                let w = settings_width(&mut app, &ctx, size);
                assert!(
                    w <= SETTINGS_WIDTH + 1.0,
                    "{name}, окно {size:?}: панель настроек {w:.0} px вместо {SETTINGS_WIDTH}"
                );
            }
        }
    }

    /// Регрессия: на узкой панели кнопки карточки вылезали за правый край.
    #[test]
    fn clip_card_fits_a_narrow_panel() {
        let ctx = egui::Context::default();
        let clip = Clip {
            path: PathBuf::from("/tmp/replay_2026-09-24_08-15-49.mp4"),
            size: 7_500_000,
            modified: std::time::SystemTime::UNIX_EPOCH,
            thumb: None,
        };
        // Как на живом скриншоте: панель библиотеки ~220 px, содержимое уже.
        let width = 190.0;
        let mut card = egui::Rect::NOTHING;
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, [width, 700.0].into())),
            ..Default::default()
        };
        let mut out = ctx.run_ui(input, |ui| {
            egui::CentralPanel::default().show(ui, |ui| {
                let mut del = None;
                card = clip_row(ui, &clip, &mut del);
            });
        });
        out.textures_delta.clear();
        assert!(
            card.right() <= width,
            "карточка до x={:.0} при ширине окна {width}",
            card.right()
        );
    }

    #[test]
    fn clips_count_declines() {
        assert_eq!(clips_count(0), "0 клипов");
        assert_eq!(clips_count(1), "1 клип");
        assert_eq!(clips_count(3), "3 клипа");
        assert_eq!(clips_count(5), "5 клипов");
        assert_eq!(clips_count(11), "11 клипов");
        assert_eq!(clips_count(12), "12 клипов");
        assert_eq!(clips_count(21), "21 клип");
        assert_eq!(clips_count(104), "104 клипа");
    }

    /// Регрессия: карточка раздувалась на всю высоту окна, а превью
    /// оказывалось посреди пустоты, далеко под названием клипа.
    #[test]
    fn clip_card_hugs_its_thumbnail() {
        let ctx = egui::Context::default();
        let clip = Clip {
            path: PathBuf::from("/tmp/replay_2026-01-01_00-00-00.mp4"),
            size: 7_500_000,
            modified: std::time::SystemTime::UNIX_EPOCH,
            thumb: None,
        };
        let mut card_rect = egui::Rect::NOTHING;
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, [900.0, 700.0].into())),
            ..Default::default()
        };
        let mut out = ctx.run_ui(input, |ui| {
            egui::CentralPanel::default().show(ui, |ui| {
                let top = ui.cursor().min.y;
                let mut del = None;
                clip_row(ui, &clip, &mut del);
                card_rect = egui::Rect::from_min_max(
                    egui::pos2(0.0, top),
                    egui::pos2(900.0, ui.cursor().min.y),
                );
            });
        });
        out.textures_delta.clear();

        // Превью 90 px плюс поля и отступ — никак не пол-окна.
        assert!(
            card_rect.height() < THUMB_SIZE.y + 60.0,
            "карточка клипа высотой {:.0} px при превью {:.0} px",
            card_rect.height(),
            THUMB_SIZE.y
        );
    }
}
