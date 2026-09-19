use ratatui::layout::Rect;

use super::{
    Overlay, TuiState,
    composer::{approval_height, composer_height},
};

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

        // Preserve two input content rows plus borders before reserving the footer.
        let footer_height = area.height.saturating_sub(input_height.clamp(1, 4)).min(2);
        let input_height = input_height.max(1).min(area.height - footer_height);
        let mut remaining = area.height - input_height - footer_height;
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

pub(crate) fn main_layout(area: Rect, state: &TuiState) -> ResponsiveLayout {
    let area = main_area(area);
    let input_height = match state.overlay() {
        Overlay::Approval | Overlay::ApprovalEdit => approval_height(state, area.width),
        Overlay::Shortcuts => 18.min(area.height).max(5),
        _ => composer_height(state, area.width),
    };
    let activity_visible = state.overlay() == Overlay::None
        && (state.activity().is_animated() || state.last_turn_elapsed().is_some());
    ResponsiveLayout::for_area(
        area,
        input_height,
        activity_visible,
        queue_height(state, area.width),
    )
}

pub(crate) fn visible_activity_rect(area: Rect, state: &TuiState) -> Rect {
    let inner = main_area(area);
    if inner.is_empty()
        || state.transcript_view_expanded()
        || state.overlay() != Overlay::None
        || !state.activity().is_animated()
    {
        return Rect::new(inner.x, inner.y, inner.width, 0);
    }

    main_layout(area, state).activity
}

pub(crate) fn queue_height(state: &TuiState, _width: u16) -> u16 {
    if state.overlay() != Overlay::None || state.transcript_view_expanded() {
        return 0;
    }
    state
        .pending_turn_count()
        .saturating_add(usize::from(state.pending_steering > 0))
        .min(3) as u16
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
