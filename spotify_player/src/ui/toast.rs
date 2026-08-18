use ratatui::{
    layout::Rect,
    style::Modifier,
    widgets::{Block, Borders, Paragraph, Wrap},
    Frame,
};

use crate::state::{
    format_toast_body_text, toast_body_height_for_message, toast_body_inner_lines, toast_box_width,
    toast_inner_text_width, toast_max_inner_lines, toast_stack_areas, ToastKind, UIStateGuard,
    TOAST_VISIBLE_COUNT,
};

/// Draw up to three toast cards and a `4+` marker for additional queued items.
pub fn render_toasts(frame: &mut Frame, ui: &UIStateGuard, content: Rect) {
    if ui.toasts.is_empty() {
        return;
    }
    let visible: Vec<_> = ui.toasts.visible().collect();
    if visible.is_empty() {
        return;
    }
    let box_width = toast_box_width(content.width);
    let inner_width = toast_inner_text_width(box_width);
    let max_inner_lines = toast_max_inner_lines();
    let heights: Vec<u16> = visible
        .iter()
        .map(|toast| {
            toast_body_height_for_message(toast.message.as_str(), inner_width, max_inner_lines)
        })
        .collect();
    let show_overflow = ui.toasts.len() > TOAST_VISIBLE_COUNT;
    let Some((areas, overflow)) = toast_stack_areas(content, &heights, show_overflow) else {
        return;
    };

    for (toast, area) in visible.into_iter().zip(areas) {
        let style = match toast.kind {
            ToastKind::Success => ui.theme.toast_success(),
            ToastKind::Error => ui.theme.toast_error(),
        };
        let title = match toast.kind {
            ToastKind::Success => "Success",
            ToastKind::Error => "Error",
        };

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(style)
            .title(title);
        let inner = block.inner(area);
        let body_text = format_toast_body_text(
            toast.message.as_str(),
            inner.width,
            toast_body_inner_lines(inner.height),
        );
        frame.render_widget(block, area);
        // Body text is not bold so wrapped lines stay within the box; terminals
        // often overflow bold glyphs on long strings.
        frame.render_widget(
            Paragraph::new(body_text)
                .style(style.remove_modifier(Modifier::BOLD))
                .wrap(Wrap { trim: true }),
            inner,
        );
    }

    if let Some(area) = overflow {
        frame.render_widget(
            Block::default()
                .borders(Borders::LEFT | Borders::RIGHT | Borders::TOP)
                .title("4+"),
            area,
        );
    }
}
