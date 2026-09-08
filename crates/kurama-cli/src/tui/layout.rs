use ratatui::layout::Rect;

use super::{Overlay, TuiState, composer::composer_height};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResponsiveLayout {
    pub transcript: Rect,
    pub activity: Rect,
    pub queue: Rect,
    pub input: Rect,
    pub footer: Rect,
}

impl ResponsiveLayout {
    pub fn for_area(
        area: Rect,
        input_height: u16,
        activity_visible: bool,
        queue_height: u16,
    ) -> Self {
        if area.is_empty() {
            return Self::empty(area);
        }

        let input_height = input_height.max(1).min(area.height);
        let mut remaining = area.height.saturating_sub(input_height);
        let footer_height = u16::from(remaining > 0);
        remaining = remaining.saturating_sub(footer_height);
        let queue_height = queue_height.min(remaining);
        remaining = remaining.saturating_sub(queue_height);
        let activity_height = u16::from(activity_visible && remaining > 0);
        remaining = remaining.saturating_sub(activity_height);
        let transcript_height = remaining;

        let transcript = Rect::new(area.x, area.y, area.width, transcript_height);
        let activity = Rect::new(area.x, transcript.bottom(), area.width, activity_height);
        let queue = Rect::new(area.x, activity.bottom(), area.width, queue_height);
        let input = Rect::new(area.x, queue.bottom(), area.width, input_height);
        let footer = Rect::new(area.x, input.bottom(), area.width, footer_height);

        Self {
            transcript,
            activity,
            queue,
            input,
            footer,
        }
    }

    fn empty(area: Rect) -> Self {
        let empty = Rect::new(area.x, area.y, area.width, 0);
        Self {
            transcript: empty,
            activity: empty,
            queue: empty,
            input: empty,
            footer: empty,
        }
    }
}

pub(crate) fn visible_activity_rect(area: Rect, state: &TuiState) -> Rect {
    let area = main_area(area);
    if area.is_empty()
        || state.transcript_view_expanded()
        || state.overlay() != Overlay::None
        || !state.activity().is_animated()
    {
        return Rect::new(area.x, area.y, area.width, 0);
    }

    ResponsiveLayout::for_area(
        area,
        composer_height(state, area.width),
        true,
        queue_height(state, area.width),
    )
    .activity
}

pub(crate) fn queue_height(state: &TuiState, _width: u16) -> u16 {
    if state.overlay() != Overlay::None || state.transcript_view_expanded() {
        return 0;
    }
    state.pending_prompts().count().min(3) as u16
}

pub(crate) fn main_area(area: Rect) -> Rect {
    let total_padding = area.width.saturating_sub(4).min(4);
    let left_padding = total_padding.saturating_add(1) / 2;
    Rect::new(
        area.x.saturating_add(left_padding),
        area.y,
        area.width.saturating_sub(total_padding),
        area.height,
    )
}
