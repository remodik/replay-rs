//! Оверлей на Wayland: поверхность слоя `overlay` через wlr-layer-shell.
//!
//! KDE Plasma (KWin), Hyprland, Sway и другие композиторы на wlroots
//! поддерживают layer-shell, и поверхность слоя `overlay` рисуется поверх
//! обычных и полноэкранных окон. Гарантией это не является: в gamescope
//! (Steam Deck, игровой режим) и GNOME layer-shell нет, а KWin, пока оверлей
//! перекрывает полноэкранную игру, отключает для неё прямой вывод (direct
//! scanout) и композитит кадр — поэтому режим «после сохранения» прячет
//! оверлей через несколько секунд. Без layer-shell оверлей показывается
//! обычным небольшим окном, о чём честно говорит окно настроек.
//!
//! Клавиатуру оверлей не берёт никогда (`KeyboardInteractivity::None`),
//! а в режиме «клики насквозь» у поверхности пустая область ввода.

use std::os::fd::AsRawFd;
use std::sync::mpsc::{Receiver, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use sctk::compositor::{CompositorHandler, CompositorState, Region};
use sctk::output::{OutputHandler, OutputState};
use sctk::reexports::client::globals::registry_queue_init;
use sctk::reexports::client::protocol::{wl_output, wl_pointer, wl_seat, wl_shm, wl_surface};
use sctk::reexports::client::{Connection, EventQueue, QueueHandle};
use sctk::registry::{ProvidesRegistryState, RegistryState};
use sctk::seat::pointer::{PointerEvent, PointerEventKind, PointerHandler};
use sctk::seat::{Capability, SeatHandler, SeatState};
use sctk::shell::wlr_layer::{
    Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
    LayerSurfaceConfigure,
};
use sctk::shell::xdg::window::{Window, WindowConfigure, WindowDecorations, WindowHandler};
use sctk::shell::xdg::XdgShell;
use sctk::shell::WaylandSurface;
use sctk::shm::slot::SlotPool;
use sctk::shm::{Shm, ShmHandler};
use sctk::{
    delegate_compositor, delegate_layer, delegate_output, delegate_pointer, delegate_registry,
    delegate_seat, delegate_shm, delegate_xdg_shell, delegate_xdg_window, registry_handlers,
};
use smithay_client_toolkit as sctk;

use super::{size_for, Availability, Ticker, MARGIN, TICK};
use crate::config::{OverlayConfig, OverlayCorner};
use crate::recorder::Recorder;
use crate::service::CaptureService;

/// Поверхность, на которой рисуем.
enum Surface {
    Layer(LayerSurface),
    /// Запасной вариант без layer-shell: обычное окно.
    Window(Window),
}

impl Surface {
    fn wl(&self) -> &wl_surface::WlSurface {
        match self {
            Surface::Layer(l) => l.wl_surface(),
            Surface::Window(w) => w.wl_surface(),
        }
    }
}

struct State {
    registry: RegistryState,
    seat: SeatState,
    output: OutputState,
    shm: Shm,
    compositor: CompositorState,
    layer_shell: Option<LayerShell>,
    xdg: Option<XdgShell>,
    pool: Option<SlotPool>,
    pointer: Option<wl_pointer::WlPointer>,
    surface: Option<Surface>,
    /// Настройки, под которые создана поверхность.
    surface_cfg: Option<OverlayConfig>,
    configured: bool,
    closed: bool,
    scale: i32,
    /// Нужно перерисовать немедленно (ввод, configure, масштаб).
    dirty: bool,
    ticker: Ticker,
}

/// Крутит оверлей, пока он включён. `Ok` — выключили; `Err` — не вышло.
pub fn run(
    rec: &Arc<Recorder>,
    service: &Arc<CaptureService>,
    wake: &Receiver<()>,
    availability: &Arc<Mutex<Availability>>,
) -> Result<()> {
    let conn = Connection::connect_to_env().context("нет Wayland-сессии")?;
    let (globals, mut queue) = registry_queue_init(&conn).context("реестр Wayland")?;
    let qh: QueueHandle<State> = queue.handle();
    let compositor = CompositorState::bind(&globals, &qh).context("нет wl_compositor")?;
    let shm = Shm::bind(&globals, &qh).context("нет wl_shm")?;
    let layer_shell = LayerShell::bind(&globals, &qh).ok();
    let xdg = if layer_shell.is_none() {
        Some(XdgShell::bind(&globals, &qh).context("нет ни layer-shell, ни xdg-shell")?)
    } else {
        None
    };
    *availability.lock().expect("состояние оверлея отравлено") = if layer_shell.is_some() {
        Availability::Overlay
    } else {
        Availability::Window(
            "композитор без layer-shell (GNOME, gamescope) — поверх игры не гарантировано".into(),
        )
    };
    let mut state = State {
        registry: RegistryState::new(&globals),
        seat: SeatState::new(&globals, &qh),
        output: OutputState::new(&globals, &qh),
        shm,
        compositor,
        layer_shell,
        xdg,
        pool: None,
        pointer: None,
        surface: None,
        surface_cfg: None,
        configured: false,
        closed: false,
        scale: 1,
        dirty: true,
        ticker: Ticker::new(rec),
    };
    // Первый обход: места вывода, устройства ввода.
    queue.roundtrip(&mut state).context("Wayland не ответил")?;

    loop {
        match wake.try_recv() {
            Ok(()) => state.ticker.invalidate(),
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => return Ok(()),
        }
        let cfg = rec.config().overlay;
        if !cfg.enabled {
            return Ok(());
        }
        let visible = state.ticker.visible(rec, &cfg) && !state.closed;
        if !visible {
            state.destroy_surface();
        } else if state
            .surface_cfg
            .as_ref()
            .is_none_or(|c| needs_new_surface(c, &cfg))
        {
            state.destroy_surface();
            state.create_surface(&qh, &cfg)?;
        }
        if state.surface.is_some() && state.configured {
            let force = std::mem::take(&mut state.dirty);
            if let Some(canvas) = state
                .ticker
                .frame(rec, service, &cfg, state.scale as f32, force)
            {
                state.present(&canvas)?;
            }
        }
        let timeout = if state.dirty { Duration::ZERO } else { TICK };
        wait_events(&conn, &mut queue, &mut state, timeout)?;
    }
}

/// Пересоздать поверхность, если поменялось то, что задаётся при создании.
fn needs_new_surface(old: &OverlayConfig, new: &OverlayConfig) -> bool {
    old.corner != new.corner || old.compact != new.compact || old.interactive != new.interactive
}

/// Ждёт событий композитора не дольше `timeout` и разбирает их.
fn wait_events(
    conn: &Connection,
    queue: &mut EventQueue<State>,
    state: &mut State,
    timeout: Duration,
) -> Result<()> {
    queue.flush().context("Wayland: отправка")?;
    if let Some(guard) = queue.prepare_read() {
        let fd = guard.connection_fd().as_raw_fd();
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: один валидный pollfd на время вызова.
        let n = unsafe { libc::poll(&mut pfd, 1, timeout.as_millis() as i32) };
        if n > 0 && (pfd.revents & libc::POLLIN) != 0 {
            // Ошибку чтения «нет данных» (EAGAIN) wayland-client прячет сам.
            guard.read().context("Wayland: чтение")?;
        }
    }
    let _ = conn;
    queue
        .dispatch_pending(state)
        .context("Wayland: разбор событий")?;
    Ok(())
}

impl State {
    fn create_surface(&mut self, qh: &QueueHandle<Self>, cfg: &OverlayConfig) -> Result<()> {
        let (w, h) = size_for(cfg);
        let surface = self.compositor.create_surface(qh);
        surface.set_buffer_scale(self.scale);
        if !cfg.interactive {
            // Пустая область ввода: клики проходят в игру насквозь.
            let region = Region::new(&self.compositor).context("wl_region")?;
            surface.set_input_region(Some(region.wl_region()));
        }
        let made = if let Some(shell) = &self.layer_shell {
            let layer = shell.create_layer_surface(
                qh,
                surface,
                Layer::Overlay,
                Some("replay-rs-overlay"),
                None,
            );
            let anchor = match cfg.corner {
                OverlayCorner::TopLeft => Anchor::TOP | Anchor::LEFT,
                OverlayCorner::TopRight => Anchor::TOP | Anchor::RIGHT,
                OverlayCorner::BottomLeft => Anchor::BOTTOM | Anchor::LEFT,
                OverlayCorner::BottomRight => Anchor::BOTTOM | Anchor::RIGHT,
            };
            layer.set_anchor(anchor);
            layer.set_margin(MARGIN, MARGIN, MARGIN, MARGIN);
            layer.set_size(w, h);
            layer.set_keyboard_interactivity(KeyboardInteractivity::None);
            layer.commit();
            Surface::Layer(layer)
        } else {
            let xdg = self.xdg.as_ref().context("нет xdg-shell")?;
            let window = xdg.create_window(surface, WindowDecorations::RequestServer, qh);
            window.set_title("replay-rs");
            window.set_app_id("replay-rs-overlay");
            window.set_min_size(Some((w, h)));
            window.set_max_size(Some((w, h)));
            window.commit();
            Surface::Window(window)
        };
        self.surface = Some(made);
        self.surface_cfg = Some(cfg.clone());
        self.configured = false;
        self.dirty = true;
        Ok(())
    }

    fn destroy_surface(&mut self) {
        if self.surface.take().is_some() {
            self.surface_cfg = None;
            self.configured = false;
            self.ticker.invalidate();
        }
    }

    fn present(&mut self, canvas: &super::raster::Canvas) -> Result<()> {
        let Some(surface) = &self.surface else {
            return Ok(());
        };
        let (w, h) = (canvas.width as i32, canvas.height as i32);
        let bytes = (w * h * 4) as usize;
        let pool = match &mut self.pool {
            Some(p) => p,
            None => self
                .pool
                .insert(SlotPool::new(bytes, &self.shm).context("shm-пул")?),
        };
        let (buffer, data) = pool
            .create_buffer(w, h, w * 4, wl_shm::Format::Argb8888)
            .context("shm-буфер")?;
        data[..bytes].copy_from_slice(&canvas.to_bytes());
        let wl = surface.wl();
        wl.set_buffer_scale(self.scale);
        wl.damage_buffer(0, 0, w, h);
        buffer.attach_to(wl).context("прикрепить буфер")?;
        wl.commit();
        Ok(())
    }

    fn surface_is(&self, s: &wl_surface::WlSurface) -> bool {
        self.surface.as_ref().is_some_and(|x| x.wl() == s)
    }
}

impl CompositorHandler for State {
    fn scale_factor_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        surface: &wl_surface::WlSurface,
        new_factor: i32,
    ) {
        if self.surface_is(surface) && new_factor != self.scale {
            self.scale = new_factor.max(1);
            self.dirty = true;
        }
    }
    fn transform_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: wl_output::Transform,
    ) {
    }
    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: u32) {}
    fn surface_enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
    fn surface_leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
}

impl OutputHandler for State {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output
    }
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}

impl LayerShellHandler for State {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &LayerSurface) {
        // Композитор убрал слой (например, выключили монитор) — создадим заново.
        self.destroy_surface();
    }
    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &LayerSurface,
        _: LayerSurfaceConfigure,
        _: u32,
    ) {
        self.configured = true;
        self.dirty = true;
    }
}

impl WindowHandler for State {
    fn request_close(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &Window) {
        // Пользователь закрыл окно — не навязываем его снова до перезапуска
        // оверлея (выключить и включить в настройках).
        self.closed = true;
        self.destroy_surface();
    }
    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &Window,
        _: WindowConfigure,
        _: u32,
    ) {
        self.configured = true;
        self.dirty = true;
    }
}

impl SeatHandler for State {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat
    }
    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
    fn new_capability(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Pointer && self.pointer.is_none() {
            self.pointer = self.seat.get_pointer(qh, &seat).ok();
        }
    }
    fn remove_capability(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Pointer {
            if let Some(p) = self.pointer.take() {
                p.release();
            }
        }
    }
    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
}

impl PointerHandler for State {
    fn pointer_frame(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_pointer::WlPointer,
        events: &[PointerEvent],
    ) {
        for e in events {
            if !self.surface_is(&e.surface) {
                continue;
            }
            let (x, y) = (e.position.0 as f32, e.position.1 as f32);
            let r = &mut self.ticker.renderer;
            match e.kind {
                PointerEventKind::Enter { .. } | PointerEventKind::Motion { .. } => {
                    r.pointer_moved(x, y)
                }
                PointerEventKind::Leave { .. } => r.pointer_left(),
                // 0x110 — BTN_LEFT.
                PointerEventKind::Press { button: 0x110, .. } => r.pointer_button(x, y, true),
                PointerEventKind::Release { button: 0x110, .. } => r.pointer_button(x, y, false),
                _ => continue,
            }
            self.dirty = true;
        }
    }
}

impl ShmHandler for State {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl ProvidesRegistryState for State {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry
    }
    registry_handlers![OutputState, SeatState];
}

delegate_compositor!(State);
delegate_output!(State);
delegate_shm!(State);
delegate_seat!(State);
delegate_pointer!(State);
delegate_layer!(State);
delegate_xdg_shell!(State);
delegate_xdg_window!(State);
delegate_registry!(State);
