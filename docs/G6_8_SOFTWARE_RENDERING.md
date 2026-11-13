# G6.8 — Программный CPU-рендеринг GUI (PocketTTS-GUI-Rust-cpu)

Дата: 2026-11-13. Задача: второй исполняемый файл с полностью программной
отрисовкой GUI на CPU — без OpenGL/WGL, Direct3D и GPU — для Windows 10 x64
22H2 (19045.6454), где eframe/glutin падает с
`glutin error: extension to create ES context with wgl is not present`.
«CPU» в имени EXE относится ТОЛЬКО к отрисовке интерфейса: CPU FP32/INT8 и
CUDA-инференс не отключаются и не меняются.

Эталон архитектуры — Demagog-RS (коммит `b25f1ab`, egui 0.34 +
egui_software_backend 0.0.3 + softbuffer + winit/egui-winit). Отличие
жизненно важное: библиотечный winit-рантайм `egui_software_backend` при
`WindowEvent::CloseRequested` немедленно вызывает `elwt.exit()`, не давая
приложению отменить закрытие. Для Demagog-RS это допустимо, для PocketTTS —
нет: машина закрытия G6.2.3 (`CloseFlow`) обязана отменить задание, дождаться
потока, автосохранить настройки и при ошибке показать диалог. Поэтому из
крейта используется ТОЛЬКО растеризатор `EguiSoftwareRender` (+`BufferMutRef`,
`ColorFieldOrder::Bgra`), а цикл событий — собственный (`gui/src/sw.rs`),
повторяющий протокол закрытия eframe 0.34.

## Версии

| Крейт | Версия | Роль |
|---|---|---|
| egui | 0.31 → **0.34.3** | общий для обоих рантаймов (требование egui_software_backend) |
| eframe | 0.31 → **0.34.3** (default-features=false, `accesskit,default_fonts,glow`) | только EXE PocketTTS-GUI-Rust; в 0.34 wgpu стал умолчанием — glow явно |
| egui_software_backend | **0.0.3** (default-features=false, `std`) | растеризация egui на CPU |
| egui-winit | **0.34.3** (default) | ввод/DPI/IME/clipboard (clipboard включён — кнопка «Копировать») |
| winit | **0.30.13** | окно/события (общий с eframe) |
| softbuffer | **0.4.8** | вывод пиксельного буфера (Windows — GDI) |

Rust 1.99.1 (MSRV egui 0.34 — 1.92).

## Схема Cargo (crates/gui)

```toml
[features]
default = ["glow"]              # cargo build --release → только GL-EXE
glow    = ["dep:eframe"]        # PocketTTS-GUI-Rust.exe (OpenGL)
sw      = ["dep:egui_software_backend", "dep:egui-winit",
           "dep:softbuffer", "dep:winit", "dep:bytemuck"]

[[bin]] name = "PocketTTS-GUI-Rust"     path = "src/main.rs"    required-features = ["glow"]
[[bin]] name = "PocketTTS-GUI-Rust-cpu" path = "src/sw_main.rs" required-features = ["sw"]
```

Сборка:

```powershell
cargo build --release                                              # PocketTTS-GUI-Rust.exe
cargo build --release --no-default-features --features sw          # PocketTTS-GUI-Rust-cpu.exe
cargo clippy --all-targets --release --features sw -- -D warnings  # оба набора
cargo test --release --features sw                                 # +тесты sw-модуля
```

В cpu-сборке eframe — опциональная зависимость, ОТКЛЮЧЁННАЯ: EXE не
содержит ни вызова, ни импорта OpenGL (проверено по таблице импортов PE:
нет `OPENGL32.dll`, нет d3d/dxgi; вывод — `gdi32`/`user32`/`kernel32`).
Fallback на GL невозможен by construction. GL-сборка не изменилась.

## Разделение UI и рантайма

- `app.rs`: кадр UI выделен из `eframe::App::update` в
  **`ShtormApp::update_ui(&mut self, ctx: &egui::Context)`** — им пользуются
  оба рантайма (`eframe::App::ui` → `self.update_ui(ui.ctx())`; sw-цикл —
  внутри `ctx.run_ui`). Дублей формы/прогресса/журнала/диалогов нет.
- `on_exit` (страховка автосохранения) — общий метод `on_exit_save()`:
  вызывается eframe-трактом и sw-циклом после завершения.
- `entry.rs`: вся точка входа/CLI (`--portable-*`, `--internal-batch-worker`,
  `--help`, шрифты `install_cyrillic_fonts`) — в библиотеке; бинарники
  различаются только `Backend`. Служебные CLI-команды выполняются ДО
  создания окна и не зависят от рантайма.
- Миграция 0.31→0.34: `Context::run`→`run_ui`, `style/set_style`→
  `global_style/set_global_style`, `eframe::App::update`→`ui`. Панели и
  `screen_rect` на контексте объявлены deprecated — сохранены (поведение тех
  же API не изменилось; миграция компоновки G6.4 сознательно вне этапа),
  `#[allow(deprecated)]` с комментарием. Виджеты не переписывались.

## Протокол закрытия в sw-цикле (как eframe 0.34)

1. `WindowEvent::CloseRequested` НЕ закрывает окно: в viewport-инфу кладётся
   `egui::ViewportEvent::Close` + двойной request_repaint.
2. Кадр: приложение видит `ctx.input(|i| i.viewport().close_requested())`;
   `handle_close_flow` (G6.2.3, без изменений) отвечает `CancelClose`, пока
   идёт отмена задания/автосохранение/диалог ошибки; сама запись настроек и
   диалог «Повторить/Выйти без сохранения» — прежние.
3. Решение: `close_decision(close_requested, commands)` = close был И НЕ
   отвечен CancelClose → флаг close.
4. `ViewportCommand::Close` (CloseFlow::Closing) через
   `egui_winit::process_viewport_commands` кладёт `ViewportEvent::Close` в
   СЛЕДУЮЩИЙ кадр (перерисовка запрашивается) — на нём CancelClose уже не
   отправляется, цикл завершается, вызывается `on_exit_save` (no-op после
   штатного потока). Гарантируются: закрытие до завершения задания — нельзя;
   блокировка GUI-потока ожиданием синтеза — нет; обход диалога ошибки — нет;
   дочерние процессы — только через штатный протокол (Job Object G6.2).

## Фоновые потоки и пробуждение

`ctx.request_repaint()`/`request_repaint_after()` из потока синтеза
(`emit` → `ctx2.request_repaint()`) работают через
`Context::set_request_repaint_callback` → `EventLoopProxy` → user-event с
дедупликацией по `cumulative_pass_nr` (алгоритм eframe), холостого цикла на
максимальной частоте нет; между событиями `ControlFlow::Wait`/`WaitUntil`.
Прогресс, журнал, «Стоп», статусы чанков, диалоги — общий `update_ui`.

**Планирование перерисовок — модель eframe `check_redraw_requests`**
(`SwGui::check_repaint`): запрос запоминается как ближайшее время
(`next_repaint`); после каждого события и на `AboutToWait` наступившее время
→ `request_redraw` (+`ControlFlow::Poll` до доставки кадра), будущее →
`WaitUntil`, ничего → `Wait`. Найденный и исправленный дефект первой
реализации: `WaitUntil(мгновение «сейчас»)` + пустой `AboutToWait` оставляли
протухший таймер — цикл winit спинился вхолостую (87 % ядра при полном
отсутствии кадров). После фикса: простой без ввода — **0.0 % CPU** и ноль
кадров; ввод/hover перерисовки — по событиям (подтверждено пользователем
визуально). Дополнительно в `start()` задан явный `ControlFlow::Wait`
(умолчание winit — Poll).

## Дочерние процессы G6.7 из cpu-EXE

Воркеры спавнятся через `current_exe()` → `PocketTTS-GUI-Rust-cpu.exe
--internal-batch-worker <канал>`. Диспетчеризация CLI в `entry::dispatch`
выполняется до любого окна; воркер-режим никогда не создаёт ни окна, ни
графического рантайма (HEADLESS-путь не зависит от features). Named
Pipes/Job Object/watchdog не менялись. Проверено headless:
`--portable-selftest-hybrid` из cpu-EXE — 2 процесса × 2 воркера, 6/6,
сирот 0.

## Проверки (машина разработки, Windows 10 19045)

- fmt OK; clippy `-D warnings` — 0 для трёх конфигураций (glow; glow+sw;
  только sw); `cargo test --release --features sw` — **293 passed / 0 failed**
  (в т.ч. 2 новых теста sw: `close_requires_no_cancel_close`,
  `close_command_induces_next_frame_close` — контракт протокола закрытия
  headless).
- Оба EXE собраны release. PE-импорты: cpu-EXE — БЕЗ opengl32/d3d/dxgi
  (gdi32/user32/kernel32/bcrypt/dwmapi/imm32/uxtheme/ole32/shell32/
  shlwapi/vcruntime140 + api-ms-*); GL-EXE — c OPENGL32.dll, как прежде.
- Headless функциональность из песочницы portable-комплекта g671
  (hardlink-копия, ресурсы exe-relative):
  - cpu-EXE `--portable-check` — PORTABLE CHECK OK;
  - cpu-EXE `--portable-selftest` — реальный CPU FP32 WAV (2 чанка, 6.62 с,
    RTF 1.2, exit 0);
  - **побитовый паритет**: WAV cpu-EXE ≡ WAV GL-EXE, одинаковые параметры
    (SHA256 `E2267CAE0D90666AD5E8009859B19A3C56EE0EFAF12296059E22A631E8BD89E2`);
  - cpu-EXE `--portable-selftest-batch` — пул 4 воркеров, 2/2, exit 0;
  - cpu-EXE `--portable-selftest-hybrid` — 2 процесса × 2 воркера (fake),
    6/6, HYBRID SELFTEST OK, сирот 0.
- GUI-смоук (скрипт, WM_CLOSE через CloseMainWindow): оба EXE — окно
  открывается, закрытие exit 0, FirstRun-автосохранение создаёт конфиг,
  повторный запуск без правок не переписывает файл (SHA256 неизменен),
  осиротевших процессов нет. Тёмная тема/растеризация — по скриншоту
  (94.8 % тёмных пикселей) и визуальной приёмке пользователя.
- Закрытие во время синтеза (интерактивное) — см. HANDOFF (этап приёмки).

## Ограничения / не проверено здесь

- Целевая машина пользователя GitHub (Win10 x64 22H2, отказ WGL) — EXE
  подготовлен для тестирования, гарантий до проверки на ней не заявляется.
- Замеры производительности отрисовки не проводились (GUI-сценарий —
  статичная форма + прогресс; «очень медленно без оптимизаций» — только
  debug-сборки; release с opt-level по умолчанию).
- IME-команды (`IMERect` и др.) рантаймом egui_software_backend/winit-путём
  не поддерживаются на уровне команд egui, ввод через IME (imm32) работает
  штатно; приложение полей с CJK-вводом не имеет.
- Панели egui на контексте — deprecated API (работает, см. выше); при
  будущей миграции на `Panel::show_inside` трогать только `update_ui`.
