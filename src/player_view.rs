//! Окно просмотра клипа и редактор диапазона для обрезки.
//!
//! Всё тяжёлое — вне потока окна: открытие плеера (первый кадр может
//! ждать инициализации аппаратного декодера), поиск ключевого кадра и
//! экспорт идут в своих потоках и возвращаются через каналы.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::Arc;
use std::time::Duration;

use eframe::egui;
use gstreamer as gst;

use crate::encoders::EncoderSpec;
use crate::player::{Event, Player};
use crate::trim::{self, Progress, TrimMode, TrimOutcome, TrimRequest};

/// Что окно просмотра просит у приложения.
pub enum ViewAction {
    None,
    Close,
    /// Фрагмент сохранён — обновить библиотеку.
    Exported(TrimOutcome),
    /// Сообщение для строки состояния.
    Notice(String, bool),
}

enum State {
    Loading(Receiver<anyhow::Result<Player>>),
    Ready(Box<Player>),
    Failed(String),
}

/// Идущий экспорт.
struct Export {
    progress: Arc<Progress>,
    rx: Receiver<anyhow::Result<TrimOutcome>>,
    /// Длительность диапазона, с — для полосы прогресса.
    total: f64,
}

pub struct PlayerView {
    pub path: PathBuf,
    state: State,
    texture: Option<egui::TextureHandle>,
    volume: f64,
    /// Длительность клипа, с.
    duration: f64,
    /// Позиция, которую тянут ползунком, — пока его не отпустили.
    scrub: Option<f64>,
    start: f64,
    end: f64,
    mode: TrimMode,
    /// Фактическое начало быстрой обрезки (ключевой кадр) для `start`.
    key_start: Option<(f64, f64)>,
    key_rx: Option<Receiver<(f64, Option<f64>)>>,
    export: Option<Export>,
}

impl PlayerView {
    /// Начинает открывать клип в фоне.
    pub fn open(path: &Path, ctx: &egui::Context) -> Self {
        let (tx, rx) = mpsc::channel();
        let p = path.to_path_buf();
        let repaint = ctx.clone();
        let wake = ctx.clone();
        let spawned = std::thread::Builder::new()
            .name("player-open".into())
            .spawn(move || {
                let r = Player::open(&p, move || repaint.request_repaint());
                let _ = tx.send(r);
                wake.request_repaint();
            });
        let state = match spawned {
            Ok(_) => State::Loading(rx),
            Err(e) => State::Failed(format!("не удалось запустить поток: {e}")),
        };
        Self {
            path: path.to_path_buf(),
            state,
            texture: None,
            volume: 1.0,
            duration: 0.0,
            scrub: None,
            start: 0.0,
            end: 0.0,
            mode: TrimMode::Fast,
            key_start: None,
            key_rx: None,
            export: None,
        }
    }

    pub fn is_exporting(&self) -> bool {
        self.export.is_some()
    }

    /// Рисует окно. `bitrate` и `encoder` — для точной обрезки.
    pub fn ui(
        &mut self,
        ui: &mut egui::Ui,
        bitrate: u32,
        encoder: Option<EncoderSpec>,
    ) -> ViewAction {
        let mut action = ViewAction::None;
        self.advance_loading();
        if let Some(a) = self.poll_export() {
            action = a;
        }
        self.poll_keyframe();

        ui.horizontal(|ui| {
            let close = ui.add_enabled(!self.is_exporting(), egui::Button::new("← Библиотека"));
            if close.clicked() {
                action = ViewAction::Close;
            }
            ui.add(
                egui::Label::new(egui::RichText::new(file_name(&self.path)).strong()).truncate(),
            );
        });
        ui.add_space(8.0);

        let player = match &mut self.state {
            State::Loading(_) => {
                ui.spinner();
                ui.label("Открываю клип…");
                return action;
            }
            State::Failed(e) => {
                ui.colored_label(
                    egui::Color32::from_rgb(255, 151, 155),
                    format!("Не открылся: {e}"),
                );
                return action;
            }
            State::Ready(p) => p,
        };

        for ev in player.poll() {
            if let Event::Error(e) = ev {
                // Например, файл удалили во время просмотра.
                action = ViewAction::Notice(format!("воспроизведение остановлено: {e}"), true);
            }
        }
        if let Some(frame) = player.take_frame() {
            let image =
                egui::ColorImage::from_rgba_unmultiplied([frame.width, frame.height], &frame.rgba);
            match &mut self.texture {
                Some(t) => t.set(image, egui::TextureOptions::LINEAR),
                None => {
                    self.texture = Some(ui.ctx().load_texture(
                        "player-frame",
                        image,
                        egui::TextureOptions::LINEAR,
                    ))
                }
            }
        }
        if let Some(d) = player.duration() {
            self.duration = secs(d);
            if self.end <= 0.0 {
                self.end = self.duration;
            }
        }

        // Видео — по ширине, но не выше половины окна.
        if let Some(t) = &self.texture {
            let size = t.size_vec2();
            let max = egui::vec2(
                ui.available_width(),
                (ui.available_height() * 0.55).max(120.0),
            );
            let scale = (max.x / size.x).min(max.y / size.y);
            ui.vertical_centered(|ui| {
                ui.image((t.id(), size * scale));
            });
        }
        ui.add_space(6.0);

        let pos = player.position().map(secs).unwrap_or(0.0);
        ui.horizontal(|ui| {
            let label = if player.is_playing() {
                "Пауза"
            } else {
                "Играть"
            };
            if ui.button(label).clicked() {
                if player.is_playing() {
                    player.pause();
                } else {
                    player.play();
                }
            }
            ui.label(format!("{} / {}", fmt_time(pos), fmt_time(self.duration)));
        });

        // Перемотка: пока тянут — к ключевым кадрам (быстро), отпустили —
        // точно в кадр.
        let mut shown = self.scrub.unwrap_or(pos);
        let resp = ui.add(
            egui::Slider::new(&mut shown, 0.0..=self.duration.max(0.01))
                .show_value(false)
                .trailing_fill(true),
        );
        if resp.dragged() {
            self.scrub = Some(shown);
            player.seek(clock(shown), false);
        }
        if resp.drag_stopped() || (resp.changed() && !resp.dragged()) {
            player.seek(clock(shown), true);
            self.scrub = None;
        }

        ui.horizontal_wrapped(|ui| {
            ui.label("Громкость");
            if ui
                .add(egui::Slider::new(&mut self.volume, 0.0..=1.5).show_value(false))
                .changed()
            {
                player.set_volume(self.volume);
            }
            let tracks = player.audio_tracks();
            if tracks > 1 {
                ui.separator();
                ui.label("Дорожка");
                let mut sel = player.selected_audio();
                egui::ComboBox::from_id_salt("player_track")
                    .selected_text(track_label(sel, tracks))
                    .show_ui(ui, |ui| {
                        for i in 0..tracks {
                            ui.selectable_value(&mut sel, i, track_label(i, tracks));
                        }
                    });
                if sel != player.selected_audio() {
                    player.set_audio(sel);
                }
            }
        });
        if player.audio_tracks() == 2 {
            ui.label(
                egui::RichText::new(
                    "В раздельном режиме replay-rs дорожка 1 — системный звук, 2 — микрофон.",
                )
                .small()
                .weak(),
            );
        }

        ui.add_space(10.0);
        ui.separator();
        if let Some(a) = self.trim_editor(ui, pos, bitrate, encoder) {
            action = a;
        }
        if self.export.is_some() || self.key_rx.is_some() {
            ui.ctx().request_repaint_after(Duration::from_millis(100));
        } else if matches!(&self.state, State::Ready(p) if p.is_playing()) {
            // Позиция и ползунок движутся и между кадрами.
            ui.ctx().request_repaint_after(Duration::from_millis(250));
        }
        action
    }

    fn trim_editor(
        &mut self,
        ui: &mut egui::Ui,
        pos: f64,
        bitrate: u32,
        encoder: Option<EncoderSpec>,
    ) -> Option<ViewAction> {
        if !matches!(self.state, State::Ready(_)) {
            return None;
        }
        ui.label(egui::RichText::new("Обрезка").strong());
        let dur = self.duration.max(0.01);
        let busy = self.export.is_some();
        let mut start_changed = false;
        ui.add_enabled_ui(!busy, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label("Начало");
                start_changed |= ui
                    .add(egui::Slider::new(&mut self.start, 0.0..=dur).custom_formatter(|v, _| fmt_time(v)))
                    .changed();
                if ui.small_button("= позиция").clicked() {
                    self.start = pos;
                    start_changed = true;
                }
            });
            ui.horizontal_wrapped(|ui| {
                ui.label("Конец");
                ui.add(egui::Slider::new(&mut self.end, 0.0..=dur).custom_formatter(|v, _| fmt_time(v)));
                if ui.small_button("= позиция").clicked() {
                    self.end = pos;
                }
            });
            if self.end < self.start {
                self.end = self.start;
            }
            ui.horizontal_wrapped(|ui| {
                ui.radio_value(&mut self.mode, TrimMode::Fast, "Быстро")
                    .on_hover_text("Без перекодирования: мгновенно и без потери качества, но начало сдвигается к ключевому кадру");
                ui.radio_value(&mut self.mode, TrimMode::Precise, "Точно")
                    .on_hover_text("С перекодированием видео и звука: начало точно в кадр, дольше и с небольшой потерей качества");
            });
        });
        if start_changed {
            self.key_start = None;
        }
        if self.mode == TrimMode::Fast && self.key_start.is_none() && self.key_rx.is_none() {
            self.request_keyframe();
        }

        let selected = (self.end - self.start).max(0.0);
        let actual_start = match self.mode {
            TrimMode::Fast => self
                .key_start
                .filter(|(req, _)| (*req - self.start).abs() < 1e-6)
                .map(|(_, k)| k),
            TrimMode::Precise => Some(self.start),
        };
        let text = match actual_start {
            Some(k) if (k - self.start).abs() > 0.001 => format!(
                "Выбрано {:.1} с. Фактически: {} – {} ({:.1} с) — начало сдвинуто к ключевому кадру",
                selected,
                fmt_time(k),
                fmt_time(self.end),
                self.end - k
            ),
            Some(_) => format!("Выбрано {selected:.1} с: {} – {}", fmt_time(self.start), fmt_time(self.end)),
            None => format!("Выбрано {selected:.1} с, ищу ключевой кадр…"),
        };
        ui.add(egui::Label::new(egui::RichText::new(text).small()).wrap());

        let mut action = None;
        let mut preview = None;
        ui.horizontal_wrapped(|ui| {
            if ui
                .add_enabled(selected > 0.05 && !busy, egui::Button::new("Предпросмотр"))
                .on_hover_text("Проиграть выбранный фрагмент")
                .clicked()
            {
                preview = Some((actual_start.unwrap_or(self.start), self.end));
            }
            if busy {
                if ui.button("Отменить сохранение").clicked() {
                    if let Some(e) = &self.export {
                        e.progress
                            .cancel
                            .store(true, std::sync::atomic::Ordering::Release);
                    }
                }
            } else if ui
                .add_enabled(selected > 0.05, egui::Button::new("Сохранить фрагмент"))
                .on_hover_text("Новый файл рядом с оригиналом; оригинал не меняется")
                .clicked()
            {
                action = Some(self.start_export(bitrate, encoder.clone()));
            }
        });
        if let (Some((from, to)), State::Ready(player)) = (preview, &mut self.state) {
            player.play_range(clock(from), clock(to));
        }
        if let Some(e) = &self.export {
            let done = e
                .progress
                .done_ns
                .load(std::sync::atomic::Ordering::Relaxed) as f64
                / 1e9;
            ui.add(egui::ProgressBar::new((done / e.total.max(0.01)) as f32).show_percentage());
        }
        action
    }

    fn start_export(&mut self, bitrate: u32, encoder: Option<EncoderSpec>) -> ViewAction {
        let (start, end) = (clock(self.start), clock(self.end));
        let req = TrimRequest {
            input: self.path.clone(),
            output: trim::output_path(&self.path, start, end),
            start,
            end,
            mode: self.mode,
            bitrate_kbps: bitrate,
            encoder,
        };
        let progress = Arc::new(Progress::default());
        let (tx, rx) = mpsc::channel();
        let p = progress.clone();
        let spawned = std::thread::Builder::new()
            .name("export".into())
            .spawn(move || {
                let _ = tx.send(trim::trim(&req, &p));
            });
        match spawned {
            Ok(_) => {
                self.export = Some(Export {
                    progress,
                    rx,
                    total: self.end - self.start,
                });
                ViewAction::None
            }
            Err(e) => ViewAction::Notice(format!("экспорт не запустился: {e}"), true),
        }
    }

    fn poll_export(&mut self) -> Option<ViewAction> {
        let e = self.export.as_ref()?;
        let result = match e.rx.try_recv() {
            Ok(r) => r,
            Err(TryRecvError::Empty) => return None,
            Err(TryRecvError::Disconnected) => Err(anyhow::anyhow!("поток экспорта упал")),
        };
        self.export = None;
        Some(match result {
            Ok(out) => ViewAction::Exported(out),
            Err(e) if format!("{e:#}").contains("отменено") => {
                ViewAction::Notice("сохранение фрагмента отменено".into(), false)
            }
            Err(e) => ViewAction::Notice(format!("фрагмент не сохранился: {e:#}"), true),
        })
    }

    fn request_keyframe(&mut self) {
        let (tx, rx) = mpsc::channel();
        let path = self.path.clone();
        let t = self.start;
        let spawned = std::thread::Builder::new()
            .name("keyframe".into())
            .spawn(move || {
                let k = crate::media::keyframe_at_or_before(&path, clock(t))
                    .ok()
                    .map(secs);
                let _ = tx.send((t, k));
            });
        if spawned.is_ok() {
            self.key_rx = Some(rx);
        }
    }

    fn poll_keyframe(&mut self) {
        let Some(rx) = &self.key_rx else {
            return;
        };
        match rx.try_recv() {
            Ok((req, k)) => {
                self.key_rx = None;
                // Пока искали, начало могли передвинуть — тогда спросим снова.
                if (req - self.start).abs() < 1e-6 {
                    self.key_start = Some((req, k.unwrap_or(req)));
                }
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => self.key_rx = None,
        }
    }

    fn advance_loading(&mut self) {
        let State::Loading(rx) = &self.state else {
            return;
        };
        match rx.try_recv() {
            Ok(Ok(p)) => self.state = State::Ready(Box::new(p)),
            Ok(Err(e)) => self.state = State::Failed(format!("{e:#}")),
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                self.state = State::Failed("поток открытия упал".into())
            }
        }
    }
}

impl Drop for PlayerView {
    fn drop(&mut self) {
        // Закрытое окно не оставляет фоновую работу: экспорт отменяется,
        // недописанный файл убирает сам экспорт.
        if let Some(e) = &self.export {
            e.progress
                .cancel
                .store(true, std::sync::atomic::Ordering::Release);
        }
    }
}

fn track_label(i: i32, tracks: i32) -> String {
    match (tracks, i) {
        (2, 0) => "1 — системный звук".into(),
        (2, 1) => "2 — микрофон".into(),
        _ => format!("{}", i + 1),
    }
}

fn secs(t: gst::ClockTime) -> f64 {
    t.nseconds() as f64 / 1e9
}

fn clock(s: f64) -> gst::ClockTime {
    gst::ClockTime::from_nseconds((s.max(0.0) * 1e9) as u64)
}

/// «1:05.3».
pub fn fmt_time(s: f64) -> String {
    let s = s.max(0.0);
    let m = (s / 60.0).floor();
    format!("{}:{:04.1}", m as u64, s - m * 60.0)
}

fn file_name(p: &Path) -> String {
    p.file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn time_is_formatted_for_people() {
        assert_eq!(fmt_time(0.0), "0:00.0");
        assert_eq!(fmt_time(65.34), "1:05.3");
        assert_eq!(fmt_time(-1.0), "0:00.0");
    }
}
