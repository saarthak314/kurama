use ratatui::{
    backend::{Backend, ClearType, WindowSize},
    buffer::Cell,
    layout::{Position, Size},
};

pub struct CursorTrackingBackend<B> {
    inner: B,
    cursor_position: Option<Position>,
}

impl<B> CursorTrackingBackend<B> {
    pub const fn new(inner: B) -> Self {
        Self {
            inner,
            cursor_position: None,
        }
    }
}

impl<B> Backend for CursorTrackingBackend<B>
where
    B: Backend,
{
    type Error = B::Error;

    fn draw<'a, I>(&mut self, content: I) -> Result<(), Self::Error>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        let mut last_position = None;
        let result = self.inner.draw(content.inspect(|(x, y, _)| {
            last_position = Some(Position::new(*x, *y));
        }));
        if result.is_ok()
            && let Some(position) = last_position
        {
            self.cursor_position = Some(position);
        }
        result
    }

    fn append_lines(&mut self, line_count: u16) -> Result<(), Self::Error> {
        let size = self.inner.size()?;
        self.inner.append_lines(line_count)?;
        if let Some(position) = self.cursor_position {
            self.cursor_position = Some(Position::new(
                position
                    .x
                    .saturating_add(1)
                    .min(size.width.saturating_sub(1)),
                position
                    .y
                    .saturating_add(line_count)
                    .min(size.height.saturating_sub(1)),
            ));
        }
        Ok(())
    }

    fn hide_cursor(&mut self) -> Result<(), Self::Error> {
        self.inner.hide_cursor()
    }

    fn show_cursor(&mut self) -> Result<(), Self::Error> {
        self.inner.show_cursor()
    }

    fn get_cursor_position(&mut self) -> Result<Position, Self::Error> {
        if let Some(position) = self.cursor_position {
            return Ok(position);
        }
        let position = self.inner.get_cursor_position()?;
        self.cursor_position = Some(position);
        Ok(position)
    }

    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> Result<(), Self::Error> {
        let position = position.into();
        self.inner.set_cursor_position(position)?;
        self.cursor_position = Some(position);
        Ok(())
    }

    fn clear(&mut self) -> Result<(), Self::Error> {
        self.inner.clear()
    }

    fn clear_region(&mut self, clear_type: ClearType) -> Result<(), Self::Error> {
        self.inner.clear_region(clear_type)
    }

    fn size(&self) -> Result<Size, Self::Error> {
        self.inner.size()
    }

    fn window_size(&mut self) -> Result<WindowSize, Self::Error> {
        self.inner.window_size()
    }

    fn flush(&mut self) -> Result<(), Self::Error> {
        self.inner.flush()
    }
}

#[cfg(test)]
mod tests {
    use std::io;

    use ratatui::{
        Terminal, TerminalOptions, Viewport,
        backend::{Backend, ClearType, WindowSize},
        buffer::Cell,
        layout::{Position, Size},
    };

    use super::CursorTrackingBackend;

    struct SingleQueryBackend {
        cursor_position: Position,
        cursor_queries: usize,
        size: Size,
    }

    impl Backend for SingleQueryBackend {
        type Error = io::Error;

        fn draw<'a, I>(&mut self, _content: I) -> Result<(), Self::Error>
        where
            I: Iterator<Item = (u16, u16, &'a Cell)>,
        {
            Ok(())
        }

        fn hide_cursor(&mut self) -> Result<(), Self::Error> {
            Ok(())
        }

        fn show_cursor(&mut self) -> Result<(), Self::Error> {
            Ok(())
        }

        fn get_cursor_position(&mut self) -> Result<Position, Self::Error> {
            self.cursor_queries += 1;
            if self.cursor_queries > 1 {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "cursor position query timed out",
                ));
            }
            Ok(self.cursor_position)
        }

        fn set_cursor_position<P: Into<Position>>(
            &mut self,
            position: P,
        ) -> Result<(), Self::Error> {
            self.cursor_position = position.into();
            Ok(())
        }

        fn clear(&mut self) -> Result<(), Self::Error> {
            Ok(())
        }

        fn clear_region(&mut self, _clear_type: ClearType) -> Result<(), Self::Error> {
            Ok(())
        }

        fn size(&self) -> Result<Size, Self::Error> {
            Ok(self.size)
        }

        fn window_size(&mut self) -> Result<WindowSize, Self::Error> {
            Ok(WindowSize {
                columns_rows: self.size,
                pixels: Size::default(),
            })
        }

        fn flush(&mut self) -> Result<(), Self::Error> {
            Ok(())
        }
    }

    #[test]
    fn inline_transcript_commit_and_resize_reuse_the_initial_cursor_query() {
        let backend = CursorTrackingBackend::new(SingleQueryBackend {
            cursor_position: Position::new(0, 4),
            cursor_queries: 0,
            size: Size::new(80, 16),
        });
        let mut terminal = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Inline(8),
            },
        )
        .expect("initialize inline terminal");

        terminal
            .insert_before(1, |_| {})
            .expect("insert committed transcript");
        terminal.backend_mut().inner.size = Size::new(100, 20);
        terminal.autoresize().expect("resize inline terminal");
        terminal
            .backend_mut()
            .set_cursor_position(Position::new(7, 11))
            .expect("move cursor");

        assert_eq!(terminal.backend().inner.cursor_queries, 1);
        assert_eq!(
            terminal
                .backend_mut()
                .get_cursor_position()
                .expect("read tracked cursor"),
            Position::new(7, 11)
        );
        assert_eq!(terminal.backend().inner.cursor_queries, 1);
    }
}
