use kurama_protocol::policy::ApprovalRequest;

#[derive(Debug, Clone)]
pub struct ApprovalState {
    pub request: ApprovalRequest,
    pub arguments: serde_json::Value,
    pub editor: String,
    pub editor_cursor: usize,
    pub validation_error: Option<String>,
    pub editing: bool,
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
        }
    }

    pub fn set_editor(&mut self, editor: impl Into<String>) {
        self.editor = editor.into();
        self.editor_cursor = self.editor.len();
        self.validation_error = None;
    }

    pub fn insert_str(&mut self, value: &str) {
        self.clamp_cursor();
        self.editor.insert_str(self.editor_cursor, value);
        self.editor_cursor = self.editor_cursor.saturating_add(value.len());
        self.validation_error = None;
    }

    pub fn backspace(&mut self) {
        self.clamp_cursor();
        let Some((previous, _)) = self.editor[..self.editor_cursor].char_indices().next_back()
        else {
            return;
        };
        self.editor.drain(previous..self.editor_cursor);
        self.editor_cursor = previous;
        self.validation_error = None;
    }

    pub fn delete(&mut self) {
        self.clamp_cursor();
        let Some(character) = self.editor[self.editor_cursor..].chars().next() else {
            return;
        };
        let next = self.editor_cursor.saturating_add(character.len_utf8());
        self.editor.drain(self.editor_cursor..next);
        self.validation_error = None;
    }

    pub fn move_left(&mut self) {
        self.clamp_cursor();
        if let Some((previous, _)) = self.editor[..self.editor_cursor].char_indices().next_back() {
            self.editor_cursor = previous;
        }
    }

    pub fn move_right(&mut self) {
        self.clamp_cursor();
        if let Some(character) = self.editor[self.editor_cursor..].chars().next() {
            self.editor_cursor = self.editor_cursor.saturating_add(character.len_utf8());
        }
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

    fn clamp_cursor(&mut self) {
        self.editor_cursor = self.editor_cursor.min(self.editor.len());
        while !self.editor.is_char_boundary(self.editor_cursor) {
            self.editor_cursor = self.editor_cursor.saturating_sub(1);
        }
    }
}
