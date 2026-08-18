use ratatui::{
    layout::Rect,
    style::Modifier,
    widgets::{Block, Borders, Paragraph, Wrap},
    Frame,
};

use crate::state::{
    format_toast_body_text, toast_body_height_for_message, toast_box_width, toast_inner_text_width,
    toast_max_inner_lines, toast_stack_areas, ToastKind, UIState, TOAST_VISIBLE_COUNT,
};

/// Draw up to three toast cards and a `4+` marker for additional queued items.
pub fn render_toasts(frame: &mut Frame, ui: &UIState, content: Rect) {
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
        // `inner` already excludes the border rows. Passing it through
        // `toast_body_inner_lines` again zeros compact (height-3) cards.
        let body_text = format_toast_card_body(toast.message.as_str(), inner);
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

/// Wrap `message` to a `Block::inner` rect (borders already removed).
fn format_toast_card_body(message: &str, inner: Rect) -> String {
    format_toast_body_text(message, inner.width, inner.height)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::Toast;
    use ratatui::{
        backend::TestBackend,
        buffer::Buffer,
        widgets::{Block, Widget},
        Terminal,
    };
    use std::time::Duration;

    fn buffer_contains(buf: &Buffer, needle: &str) -> bool {
        let area = *buf.area();
        for y in area.y..area.bottom() {
            let mut row = String::new();
            for x in area.x..area.right() {
                row.push_str(buf[(x, y)].symbol());
            }
            if row.contains(needle) {
                return true;
            }
        }
        false
    }

    #[test]
    fn compact_toast_inner_keeps_short_message() {
        let area = Rect::new(0, 0, 20, 3);
        let mut buf = Buffer::empty(area);
        let block = Block::default().borders(Borders::ALL).title("Success");
        let inner = block.inner(area);
        assert_eq!(inner.height, 1);
        let body = format_toast_card_body("Copied link", inner);
        assert_eq!(body, "Copied link");
        Widget::render(block, area, &mut buf);
        Widget::render(Paragraph::new(body), inner, &mut buf);
        assert!(
            buffer_contains(&buf, "Copied link"),
            "compact toast body should show the message, got {buf:?}"
        );
    }

    #[test]
    fn compact_render_toasts_shows_short_message() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        let mut ui = UIState::default();
        ui.toasts
            .push(Toast::success("Copied link", Duration::from_secs(3)));
        terminal
            .draw(|frame| {
                render_toasts(frame, &ui, frame.area());
            })
            .expect("draw toasts");
        let buf = terminal.backend().buffer();
        assert!(
            buffer_contains(buf, "Success"),
            "render_toasts should draw the Success title, got {buf:?}"
        );
        assert!(
            buffer_contains(buf, "Copied link"),
            "render_toasts should paint the compact body, got {buf:?}"
        );
    }
}
