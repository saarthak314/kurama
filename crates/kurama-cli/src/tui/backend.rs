use std::{cell::RefCell, rc::Rc};

use ratatui::{
    backend::{Backend, ClearType, WindowSize},
    buffer::Cell,
    layout::{Position, Size},
};

pub struct SharedBackend<B> {
    inner: Rc<RefCell<B>>,
}

impl<B> SharedBackend<B> {
    pub fn new(inner: B) -> Self {
        Self {
            inner: Rc::new(RefCell::new(inner)),
        }
    }
}

impl<B> Clone for SharedBackend<B> {
    fn clone(&self) -> Self {
        Self {
            inner: Rc::clone(&self.inner),
        }
    }
}

impl<B> Backend for SharedBackend<B>
where
    B: Backend,
{
    type Error = B::Error;

    fn draw<'a, I>(&mut self, content: I) -> Result<(), Self::Error>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        self.inner.borrow_mut().draw(content)
    }

    fn append_lines(&mut self, line_count: u16) -> Result<(), Self::Error> {
        self.inner.borrow_mut().append_lines(line_count)
    }

    fn hide_cursor(&mut self) -> Result<(), Self::Error> {
        self.inner.borrow_mut().hide_cursor()
    }

    fn show_cursor(&mut self) -> Result<(), Self::Error> {
        self.inner.borrow_mut().show_cursor()
    }

    fn get_cursor_position(&mut self) -> Result<Position, Self::Error> {
        self.inner.borrow_mut().get_cursor_position()
    }

    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> Result<(), Self::Error> {
        self.inner.borrow_mut().set_cursor_position(position)
    }

    fn clear(&mut self) -> Result<(), Self::Error> {
        self.inner.borrow_mut().clear()
    }

    fn clear_region(&mut self, clear_type: ClearType) -> Result<(), Self::Error> {
        self.inner.borrow_mut().clear_region(clear_type)
    }

    fn size(&self) -> Result<Size, Self::Error> {
        self.inner.borrow().size()
    }

    fn window_size(&mut self) -> Result<WindowSize, Self::Error> {
        self.inner.borrow_mut().window_size()
    }

    fn flush(&mut self) -> Result<(), Self::Error> {
        self.inner.borrow_mut().flush()
    }
}

pub struct CursorTrackingBackend<B> {
    inner: B,
    cursor_position: Option<Position>,
    size: std::cell::Cell<Size>,
}

impl<B> CursorTrackingBackend<B> {
    pub const fn new(inner: B) -> Self {
        Self {
            inner,
            cursor_position: None,
            size: std::cell::Cell::new(Size::new(0, 0)),
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
        let width = if self.size.get().width == 0 {
            self.size()?.width
        } else {
            self.size.get().width
        };
        let result = self.inner.draw(content.inspect(|(x, y, cell)| {
            let cell_width = ratatui::text::Span::raw(cell.symbol()).width() as u16;
            last_position = Some(Position::new(
                x.saturating_add(cell_width).min(width.saturating_sub(1)),
                *y,
            ));
        }));
        if result.is_ok()
            && let Some(position) = last_position
        {
            self.cursor_position = Some(position);
        }
        if result.is_err() {
            self.cursor_position = None;
        }
        result
    }

    fn append_lines(&mut self, line_count: u16) -> Result<(), Self::Error> {
        let size = self.size()?;
        self.inner.append_lines(line_count)?;
        if let Some(position) = self.cursor_position {
            self.cursor_position = Some(Position::new(
                position.x.min(size.width.saturating_sub(1)),
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
            let size = self.size.get();
            return Ok(Position::new(
                position.x.min(size.width.saturating_sub(1)),
                position.y.min(size.height.saturating_sub(1)),
            ));
        }
        self.size()?;
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
        let size = self.inner.size()?;
        self.size.set(size);
        Ok(size)
    }

    fn window_size(&mut self) -> Result<WindowSize, Self::Error> {
        let size = self.inner.window_size()?;
        self.size.set(size.columns_rows);
        Ok(size)
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

        fn draw<'a, I>(&mut self, content: I) -> Result<(), Self::Error>
        where
            I: Iterator<Item = (u16, u16, &'a Cell)>,
        {
            for (x, y, cell) in content {
                self.cursor_position = Position::new(
                    x.saturating_add(ratatui::text::Span::raw(cell.symbol()).width() as u16)
                        .min(self.size.width.saturating_sub(1)),
                    y,
                );
            }
            Ok(())
        }

        fn append_lines(&mut self, line_count: u16) -> Result<(), Self::Error> {
            self.cursor_position.y = self
                .cursor_position
                .y
                .saturating_add(line_count)
                .min(self.size.height.saturating_sub(1));
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
    #[test]
    fn cursor_tracks_wide_cells_empty_draws_linefeeds_and_shrink() {
        let mut backend = CursorTrackingBackend::new(SingleQueryBackend {
            cursor_position: Position::new(3, 2),
            cursor_queries: 0,
            size: Size::new(10, 6),
        });
        assert_eq!(backend.get_cursor_position().unwrap(), Position::new(3, 2));
        let mut cell = Cell::default();
        cell.set_symbol("界");
        backend.draw([(3, 2, &cell)].into_iter()).unwrap();
        assert_eq!(backend.get_cursor_position().unwrap(), Position::new(5, 2));
        backend.draw(std::iter::empty()).unwrap();
        backend.append_lines(2).unwrap();
        assert_eq!(backend.get_cursor_position().unwrap(), Position::new(5, 4));
        assert_eq!(
            backend.get_cursor_position().unwrap(),
            backend.inner.cursor_position
        );
        backend.clear_region(ClearType::AfterCursor).unwrap();
        assert_eq!(backend.get_cursor_position().unwrap(), Position::new(5, 4));
        backend.inner.size = Size::new(4, 3);
        backend.size().unwrap();
        assert_eq!(backend.get_cursor_position().unwrap(), Position::new(3, 2));
        assert_eq!(backend.inner.cursor_queries, 1);
    }
}
