use ratatui::{
    layout::Rect,
    style::Modifier,
    widgets::{Block, Borders, Paragraph, Wrap},
    Frame,
};

use crate::state::{
    format_toast_body_text, toast_area, toast_body_height_for_message, toast_body_inner_lines,
    toast_box_width, toast_inner_text_width, toast_max_inner_lines, ToastKind, UIStateGuard,
};

/// Draw the current toast (and optional peek sliver) in the leftover content rect.
pub fn render_toasts(frame: &mut Frame, ui: &UIStateGuard, content: Rect) {
    if ui.toasts.is_empty() {
        return;
    }
    let Some(current) = ui.toasts.current() else {
        return;
    };
    let has_peek = ui.toasts.len() > 1;
    let box_width = toast_box_width(content.width);
    let inner_width = toast_inner_text_width(box_width);
    let max_inner_lines = toast_max_inner_lines();
    let body_height =
        toast_body_height_for_message(current.message.as_str(), inner_width, max_inner_lines);
    let Some((body, peek)) = toast_area(content, has_peek, body_height) else {
        return;
    };

    let style = match current.kind {
        ToastKind::Success => ui.theme.toast_success(),
        ToastKind::Error => ui.theme.toast_error(),
    };
    let title = match current.kind {
        ToastKind::Success => "Success",
        ToastKind::Error => "Error",
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(style)
        .title(title);
    let inner = block.inner(body);
    let body_text = format_toast_body_text(
        current.message.as_str(),
        inner.width,
        toast_body_inner_lines(inner.height),
    );
    frame.render_widget(block, body);
    // Body text is not bold so wrapped lines stay within the box; terminals
    // often overflow bold glyphs on long strings.
    frame.render_widget(
        Paragraph::new(body_text)
            .style(style.remove_modifier(Modifier::BOLD))
            .wrap(Wrap { trim: true }),
        inner,
    );

    if let (Some(peek_rect), Some(next)) = (peek, ui.toasts.peek()) {
        let peek_style = match next.kind {
            ToastKind::Success => ui.theme.toast_success(),
            ToastKind::Error => ui.theme.toast_error(),
        }
        .add_modifier(Modifier::DIM);
        let peek_title = format_toast_body_text(
            next.message.as_str(),
            toast_inner_text_width(peek_rect.width),
            1,
        );
        frame.render_widget(
            Block::default()
                .borders(Borders::LEFT | Borders::RIGHT | Borders::TOP)
                .border_style(peek_style)
                .title(peek_title),
            peek_rect,
        );
    }
}
