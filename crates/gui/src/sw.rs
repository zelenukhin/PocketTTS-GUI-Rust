//! G6.8: программный CPU-рантайм GUI (EXE `PocketTTS-GUI-Rust-cpu`).
//!
//! Отрисовка — полностью на CPU: `egui_software_backend::EguiSoftwareRender`
//! растеризует тесселлированные примитивы egui в пиксельный буфер, вывод в
//! окно — `softbuffer` (на Windows — GDI). Ни OpenGL/WGL, ни D3D, ни GPU
//! не инициализируются; eframe в этом бинарнике отсутствует (feature `sw`
//! без `glow`), так что GL-путь невозможен и как fallback.
//!
//! Почему собственный цикл, а не библиотечный рантайм
//! `egui_software_backend::run_app_with_software_backend`: тот при
//! `WindowEvent::CloseRequested` немедленно вызывает `elwt.exit()`, не дав
//! приложению отменить закрытие. Для PocketTTS это неприемлемо: машина
//! закрытия G6.2.3 (`CloseFlow`) обязана сначала отменить задание, дождаться
//! фонового потока, автосохранить настройки и (при ошибке) показать диалог
//! «Повторить / Выйти без сохранения». Поэтому здесь реализован цикл
//! событий, повторяющий протокол закрытия eframe 0.34 (см.
//! `eframe::native::epi_integration::update` и `glow_integration`):
//!
//! 1. `WindowEvent::CloseRequested` НЕ закрывает окно: в viewport-инфо
//!    кладётся событие `egui::ViewportEvent::Close` и запрашивается кадр.
//! 2. Кадр: `close_requested` виден приложению через
//!    `ctx.input(|i| i.viewport().close_requested())`; если приложение
//!    ответило `ViewportCommand::CancelClose` — окно остаётся (CloseFlow
//!    Idle/WaitingForJob/SaveFailed), иначе принимается решение закрыться.
//! 3. `ViewportCommand::Close` (завершение CloseFlow::Closing) кладёт
//!    `ViewportEvent::Close` в viewport-инфо следующего кадра — на нём
//!    приложение уже не отменяет закрытие, цикл завершается, вызывается
//!    страховочное автосохранение `ShtormApp::on_exit_save` (no-op после
//!    штатного потока).
//!
//! Пробуждение GUI из фоновых потоков синтеза (`ctx.request_repaint()` /
//! `request_repaint_after` из `emit` в потоке задания) работает через
//! `EventLoopProxy`: колбэк `Context::set_request_repaint_callback`
//! отправляет пользовательское событие с дедупликацией по
//! `cumulative_pass_nr` — алгоритм eframe, без холостой перерисовки на
//! максимальной частоте.

use std::num::NonZeroU32;
use std::rc::Rc;
use std::time::Instant;

use egui_software_backend::{BufferMutRef, ColorFieldOrder, EguiSoftwareRender};
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::window::Window;

use crate::app::ShtormApp;
use crate::entry;

/// Пользовательское событие цикла: пробуждение для перерисовки (из
/// `Context::set_request_repaint_callback`, в т.ч. из фоновых потоков).
#[derive(Debug)]
enum UserEvent {
    RequestRepaint {
        viewport_id: egui::ViewportId,
        when: Instant,
        /// Счётчик проходов на момент запроса (дедупликация eframe).
        cumulative_pass_nr: u64,
    },
}

/// Решение о закрытии по протоколу eframe: окно закрывается, только если
/// кадр ВИДЕЛ close_requested и приложение НЕ ответило CancelClose.
/// (Чистая функция — покрывается юнит-тестами без окна.)
fn close_decision(close_requested: bool, commands: &[egui::ViewportCommand]) -> bool {
    close_requested && !commands.contains(&egui::ViewportCommand::CancelClose)
}

/// Запуск GUI в программном рантайме. Возвращает управление при штатном
/// закрытии окна (после CloseFlow) или ошибке инициализации.
pub fn run() -> anyhow::Result<()> {
    let event_loop: EventLoop<UserEvent> = EventLoop::with_user_event()
        .build()
        .map_err(|e| anyhow::anyhow!("winit: {e}"))?;
    // softbuffer требует владелец display handle цикла
    let softbuffer_context = softbuffer::Context::new(event_loop.owned_display_handle())
        .map_err(|e| anyhow::anyhow!("softbuffer::Context: {e}"))?;

    let egui_ctx = egui::Context::default();

    // Пробуждение из фоновых потоков: ctx.request_repaint[_after] (вызывается
    // и из потока синтеза через emit) → EventLoopProxy → кадр.
    let proxy: EventLoopProxy<UserEvent> = event_loop.create_proxy();
    egui_ctx.set_request_repaint_callback(move |info| {
        let when = Instant::now() + info.delay;
        let cumulative_pass_nr = info.current_cumulative_pass_nr;
        let _ = proxy.send_event(UserEvent::RequestRepaint {
            viewport_id: info.viewport_id,
            when,
            cumulative_pass_nr,
        });
    });

    let mut app = SwGui::new(egui_ctx, softbuffer_context);
    event_loop
        .run_app(&mut app)
        .map_err(|e| anyhow::anyhow!("winit event loop: {e}"))?;

    // Страховка eframe::App::on_exit: после штатного CloseFlow — no-op,
    // при аварийном завершении цикла (close_flow остался Idle) — лучшая
    // попытка автосохранения. На Drop не полагаемся (G6.2.3).
    if let Some(mut running) = app.state.take() {
        running.app.on_exit_save();
    }
    Ok(())
}

struct SwGui {
    egui_ctx: egui::Context,
    softbuffer_context: softbuffer::Context<winit::event_loop::OwnedDisplayHandle>,
    state: Option<Running>,
    /// Запланированная перерисовка (модель eframe `windows_next_repaint_times`
    /// для единственного окна). None — перерисовок не запланировано.
    next_repaint: Option<std::time::Instant>,
}

struct Running {
    window: Rc<Window>,
    surface: softbuffer::Surface<winit::event_loop::OwnedDisplayHandle, Rc<Window>>,
    egui_winit: egui_winit::State,
    renderer: EguiSoftwareRender,
    app: ShtormApp,
    /// Viewport-инфо ROOT: сюда попадают события Close (от крестика и от
    /// команды Close) и отсюда читаются кадром; после прохода очищаются.
    viewport_info: egui::ViewportInfo,
    /// Отложенные эффекты команд (например, StartDrag) — обрабатываются
    /// winit-ом, нам содержимое не нужно.
    actions_requested: Vec<egui_winit::ActionRequested>,
    /// Решение закрыться принято (eframe `should_close`).
    close: bool,
    /// Счётчик кадров (диагностика холостых перерисовок).
    frame_nr: u64,
}

impl SwGui {
    fn new(
        egui_ctx: egui::Context,
        softbuffer_context: softbuffer::Context<winit::event_loop::OwnedDisplayHandle>,
    ) -> Self {
        Self {
            egui_ctx,
            softbuffer_context,
            state: None,
            next_repaint: None,
        }
    }
}

impl ApplicationHandler<UserEvent> for SwGui {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        if self.state.is_some() {
            return; // уже запущено (повторный resumed на Windows не приходит)
        }
        match self.start(el) {
            Ok(()) => {}
            Err(e) => {
                eprintln!("программный рендер: инициализация не удалась: {e:#}");
                el.exit();
            }
        }
    }

    fn window_event(
        &mut self,
        el: &ActiveEventLoop,
        window_id: winit::window::WindowId,
        event: WindowEvent,
    ) {
        let Some(state) = &mut self.state else {
            return;
        };
        if window_id != state.window.id() {
            return;
        }
        match event {
            // G6.2.3/G6.8: закрытие решает CloseFlow, а не ОС. Окно НЕ
            // уничтожаем: приложение увидит close_requested в следующем
            // кадре и ответит CancelClose (пока идёт отмена задания/диалог
            // сохранения) либо разрешит закрытие.
            WindowEvent::CloseRequested => {
                if state.close {
                    // решение уже принято приложением — закрываемся сразу
                    // (eframe: should_close && CloseRequested)
                    el.exit();
                } else {
                    state.viewport_info.events.push(egui::ViewportEvent::Close);
                    // может понадобиться два кадра (заметить close-событие и
                    // закрепить решение) — как eframe, двойной перерисовкой
                    self.egui_ctx.request_repaint();
                    self.egui_ctx.request_repaint();
                }
            }
            WindowEvent::RedrawRequested => {
                if let Err(e) = state.frame(&self.egui_ctx, el) {
                    eprintln!("программный рендер: кадр не удался: {e:#}");
                    el.exit();
                }
            }
            other => {
                let response = state.egui_winit.on_window_event(&state.window, &other);
                if response.repaint {
                    state.window.request_redraw();
                }
            }
        }
    }

    fn user_event(&mut self, el: &ActiveEventLoop, event: UserEvent) {
        let UserEvent::RequestRepaint {
            viewport_id,
            when,
            cumulative_pass_nr,
        } = event;
        if self.state.is_none() {
            return;
        }
        // Алгоритм eframe: перерисовка нужна, только если после запроса
        // ещё не было прохода (или был ровно один — запрос изнутри кадра);
        // устаревший запрос игнорируется (уже перерисовано).
        let current_pass_nr = self.egui_ctx.cumulative_pass_nr_for(viewport_id);
        if current_pass_nr == cumulative_pass_nr || current_pass_nr == cumulative_pass_nr + 1 {
            // ближайшее из записанного и нового времени (не WaitUntil «в
            // прошлом» — иначе цикл событий winit начинает холостой спин)
            self.next_repaint = Some(self.next_repaint.map_or(when, |t| t.min(when)));
        }
        self.check_repaint(el);
    }

    fn about_to_wait(&mut self, el: &ActiveEventLoop) {
        // Пробуждение по ControlFlow::WaitUntil наступает здесь: наступившие
        // перерисовки запускаются, будущие — перепланируются, протухший
        // WaitUntil не остаётся (иначе winit крутится вхолостую).
        self.check_repaint(el);
    }

    fn suspended(&mut self, _el: &ActiveEventLoop) {
        // Windows не присылает suspended для обычных окон; окно и surface
        // сохраняются (в отличие от Android, пересоздание не требуется).
    }
}

impl SwGui {
    /// Планирование перерисовок по модели eframe (`check_redraw_requests`):
    /// наступившее время → request_redraw (+ Poll до доставки кадра);
    /// будущее → WaitUntil; ничего → Wait. Вызывается после пользовательских
    /// событий и на каждом AboutToWait — холостого цикла не остаётся.
    fn check_repaint(&mut self, el: &ActiveEventLoop) {
        let Some(state) = &self.state else {
            el.set_control_flow(ControlFlow::Wait);
            return;
        };
        if let Some(t) = self.next_repaint {
            if t <= Instant::now() {
                self.next_repaint = None;
                el.set_control_flow(ControlFlow::Poll);
                state.window.request_redraw();
                return;
            }
            el.set_control_flow(ControlFlow::WaitUntil(t));
        } else {
            el.set_control_flow(ControlFlow::Wait);
        }
    }
}

impl SwGui {
    fn start(&mut self, el: &ActiveEventLoop) -> anyhow::Result<()> {
        // G6.4: стартовый размер — по рабочей области монитора запуска и его
        // DPI (те же initial_plan/место размещения, что у eframe-рантайма).
        let plan = crate::window::initial_plan();
        let builder = egui::ViewportBuilder::default()
            .with_inner_size(plan.inner_size)
            .with_min_inner_size(plan.min_inner_size)
            .with_title("PocketTTS-GUI-Rust v1.0");

        // Баг Wayland в egui-winit: resizable нельзя задать при создании —
        // выставляется после (на Windows безопасно в любом порядке).
        let window = egui_winit::create_window(&self.egui_ctx, el, &builder)
            .map_err(|e| anyhow::anyhow!("создание окна: {e}"))?;
        let window = Rc::new(window);

        // G6.4: однократное размещение по центру рабочей области монитора
        // запуска (физические пиксели, до первого кадра — без мерцания).
        crate::window::place_on_launch_monitor(window.as_ref());

        let surface = softbuffer::Surface::new(&self.softbuffer_context, window.clone())
            .map_err(|e| anyhow::anyhow!("softbuffer::Surface: {e}"))?;

        let egui_winit = egui_winit::State::new(
            self.egui_ctx.clone(),
            egui::ViewportId::ROOT,
            window.as_ref(),
            Some(window.scale_factor() as f32),
            None,
            None,
        );

        // Кириллица — как в eframe-рантайме (системный Segoe UI).
        entry::install_cyrillic_fonts(&self.egui_ctx);

        // BGRA — формат пиксельного буфера softbuffer на Windows.
        let renderer = EguiSoftwareRender::new(ColorFieldOrder::Bgra);

        self.state = Some(Running {
            window,
            surface,
            egui_winit,
            renderer,
            app: ShtormApp::new(),
            viewport_info: egui::ViewportInfo::default(),
            actions_requested: Vec::new(),
            close: false,
            frame_nr: 0,
        });
        if let Some(state) = &self.state {
            state.window.request_redraw();
        }
        // умолчание winit — Poll: без явного Wait цикл крутился бы вхолостую
        el.set_control_flow(ControlFlow::Wait);
        Ok(())
    }
}

impl Running {
    /// Один кадр: ввод → проход UI (update_ui обоих рантаймов) → решение о
    /// закрытии по протоколу eframe → растеризация на CPU → present →
    /// viewport-команды.
    fn frame(&mut self, ctx: &egui::Context, el: &ActiveEventLoop) -> anyhow::Result<()> {
        self.frame_nr = self.frame_nr.wrapping_add(1);
        if std::env::var_os("SW_DEBUG_FRAMES").is_some() {
            eprintln!("SW FRAME #{}", self.frame_nr);
        }
        let size = self.window.inner_size();
        let width = NonZeroU32::new(size.width).unwrap_or(NonZeroU32::MIN);
        let height = NonZeroU32::new(size.height).unwrap_or(NonZeroU32::MIN);
        self.surface
            .resize(width, height)
            .map_err(|e| anyhow::anyhow!("softbuffer resize: {e}"))?;

        // viewport-инфо: геометрия окна + накопленные события Close
        egui_winit::update_viewport_info(&mut self.viewport_info, ctx, &self.window, false);
        let close_requested = self
            .viewport_info
            .events
            .contains(&egui::ViewportEvent::Close);

        let mut raw_input = self.egui_winit.take_egui_input(&self.window);
        let mut viewports = egui::ViewportIdMap::default();
        viewports.insert(egui::ViewportId::ROOT, self.viewport_info.clone());
        raw_input.viewports = viewports;

        // Общий кадр UI (та же update_ui, что у eframe::App::ui)
        let full_output = ctx.run_ui(raw_input, |ui| self.app.update_ui(ui.ctx()));

        // Протокол закрытия eframe: close без CancelClose → закрыться
        if close_decision(
            close_requested,
            &full_output.viewport_output[&egui::ViewportId::ROOT].commands,
        ) {
            self.close = true;
        }
        // события viewport-инфо потреблены этим проходом
        self.viewport_info.events.clear();

        self.egui_winit
            .handle_platform_output(&self.window, full_output.platform_output);

        // Растеризация на CPU и вывод пиксельного буфера (GDI)
        let clipped_primitives = ctx.tessellate(full_output.shapes, full_output.pixels_per_point);
        let mut buffer = self
            .surface
            .buffer_mut()
            .map_err(|e| anyhow::anyhow!("softbuffer buffer: {e}"))?;
        buffer.fill(0);
        let mut buffer_ref = BufferMutRef::new(
            bytemuck::cast_slice_mut(&mut buffer),
            width.get() as usize,
            height.get() as usize,
        );
        self.renderer.render(
            &mut buffer_ref,
            &clipped_primitives,
            &full_output.textures_delta,
            full_output.pixels_per_point,
        );
        buffer
            .present()
            .map_err(|e| anyhow::anyhow!("softbuffer present: {e}"))?;

        // viewport-команды (eframe handle_viewport_output): Title/размеры —
        // на окно; Close кладёт ViewportEvent::Close в следующий кадр
        // (проходит без CancelClose → завершение); CancelClose здесь уже
        // учтён решением выше.
        let commands = full_output
            .viewport_output
            .get(&egui::ViewportId::ROOT)
            .map(|o| o.commands.clone())
            .unwrap_or_default();
        egui_winit::process_viewport_commands(
            ctx,
            &mut self.viewport_info,
            commands,
            &self.window,
            &mut self.actions_requested,
        );
        // команда Close индуцирует кадр-завершение — не ждём ввода
        if !self.viewport_info.events.is_empty() {
            self.window.request_redraw();
        }

        if self.close {
            // как eframe: выход из цикла после отрисовки решающего кадра;
            // on_exit_save вызывается после run_app в sw::run
            el.exit();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Протокол закрытия eframe (G6.8): close_requested закрывает окно,
    /// только если приложение НЕ ответило CancelClose.
    #[test]
    fn close_requires_no_cancel_close() {
        use egui::ViewportCommand::CancelClose;
        assert!(!close_decision(false, &[]), "без close_requested — работа");
        assert!(close_decision(true, &[]), "close без отмены — закрываемся");
        assert!(
            !close_decision(true, &[CancelClose]),
            "CancelClose удерживает окно (CloseFlow: отмена задания/диалог)"
        );
    }

    /// Машина CloseFlow поверх программного рантайма: тот же контракт, что
    /// проверен headless-тестами app.rs, — команда Close из Closing
    /// индуцирует закрытие на СЛЕДУЮЩЕМ кадре (app больше не отменяет).
    #[test]
    fn close_command_induces_next_frame_close() {
        let ctx = egui::Context::default();
        let mut app = crate::app::ShtormApp::new();
        // Крестик: кадр 1 — close_requested, приложение отвечает
        // CancelClose + переводит CloseFlow (без задания сразу в Closing→Close)
        let mut info = egui::ViewportInfo::default();
        info.events.push(egui::ViewportEvent::Close);
        let mut raw = egui::RawInput::default();
        raw.viewports.insert(egui::ViewportId::ROOT, info);
        let full = ctx.run_ui(raw, |ui| app.handle_close_flow(ui.ctx()));
        let cmds1 = &full.viewport_output[&egui::ViewportId::ROOT].commands;
        assert!(cmds1.contains(&egui::ViewportCommand::CancelClose));
        assert!(!close_decision(true, cmds1), "кадр 1: окно удержано");
        assert!(cmds1.contains(&egui::ViewportCommand::Close));

        // Кадр 2 (индуцирован командой Close): close_requested снова,
        // приложение в Closing больше НЕ отменяет — решение закрыться принято
        let mut info2 = egui::ViewportInfo::default();
        info2.events.push(egui::ViewportEvent::Close);
        let mut raw2 = egui::RawInput::default();
        raw2.viewports.insert(egui::ViewportId::ROOT, info2);
        let full2 = ctx.run_ui(raw2, |ui| app.handle_close_flow(ui.ctx()));
        let cmds2 = &full2.viewport_output[&egui::ViewportId::ROOT].commands;
        assert!(!cmds2.contains(&egui::ViewportCommand::CancelClose));
        assert!(close_decision(true, cmds2), "кадр 2: закрываемся");
    }
}
