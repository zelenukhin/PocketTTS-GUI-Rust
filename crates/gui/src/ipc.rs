//! G6.2: локальный IPC пакетного GUI с дочерними worker-процессами синтеза.
//!
//! Транспорт — Windows Named Pipes (duplex, byte mode, OVERLAPPED I/O):
//! - диспетчер (поток задания GUI) создаёт ОДИН уникальный канал на каждого
//!   дочернего процесса ДО его запуска и принимает соединение
//!   (`ConnectNamedPipe` с OVERLAPPED и таймаутом);
//! - дочерний процесс подключается `CreateFileW` и связывается только с
//!   каналом, имя которого получил аргументом командной строки;
//! - идентичность дочернего процесса проверяется по PID клиента канала
//!   (`GetNamedPipeClientProcessId` == PID запущенного `Child`);
//! - канал создаётся с `PIPE_REJECT_REMOTE_CLIENTS` и DACL по умолчанию
//!   (текущий пользователь): это ЛОКАЛЬНОЕ соединение, не сетевой сервер;
//! - кадрирование сообщений: u32 little-endian длина + JSON-байты;
//! - ВСЕ операции чтения и записи — OVERLAPPED (`FILE_FLAG_OVERLAPPED` +
//!   событие на операцию + `GetOverlappedResult`): синхронный блокирующий
//!   ReadFile приостанавливает WriteFile на ТОМ ЖЕ handle, и duplex-протокол
//!   «постоянное чтение событий + команды воркеру» на синхронных handle
//!   взаимно блокируется (обнаружено E2E-тестом G6.2);
//! - stdout/stderr в протоколе НЕ участвуют (управляющий протокол — только
//!   канал; логи процесса остаются в его консоли).
//!
//! ## G6.2.2 — правила завершения IPC (устранение двух дефектов)
//!
//! 1. **Отмена не ждёт мьютекса записи.** `PipeEnd::disconnect()` переводит
//!    конец канала в Closing и вызывает `CancelIoEx` БЕЗ захвата `write_lock`:
//!    зависшая в `WriteFile` запись (противоположная сторона перестала
//!    читать) снимается отменой, а не освобождением мьютекса. Активный
//!    writer координируется ограниченным `try_lock`-ожиданием; новые
//!    отправки после Closing отклоняются (проверка состояния под мьютексом и
//!    между чанками кадра); `DisconnectNamedPipe` выполняется только после
//!    этого. Ни один поток не остаётся заблокированным на `write_lock`.
//! 2. **Ожидания ограничены и подтверждаемы.** Одна overlapped-запись ждёт
//!    не дольше `WRITE_WAIT_TIMEOUT` (тесты переопределяют): по таймауту
//!    операция отменяется адресным `CancelIoEx` (только своя) и завершение
//!    ПОДТВЕРЖДАЕТСЯ — буфер, событие и `OVERLAPPED` освобождаются только
//!    после фактического завершения операции. Если операция не завершилась
//!    даже после отмены (для named pipes не наблюдается; защита от
//!    аномалий), ресурсы передаются фоновому «косильщику», который
//!    освобождает их только после события завершения: use-after-free
//!    исключён, а отправитель возвращается с ошибкой — GUI-процесс не
//!    завершается аварийно ради одной IPC-операции. То же правило действует
//!    при `WAIT_FAILED` и для `accept` (таймаут подключения).
//! 3. **Чтение** ждёт неограниченно только в ВЫДЕЛЕННОМ потоке читателя:
//!    внешние механизмы отмены гарантируют завершение — разрыв соединения
//!    при гибели противоположной стороны (EOF/`ERROR_BROKEN_PIPE`),
//!    `disconnect()`/`cancel_io()` при завершении, watchdog/startup-timeout
//!    диспетчера (kill своего PID) и Job Object (гибель GUI). При сбое
//!    ожидания чтение отменяется и подтверждается неограниченно: буфер
//!    читателя принадлежит его потоку и не освобождается до возврата.
//! 4. **Кадрирование неизменно** (u32 LE + JSON): неполный кадр —
//!    протокольная ошибка («канал закрыт посреди кадра»), а не EOF и не
//!    валидное сообщение; превышение лимита кадра — ошибка.
//!
//! Реализация минимимальная и без новых зависимостей: `windows-sys`.

use std::io;
use std::sync::Mutex;
use std::time::Duration;

/// Максимальный размер кадра протокола (FileReport с путями — килобайты;
/// 64 МиБ — щедрый запас, отсекающий лишь бессмысленные длины).
const MAX_FRAME_BYTES: u32 = 64 * 1024 * 1024;

#[cfg(windows)]
mod imp {
    use super::*;
    use std::os::windows::ffi::OsStrExt;
    use std::sync::atomic::{AtomicU64, AtomicU8, AtomicUsize, Ordering};
    use std::sync::{mpsc, OnceLock};
    use std::time::Instant;
    use windows_sys::Win32::Foundation::{
        CloseHandle, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE, WAIT_TIMEOUT,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, ReadFile, WriteFile, FILE_ATTRIBUTE_NORMAL, FILE_FLAG_OVERLAPPED,
        OPEN_EXISTING, PIPE_ACCESS_DUPLEX,
    };
    use windows_sys::Win32::System::Pipes::{
        ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, GetNamedPipeClientProcessId,
        WaitNamedPipeW, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE,
        PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
    };
    use windows_sys::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
    use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};

    /// ERROR_IO_PENDING: overlapped-операция принята и будет завершена позже.
    const ERROR_IO_PENDING_CODE: i32 = 997;
    /// ERROR_PIPE_CONNECTED: клиент подключился ДО ConnectNamedPipe.
    const ERROR_PIPE_CONNECTED_CODE: i32 = 535;
    /// ERROR_NOT_FOUND: CancelIoEx вызван без ожидающих операций — не ошибка.
    const ERROR_NOT_FOUND_CODE: i32 = 1168;
    /// ERROR_OPERATION_ABORTED: операция снята CancelIoEx.
    const ERROR_OPERATION_ABORTED_CODE: i32 = 995;
    /// WaitForSingleObject: событие сигнально (операция завершена).
    const WAIT_OBJECT_0_CODE: u32 = 0;

    /// G6.2.2: таймаут одной overlapped-записи кадра. Кадры — килобайты,
    /// противоположная сторона читает выделенным потоком; ожидающая запись
    /// означает зависание читателя — операция отменяется, отправитель
    /// получает ошибку, диспетчер продолжает работу (не полагается на
    /// собственные watchdog'и, которые не срабатывают, пока цикл заблокирован).
    pub const WRITE_WAIT_TIMEOUT: Duration = Duration::from_secs(10);
    /// G6.2.2: подтверждение завершения операции после CancelIoEx. Отмена
    /// named-pipe операций выполняется ядром немедленно; запас — планировщик.
    const CANCEL_JOIN_TIMEOUT: Duration = Duration::from_secs(5);
    /// G6.2.2: ограниченное ожидание активного writer в `disconnect()` после
    /// отмены его операции (writer завершается по отмене либо по проверке
    /// состояния Closing между чанками кадра).
    const DISCONNECT_WRITER_JOIN: Duration = Duration::from_secs(2);

    /// Состояние конца канала: Open → Closing → Disconnected.
    const ST_OPEN: u8 = 0;
    const ST_CLOSING: u8 = 1;
    const ST_DISCONNECTED: u8 = 2;

    // ====================== тестируемый слой ожидания (E2E-P) ======================

    /// Результат шага ожидания. Чистая функция решения (`plan_after_wait`)
    /// отделена от Win32: unit-тесты проверяют политику без внедрения сбоев
    /// в API Windows.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum WaitStep {
        /// Событие сигнально — операция ЗАВЕРШЕНА.
        Signaled,
        /// Время ожидания истекло — операция МОЖЕТ ещё выполняться.
        TimedOut,
        /// Отказ WaitForSingleObject — операция МОЖЕТ ещё выполняться.
        Failed,
    }

    /// Действие после шага ожидания.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum Recovery {
        /// Забрать результат завершённой операции.
        TakeResult,
        /// Отменить СВОЮ операцию, подтвердить завершение; при зависании
        /// отмены передать ресурсы косильщику и вернуть ошибку (запись).
        CancelThenDetach,
        /// Отменить и ждать подтверждения неограниченно (чтение: буфер
        /// принадлежит выделенному потоку читателя, который блокирующий по
        /// своей природе; освобождение до подтверждения невозможно).
        CancelThenWaitForever,
    }

    /// Политика завершения по результату ожидания и роду операции.
    pub(crate) fn plan_after_wait(step: WaitStep, is_write: bool) -> Recovery {
        match step {
            WaitStep::Signaled => Recovery::TakeResult,
            WaitStep::TimedOut | WaitStep::Failed => {
                if is_write {
                    Recovery::CancelThenDetach
                } else {
                    Recovery::CancelThenWaitForever
                }
            }
        }
    }

    /// Перевести код WaitForSingleObject в шаг ожидания. `None` — INFINITE.
    unsafe fn wait_step(event: HANDLE, timeout: Option<Duration>) -> WaitStep {
        let ms = match timeout {
            None => u32::MAX,
            Some(t) => u32::try_from(t.as_millis()).unwrap_or(u32::MAX - 1),
        };
        match WaitForSingleObject(event, ms) {
            WAIT_OBJECT_0_CODE => WaitStep::Signaled,
            WAIT_TIMEOUT => WaitStep::TimedOut,
            _ => WaitStep::Failed,
        }
    }

    // ======================= косильщик неподтверждённых операций =======================

    /// Операция, не подтверждённая даже после отмены (для named pipes не
    /// наблюдается; защита от аномалий): ОС продолжает использовать её буфер
    /// и `OVERLAPPED`, поэтому ресурсы освобождаются ТОЛЬКО после
    /// фактического завершения — фоновым потоком-косильщиком. Отправитель
    /// возвращается с ошибкой; приложение продолжает работу.
    struct DetachedOp {
        event: HANDLE,
        ov: Box<OVERLAPPED>,
        /// Буфер незавершённой записи (владение передано отправителем).
        buf: Vec<u8>,
        /// HANDLE, который разрешено закрыть ПОСЛЕ завершения операции
        /// (например, канал, не принявший подключение по таймауту accept).
        close_after: Option<HANDLE>,
    }

    // HANDLE и OVERLAPPED переходят к косильщику в эксклюзивное владение;
    // отправитель к ним больше не обращается.
    unsafe impl Send for DetachedOp {}

    static DETACHED_TX: OnceLock<mpsc::Sender<DetachedOp>> = OnceLock::new();
    static DETACHED_COUNT: AtomicUsize = AtomicUsize::new(0);

    /// Число операций, ожидающих подтверждения косильщиком (диагностика
    /// тестов E2E-P; в бою всегда 0).
    #[cfg(test)]
    pub(crate) fn detached_ops_pending() -> usize {
        DETACHED_COUNT.load(Ordering::SeqCst)
    }

    /// Передать неподтверждённую операцию косильщику. Если косильщик
    /// недоступен (исчерпание потоков), остаётся единственный безопасный
    /// исход — ждать завершения на месте, не отдавая невалидную память.
    fn detach_stuck(event: HANDLE, ov: Box<OVERLAPPED>, buf: Vec<u8>, close_after: Option<HANDLE>) {
        DETACHED_COUNT.fetch_add(1, Ordering::SeqCst);
        let tx = DETACHED_TX.get_or_init(|| {
            let (tx, rx) = mpsc::channel::<DetachedOp>();
            // Поток живёт до конца процесса и паркется на канале; событие
            // незавершённой операции он не трогает до сигнала завершения.
            let _ = std::thread::Builder::new()
                .name("shtorm-ipc-stuck-io".into())
                .spawn(move || {
                    for op in rx {
                        unsafe {
                            WaitForSingleObject(op.event, u32::MAX);
                            CloseHandle(op.event);
                            if let Some(h) = op.close_after {
                                CloseHandle(h);
                            }
                        }
                        drop(op.buf); // освобождение ПОСЛЕ фактического завершения
                        drop(op.ov);
                        DETACHED_COUNT.fetch_sub(1, Ordering::SeqCst);
                    }
                });
            tx
        });
        if let Err(e) = tx.send(DetachedOp {
            event,
            ov,
            buf,
            close_after,
        }) {
            // косильщик недоступен: ждать на месте — память валидна до конца
            let op = e.0; // SendError возвращает владение операцией
            unsafe {
                WaitForSingleObject(op.event, u32::MAX);
                CloseHandle(op.event);
                if let Some(h) = op.close_after {
                    CloseHandle(h);
                }
            }
            drop(op.buf);
            drop(op.ov);
            DETACHED_COUNT.fetch_sub(1, Ordering::SeqCst);
        }
    }

    // ============================ одна overlapped-операция ============================

    /// Одна overlapped-операция: событие на операцию, `OVERLAPPED` в куче.
    /// Инвариант владения: событие и `OVERLAPPED` освобождаются только после
    /// ПОДТВЕРЖДЁННОГО завершения операции (сигнальное событие либо
    /// синхронное завершение при ненулевом возврате Win32-функции).
    struct OverlappedOp {
        handle: HANDLE,
        event: HANDLE,
        ov: Box<OVERLAPPED>,
    }

    impl OverlappedOp {
        unsafe fn new(handle: HANDLE) -> io::Result<Self> {
            let event = CreateEventW(std::ptr::null(), 0, 0, std::ptr::null());
            if event.is_null() {
                return Err(last_err());
            }
            let mut ov: Box<OVERLAPPED> = Box::new(std::mem::zeroed());
            ov.hEvent = event;
            Ok(Self { handle, event, ov })
        }

        /// Запустить операцию. `Ok(true)` — завершена синхронно (результат
        /// доступен уже), `Ok(false)` — ожидает (`ERROR_IO_PENDING`),
        /// `Err` — Win32 отказал до старта (ресурсы можно освобождать).
        unsafe fn submit<F: FnOnce(*mut OVERLAPPED) -> i32>(&mut self, op: F) -> io::Result<bool> {
            let r = op(self.ov.as_mut());
            if r != 0 {
                return Ok(true);
            }
            let e = last_err();
            if e.raw_os_error() == Some(ERROR_IO_PENDING_CODE) {
                Ok(false)
            } else {
                Err(e)
            }
        }

        /// Результат ЗАВЕРШЁННОЙ операции (событие сигнально или было
        /// синхронное завершение). Возврат здесь — подтверждение: буферы
        /// вызывающего можно освобождать.
        unsafe fn result(&mut self) -> io::Result<u32> {
            let mut bytes: u32 = 0;
            let ok = GetOverlappedResult(self.handle, self.ov.as_mut(), &mut bytes, 0);
            if ok == 0 {
                Err(last_err())
            } else {
                Ok(bytes)
            }
        }

        /// Адресная отмена СВОЕЙ операции (не трогает операции других
        /// потоков на том же handle). Уже завершённая операция
        /// (`ERROR_NOT_FOUND`) — не ошибка: событие сигнально.
        unsafe fn cancel(&mut self) -> io::Result<()> {
            let ok = CancelIoEx(self.handle, self.ov.as_mut());
            if ok == 0 {
                let e = last_err();
                if e.raw_os_error() == Some(ERROR_NOT_FOUND_CODE) {
                    return Ok(());
                }
                return Err(e);
            }
            Ok(())
        }

        unsafe fn wait(&self, timeout: Option<Duration>) -> WaitStep {
            wait_step(self.event, timeout)
        }

        /// Передать незавершённую операцию косильщику: после вызова `self`
        /// ничем не владеет (drop безопасен, событие закрывать не нужно).
        unsafe fn into_detached(mut self, buf: Vec<u8>, close_after: Option<HANDLE>) {
            let event = std::mem::replace(&mut self.event, std::ptr::null_mut());
            let ov = std::mem::replace(&mut self.ov, Box::new(std::mem::zeroed()));
            detach_stuck(event, ov, buf, close_after);
        }
    }

    impl Drop for OverlappedOp {
        fn drop(&mut self) {
            if !self.event.is_null() {
                unsafe { CloseHandle(self.event) };
            }
        }
    }

    /// Итог ожидания pending-записи.
    enum WriteWait {
        /// Операция подтверждена: результат (байты или ошибка Win32).
        Done(io::Result<u32>),
        /// Отмена не подтвердилась: операция и её буфер переданы косильщику
        /// ВЫЗЫВАЮЩИМ (буфер кадра переходит во владение отсюда).
        Stuck(OverlappedOp),
    }

    fn wide(s: &str) -> Vec<u16> {
        std::ffi::OsStr::new(s)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    fn last_err() -> io::Error {
        io::Error::last_os_error()
    }

    /// Подключённый конец канала (сервер после accept, клиент после connect).
    /// Владеет HANDLE; запись сериализуется мьютексом (события шлют несколько
    /// потоков воркеров), чтение выполняет один выделенный поток. Все операции
    /// overlapped — чтение и запись не блокируют друг друга.
    pub struct PipeEnd {
        handle: HANDLE,
        write_lock: Mutex<()>,
        /// 0 — Open; 1 — Closing (shutdown начался: отправки запрещены,
        /// операции отменяются); 2 — Disconnected.
        state: AtomicU8,
        /// Таймаут одной записи в миллисекундах (0 — WRITE_WAIT_TIMEOUT).
        write_timeout_ms: AtomicU64,
        /// Серверный конец (accept): поддерживает DisconnectNamedPipe.
        is_server: bool,
    }

    // HANDLE — сырой указатель; отправляем его только в Win32-вызовы этого
    // мода, владение эксклюзивное.
    unsafe impl Send for PipeEnd {}
    unsafe impl Sync for PipeEnd {}

    impl Drop for PipeEnd {
        fn drop(&mut self) {
            // К этому моменту все операции этого конца подтверждены либо
            // переданы косильщику (их память валидна независимо от handle).
            unsafe { CloseHandle(self.handle) };
        }
    }

    impl PipeEnd {
        /// G6.2.2: таймаут одной записи (тесты уменьшают для быстрых
        /// проверек отмены; производственный путь — WRITE_WAIT_TIMEOUT).
        pub fn set_write_timeout(&self, t: Duration) {
            let ms = u64::try_from(t.as_millis()).unwrap_or(u64::MAX);
            self.write_timeout_ms.store(ms, Ordering::SeqCst);
        }

        fn write_timeout(&self) -> Duration {
            let ms = self.write_timeout_ms.load(Ordering::SeqCst);
            if ms == 0 {
                WRITE_WAIT_TIMEOUT
            } else {
                Duration::from_millis(ms)
            }
        }

        /// Перевести в Closing, только из Open (не откатывает Disconnected).
        fn poison(&self) {
            let _ = self.state.compare_exchange(
                ST_OPEN,
                ST_CLOSING,
                Ordering::SeqCst,
                Ordering::SeqCst,
            );
        }

        /// Отправить кадр: `u32 LE` длина + полезная нагрузка. Запись
        /// сериализована `write_lock` (кадр атомарен в потоке байт), ожидание
        /// ограничено `write_timeout`; после начала shutdown отправки
        /// отклоняются. Ошибка посреди кадра отравляет конец канала: частичный
        /// кадр в потоке делает протокол этого направления невосстановимым.
        pub fn send(&self, bytes: &[u8]) -> io::Result<()> {
            let len = u32::try_from(bytes.len())
                .map_err(|_| io::Error::other("сообщение протокола слишком велико"))?;
            let mut frame = Vec::with_capacity(4 + bytes.len());
            frame.extend_from_slice(&len.to_le_bytes());
            frame.extend_from_slice(bytes);
            self.send_frame_owned(frame)
        }

        /// Запись одного owned-кадра с ограниченным ожиданием. Владение
        /// буфером: на время записи буфер принадлежит операции; при
        /// неподтверждённой отмене он передаётся косильщику целиком.
        fn send_frame_owned(&self, mut frame: Vec<u8>) -> io::Result<()> {
            let _guard = self
                .write_lock
                .lock()
                .map_err(|_| io::Error::other("мьютекс записи канала отравлен"))?;
            if self.state.load(Ordering::SeqCst) != ST_OPEN {
                return Err(io::Error::other("канал закрывается — отправка отклонена"));
            }
            let mut done = 0usize;
            while done < frame.len() {
                // между чанками кадра shutdown конкурирует с записью
                if self.state.load(Ordering::SeqCst) != ST_OPEN {
                    return Err(io::Error::other("канал закрывается — запись прервана"));
                }
                let handle = self.handle;
                let n = u32::try_from(frame.len() - done).unwrap_or(u32::MAX);
                let mut op = unsafe { OverlappedOp::new(handle)? };
                // любая ошибка посреди кадра отравляет канал: частичный кадр
                // в потоке делает это направление невосстановимым
                let written = unsafe {
                    let submitted = op.submit(|ov| {
                        WriteFile(
                            handle,
                            frame.as_ptr().add(done).cast(),
                            n,
                            std::ptr::null_mut(),
                            ov,
                        )
                    });
                    match submitted {
                        Ok(true) => op.result(),
                        Ok(false) => match self.await_write(op) {
                            WriteWait::Done(r) => r,
                            WriteWait::Stuck(op) => {
                                // эскалация: буфер кадра передан косильщику,
                                // канал отравлен, отправитель возвращается
                                self.poison();
                                let leaked = std::mem::take(&mut frame);
                                op.into_detached(leaked, None);
                                return Err(io::Error::new(
                                    io::ErrorKind::TimedOut,
                                    "запись не подтвердилась после отмены — ресурсы переданы \
                                     фоновой очистке",
                                ));
                            }
                        },
                        Err(e) => Err(e),
                    }
                };
                let written = match written {
                    Ok(n) => n,
                    Err(e) => {
                        self.poison();
                        if e.raw_os_error() == Some(ERROR_OPERATION_ABORTED_CODE) {
                            return Err(io::Error::new(
                                io::ErrorKind::TimedOut,
                                format!(
                                    "запись отменена по таймауту {:?} — кадр отправлен частично",
                                    self.write_timeout()
                                ),
                            ));
                        }
                        return Err(e);
                    }
                };
                if written == 0 {
                    self.poison();
                    return Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "канал закрыт на запись",
                    ));
                }
                done += written as usize;
            }
            Ok(())
        }

        /// Ожидание pending-записи с ограничением времени. Таймаут/сбой
        /// ожидания → отмена своей операции → подтверждение завершения
        /// (ограниченное) → при неподтверждении возврат операции для
        /// передачи косильщику.
        unsafe fn await_write(&self, mut op: OverlappedOp) -> WriteWait {
            let step = op.wait(Some(self.write_timeout()));
            match plan_after_wait(step, true) {
                Recovery::TakeResult => WriteWait::Done(op.result()),
                Recovery::CancelThenDetach => {
                    let _ = op.cancel();
                    match op.wait(Some(CANCEL_JOIN_TIMEOUT)) {
                        WaitStep::Signaled => WriteWait::Done(op.result()),
                        WaitStep::TimedOut | WaitStep::Failed => WriteWait::Stuck(op),
                    }
                }
                Recovery::CancelThenWaitForever => unreachable!("политика только для чтения"),
            }
        }

        /// Ожидание pending-чтения: неограниченное (выделенный поток
        /// читателя; внешние механизмы отмены — см. правила модуля). Сбой
        /// ожидания → отмена + неограниченное подтверждение: буфер читателя
        /// освобождается только после возврата из операции.
        unsafe fn await_read(&self, mut op: OverlappedOp) -> io::Result<u32> {
            let step = op.wait(None);
            match plan_after_wait(step, false) {
                Recovery::TakeResult => op.result(),
                Recovery::CancelThenWaitForever => {
                    let _ = op.cancel();
                    let _ = op.wait(None);
                    op.result()
                }
                Recovery::CancelThenDetach => unreachable!("политика только для записи"),
            }
        }

        /// Блокирующее чтение одного кадра. `Ok(None)` — канал закрыт
        /// (EOF на границе кадра): сторона умерла или завершилась. Неполный
        /// кадр — ошибка протокола, а не EOF и не сообщение.
        pub fn recv(&self) -> io::Result<Option<Vec<u8>>> {
            let mut len_buf = [0u8; 4];
            if !self.read_exact_or_eof(&mut len_buf)? {
                return Ok(None);
            }
            let len = u32::from_le_bytes(len_buf);
            if len > MAX_FRAME_BYTES {
                return Err(io::Error::other(format!(
                    "размер кадра протокола {len} байт превышает лимит"
                )));
            }
            let mut buf = vec![0u8; len as usize];
            if len > 0 {
                self.read_full(&mut buf)?;
            }
            Ok(Some(buf))
        }

        /// PID противоположной стороны (для проверки идентичности клиента).
        pub fn peer_pid(&self) -> io::Result<u32> {
            let mut pid: u32 = 0;
            let ok = unsafe { GetNamedPipeClientProcessId(self.handle, &mut pid) };
            if ok == 0 {
                return Err(last_err());
            }
            Ok(pid)
        }

        /// Снять ВСЕ ожидающие overlapped-операции этого handle
        /// (`CancelIoEx`). Нужно для ограниченного по времени завершения:
        /// поток чтения может быть заблокирован в `ReadFile` до тех пор,
        /// пока диспетчер не закроет канал; отмена гарантированно возвращает
        /// `GetOverlappedResult(ERROR_OPERATION_ABORTED)`. Буферы и OVERLAPPED
        /// отменённой операции принадлежат ожидающему потоку и не
        /// освобождаются, пока он не вернулся из вызова.
        pub fn cancel_io(&self) -> io::Result<()> {
            let ok = unsafe { CancelIoEx(self.handle, std::ptr::null()) };
            if ok == 0 {
                let e = last_err();
                // нет ожидающих операций — отменять нечего
                if e.raw_os_error() == Some(ERROR_NOT_FOUND_CODE) {
                    return Ok(());
                }
                return Err(e);
            }
            Ok(())
        }

        /// Принудительно разорвать соединение: перевести конец канала в
        /// Closing, снять ожидающие операции (`CancelIoEx` БЕЗ захвата
        /// `write_lock` — ключевое исправление G6.2.2: зависшая запись
        /// снимается отменой, а не ожиданием мьютекса), ограниченно дождаться
        /// активного writer, затем `DisconnectNamedPipe` (серверный конец):
        /// противоположная сторона увидит EOF/`ERROR_BROKEN_PIPE`. Идемпотентно.
        pub fn disconnect(&self) -> io::Result<()> {
            if self
                .state
                .compare_exchange(ST_OPEN, ST_CLOSING, Ordering::SeqCst, Ordering::SeqCst)
                .is_err()
            {
                return Ok(()); // уже закрывается/закрыто
            }
            // 1) отмена без write_lock: прерывает зависшую запись и чтение
            self.cancel_io()?;
            // 2) ограниченная координация активного writer: он завершается по
            //    отмене либо по проверке состояния между чанками; DISCONNECT_
            //    WRITER_JOIN истёк — DisconnectNamedPipe всё равно безопасен
            //    (буферы принадлежат потоку writer и живут до его возврата)
            let deadline = Instant::now() + DISCONNECT_WRITER_JOIN;
            loop {
                match self.write_lock.try_lock() {
                    Ok(guard) => {
                        drop(guard);
                        break;
                    }
                    Err(std::sync::TryLockError::Poisoned(_)) => break,
                    Err(std::sync::TryLockError::WouldBlock) => {
                        if Instant::now() >= deadline {
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(10));
                    }
                }
            }
            // 3) разрыв серверного конца канала
            if self.is_server {
                let ok = unsafe { DisconnectNamedPipe(self.handle) };
                if ok == 0 {
                    return Err(last_err());
                }
            }
            self.state.store(ST_DISCONNECTED, Ordering::SeqCst);
            Ok(())
        }

        /// true — прочитаны все байты; false — EOF до первого байта.
        fn read_exact_or_eof(&self, buf: &mut [u8]) -> io::Result<bool> {
            let mut done = 0usize;
            while done < buf.len() {
                let handle = self.handle;
                let n = u32::try_from(buf.len() - done).unwrap_or(u32::MAX);
                let mut op = unsafe { OverlappedOp::new(handle)? };
                let read = unsafe {
                    let res = match op.submit(|ov| {
                        ReadFile(
                            handle,
                            buf[done..].as_mut_ptr().cast(),
                            n,
                            std::ptr::null_mut(),
                            ov,
                        )
                    }) {
                        Ok(true) => op.result(),
                        Ok(false) => self.await_read(op),
                        Err(e) => Err(e),
                    };
                    match res {
                        Ok(r) => r,
                        Err(e) => {
                            // ERROR_BROKEN_PIPE (109): сторона закрыла канал
                            // после записи — это EOF, если мы в начале кадра.
                            // ERROR_OPERATION_ABORTED (995): операция снята
                            // нашим cancel_io/disconnect — EOF на границе кадра.
                            let code = e.raw_os_error();
                            if done == 0
                                && (code == Some(109) || code == Some(ERROR_OPERATION_ABORTED_CODE))
                            {
                                return Ok(false);
                            }
                            return Err(e);
                        }
                    }
                };
                match read {
                    0 => {
                        if done == 0 {
                            return Ok(false); // чистый EOF на границе кадра
                        }
                        return Err(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "канал закрыт посреди кадра",
                        ));
                    }
                    r => done += r as usize,
                }
            }
            Ok(true)
        }

        fn read_full(&self, buf: &mut [u8]) -> io::Result<()> {
            if !self.read_exact_or_eof(buf)? {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "канал закрыт посреди кадра",
                ));
            }
            Ok(())
        }

        /// ТОЛЬКО тесты (E2E-N): запись произвольных байтов БЕЗ кадрирования —
        /// проверка целостности протокола при разрыве посреди кадра.
        #[cfg(test)]
        pub(crate) fn send_raw(&self, bytes: &[u8]) -> io::Result<()> {
            self.send_frame_owned(bytes.to_vec())
        }

        /// ТОЛЬКО тесты (E2E-P): поставить РЕАЛЬНУЮ pending-запись готового
        /// кадра (противоположная сторона не читает; буфер больше ёмкости
        /// канала) и НЕМЕДЛЕННО передать её ресурсы косильщику — точная
        /// имитация «отмена не подтверждена». false — запись не перевисела
        /// (кадр прошёл в буфер целиком).
        #[cfg(test)]
        pub(crate) fn submit_detached_write_for_test(&self, frame: Vec<u8>) -> bool {
            let handle = self.handle;
            let mut op = unsafe { OverlappedOp::new(handle).expect("событие") };
            let n = u32::try_from(frame.len()).unwrap_or(u32::MAX);
            let pending = unsafe {
                match op.submit(|ov| {
                    WriteFile(handle, frame.as_ptr().cast(), n, std::ptr::null_mut(), ov)
                }) {
                    Ok(true) => false,
                    Ok(false) => true,
                    Err(_) => false,
                }
            };
            if !pending {
                return false;
            }
            unsafe { op.into_detached(frame, None) };
            true
        }
    }

    /// Серверная сторона: созданный, но ещё не подключённый экземпляр канала.
    pub struct PipeServer {
        name: String,
        handle: HANDLE,
    }

    // HANDLE — сырой указатель с эксклюзивным владением; потокобезопасность
    // обеспечивается использованием из одного потока за раз (create → accept;
    // accept забирает self по значению).
    unsafe impl Send for PipeServer {}
    unsafe impl Sync for PipeServer {}

    impl Drop for PipeServer {
        fn drop(&mut self) {
            unsafe { CloseHandle(self.handle) };
        }
    }

    impl PipeServer {
        /// Создать уникальный канал `\\.\pipe\shtorm_tts_<tag>_<pid>_<нанос>`
        /// и вернуть серверный экземпляр. Имя передаётся дочернему процессу.
        pub fn create_unique(tag: &str) -> io::Result<Self> {
            Self::create_unique_sized(tag, 1 << 20)
        }

        /// Тестовая конфигурация (E2E-M): малые буферы канала, чтобы запись
        /// объёмом больше буфера РЕАЛЬНО ожидала чтения противоположной
        /// стороной. Боевой путь использует create_unique.
        pub fn create_unique_sized(tag: &str, buffer_bytes: u32) -> io::Result<Self> {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let name = format!(
                r"\\.\pipe\shtorm_tts_{}_{}_{}",
                tag,
                std::process::id(),
                nanos
            );
            let wname = wide(&name);
            // DACL по умолчанию: только создатель (текущий пользователь) и
            // администраторы; REJECT_REMOTE_CLIENTS запрещает сетевые клиенты.
            // FILE_FLAG_OVERLAPPED — все операции чтения/записи асинхронные.
            let handle = unsafe {
                CreateNamedPipeW(
                    wname.as_ptr(),
                    PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED,
                    PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                    PIPE_UNLIMITED_INSTANCES,
                    buffer_bytes,
                    buffer_bytes,
                    0,
                    std::ptr::null(),
                )
            };
            if handle == INVALID_HANDLE_VALUE {
                return Err(last_err());
            }
            Ok(Self { name, handle })
        }

        pub fn name(&self) -> &str {
            &self.name
        }

        /// Дождаться подключения клиента (до `timeout`). G6.2.2: если дочерний
        /// процесс не подключился, ожидание снимается ПОДТВЕРЖДАЕМОЙ отменой —
        /// событие, `OVERLAPPED` и HANDLE канала не освобождаются и не
        /// закрываются, пока операция подключения фактически не завершена;
        /// при неподтверждённой отмене закрытие HANDLE передаётся косильщику.
        pub fn accept(self, timeout: Duration) -> io::Result<PipeEnd> {
            let handle = self.handle;
            let mut op = unsafe { OverlappedOp::new(handle)? };
            let connected = match unsafe { op.submit(|ov| ConnectNamedPipe(handle, ov)) } {
                Ok(true) => true,
                Ok(false) => false, // ERROR_IO_PENDING — ждать событие
                Err(e) => {
                    if e.raw_os_error() == Some(ERROR_PIPE_CONNECTED_CODE) {
                        true // клиент успел подключиться до accept
                    } else {
                        return Err(e); // операция не запущена — ресурсы подтверждены
                    }
                }
            };
            if !connected {
                match unsafe { op.wait(Some(timeout)) } {
                    WaitStep::Signaled => {}
                    WaitStep::TimedOut | WaitStep::Failed => {
                        let name = self.name.clone();
                        unsafe {
                            let _ = op.cancel();
                        }
                        match unsafe { op.wait(Some(CANCEL_JOIN_TIMEOUT)) } {
                            WaitStep::Signaled => {
                                drop(op);
                                drop(self);
                                return Err(io::Error::new(
                                    io::ErrorKind::TimedOut,
                                    format!("к каналу {name} не подключились за {timeout:?}"),
                                ));
                            }
                            WaitStep::TimedOut | WaitStep::Failed => {
                                // отмена не подтверждена: право закрыть HANDLE
                                // канала передаётся косильщику ПОСЛЕ завершения
                                unsafe { op.into_detached(Vec::new(), Some(handle)) };
                                std::mem::forget(self); // HANDLE закроет косильщик
                                return Err(io::Error::other(
                                    "accept не подтверждён после отмены — канал закрывается \
                                     фоновой очисткой",
                                ));
                            }
                        }
                    }
                }
            }
            drop(op);
            let end = PipeEnd {
                handle,
                write_lock: Mutex::new(()),
                state: AtomicU8::new(ST_OPEN),
                write_timeout_ms: AtomicU64::new(0),
                is_server: true,
            };
            std::mem::forget(self); // HANDLE перешёл к PipeEnd
            Ok(end)
        }
    }

    /// Клиентская сторона (дочерний процесс): подключиться к каналу по имени.
    /// HANDLE — overlapped (FILE_FLAG_OVERLAPPED): чтение и запись независимы.
    pub fn connect(name: &str, timeout: Duration) -> io::Result<PipeEnd> {
        let wname = wide(name);
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let handle = unsafe {
                CreateFileW(
                    wname.as_ptr(),
                    GENERIC_READ | GENERIC_WRITE,
                    0,
                    std::ptr::null(),
                    OPEN_EXISTING,
                    FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OVERLAPPED,
                    std::ptr::null_mut() as _,
                )
            };
            if handle != INVALID_HANDLE_VALUE {
                return Ok(PipeEnd {
                    handle,
                    write_lock: Mutex::new(()),
                    state: AtomicU8::new(ST_OPEN),
                    write_timeout_ms: AtomicU64::new(0),
                    is_server: false,
                });
            }
            let e = last_err();
            if e.raw_os_error() == Some(231) {
                // ERROR_PIPE_BUSY: все экземпляры заняты — подождать
                let wbusy = wide(name);
                unsafe { WaitNamedPipeW(wbusy.as_ptr(), 1000) };
            }
            if std::time::Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("канал {name} не доступен за отведённое время: {e}"),
                ));
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

#[cfg(windows)]
pub use imp::{connect, PipeEnd, PipeServer, WRITE_WAIT_TIMEOUT};

#[cfg(not(windows))]
mod imp {
    use super::*;

    pub struct PipeEnd;
    pub struct PipeServer;

    impl PipeEnd {
        pub fn cancel_io(&self) -> io::Result<()> {
            Err(io::Error::other(
                "гибридный режим поддерживается только на Windows",
            ))
        }
        pub fn disconnect(&self) -> io::Result<()> {
            Err(io::Error::other(
                "гибридный режим поддерживается только на Windows",
            ))
        }
        pub fn set_write_timeout(&self, _t: Duration) {}
    }

    impl PipeServer {
        pub fn create_unique(_tag: &str) -> io::Result<Self> {
            Err(io::Error::other(
                "гибридный режим поддерживается только на Windows",
            ))
        }
        pub fn create_unique_sized(_tag: &str, _buffer_bytes: u32) -> io::Result<Self> {
            Err(io::Error::other(
                "гибридный режим поддерживается только на Windows",
            ))
        }
        pub fn name(&self) -> &str {
            ""
        }
        pub fn accept(self, _timeout: Duration) -> io::Result<PipeEnd> {
            Err(io::Error::other(
                "гибридный режим поддерживается только на Windows",
            ))
        }
    }

    pub fn connect(_name: &str, _timeout: Duration) -> io::Result<PipeEnd> {
        Err(io::Error::other(
            "гибридный режим поддерживается только на Windows",
        ))
    }
}

#[cfg(not(windows))]
pub use imp::{connect, PipeEnd, PipeServer};

/// Надстроенный JSON-транспорт: кадр = u32 LE длина + JSON.
pub mod wire {
    use super::PipeEnd;
    use std::io;

    pub fn send_json(end: &PipeEnd, v: &serde_json::Value) -> io::Result<()> {
        let bytes = serde_json::to_vec(v).map_err(io::Error::other)?;
        end.send(&bytes)
    }

    /// `Ok(None)` — канал закрыт (противоположная сторона завершилась).
    pub fn recv_json(end: &PipeEnd) -> io::Result<Option<serde_json::Value>> {
        match end.recv()? {
            None => Ok(None),
            Some(bytes) => serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(io::Error::other),
        }
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Instant;

    /// Обёртка «join с таймаутом»: поток обязан завершиться за `t`, иначе
    /// тест падает (зависший поток не маскируется).
    fn join_bounded<T: Send + 'static>(
        h: std::thread::JoinHandle<T>,
        t: Duration,
        what: &str,
    ) -> T {
        let (tx, rx) = std::sync::mpsc::channel::<Option<T>>();
        std::thread::spawn(move || {
            let _ = tx.send(h.join().ok());
        });
        match rx.recv_timeout(t) {
            Ok(Some(v)) => v,
            Ok(None) => panic!("{what}: поток завершился паникой"),
            Err(_) => panic!("{what}: поток не завершился за {t:?}"),
        }
    }

    /// Loopback: сервер + клиент в одном процессе — кадрирование, EOF,
    /// JSON-обмен и peer_pid (здесь — собственный PID).
    #[test]
    fn pipe_roundtrip_framing_json_and_peer_pid() {
        let (tx_name, rx_name) = std::sync::mpsc::channel::<String>();
        let accepted = std::thread::spawn(move || {
            let server = PipeServer::create_unique("test").expect("создать канал");
            tx_name
                .send(server.name().to_string())
                .expect("отправить имя");
            let end = server.accept(Duration::from_secs(5)).expect("accept");
            let got = wire::recv_json(&end).expect("recv").expect("кадр");
            wire::send_json(&end, &serde_json::json!({"echo": got})).expect("send");
            end
        });
        let name = rx_name.recv().expect("имя канала");
        let client = connect(&name, Duration::from_secs(5)).expect("connect");
        wire::send_json(&client, &serde_json::json!({"x": 1, "s": "текст"})).expect("client send");
        let reply = wire::recv_json(&client)
            .expect("client recv")
            .expect("reply");
        assert_eq!(reply["echo"]["x"], 1);
        assert_eq!(reply["echo"]["s"], "текст");
        let end = accepted.join().expect("сервер");
        assert_eq!(end.peer_pid().expect("peer pid"), std::process::id());
        drop(client);
        let got = wire::recv_json(&end).expect("recv после закрытия");
        assert!(got.is_none(), "EOF после закрытия противоположной стороны");
    }

    /// Полный duplex: обе стороны одновременно держат блокирующее чтение и
    /// пишут несколько сообщений (события диспетчеру и команды воркеру).
    /// На синхронных handle этот сценарий взаимно блокируется — регрессия
    /// overlapped-транспорта G6.2.
    #[test]
    fn duplex_concurrent_read_write() {
        let (tx_name, rx_name) = std::sync::mpsc::channel::<String>();
        let server = std::thread::spawn(move || {
            let srv = PipeServer::create_unique("duplex").expect("создать");
            tx_name.send(srv.name().to_string()).unwrap();
            let end = Arc::new(srv.accept(Duration::from_secs(10)).expect("accept"));
            let r = Arc::clone(&end);
            let reader = std::thread::spawn(move || {
                let mut got = Vec::new();
                for _ in 0..3 {
                    got.push(wire::recv_json(&r).expect("recv").expect("кадр"));
                }
                got
            });
            for i in 0..3 {
                wire::send_json(&end, &serde_json::json!({"srv": i})).expect("srv send");
            }
            let got = reader.join().unwrap();
            assert_eq!(got[0]["cli"], 0);
            assert_eq!(got[1]["cli"], 1);
            assert_eq!(got[2]["cli"], 2);
            end
        });
        let name = rx_name.recv().unwrap();
        let client = Arc::new(connect(&name, Duration::from_secs(10)).expect("connect"));
        let c = Arc::clone(&client);
        let reader = std::thread::spawn(move || {
            let mut got = Vec::new();
            for _ in 0..3 {
                got.push(wire::recv_json(&c).expect("recv").expect("кадр"));
            }
            got
        });
        for i in 0..3 {
            wire::send_json(&client, &serde_json::json!({"cli": i})).expect("cli send");
        }
        let got = reader.join().unwrap();
        assert_eq!(got[0]["srv"], 0);
        assert_eq!(got[1]["srv"], 1);
        assert_eq!(got[2]["srv"], 2);
        let _ = server.join().unwrap();
    }

    /// ДВА последовательных соединения в одном процессе (как диспетчер с
    /// двумя дочерними процессами): второе не блокируется и работает.
    #[test]
    fn two_sequential_connections_both_work() {
        for round in 0..2 {
            let (tx_name, rx_name) = std::sync::mpsc::channel::<String>();
            let server_thread = std::thread::spawn(move || {
                let server = PipeServer::create_unique("seq").expect("создать");
                tx_name.send(server.name().to_string()).unwrap();
                let end = server.accept(Duration::from_secs(10)).expect("accept");
                let got = wire::recv_json(&end).expect("recv").expect("кадр");
                wire::send_json(&end, &serde_json::json!({"round": got})).expect("send");
                end
            });
            let name = rx_name.recv().unwrap();
            let client = connect(&name, Duration::from_secs(10)).expect("connect");
            wire::send_json(&client, &serde_json::json!(round)).expect("client send");
            let reply = wire::recv_json(&client)
                .expect("client recv")
                .expect("reply");
            assert_eq!(reply["round"], round);
            drop(client);
            let _ = server_thread.join().expect("сервер раунда");
        }
    }

    /// Подключение к несуществующему каналу — ошибка по таймауту (не паника,
    /// не бесконечное ожидание).
    #[test]
    fn connect_to_missing_pipe_times_out() {
        let r = connect(
            r"\\.\pipe\shtorm_tts_definitely_absent_999",
            Duration::from_millis(150),
        );
        assert!(r.is_err(), "подключение к несуществующему каналу — ошибка");
    }

    /// G6.2.1: разрыв канала снимает ЗАБЛОКИРОВАННОЕ overlapped-чтение
    /// (CancelIoEx + DisconnectNamedPipe) — ограниченное завершение без
    /// ожидания, пока вторая сторона закроет свой конец. Регрессия дедлока
    /// `reader.join()` при фатальной ошибке инициализации.
    #[test]
    fn disconnect_unblocks_pending_read() {
        let (tx_name, rx_name) = std::sync::mpsc::channel::<String>();
        let server = std::thread::spawn(move || {
            let srv = PipeServer::create_unique("cancel").expect("создать");
            tx_name.send(srv.name().to_string()).unwrap();
            srv.accept(Duration::from_secs(5)).expect("accept")
        });
        let name = rx_name.recv().unwrap();
        let client = connect(&name, Duration::from_secs(5)).expect("connect");
        let end = Arc::new(server.join().expect("сервер"));

        let read_end = Arc::clone(&end);
        let (started_tx, started_rx) = std::sync::mpsc::channel::<()>();
        let reader = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            wire::recv_json(&read_end)
        });
        started_rx.recv().unwrap();
        // дать потоку войти в ReadFile (ожидание overlapped-операции)
        std::thread::sleep(Duration::from_millis(200));

        let t0 = Instant::now();
        end.disconnect().expect("разрыв канала");
        let got = join_bounded(reader, Duration::from_secs(5), "поток чтения");
        assert!(
            t0.elapsed() < Duration::from_secs(5),
            "чтение снято за {:?}",
            t0.elapsed()
        );
        match got {
            Ok(None) => {}                                 // EOF на границе кадра
            Err(e) => assert!(e.raw_os_error().is_some()), // отмена операции
            Ok(Some(_)) => panic!("после разрыва не должно быть кадра"),
        }
        drop(client);
    }

    // ======================= G6.2.2: E2E K–P =======================

    /// E2E-K: зависшая запись (клиент не читает, 4 МиБ > буфера 1 МиБ) +
    /// конкурентный `disconnect()` из другого потока. Главный критерий:
    /// НИ ОДИН поток не остаётся заблокированным на `write_lock` — отмена
    /// снимает запись, disconnect не ждёт мьютекс записи.
    #[test]
    fn e2e_k_blocked_write_concurrent_disconnect() {
        let (tx_name, rx_name) = std::sync::mpsc::channel::<String>();
        let server = std::thread::spawn(move || {
            let srv = PipeServer::create_unique("e2ek").expect("создать");
            tx_name.send(srv.name().to_string()).unwrap();
            Arc::new(srv.accept(Duration::from_secs(5)).expect("accept"))
        });
        let name = rx_name.recv().unwrap();
        // клиент подключился и НЕ читает: запись обязана перевисеть
        let client = connect(&name, Duration::from_secs(5)).expect("connect");
        let end = server.join().expect("сервер");

        let (started_tx, started_rx) = std::sync::mpsc::channel::<()>();
        let w_end = Arc::clone(&end);
        let writer = std::thread::spawn(move || {
            let big = vec![b'k'; 4 << 20];
            started_tx.send(()).unwrap();
            w_end.send(&big)
        });
        started_rx.recv().unwrap();
        std::thread::sleep(Duration::from_millis(300)); // запись гарантированно висит

        let t0 = Instant::now();
        end.disconnect()
            .expect("disconnect возвращается без ожидания мьютекса записи");
        let res = join_bounded(writer, Duration::from_secs(5), "writer");
        let dt = t0.elapsed();
        assert!(
            dt < Duration::from_secs(5),
            "disconnect + writer завершились за {dt:?}"
        );
        assert!(
            res.is_err(),
            "зависшая запись обязана завершиться ошибкой отмены"
        );
        // повторный disconnect идемпотентен
        end.disconnect().expect("повторный disconnect");
        drop(client);
    }

    /// E2E-L: несколько потоков отправки + одновременное закрытие,
    /// многократно. Проверяется: отсутствие deadlock; корректные ошибки
    /// после Closing; принятые кадры целы (без повреждений и повторов);
    /// отсутствие use-after-free (каждый раунд — новый канал, после серии
    /// свежий канал работает).
    #[test]
    fn e2e_l_concurrent_send_disconnect_stress() {
        for round in 0..12 {
            let (tx_name, rx_name) = std::sync::mpsc::channel::<String>();
            let server = std::thread::spawn(move || {
                let srv = PipeServer::create_unique("e2el").expect("создать");
                tx_name.send(srv.name().to_string()).unwrap();
                Arc::new(srv.accept(Duration::from_secs(10)).expect("accept"))
            });
            let name = rx_name.recv().unwrap();
            let client = connect(&name, Duration::from_secs(10)).expect("connect");
            let end = server.join().expect("сервер");
            // короткий таймаут записи: зависание до закрытия обнаруживается быстро
            end.set_write_timeout(Duration::from_millis(200));

            // читатель клиента: принимает только ЦЕЛЫЕ валидные кадры
            let reader = std::thread::spawn(move || {
                let mut seen = std::collections::HashSet::new();
                let mut frames = 0usize;
                while let Ok(Some(v)) = wire::recv_json(&client) {
                    let seq = v["seq"].as_u64().expect("целостный кадр с seq");
                    assert!(
                        seen.insert(seq),
                        "раунд {round}: кадр {seq} доставлен повторно (повторная отправка)"
                    );
                    frames += 1;
                }
                frames
            });

            // три потока отправки БОЛЬШИМИ кадрами (128 КиБ: записи
            // постоянно перевисают в буфере 1 МиБ и конкурируют с закрытием).
            // Каждый шлёт ДО ПЕРВОГО ОТКАЗА (после Closing все отправки
            // отклоняются) с защитным потолком; закрытие приходит в раунде
            // гарантированно раньше потолка.
            const SEND_CAP: usize = 512;
            let mut results = Vec::new();
            for t in 0..3usize {
                let e = Arc::clone(&end);
                results.push(std::thread::spawn(move || {
                    let mut ok = 0usize;
                    let mut refused = 0usize;
                    let pad = "p".repeat(128 * 1024);
                    for i in 0..SEND_CAP {
                        let seq = (t * 100_000 + i) as u64;
                        match wire::send_json(&e, &serde_json::json!({"seq": seq, "pad": pad})) {
                            Ok(()) => ok += 1,
                            Err(_) => {
                                refused += 1;
                                break; // канал закрывается — дальше только отказы
                            }
                        }
                    }
                    (ok, refused)
                }));
            }
            std::thread::sleep(Duration::from_millis(10 + (round % 5) * 10));
            end.disconnect().expect("disconnect в раунде");

            let mut total_ok = 0usize;
            let mut total_refused = 0usize;
            for r in results {
                let (ok, refused) = join_bounded(r, Duration::from_secs(10), "отправитель");
                total_ok += ok;
                total_refused += refused;
            }
            let frames = join_bounded(reader, Duration::from_secs(10), "читатель");
            // завершённые в буфер кадры могут не дойти до читателя после
            // разрыва — принято НЕ БОЛЬШЕ отправленного, без потерь целостности
            assert!(
                frames <= total_ok,
                "раунд {round}: принято {frames} кадров, отправлено {total_ok}"
            );
            assert!(
                total_refused > 0,
                "раунд {round}: после Closing отправки обязаны отклоняться"
            );
            drop(end);
        }
        // после стресса ресурсы модуля целы — свежий канал работает
        let (tx_name, rx_name) = std::sync::mpsc::channel::<String>();
        let server = std::thread::spawn(move || {
            let srv = PipeServer::create_unique("e2el_final").expect("создать");
            tx_name.send(srv.name().to_string()).unwrap();
            let end = srv.accept(Duration::from_secs(5)).expect("accept");
            let got = wire::recv_json(&end).expect("recv").expect("кадр");
            wire::send_json(&end, &got).expect("send");
            end
        });
        let name = rx_name.recv().unwrap();
        let client = connect(&name, Duration::from_secs(5)).expect("connect");
        wire::send_json(&client, &serde_json::json!({"final": true})).expect("send");
        let reply = wire::recv_json(&client).expect("recv").expect("reply");
        assert_eq!(reply["final"], true);
        drop(client);
        drop(server.join().expect("сервер"));
    }

    /// E2E-N: разрыв соединения посреди кадра не рождает валидных сообщений.
    /// Части заголовка, полный заголовок + часть JSON, сразу после полного
    /// кадра, завышенная длина — неполные кадры суть протокольные ошибки.
    #[test]
    fn e2e_n_partial_frame_break_is_protocol_error() {
        // (a) 2 байта из 4-байтового заголовка, затем закрытие. Windows
        // завершает ожидающее чтение нативной ошибкой канала (BrokenPipe)
        // либо UnexpectedEof — оба исхода суть протокольная ошибка
        {
            let (end, client) = setup_connected_pair("e2en_a");
            end.send_raw(&123u32.to_le_bytes()[..2])
                .expect("частичный заголовок");
            drop(end);
            let err = client
                .recv()
                .expect_err("неполный заголовок — ошибка протокола");
            assert!(
                matches!(
                    err.kind(),
                    std::io::ErrorKind::UnexpectedEof | std::io::ErrorKind::BrokenPipe
                ),
                "протокольная ошибка, а не EOF/кадр: {err}"
            );
        }
        // (b) полный заголовок + часть JSON, затем закрытие
        {
            let (end, client) = setup_connected_pair("e2en_b");
            let payload = b"{\"x\":1}";
            let declared = (payload.len() + 5) as u32; // больше реального
            end.send_raw(&declared.to_le_bytes()).expect("заголовок");
            end.send_raw(&payload[..3]).expect("часть JSON");
            drop(end);
            let err = client.recv().expect_err("неполный кадр — ошибка протокола");
            assert!(
                matches!(
                    err.kind(),
                    std::io::ErrorKind::UnexpectedEof | std::io::ErrorKind::BrokenPipe
                ),
                "протокольная ошибка, а не EOF/кадр: {err}"
            );
        }
        // (c) полный кадр, затем закрытие: кадр принят, затем чистый EOF
        {
            let (end, client) = setup_connected_pair("e2en_c");
            wire::send_json(&end, &serde_json::json!({"ok": 1})).expect("кадр");
            drop(end);
            let got = client.recv().expect("полный кадр принят").expect("кадр");
            let v: serde_json::Value = serde_json::from_slice(&got).expect("валидный JSON");
            assert_eq!(v["ok"], 1);
            assert!(
                client.recv().expect("EOF").is_none(),
                "после полного кадра — EOF, а не мусор"
            );
        }
        // (d) заголовок с длиной больше лимита кадра — протокольная ошибка
        {
            let (end, client) = setup_connected_pair("e2en_d");
            end.send_raw(&u32::MAX.to_le_bytes())
                .expect("завышенная длина");
            let err = client.recv().expect_err("превышение лимита кадра");
            assert!(
                err.to_string().contains("превышает лимит"),
                "проверка лимита: {err}"
            );
            drop(end);
        }
    }

    /// E2E-O: гонка CancelIoEx и естественного завершения операции. Отмена
    /// не гарантирует ERROR_OPERATION_ABORTED — операция может успеть
    /// завершиться успешно. Оба исхода корректны; двойного освобождения
    /// ресурсов нет (каждый раунд — новый канал; после серии всё работает).
    #[test]
    fn e2e_o_cancel_races_natural_completion() {
        let mut natural = 0usize;
        let mut aborted = 0usize;
        for round in 0..40usize {
            let (end, client) = setup_connected_pair("e2eo");
            end.set_write_timeout(Duration::from_millis(300));

            // клиент читает: часть записей завершается естественно
            let c = client;
            let reader = std::thread::spawn(move || {
                let mut n = 0usize;
                while let Ok(Some(_)) = wire::recv_json(&c) {
                    n += 1;
                }
                n
            });

            let e = Arc::clone(&end);
            let (tx, rx) = std::sync::mpsc::channel::<bool>();
            let writer = std::thread::spawn(move || {
                // > буфера канала: запись реально перевисает
                let pad = "o".repeat(2 << 20);
                let failed =
                    wire::send_json(&e, &serde_json::json!({"round": round, "pad": pad})).is_err();
                let _ = tx.send(failed);
                failed
            });
            // конкурентная отмена в случайный момент гонки
            std::thread::sleep(Duration::from_micros(((round % 20) * 100) as u64));
            let _ = end.cancel_io();

            match rx.recv_timeout(Duration::from_secs(5)) {
                Ok(false) => natural += 1,
                Ok(true) => aborted += 1,
                Err(_) => panic!("раунд {round}: writer завис"),
            }
            // оба исхода допустимы; завершение потока обязательно
            join_bounded(writer, Duration::from_secs(5), "writer раунда");
            let _ = end.disconnect();
            let _ = join_bounded(reader, Duration::from_secs(5), "читатель раунда");
        }
        assert!(
            natural + aborted == 40,
            "каждый раунд получил исход ({natural} естественных, {aborted} отменённых)"
        );
        // после серии свежий канал работает (ресурсы не попорчены)
        let (end, client) = setup_connected_pair("e2eo_final");
        wire::send_json(&end, &serde_json::json!({"final": 1})).expect("send");
        assert_eq!(
            wire::recv_json(&client).expect("recv").expect("кадр")["final"],
            1
        );
        drop(client);
        drop(end);
    }

    /// E2E-P: слой ожидания. Часть 1 — чистая политика (все комбинации
    /// результата ожидания × род операции) без внедрения сбоев в Windows API.
    /// Часть 2 — РЕАЛЬНЫЙ путь косильщика: pending-запись передаётся ему, и
    /// ресурсы освобождаются только после того, как противоположная сторона
    /// прочитала данные (подтверждённое завершение).
    #[test]
    fn e2e_p_wait_policy_and_detached_reaper() {
        use super::imp::{detached_ops_pending, plan_after_wait, Recovery, WaitStep};

        // --- часть 1: политика после WaitForSingleObject ---
        // сигнальное событие: операция завершена — забрать результат
        assert_eq!(
            plan_after_wait(WaitStep::Signaled, true),
            Recovery::TakeResult
        );
        assert_eq!(
            plan_after_wait(WaitStep::Signaled, false),
            Recovery::TakeResult
        );
        // таймаут записи: отмена → подтверждение → эскалация косильщиком
        assert_eq!(
            plan_after_wait(WaitStep::TimedOut, true),
            Recovery::CancelThenDetach
        );
        // WAIT_FAILED записи: операция может быть жива — тот же путь
        assert_eq!(
            plan_after_wait(WaitStep::Failed, true),
            Recovery::CancelThenDetach
        );
        // чтение: буфер принадлежит потоку читателя — отмена и
        // неограниченное подтверждение (никакого преждевременного освобождения)
        assert_eq!(
            plan_after_wait(WaitStep::TimedOut, false),
            Recovery::CancelThenWaitForever
        );
        assert_eq!(
            plan_after_wait(WaitStep::Failed, false),
            Recovery::CancelThenWaitForever
        );

        // --- часть 2: реальный косильщик ---
        let (end, client) = setup_connected_pair("e2ep");
        // полный кадр (заголовок + 2 МиБ): запись реально перевисит
        let payload = serde_json::to_vec(&serde_json::json!({
            "seq": 7,
            "pad": "p".repeat(2 << 20)
        }))
        .unwrap();
        let mut frame = Vec::with_capacity(4 + payload.len());
        frame.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        frame.extend_from_slice(&payload);
        let was_pending = end.submit_detached_write_for_test(frame);
        assert!(
            was_pending,
            "запись обязана перевисеть (2 МиБ > буфера канала)"
        );
        assert_eq!(
            detached_ops_pending(),
            1,
            "операция передана косильщику и ждёт подтверждения"
        );

        // клиент вычитывает кадр → операция завершается → косильщик освобождает
        let drainer = std::thread::spawn(move || wire::recv_json(&client));
        let got = drainer
            .join()
            .expect("дренаж")
            .expect("recv")
            .expect("кадр");
        assert_eq!(got["seq"], 7, "кадр дошёл целиком");

        let deadline = Instant::now() + Duration::from_secs(5);
        while detached_ops_pending() > 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(
            detached_ops_pending(),
            0,
            "косильщик освободил ресурсы после фактического завершения"
        );
        drop(end);
    }

    /// Сервер+клиент в одном процессе (фабрика тестов кадрирования/гонок).
    fn setup_connected_pair(tag: &str) -> (Arc<PipeEnd>, PipeEnd) {
        let tag = tag.to_string();
        let (tx_name, rx_name) = std::sync::mpsc::channel::<String>();
        let server = std::thread::spawn(move || {
            let srv = PipeServer::create_unique(&tag).expect("создать");
            tx_name.send(srv.name().to_string()).unwrap();
            srv.accept(Duration::from_secs(10)).expect("accept")
        });
        let name = rx_name.recv().unwrap();
        let client = connect(&name, Duration::from_secs(10)).expect("connect");
        (Arc::new(server.join().expect("сервер")), client)
    }
}
