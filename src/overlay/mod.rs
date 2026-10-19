//! Игровой оверлей: маленькая карточка поверх экрана с состоянием записи
//! и кнопкой сохранения.
//!
//! Оверлей ничего не захватывает сам: он только читает `Recorder` и
//! `CaptureService` и зовёт `Recorder::save` в своём потоке. Если его не
//! удалось показать, запись это никак не затрагивает.
//!
//! Рисуется программно: egui строит треугольники и атлас шрифтов, а
//! `raster` растеризует их в буфер ARGB с premultiplied alpha. На Wayland
//! буфер уходит в `wl_shm` слоя layer-shell, на Windows — в
//! `UpdateLayeredWindow`. Так рядом с игрой не появляется второй
//! GL-контекст, а картинку можно проверить в тесте без дисплея. Карточка
//! ~300×130 и обновляется дважды в секунду — растеризация занимает доли
//! миллисекунды.

pub mod raster;
#[cfg(target_os = "linux")]
mod wayland;
#[cfg(windows)]
mod windows;

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use eframe::egui;

use crate::audio::{AudioSource, SourceStatus};
use crate::config::{OverlayConfig, OverlayCorner, OverlayShow};
use crate::recorder::{Recorder, SaveOutcome};
use crate::service::{CaptureService, CaptureState};

/// Размер карточки в логических пикселях.
pub const FULL_SIZE: (u32, u32) = (300, 128);
pub const COMPACT_SIZE: (u32, u32) = (300, 38);
/// Отступ от края экрана.
pub const MARGIN: i32 = 16;
/// Как часто обновлять видимый оверлей.
pub const TICK: Duration = Duration::from_millis(500);
/// Сколько держать оверлей в режиме «после сохранения».
pub const AFTER_SAVE: Duration = Duration::from_secs(4);

/// Что оверлей показывает. Строки готовые — платформенному коду остаётся
/// только нарисовать.
#[derive(Debug, Clone, PartialEq)]
pub struct Model {
    pub recording: bool,
    pub status: String,
    pub buffer: String,
    pub mic: String,
    pub mic_ok: Option<bool>,
    pub encoder: String,
    pub save_label: String,
    pub saving: bool,
    pub last: Option<(String, bool)>,
}

impl Model {
    pub fn read(rec: &Recorder, service: &CaptureService) -> Self {
        let cfg = rec.config();
        let stats = rec.stats();
        let (recording, status) = match service.state() {
            CaptureState::Running => (true, "Запись идёт".to_owned()),
            CaptureState::WaitingForPortal => (false, "Ждёт выбора экрана".to_owned()),
            CaptureState::Stopped => (false, "Остановлено".to_owned()),
            CaptureState::Failed(_) => (false, "Ошибка записи".to_owned()),
        };
        let (mic, mic_ok) = if !cfg.audio.mic {
            ("микрофон выкл.".to_owned(), None)
        } else {
            match rec.meters.get(AudioSource::Mic).status() {
                SourceStatus::Failed(_) => ("микрофон недоступен".to_owned(), Some(false)),
                SourceStatus::Signal { .. } => ("микрофон: сигнал".to_owned(), Some(true)),
                s => (format!("микрофон: {}", s.label()), Some(true)),
            }
        };
        let encoder = match rec.encoder_status().active {
            Some(s) => format!(
                "{} · {}",
                s.codec.label(),
                if s.hardware {
                    s.backend.label()
                } else {
                    "программно"
                }
            ),
            None => "кодировщик не выбран".to_owned(),
        };
        let last = rec.last_save().map(|r| {
            let at: chrono::DateTime<chrono::Local> = r.at.into();
            match r.result {
                Ok(p) => (
                    format!(
                        "{} сохранён {}",
                        p.file_name().unwrap_or_default().to_string_lossy(),
                        at.format("%H:%M:%S")
                    ),
                    true,
                ),
                Err(e) => (
                    format!("{} не сохранено: {e}", at.format("%H:%M:%S")),
                    false,
                ),
            }
        });
        Self {
            recording,
            status,
            buffer: format!("{:.0} / {:.0} с", stats.seconds(), cfg.seconds),
            mic,
            mic_ok,
            encoder,
            save_label: format!("Сохранить {:.0} с", cfg.save_seconds),
            saving: rec.is_saving(),
            last,
        }
    }
}

const BG: egui::Color32 = egui::Color32::from_rgb(15, 20, 29);
const ACCENT: egui::Color32 = egui::Color32::from_rgb(99, 224, 193);
const MUTED: egui::Color32 = egui::Color32::from_rgb(159, 174, 192);
const DANGER: egui::Color32 = egui::Color32::from_rgb(255, 151, 155);
const TEXT: egui::Color32 = egui::Color32::from_rgb(231, 238, 246);

/// Рисует карточку. Возвращает `true`, если нажали «Сохранить».
pub fn ui(ui: &mut egui::Ui, m: &Model, compact: bool, interactive: bool) -> bool {
    let mut save = false;
    let rect = ui.max_rect();
    ui.painter().rect_filled(rect, 10.0, BG);
    ui.painter().rect_stroke(
        rect.shrink(0.5),
        10.0,
        egui::Stroke::new(1.0, egui::Color32::from_rgb(43, 55, 71)),
        egui::StrokeKind::Inside,
    );
    let inner = rect.shrink2(egui::vec2(12.0, 7.0));
    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(inner));
    let ui = &mut child;
    ui.style_mut().spacing.item_spacing = egui::vec2(8.0, 3.0);
    ui.style_mut().visuals.override_text_color = Some(TEXT);
    let dot = if m.recording { DANGER } else { MUTED };
    let button = |ui: &mut egui::Ui, m: &Model| {
        let label = if m.saving {
            "Сохранение…"
        } else {
            &m.save_label
        };
        ui.add_enabled(
            interactive && !m.saving && m.recording,
            egui::Button::new(egui::RichText::new(label).color(BG).strong()).fill(ACCENT),
        )
        .clicked()
    };
    if compact {
        ui.horizontal_centered(|ui| {
            status_dot(ui, dot);
            ui.label(&m.buffer);
            if let Some(ok) = m.mic_ok {
                ui.colored_label(if ok { ACCENT } else { DANGER }, "мик");
            }
            if interactive {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    save = button(ui, m);
                });
            }
        });
        return save;
    }
    ui.horizontal(|ui| {
        status_dot(ui, dot);
        ui.label(egui::RichText::new(&m.status).strong());
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(&m.buffer);
        });
    });
    ui.horizontal(|ui| {
        let color = match m.mic_ok {
            Some(true) => ACCENT,
            Some(false) => DANGER,
            None => MUTED,
        };
        ui.colored_label(color, &m.mic);
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.colored_label(MUTED, &m.encoder);
        });
    });
    if interactive {
        save = button(ui, m);
    }
    if let Some((text, ok)) = &m.last {
        ui.add(
            egui::Label::new(egui::RichText::new(text).small().color(if *ok {
                MUTED
            } else {
                DANGER
            }))
            .truncate(),
        );
    }
    save
}

/// Точка состояния. Кругом, а не глифом «●»: во встроенных шрифтах egui
/// его нет, и на карточке рисовался пустой квадрат.
fn status_dot(ui: &mut egui::Ui, color: egui::Color32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(10.0, 14.0), egui::Sense::hover());
    ui.painter().circle_filled(rect.center(), 4.0, color);
}

/// Готовый кадр оверлея и то, что из него следует.
pub struct Rendered {
    pub canvas: raster::Canvas,
    pub save_clicked: bool,
}

/// egui-контекст оверлея и его программная отрисовка.
pub struct Renderer {
    ctx: egui::Context,
    painter: raster::SoftPainter,
    events: Vec<egui::Event>,
    started: Instant,
}

impl Default for Renderer {
    fn default() -> Self {
        let ctx = egui::Context::default();
        ctx.set_theme(egui::Theme::Dark);
        Self {
            ctx,
            painter: raster::SoftPainter::default(),
            events: Vec::new(),
            started: Instant::now(),
        }
    }
}

impl Renderer {
    /// Указатель передвинулся или нажал — в логических пикселях.
    pub fn pointer_moved(&mut self, x: f32, y: f32) {
        self.events
            .push(egui::Event::PointerMoved(egui::pos2(x, y)));
    }

    pub fn pointer_button(&mut self, x: f32, y: f32, pressed: bool) {
        self.events.push(egui::Event::PointerButton {
            pos: egui::pos2(x, y),
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        });
    }

    pub fn pointer_left(&mut self) {
        self.events.push(egui::Event::PointerGone);
    }

    /// Рисует карточку размером `size` (логические пиксели) с масштабом `scale`.
    pub fn render(
        &mut self,
        model: &Model,
        cfg: &OverlayConfig,
        size: (u32, u32),
        scale: f32,
    ) -> Rendered {
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(size.0 as f32, size.1 as f32),
            )),
            time: Some(self.started.elapsed().as_secs_f64()),
            events: std::mem::take(&mut self.events),
            ..Default::default()
        };
        self.ctx.set_pixels_per_point(scale);
        let mut save = false;
        let out = self.ctx.run_ui(input, |ui| {
            save = self::ui(ui, model, cfg.compact, cfg.interactive);
        });
        let mut delta = out.textures_delta;
        self.painter.update_textures(&delta);
        // Применили сами — egui проверяет это при удалении.
        delta.clear();
        let prims = self.ctx.tessellate(out.shapes, out.pixels_per_point);
        let w = (size.0 as f32 * scale).round() as usize;
        let h = (size.1 as f32 * scale).round() as usize;
        let mut canvas = raster::Canvas::new(w, h);
        self.painter
            .paint(&mut canvas, &prims, out.pixels_per_point);
        canvas.fade(cfg.opacity.clamp(0.3, 1.0));
        Rendered {
            canvas,
            save_clicked: save,
        }
    }
}

/// Размер карточки под настройки.
pub fn size_for(cfg: &OverlayConfig) -> (u32, u32) {
    if cfg.compact {
        COMPACT_SIZE
    } else {
        FULL_SIZE
    }
}

/// Нужно ли сейчас показывать оверлей.
pub fn should_show(cfg: &OverlayConfig, last_save_at: Option<Instant>) -> bool {
    cfg.enabled
        && match cfg.show {
            OverlayShow::Always => true,
            OverlayShow::AfterSave => last_save_at.is_some_and(|t| t.elapsed() < AFTER_SAVE),
        }
}

/// Сохранение по кнопке — в своём потоке: оверлей не ждёт мьюксинга.
pub fn save_async(rec: &Arc<Recorder>) {
    let rec = rec.clone();
    let spawned = std::thread::Builder::new()
        .name("overlay-save".into())
        .spawn(move || match rec.save(None) {
            Ok(SaveOutcome::Saved(_)) | Ok(SaveOutcome::BufferEmpty) => {}
            Ok(SaveOutcome::AlreadySaving) => log::info!("сохранение уже идёт"),
            Err(e) => log::error!("сохранение из оверлея не удалось: {e:#}"),
        });
    if let Err(e) = spawned {
        log::error!("не удалось запустить сохранение: {e}");
    }
}

/// Состояние оверлея для окна настроек.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Availability {
    /// Ещё не пробовали (оверлей выключен).
    #[default]
    Unknown,
    /// Слой поверх окон (layer-shell / topmost-окно).
    Overlay,
    /// Обычное окно: поверх полноэкранной игры не гарантировано.
    Window(String),
    Unavailable(String),
}

impl Availability {
    pub fn label(&self) -> String {
        match self {
            Availability::Unknown => "выключен".into(),
            Availability::Overlay => "показывается поверх окон".into(),
            Availability::Window(why) => format!("обычным окном: {why}"),
            Availability::Unavailable(why) => format!("недоступен: {why}"),
        }
    }
}

/// Поток оверлея и его состояние.
pub struct Overlay {
    pub availability: Arc<Mutex<Availability>>,
    wake: std::sync::mpsc::Sender<()>,
}

impl Overlay {
    /// Запускает поток оверлея. Он сам следит за настройками: выключенный
    /// оверлей спит до `wake` и не держит соединения с композитором.
    pub fn spawn(rec: Arc<Recorder>, service: Arc<CaptureService>) -> Self {
        let availability = Arc::new(Mutex::new(Availability::Unknown));
        let (tx, rx) = std::sync::mpsc::channel();
        let av = availability.clone();
        let spawned = std::thread::Builder::new()
            .name("overlay".into())
            .spawn(move || run(rec, service, rx, av));
        if let Err(e) = spawned {
            *availability.lock().expect("состояние оверлея отравлено") =
                Availability::Unavailable(format!("поток не запустился: {e}"));
        }
        Self {
            availability,
            wake: tx,
        }
    }

    /// Настройки поменялись — пусть поток перечитает их сразу.
    pub fn wake(&self) {
        let _ = self.wake.send(());
    }

    pub fn availability(&self) -> Availability {
        self.availability
            .lock()
            .expect("состояние оверлея отравлено")
            .clone()
    }
}

/// Главный цикл потока оверлея.
fn run(
    rec: Arc<Recorder>,
    service: Arc<CaptureService>,
    wake: std::sync::mpsc::Receiver<()>,
    availability: Arc<Mutex<Availability>>,
) {
    let set = |a: Availability| *availability.lock().expect("состояние оверлея отравлено") = a;
    loop {
        let cfg = rec.config().overlay;
        if !cfg.enabled {
            set(Availability::Unknown);
            // Выключенный оверлей не опрашивает ничего: ждёт настроек.
            if wake.recv().is_err() {
                return;
            }
            continue;
        }
        let result = backend_run(&rec, &service, &wake, &availability);
        match result {
            // Выключили в настройках или закрыли — снова ждём.
            Ok(()) => continue,
            Err(e) => {
                log::warn!("оверлей недоступен: {e:#}");
                set(Availability::Unavailable(format!("{e:#}")));
                // Повторная попытка — по следующему изменению настроек.
                if wake.recv().is_err() {
                    return;
                }
            }
        }
    }
}

#[cfg(target_os = "linux")]
fn backend_run(
    rec: &Arc<Recorder>,
    service: &Arc<CaptureService>,
    wake: &std::sync::mpsc::Receiver<()>,
    availability: &Arc<Mutex<Availability>>,
) -> anyhow::Result<()> {
    wayland::run(rec, service, wake, availability)
}

#[cfg(windows)]
fn backend_run(
    rec: &Arc<Recorder>,
    service: &Arc<CaptureService>,
    wake: &std::sync::mpsc::Receiver<()>,
    availability: &Arc<Mutex<Availability>>,
) -> anyhow::Result<()> {
    windows::run(rec, service, wake, availability)
}

/// Общая логика тика: что показывать и когда скрываться. Платформенный код
/// держит поверхность и отдаёт события указателя.
pub struct Ticker {
    pub renderer: Renderer,
    seen_saves: u64,
    shown_after_save: Option<Instant>,
    last_model: Option<(Model, OverlayConfig)>,
}

impl Ticker {
    pub fn new(rec: &Recorder) -> Self {
        Self {
            renderer: Renderer::default(),
            seen_saves: rec.save_count(),
            shown_after_save: None,
            last_model: None,
        }
    }

    /// Должна ли поверхность существовать сейчас.
    pub fn visible(&mut self, rec: &Recorder, cfg: &OverlayConfig) -> bool {
        let n = rec.save_count();
        if n != self.seen_saves {
            self.seen_saves = n;
            self.shown_after_save = Some(Instant::now());
        }
        should_show(cfg, self.shown_after_save)
    }

    /// Новый кадр, если что-то изменилось с прошлого (или `force`).
    pub fn frame(
        &mut self,
        rec: &Arc<Recorder>,
        service: &CaptureService,
        cfg: &OverlayConfig,
        scale: f32,
        force: bool,
    ) -> Option<raster::Canvas> {
        let model = Model::read(rec, service);
        let key = (model.clone(), cfg.clone());
        if !force && self.last_model.as_ref() == Some(&key) {
            return None;
        }
        let r = self.renderer.render(&model, cfg, size_for(cfg), scale);
        if r.save_clicked {
            save_async(rec);
        }
        self.last_model = Some(key);
        Some(r.canvas)
    }

    pub fn invalidate(&mut self) {
        self.last_model = None;
    }
}

/// Позиция левого верхнего угла карточки на экране `screen` (логические
/// пиксели) — для платформ, где позицию считаем сами.
pub fn place(corner: OverlayCorner, screen: (i32, i32), size: (u32, u32)) -> (i32, i32) {
    let (w, h) = (size.0 as i32, size.1 as i32);
    match corner {
        OverlayCorner::TopLeft => (MARGIN, MARGIN),
        OverlayCorner::TopRight => (screen.0 - w - MARGIN, MARGIN),
        OverlayCorner::BottomLeft => (MARGIN, screen.1 - h - MARGIN),
        OverlayCorner::BottomRight => (screen.0 - w - MARGIN, screen.1 - h - MARGIN),
    }
}

/// Время последнего сохранения в подписи — для тестов модели.
pub fn format_saved_at(at: SystemTime) -> String {
    let at: chrono::DateTime<chrono::Local> = at.into();
    at.format("%H:%M:%S").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model() -> Model {
        Model {
            recording: true,
            status: "Запись идёт".into(),
            buffer: "28 / 30 с".into(),
            mic: "микрофон: сигнал".into(),
            mic_ok: Some(true),
            encoder: "H.264 · VA-API".into(),
            save_label: "Сохранить 30 с".into(),
            saving: false,
            last: Some(("replay_x.mp4 сохранён 16:05:00".into(), true)),
        }
    }

    /// Карточка рисуется без дисплея: есть непрозрачные пиксели фона и
    /// светлые пиксели текста, углы скруглены (прозрачны).
    #[test]
    fn card_renders_offscreen() {
        let mut r = Renderer::default();
        let cfg = OverlayConfig {
            enabled: true,
            opacity: 1.0,
            ..OverlayConfig::default()
        };
        // Первый кадр egui только строит атлас — рисуем дважды.
        r.render(&model(), &cfg, FULL_SIZE, 1.0);
        let out = r.render(&model(), &cfg, FULL_SIZE, 1.0);
        let c = &out.canvas;
        assert_eq!((c.width, c.height), (300, 128));
        let alpha = |x: usize, y: usize| c.pixels[y * c.width + x] >> 24;
        assert_eq!(alpha(0, 0), 0, "угол не скруглён");
        assert_eq!(alpha(150, 64), 255, "фон не непрозрачен");
        let bright = c
            .pixels
            .iter()
            .filter(|p| ((*p >> 16) & 0xff) > 180 && ((*p >> 8) & 0xff) > 180)
            .count();
        assert!(bright > 200, "текста почти нет: {bright} светлых пикселей");
        if let Ok(dir) = std::env::var("REPLAY_OVERLAY_DUMP") {
            c.save_jpeg(&std::path::Path::new(&dir).join("overlay_full.jpg"))
                .unwrap();
        }
    }

    /// Прозрачность настройки применяется ко всей карточке.
    #[test]
    fn opacity_scales_alpha() {
        let mut r = Renderer::default();
        let cfg = OverlayConfig {
            opacity: 0.5,
            ..OverlayConfig::default()
        };
        r.render(&model(), &cfg, COMPACT_SIZE, 1.0);
        let out = r.render(&model(), &cfg, COMPACT_SIZE, 1.0);
        let a = out.canvas.pixels[19 * out.canvas.width + 150] >> 24;
        assert!((120..=135).contains(&a), "альфа {a}");
    }

    /// Клик по кнопке «Сохранить» распознаётся из событий указателя.
    #[test]
    fn click_on_save_button_is_reported() {
        let mut r = Renderer::default();
        let cfg = OverlayConfig::default();
        r.render(&model(), &cfg, FULL_SIZE, 1.0);
        // Кнопка — третья строка слева.
        let (x, y) = (60.0, 66.0);
        r.pointer_moved(x, y);
        r.render(&model(), &cfg, FULL_SIZE, 1.0);
        r.pointer_button(x, y, true);
        r.render(&model(), &cfg, FULL_SIZE, 1.0);
        r.pointer_button(x, y, false);
        let out = r.render(&model(), &cfg, FULL_SIZE, 1.0);
        assert!(out.save_clicked, "клик по кнопке не дошёл");
        // Без интерактивности кнопки нет — и клика тоже.
        let passive = OverlayConfig {
            interactive: false,
            ..cfg
        };
        r.pointer_button(x, y, true);
        r.render(&model(), &passive, FULL_SIZE, 1.0);
        r.pointer_button(x, y, false);
        assert!(!r.render(&model(), &passive, FULL_SIZE, 1.0).save_clicked);
    }

    #[test]
    fn after_save_mode_hides_itself() {
        let cfg = OverlayConfig {
            enabled: true,
            show: OverlayShow::AfterSave,
            ..OverlayConfig::default()
        };
        assert!(!should_show(&cfg, None));
        assert!(should_show(&cfg, Some(Instant::now())));
        assert!(!should_show(
            &cfg,
            Some(Instant::now() - AFTER_SAVE - Duration::from_millis(10))
        ));
        let off = OverlayConfig {
            enabled: false,
            ..cfg
        };
        assert!(!should_show(&off, Some(Instant::now())));
    }

    #[test]
    fn corners_keep_a_margin() {
        let screen = (1920, 1080);
        assert_eq!(place(OverlayCorner::TopLeft, screen, FULL_SIZE), (16, 16));
        assert_eq!(
            place(OverlayCorner::BottomRight, screen, FULL_SIZE),
            (1920 - 300 - 16, 1080 - 128 - 16)
        );
    }
}
