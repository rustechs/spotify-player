use std::time::Instant;

use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::Line,
    widgets::{Block, BorderType, Borders, Clear, Paragraph},
    Frame,
};

use crate::{
    config,
    state::{
        login_popup_area, login_popup_inner_width, wrap_toast_lines, wrap_url_lines, PendingLogin,
        PopupState, UIStateGuard, LOGIN_POPUP_HINTS,
    },
};

/// Draw the modal login popup over `content` while a browser login is pending.
pub fn render_login_popup(frame: &mut Frame, ui: &UIStateGuard, content: Rect) {
    let Some(PopupState::Login(login)) = &ui.popup else {
        return;
    };
    let border_type = &config::get_config().app_config.border_type;
    render_pending_login(
        frame,
        login,
        &ui.theme,
        border_type,
        content,
        Instant::now(),
    );
}

fn render_pending_login(
    frame: &mut Frame,
    login: &PendingLogin,
    theme: &config::Theme,
    border_type: &config::BorderType,
    content: Rect,
    now: Instant,
) {
    let inner_width = login_popup_inner_width(content.width);
    // Wrapped here rather than by `Paragraph` so the row count is known up front.
    let headline = wrap_toast_lines(&login.headline(), usize::from(inner_width));
    let status = wrap_toast_lines(&login.status_line(now), usize::from(inner_width));
    let url_rows = wrap_url_lines(&login.url, inner_width);
    let (headline_rows, status_rows) = (headline.len() as u16, status.len() as u16);
    // Borders, the two text blocks, a blank row, the URL label, the URL rows.
    let rows = 2 + headline_rows + status_rows + 2 + url_rows.len() as u16;
    let Some(area) = login_popup_area(content, rows) else {
        return;
    };

    // The popup floats over the main layout, so reset the cells first or the
    // page shows through.
    frame.render_widget(Clear, area);
    frame.render_widget(Block::default().style(theme.app()), area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(modal_border_type(border_type))
        .border_style(theme.border())
        .title(Line::styled(" Spotify login ", theme.block_title()))
        .title_bottom(Line::styled(LOGIN_POPUP_HINTS, theme.block_title()));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let chunks = Layout::vertical([
        Constraint::Length(headline_rows),
        Constraint::Length(status_rows),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Fill(1),
    ])
    .split(inner);
    frame.render_widget(Paragraph::new(headline.join("\n")), chunks[0]);
    frame.render_widget(Paragraph::new(status.join("\n")), chunks[1]);
    frame.render_widget(
        Paragraph::new("Authorization URL (select it to copy, or press c):")
            .style(Style::default().add_modifier(Modifier::BOLD)),
        chunks[3],
    );
    // One row per slice and no wrapping: every row is a complete piece of the URL.
    frame.render_widget(Paragraph::new(url_rows.join("\n")), chunks[4]);
}

/// Border glyphs of the modal popup. A hidden border type still draws one: the
/// popup floats over other content and needs its outline.
fn modal_border_type(border_type: &config::BorderType) -> BorderType {
    match border_type {
        config::BorderType::Hidden | config::BorderType::Plain => BorderType::Plain,
        config::BorderType::Rounded => BorderType::Rounded,
        config::BorderType::Double => BorderType::Double,
        config::BorderType::Thick => BorderType::Thick,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{backend::TestBackend, Terminal};

    const URL: &str = "https://accounts.spotify.com/authorize?response_type=code&client_id=0123456789abcdef0123456789abcdef&redirect_uri=http%3A%2F%2F127.0.0.1%3A8989%2Flogin&scope=user-read-playback-state+user-modify-playback-state+user-read-currently-playing+streaming&code_challenge_method=S256&code_challenge=E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM&state=c2VjcmV0c3RhdGU";

    fn pending_login() -> PendingLogin {
        PendingLogin::new(
            "Web API access for the configured client",
            "0123456789abcdef0123456789abcdef",
            URL,
            "http://127.0.0.1:8989/login",
        )
    }

    /// Draw a page of `X`s with the login popup over it and return the screen rows.
    fn draw(width: u16, height: u16, login: &PendingLogin) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| {
                let area = frame.area();
                let page = vec!["X".repeat(area.width as usize); area.height as usize].join("\n");
                frame.render_widget(Paragraph::new(page), area);
                render_pending_login(
                    frame,
                    login,
                    &config::Theme::default(),
                    &config::BorderType::Plain,
                    area,
                    login.started_at,
                );
            })
            .unwrap();
        let buf = terminal.backend().buffer();
        (0..height)
            .map(|y| (0..width).map(|x| buf[(x, y)].symbol()).collect())
            .collect()
    }

    #[test]
    fn login_popup_covers_the_page_and_shows_the_whole_url() {
        let login = pending_login();
        // Wider than the popup cap, so the page stays visible beside the popup.
        let rows = draw(130, 30, &login);

        let top = rows
            .iter()
            .position(|r| r.contains("Spotify login"))
            .expect("title row");
        let bottom = rows
            .iter()
            .position(|r| r.contains("esc/q: cancel and quit"))
            .expect("hint row");
        assert!(bottom > top);
        // Nothing of the page shows through between the borders.
        for row in &rows[top + 1..bottom] {
            let left = row.find('│').unwrap();
            let right = row.rfind('│').unwrap();
            assert!(!row[left..right].contains('X'), "page leaked in: {row:?}");
        }
        // The page stays visible around the popup.
        assert!(rows[0].chars().all(|c| c == 'X'), "{:?}", rows[0]);
        assert!(rows[top].starts_with('X'), "{:?}", rows[top]);
        assert!(rows[top].ends_with('X'), "{:?}", rows[top]);

        // Every slice of the URL is on screen as one complete row.
        let inner_width = login_popup_inner_width(130);
        let slices = wrap_url_lines(URL, inner_width);
        assert!(slices.len() > 1);
        for slice in &slices {
            assert!(rows.iter().any(|r| r.contains(slice)), "missing {slice:?}");
        }
        assert!(rows
            .iter()
            .any(|r| r.contains("⠋ Waiting for your approval")));
        // The headline may wrap between the label and the id.
        assert!(rows.iter().any(|r| r.contains("approve Web API access")));
        assert!(rows
            .iter()
            .any(|r| r.contains("0123456789abcdef0123456789abcdef).")));
    }

    #[test]
    fn login_popup_is_skipped_when_the_content_is_too_small() {
        let rows = draw(20, 4, &pending_login());
        assert!(rows.iter().all(|r| r.chars().all(|c| c == 'X')), "{rows:?}");
    }
}
