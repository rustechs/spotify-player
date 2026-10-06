use std::time::{Duration, Instant};

use ratatui::layout::Rect;

/// Widest the login popup gets (borders included); a longer URL wraps onto more rows.
pub const LOGIN_POPUP_MAX_WIDTH: u16 = 110;
/// Below this outer size the popup is not drawn at all.
const LOGIN_POPUP_MIN_WIDTH: u16 = 24;
const LOGIN_POPUP_MIN_HEIGHT: u16 = 5;

const SPINNER_FRAMES: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
const SPINNER_FRAME_DURATION: Duration = Duration::from_millis(100);

/// Keys the login popup reacts to; every other key is swallowed while it is open.
pub const LOGIN_POPUP_HINTS: &str =
    " o/enter: open the browser again │ c: copy the URL │ esc/q: cancel and quit ";

/// Where a browser login currently is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LoginPhase {
    /// The authorization page is open in the browser; its redirect has not arrived yet.
    WaitingForBrowser,
    /// The redirect arrived; the auth code is being exchanged for a token.
    Approved,
}

/// A browser login the TUI shows a modal popup for.
#[derive(Clone, Debug)]
pub struct PendingLogin {
    /// What the user is asked to approve, e.g. `Web API access for the configured client`.
    pub purpose: String,
    /// Spotify client id the authorization is requested under.
    pub client_id: String,
    /// Full authorization URL, shown so it can be copied or opened again.
    pub url: String,
    /// Where Spotify sends the browser after the approval.
    pub redirect_uri: String,
    pub phase: LoginPhase,
    pub started_at: Instant,
}

impl PendingLogin {
    pub fn new(
        purpose: impl Into<String>,
        client_id: impl Into<String>,
        url: impl Into<String>,
        redirect_uri: impl Into<String>,
    ) -> Self {
        Self {
            purpose: purpose.into(),
            client_id: client_id.into(),
            url: url.into(),
            redirect_uri: redirect_uri.into(),
            phase: LoginPhase::WaitingForBrowser,
            started_at: Instant::now(),
        }
    }

    pub fn headline(&self) -> String {
        format!(
            "Spotify asks you to approve {} (client id {}).",
            self.purpose, self.client_id
        )
    }

    /// Progress line, with the spinner frame for `now`.
    pub fn status_line(&self, now: Instant) -> String {
        let spinner = spinner_frame(now.saturating_duration_since(self.started_at));
        match self.phase {
            LoginPhase::WaitingForBrowser => format!(
                "{spinner} Waiting for your approval in the browser; Spotify then redirects to {}.",
                self.redirect_uri
            ),
            LoginPhase::Approved => {
                format!("{spinner} Approved in the browser, finishing the login…")
            }
        }
    }
}

/// Spinner frame for a login that has been running for `elapsed`.
pub fn spinner_frame(elapsed: Duration) -> char {
    let frame = (elapsed.as_millis() / SPINNER_FRAME_DURATION.as_millis()) as usize;
    SPINNER_FRAMES[frame % SPINNER_FRAMES.len()]
}

/// Split `url` into rows of at most `width` columns.
///
/// A URL has no natural break points, and each row has to hold a complete slice
/// so that selecting the rows reconstructs the URL; rows therefore break by
/// display width, never on words.
pub fn wrap_url_lines(url: &str, width: u16) -> Vec<String> {
    let width = usize::from(width);
    if width == 0 {
        return Vec::new();
    }
    let mut lines = Vec::new();
    let mut line = String::new();
    let mut line_width = 0;
    for ch in url.chars() {
        let ch_width = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if !line.is_empty() && line_width + ch_width > width {
            lines.push(std::mem::take(&mut line));
            line_width = 0;
        }
        line.push(ch);
        line_width += ch_width;
    }
    if !line.is_empty() {
        lines.push(line);
    }
    lines
}

/// Columns available for text inside the popup drawn into a content area
/// `content_width` columns wide.
pub fn login_popup_inner_width(content_width: u16) -> u16 {
    content_width.min(LOGIN_POPUP_MAX_WIDTH).saturating_sub(2)
}

/// Outer rectangle of the login popup inside `content`: centered, as wide as the
/// content allows up to [`LOGIN_POPUP_MAX_WIDTH`], `rows` tall (borders included)
/// up to the content height. `None` when the content is too small for a popup.
pub fn login_popup_area(content: Rect, rows: u16) -> Option<Rect> {
    if content.width < LOGIN_POPUP_MIN_WIDTH || content.height < LOGIN_POPUP_MIN_HEIGHT {
        return None;
    }
    let width = content.width.min(LOGIN_POPUP_MAX_WIDTH);
    let height = rows.clamp(LOGIN_POPUP_MIN_HEIGHT, content.height);
    Some(Rect {
        x: content.x + (content.width - width) / 2,
        y: content.y + (content.height - height) / 2,
        width,
        height,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const URL: &str = "https://accounts.spotify.com/authorize?response_type=code&client_id=0123456789abcdef0123456789abcdef&redirect_uri=http%3A%2F%2F127.0.0.1%3A8989%2Flogin&scope=user-read-playback-state+user-modify-playback-state&code_challenge_method=S256&code_challenge=E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM&state=c2VjcmV0c3RhdGU";

    #[test]
    fn url_rows_are_full_width_slices_that_rebuild_the_url() {
        let rows = wrap_url_lines(URL, 40);
        assert!(rows.len() > 1);
        let (last, full) = rows.split_last().unwrap();
        assert!(full.iter().all(|row| row.chars().count() == 40), "{rows:?}");
        assert!(last.chars().count() <= 40);
        assert_eq!(rows.concat(), URL);
    }

    #[test]
    fn url_rows_break_by_display_width() {
        assert_eq!(wrap_url_lines("日本語テスト", 4), ["日本", "語テ", "スト"]);
        assert_eq!(wrap_url_lines("abc", 10), ["abc"]);
        assert!(wrap_url_lines(URL, 0).is_empty());
        assert!(wrap_url_lines("", 10).is_empty());
    }

    #[test]
    fn spinner_cycles_through_its_frames() {
        assert_eq!(spinner_frame(Duration::ZERO), SPINNER_FRAMES[0]);
        assert_eq!(spinner_frame(Duration::from_millis(150)), SPINNER_FRAMES[1]);
        assert_eq!(spinner_frame(Duration::from_millis(950)), SPINNER_FRAMES[9]);
        assert_eq!(spinner_frame(Duration::from_secs(1)), SPINNER_FRAMES[0]);
        assert_eq!(spinner_frame(Duration::from_secs(3600)), SPINNER_FRAMES[0]);
    }

    #[test]
    fn popup_area_is_centered_and_capped() {
        let content = Rect::new(0, 3, 200, 40);
        let area = login_popup_area(content, 12).unwrap();
        assert_eq!(area.width, LOGIN_POPUP_MAX_WIDTH);
        assert_eq!(area.height, 12);
        assert_eq!(area.x, (200 - LOGIN_POPUP_MAX_WIDTH) / 2);
        assert_eq!(area.y, 3 + (40 - 12) / 2);
        assert_eq!(login_popup_inner_width(200), LOGIN_POPUP_MAX_WIDTH - 2);

        // Narrow content gives the popup its full width; a short one is never exceeded.
        let narrow = Rect::new(5, 0, 60, 10);
        assert_eq!(login_popup_area(narrow, 30), Some(narrow));
        assert_eq!(login_popup_inner_width(60), 58);
    }

    #[test]
    fn popup_area_is_none_for_tiny_content() {
        assert!(login_popup_area(Rect::new(0, 0, 10, 10), 5).is_none());
        assert!(login_popup_area(Rect::new(0, 0, 80, 3), 5).is_none());
    }

    #[test]
    fn status_line_follows_the_phase() {
        let mut login = PendingLogin::new(
            "Web API access for the configured client",
            "0123456789abcdef0123456789abcdef",
            URL,
            "http://127.0.0.1:8989/login",
        );
        assert_eq!(
            login.headline(),
            "Spotify asks you to approve Web API access for the configured client \
             (client id 0123456789abcdef0123456789abcdef)."
        );
        let waiting = login.status_line(login.started_at);
        assert!(waiting.starts_with(SPINNER_FRAMES[0]), "{waiting}");
        assert!(waiting.contains("Waiting for your approval"), "{waiting}");
        assert!(waiting.contains("http://127.0.0.1:8989/login"), "{waiting}");

        login.phase = LoginPhase::Approved;
        let approved = login.status_line(login.started_at + Duration::from_millis(100));
        assert!(approved.starts_with(SPINNER_FRAMES[1]), "{approved}");
        assert!(approved.contains("Approved"), "{approved}");
    }
}
