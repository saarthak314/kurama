use ratatui::layout::Rect;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResponsiveLayout {
    pub transcript: Rect,
    pub activity: Rect,
    pub input: Rect,
    pub footer: Rect,
}

impl ResponsiveLayout {
    pub fn for_area(area: Rect, input_height: u16, activity_visible: bool) -> Self {
        if area.is_empty() {
            return Self::empty(area);
        }

        let input_height = input_height.max(1).min(area.height);
        let mut remaining = area.height.saturating_sub(input_height);
        let activity_height = u16::from(activity_visible && remaining > 0);
        remaining = remaining.saturating_sub(activity_height);
        let footer_height = u16::from(remaining > 0);
        let transcript_height = remaining.saturating_sub(footer_height);

        let transcript = Rect::new(area.x, area.y, area.width, transcript_height);
        let activity = Rect::new(area.x, transcript.bottom(), area.width, activity_height);
        let input = Rect::new(area.x, activity.bottom(), area.width, input_height);
        let footer = Rect::new(area.x, input.bottom(), area.width, footer_height);

        Self {
            transcript,
            activity,
            input,
            footer,
        }
    }

    fn empty(area: Rect) -> Self {
        let empty = Rect::new(area.x, area.y, area.width, 0);
        Self {
            transcript: empty,
            activity: empty,
            input: empty,
            footer: empty,
        }
    }
}
