use std::time::Duration;

use ratatui::layout::Position;

pub(crate) fn cursor_position(timeout: Duration) -> Option<Position> {
    #[cfg(unix)]
    {
        unix::cursor_position(timeout).ok().flatten()
    }
    #[cfg(windows)]
    {
        let _ = timeout;
        crossterm::cursor::position()
            .ok()
            .map(|(x, y)| Position::new(x, y))
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = timeout;
        None
    }
}

#[cfg(unix)]
mod unix {
    use std::{
        fs::{File, OpenOptions},
        io::{self, Read, Write},
        time::{Duration, Instant},
    };

    use ratatui::layout::Position;
    use rustix::{
        event::{PollFd, PollFlags, Timespec, poll},
        fs::{OFlags, fcntl_getfl, fcntl_setfl},
    };

    const MAX_RESPONSE_BYTES: usize = 256;

    struct Tty {
        reader: File,
        writer: File,
        original_flags: OFlags,
    }

    impl Tty {
        fn open() -> io::Result<Self> {
            let reader = OpenOptions::new()
                .read(true)
                .open("/dev/stdin")
                .or_else(|_| OpenOptions::new().read(true).open("/dev/tty"))?;
            let writer = OpenOptions::new()
                .write(true)
                .open("/dev/stdout")
                .or_else(|_| OpenOptions::new().write(true).open("/dev/tty"))?;
            let original_flags = fcntl_getfl(&reader).map_err(io::Error::from)?;
            fcntl_setfl(&reader, original_flags | OFlags::NONBLOCK).map_err(io::Error::from)?;
            Ok(Self {
                reader,
                writer,
                original_flags,
            })
        }

        fn poll_readable(&self, timeout: Duration) -> io::Result<bool> {
            let timeout = Timespec::try_from(timeout)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
            let mut descriptors = [PollFd::new(&self.reader, PollFlags::IN)];
            let ready = poll(&mut descriptors, Some(&timeout)).map_err(io::Error::from)?;
            Ok(ready > 0 && descriptors[0].revents().contains(PollFlags::IN))
        }
    }

    impl Drop for Tty {
        fn drop(&mut self) {
            let _ = fcntl_setfl(&self.reader, self.original_flags);
        }
    }

    pub(super) fn cursor_position(timeout: Duration) -> io::Result<Option<Position>> {
        let mut tty = Tty::open()?;
        tty.writer.write_all(b"\x1b[6n")?;
        tty.writer.flush()?;

        let deadline = Instant::now() + timeout;
        let mut response = Vec::new();
        loop {
            let mut chunk = [0_u8; 64];
            match tty.reader.read(&mut chunk) {
                Ok(0) => return Ok(None),
                Ok(count) => {
                    response.extend_from_slice(&chunk[..count]);
                    if let Some(position) = parse_cursor_position(&response) {
                        return Ok(Some(position));
                    }
                    if response.len() >= MAX_RESPONSE_BYTES {
                        return Ok(None);
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) => return Err(error),
            }

            let now = Instant::now();
            if now >= deadline || !tty.poll_readable(deadline.saturating_duration_since(now))? {
                return Ok(None);
            }
        }
    }

    fn parse_cursor_position(response: &[u8]) -> Option<Position> {
        for (index, bytes) in response.windows(2).enumerate() {
            if bytes != b"\x1b[" {
                continue;
            }
            let payload = &response[index + 2..];
            let Some(end) = payload.iter().position(|byte| *byte == b'R') else {
                continue;
            };
            let Ok(payload) = std::str::from_utf8(&payload[..end]) else {
                continue;
            };
            let payload = payload.strip_prefix('?').unwrap_or(payload);
            let Some((row, column)) = payload.split_once(';') else {
                continue;
            };
            let Ok(row) = row.parse::<u16>() else {
                continue;
            };
            let Ok(column) = column.parse::<u16>() else {
                continue;
            };
            let row = row.saturating_sub(1);
            let column = column.saturating_sub(1);
            return Some(Position::new(column, row));
        }
        None
    }

    #[cfg(test)]
    mod tests {
        use super::parse_cursor_position;
        use ratatui::layout::Position;

        #[test]
        fn parses_cursor_position_amid_terminal_reports() {
            assert_eq!(
                parse_cursor_position(b"\x1b[I\x1b[12;34R"),
                Some(Position::new(33, 11))
            );
            assert_eq!(
                parse_cursor_position(b"\x1b[?7;9R"),
                Some(Position::new(8, 6))
            );
        }
    }
}
