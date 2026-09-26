//! Запись подготовленного вывода в поток.
//!
//! Макросы `print!`/`eprint!` паникуют при любой ошибке записи: Rust по
//! умолчанию игнорирует `SIGPIPE`, поэтому закрытый pipe (`anki-repo … | head`)
//! приводил бы к panic и exit code 101, которого нет в контракте CLI. Здесь
//! запись выполняется явно, а ошибка возвращается вызывающему коду, который
//! решает, считать ли её внутренней (закрытый читателем pipe — не ошибка).

use std::io::{self, Write};

/// Записывает текст целиком и сбрасывает буфер потока.
///
/// # Errors
///
/// Возвращает ошибку ввода-вывода самого потока, включая `BrokenPipe`.
pub fn write_text(stream: &mut dyn Write, text: &str) -> io::Result<()> {
    stream.write_all(text.as_bytes())?;
    stream.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Buffer(Vec<u8>);

    impl Write for Buffer {
        fn write(&mut self, data: &[u8]) -> io::Result<usize> {
            self.0.extend_from_slice(data);
            Ok(data.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// Поток, который всегда отказывает при записи.
    struct FailingWriter;

    impl Write for FailingWriter {
        fn write(&mut self, _data: &[u8]) -> io::Result<usize> {
            Err(io::Error::new(io::ErrorKind::BrokenPipe, "pipe closed"))
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// Поток, который принимает данные, но отказывает при flush.
    struct FailingFlush(Buffer);

    impl Write for FailingFlush {
        fn write(&mut self, data: &[u8]) -> io::Result<usize> {
            self.0.write(data)
        }

        fn flush(&mut self) -> io::Result<()> {
            Err(io::Error::other("flush failed"))
        }
    }

    #[test]
    fn text_is_written_and_flushed() {
        let mut buffer = Buffer::default();
        write_text(&mut buffer, "строка").expect("запись");
        assert_eq!(String::from_utf8(buffer.0).expect("utf-8"), "строка");
    }

    #[test]
    fn broken_pipe_is_reported_as_error() {
        let error = write_text(&mut FailingWriter, "строка").expect_err("ошибка записи");
        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
    }

    #[test]
    fn flush_error_is_reported_as_error() {
        let mut stream = FailingFlush(Buffer::default());
        write_text(&mut stream, "строка").expect_err("ошибка flush");
    }
}
