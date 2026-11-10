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
//! - ВСЕ операции чтения и записи — OVERLAPPED (FILE_FLAG_OVERLAPPED +
//!   событие на операцию + GetOverlappedResult): синхронный блокирующий
//!   ReadFile приостанавливает WriteFile на ТОМ ЖЕ handle, и duplex-протокол
//!   «постоянное чтение событий + команды диспетчера» на синхронных handle
//!   взаимно блокируется (обнаружено E2E-тестом G6.2); overlapped-операции
//!   независимы и допускают одновременное чтение и запись;
//! - stdout/stderr в протоколе НЕ участвуют (управляющий протокол — только
//!   канал; логи процесса остаются в его консоли).
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
    use windows_sys::Win32::Foundation::{
        CloseHandle, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE, WAIT_FAILED,
        WAIT_TIMEOUT,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, ReadFile, WriteFile, FILE_ATTRIBUTE_NORMAL, FILE_FLAG_OVERLAPPED,
        OPEN_EXISTING, PIPE_ACCESS_DUPLEX,
    };
    use windows_sys::Win32::System::Pipes::{
        ConnectNamedPipe, CreateNamedPipeW, GetNamedPipeClientProcessId, WaitNamedPipeW,
        PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES,
        PIPE_WAIT,
    };
    use windows_sys::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
    use windows_sys::Win32::System::IO::{GetOverlappedResult, OVERLAPPED};

    /// ERROR_IO_PENDING: overlapped-операция принята и будет завершена позже.
    const ERROR_IO_PENDING_CODE: i32 = 997;
    /// ERROR_PIPE_CONNECTED: клиент подключился ДО ConnectNamedPipe.
    const ERROR_PIPE_CONNECTED_CODE: i32 = 535;

    fn wide(s: &str) -> Vec<u16> {
        std::ffi::OsStr::new(s)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    fn last_err() -> io::Error {
        io::Error::last_os_error()
    }

    /// Выполнить одну overlapped-операцию (ReadFile/WriteFile/ConnectNamedPipe)
    /// и дождаться её завершения. `op` получает указатель на OVERLAPPED,
    /// возвращает BOOL как в Win32; возвращает число переданных байт.
    /// Событие создаётся на КАЖДУЮ операцию (несколько потоков могут держать
    /// операции на одном handle одновременно — событие общее было бы гонкой).
    unsafe fn overlapped_wait<F>(handle: HANDLE, op: F) -> io::Result<u32>
    where
        F: FnOnce(*mut OVERLAPPED) -> i32,
    {
        let event = CreateEventW(std::ptr::null(), 0, 0, std::ptr::null());
        if event.is_null() {
            return Err(last_err());
        }
        let mut ov: OVERLAPPED = std::mem::zeroed();
        ov.hEvent = event;
        let mut bytes: u32 = 0;
        let immediate = op(&mut ov);
        if immediate != 0 {
            // операция завершилась синхронно — результата ожидания нет
            let ok = GetOverlappedResult(handle, &ov, &mut bytes, 0);
            CloseHandle(event);
            if ok == 0 {
                return Err(last_err());
            }
            return Ok(bytes);
        }
        let e = last_err();
        if e.raw_os_error() != Some(ERROR_IO_PENDING_CODE) {
            CloseHandle(event);
            return Err(e);
        }
        let waited = WaitForSingleObject(event, u32::MAX); // INFINITE
        if waited == WAIT_FAILED {
            let e = last_err();
            CloseHandle(event);
            return Err(e);
        }
        let ok = GetOverlappedResult(handle, &ov, &mut bytes, 0);
        let res = if ok == 0 { Err(last_err()) } else { Ok(bytes) };
        CloseHandle(event);
        res
    }

    /// Подключённый конец канала (сервер после accept, клиент после connect).
    /// Владеет HANDLE; запись сериализуется мьютексом (события шлют несколько
    /// потоков воркеров), чтение выполняет один выделенный поток. Все операции
    /// overlapped — чтение и запись не блокируют друг друга.
    pub struct PipeEnd {
        handle: HANDLE,
        write_lock: Mutex<()>,
    }

    // HANDLE — сырой указатель; отправляем его только в Win32-вызовы этого
    // мода, владение эксклюзивное.
    unsafe impl Send for PipeEnd {}
    unsafe impl Sync for PipeEnd {}

    impl Drop for PipeEnd {
        fn drop(&mut self) {
            unsafe { CloseHandle(self.handle) };
        }
    }

    impl PipeEnd {
        pub fn send(&self, bytes: &[u8]) -> io::Result<()> {
            let _guard = self
                .write_lock
                .lock()
                .map_err(|_| io::Error::other("мьютекс записи канала отравлен"))?;
            let len = u32::try_from(bytes.len())
                .map_err(|_| io::Error::other("сообщение протокола слишком велико"))?;
            self.write_all_raw(&len.to_le_bytes())?;
            self.write_all_raw(bytes)
        }

        /// Блокирующее чтение одного кадра. `Ok(None)` — канал закрыт
        /// (EOF до первого байта длины): сторона умерла или завершилась.
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

        fn write_all_raw(&self, mut bytes: &[u8]) -> io::Result<()> {
            while !bytes.is_empty() {
                let handle = self.handle;
                let n = u32::try_from(bytes.len()).unwrap_or(u32::MAX);
                let written = unsafe {
                    overlapped_wait(handle, |ov| {
                        WriteFile(handle, bytes.as_ptr().cast(), n, std::ptr::null_mut(), ov)
                    })
                }?;
                if written == 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "канал закрыт на запись",
                    ));
                }
                bytes = &bytes[written as usize..];
            }
            Ok(())
        }

        /// true — прочитаны все байты; false — EOF до первого байта.
        fn read_exact_or_eof(&self, buf: &mut [u8]) -> io::Result<bool> {
            let mut done = 0usize;
            while done < buf.len() {
                let handle = self.handle;
                let n = u32::try_from(buf.len() - done).unwrap_or(u32::MAX);
                let read = unsafe {
                    overlapped_wait(handle, |ov| {
                        ReadFile(
                            handle,
                            buf[done..].as_mut_ptr().cast(),
                            n,
                            std::ptr::null_mut(),
                            ov,
                        )
                    })
                };
                match read {
                    Ok(0) => {
                        if done == 0 {
                            return Ok(false); // чистый EOF на границе кадра
                        }
                        return Err(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "канал закрыт посреди кадра",
                        ));
                    }
                    Ok(r) => done += r as usize,
                    Err(e) => {
                        // ERROR_BROKEN_PIPE (109): сторона закрыла канал после
                        // записи — это EOF, если мы ещё в начале кадра
                        if e.raw_os_error() == Some(109) && done == 0 {
                            return Ok(false);
                        }
                        return Err(e);
                    }
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
                    1 << 20,
                    1 << 20,
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

        /// Дождаться подключения клиента (до `timeout`). Если дочерний процесс
        /// погиб ДО подключения, ожидание снимается по таймауту (overlapped
        /// ConnectNamedPipe + событие), а не висит вечно.
        pub fn accept(self, timeout: Duration) -> io::Result<PipeEnd> {
            let handle = self.handle;
            let event = unsafe { CreateEventW(std::ptr::null(), 0, 0, std::ptr::null()) };
            if event.is_null() {
                return Err(last_err());
            }
            let mut ov: OVERLAPPED = unsafe { std::mem::zeroed() };
            ov.hEvent = event;
            let ok = unsafe { ConnectNamedPipe(handle, &mut ov) };
            let mut connected = ok != 0;
            if ok == 0 {
                let e = last_err();
                if e.raw_os_error() == Some(ERROR_PIPE_CONNECTED_CODE) {
                    connected = true; // клиент успел раньше
                } else if e.raw_os_error() != Some(ERROR_IO_PENDING_CODE) {
                    unsafe { CloseHandle(event) };
                    return Err(e);
                }
            }
            if !connected {
                let ms = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX - 1);
                let waited = unsafe { WaitForSingleObject(event, ms) };
                if waited == WAIT_FAILED {
                    let e = last_err();
                    unsafe { CloseHandle(event) };
                    return Err(e);
                }
                if waited == WAIT_TIMEOUT {
                    unsafe { CloseHandle(event) };
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        format!("к каналу {} не подключились за {:?}", self.name, timeout),
                    ));
                }
                // операция завершена — забрать результат (без ожидания)
                let mut bytes: u32 = 0;
                let ok = unsafe { GetOverlappedResult(handle, &ov, &mut bytes, 0) };
                if ok == 0 {
                    let e = last_err();
                    unsafe { CloseHandle(event) };
                    return Err(e);
                }
            }
            unsafe { CloseHandle(event) };
            let end = PipeEnd {
                handle,
                write_lock: Mutex::new(()),
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
pub use imp::{connect, PipeEnd, PipeServer};

#[cfg(not(windows))]
mod imp {
    use super::*;

    pub struct PipeEnd;
    pub struct PipeServer;

    impl PipeServer {
        pub fn create_unique(_tag: &str) -> io::Result<Self> {
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
            let end = std::sync::Arc::new(srv.accept(Duration::from_secs(10)).expect("accept"));
            let r = std::sync::Arc::clone(&end);
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
        let client = std::sync::Arc::new(connect(&name, Duration::from_secs(10)).expect("connect"));
        let c = std::sync::Arc::clone(&client);
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
}
