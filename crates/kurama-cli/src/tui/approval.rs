use std::cell::Cell;

use super::{
    composer::editor_cursor_vertical,
    input::{grapheme_boundary_at_or_after, next_grapheme_boundary, previous_grapheme_boundary},
};

use kurama_protocol::policy::ApprovalRequest;

#[derive(Debug, Clone)]
pub struct ApprovalState {
    pub request: ApprovalRequest,
    pub arguments: serde_json::Value,
    pub editor: String,
    pub editor_cursor: usize,
    pub validation_error: Option<String>,
    pub editing: bool,
    pub selected: usize,
    pub(crate) editor_width: Cell<usize>,
}

impl ApprovalState {
    pub fn new(request: ApprovalRequest) -> Self {
        let arguments = request.arguments.clone();
        let editor = serde_json::to_string_pretty(&arguments).unwrap_or_else(|_| "{}".into());
        Self {
            request,
            arguments,
            editor_cursor: editor.len(),
            editor,
            validation_error: None,
            editing: false,
            selected: 0,
            editor_width: Cell::new(usize::MAX),
        }
    }

    pub fn select_next(&mut self) {
        self.selected = (self.selected + 1).min(3);
    }

    pub fn select_previous(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    pub fn set_editor(&mut self, editor: impl Into<String>) {
        self.editor = editor.into();
        self.editor_cursor = self.editor.len();
        self.validation_error = None;
    }

    pub fn insert_str(&mut self, value: &str) {
        self.clamp_cursor();
        self.editor.insert_str(self.editor_cursor, value);
        self.editor_cursor = grapheme_boundary_at_or_after(
            &self.editor,
            self.editor_cursor.saturating_add(value.len()),
        );
        self.validation_error = None;
    }

    pub fn backspace(&mut self) {
        self.clamp_cursor();
        let previous = previous_grapheme_boundary(&self.editor, self.editor_cursor);
        self.editor.drain(previous..self.editor_cursor);
        self.editor_cursor = previous;
        self.validation_error = None;
    }

    pub fn delete(&mut self) {
        self.clamp_cursor();
        let next = next_grapheme_boundary(&self.editor, self.editor_cursor);
        self.editor.drain(self.editor_cursor..next);
        self.validation_error = None;
    }

    pub fn move_left(&mut self) {
        self.clamp_cursor();
        self.editor_cursor = previous_grapheme_boundary(&self.editor, self.editor_cursor);
    }

    pub fn move_right(&mut self) {
        self.clamp_cursor();
        self.editor_cursor = next_grapheme_boundary(&self.editor, self.editor_cursor);
    }

    pub fn move_home(&mut self) {
        self.clamp_cursor();
        self.editor_cursor = self.editor[..self.editor_cursor]
            .rfind('\n')
            .map_or(0, |index| index.saturating_add(1));
    }

    pub fn move_end(&mut self) {
        self.clamp_cursor();
        self.editor_cursor = self.editor[self.editor_cursor..]
            .find('\n')
            .map_or(self.editor.len(), |offset| self.editor_cursor + offset);
    }

    pub fn move_up(&mut self) {
        self.clamp_cursor();
        if let Some(cursor) = editor_cursor_vertical(
            &self.editor,
            self.editor_cursor,
            self.editor_width.get(),
            -1,
        ) {
            self.editor_cursor = cursor;
        }
    }

    pub fn move_down(&mut self) {
        self.clamp_cursor();
        if let Some(cursor) =
            editor_cursor_vertical(&self.editor, self.editor_cursor, self.editor_width.get(), 1)
        {
            self.editor_cursor = cursor;
        }
    }

    fn clamp_cursor(&mut self) {
        self.editor_cursor = self.editor_cursor.min(self.editor.len());
        if self.editor_cursor < self.editor.len() {
            self.editor_cursor = previous_grapheme_boundary(
                &self.editor,
                next_grapheme_boundary(&self.editor, self.editor_cursor),
            );
        }
    }
}
