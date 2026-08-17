use std::collections::VecDeque;
use std::time::{Duration, Instant};

use ratatui::layout::Rect;

/// Maximum number of toasts stored (current + waiting).
pub const TOAST_QUEUE_CAP: usize = 10;

const TOAST_MAX_WIDTH: u16 = 60;
const TOAST_BODY_HEIGHT: u16 = 6;
const TOAST_BODY_MIN_HEIGHT: u16 = 3;
const TOAST_PEEK_HEIGHT: u16 = 1;
const TOAST_BODY_BORDER_ROWS: u16 = 2;
const TOAST_ELLIPSIS: char = '…';

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToastKind {
    Success,
    Error,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Toast {
    pub kind: ToastKind,
    pub message: String,
    /// `None` means sticky (errors). Success toasts set an expiry.
    pub expires_at: Option<Instant>,
}

impl Toast {
    pub fn success(message: impl Into<String>, timeout: Duration) -> Self {
        Self {
            kind: ToastKind::Success,
            message: message.into(),
            expires_at: Some(Instant::now() + timeout),
        }
    }

    pub fn error(message: impl Into<String>) -> Self {
        Self {
            kind: ToastKind::Error,
            message: message.into(),
            expires_at: None,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct ToastQueue {
    items: VecDeque<Toast>,
    /// Count of incoming toasts refused because the queue was already full.
    pub dropped_newest: usize,
}

impl ToastQueue {
    pub fn current(&self) -> Option<&Toast> {
        self.items.front()
    }

    pub fn peek(&self) -> Option<&Toast> {
        self.items.get(1)
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Push onto the FIFO. If the queue is at cap, drop this newest toast.
    /// Returns whether the toast was stored.
    pub fn push(&mut self, toast: Toast) -> bool {
        if self.items.len() >= TOAST_QUEUE_CAP {
            self.dropped_newest += 1;
            tracing::warn!(
                "Dropped newest toast (queue at cap {TOAST_QUEUE_CAP}): {}",
                toast.message
            );
            return false;
        }
        self.items.push_back(toast);
        true
    }

    /// Remove expired success toasts from the front. Sticky errors are never expired.
    pub fn expire_due(&mut self, now: Instant) {
        while let Some(front) = self.items.front() {
            match front.expires_at {
                Some(deadline) if deadline <= now => {
                    self.items.pop_front();
                }
                _ => break,
            }
        }
    }

    pub fn dismiss_current(&mut self) {
        self.items.pop_front();
    }
}

/// Whether a toast should be stored. Daemon and `enable_toast = false` skip enqueue.
pub(crate) fn should_enqueue_toast(enable_toast: bool, is_daemon: bool) -> bool {
    enable_toast && !is_daemon
}

/// If a popup is open, close it and leave the toast queue alone.
/// Otherwise dismiss the current toast.
pub fn close_popup_or_dismiss_toast<P>(popup: &mut Option<P>, toasts: &mut ToastQueue) {
    if popup.is_some() {
        *popup = None;
    } else {
        toasts.dismiss_current();
    }
}

/// Lower-right toast body (and optional peek sliver) clipped to `content`.
/// `body_height` is the bordered body block height (clamped to min/max defaults).
/// `None` when the content rect is too small to draw anything.
pub fn toast_area(content: Rect, has_peek: bool, body_height: u16) -> Option<(Rect, Option<Rect>)> {
    if content.width == 0 || content.height == 0 {
        return None;
    }

    let width = toast_box_width(content.width);
    let peek_h = if has_peek { TOAST_PEEK_HEIGHT } else { 0 };
    let body_height = body_height
        .clamp(TOAST_BODY_MIN_HEIGHT, TOAST_BODY_HEIGHT)
        .min(content.height.saturating_sub(peek_h));
    let total_h = body_height.saturating_add(peek_h);
    let draw_h = total_h.min(content.height);

    let x = content.x + content.width.saturating_sub(width);
    let y = content.y + content.height.saturating_sub(draw_h);

    if has_peek && draw_h > TOAST_PEEK_HEIGHT {
        let peek = Rect {
            x,
            y,
            width,
            height: TOAST_PEEK_HEIGHT,
        };
        let body = Rect {
            x,
            y: y + TOAST_PEEK_HEIGHT,
            width,
            height: draw_h.saturating_sub(TOAST_PEEK_HEIGHT),
        };
        if !rect_inside(body, content) || !rect_inside(peek, content) {
            return None;
        }
        Some((body, Some(peek)))
    } else {
        let body = Rect {
            x,
            y,
            width,
            height: draw_h,
        };
        if !rect_inside(body, content) {
            return None;
        }
        Some((body, None))
    }
}

fn rect_inside(inner: Rect, outer: Rect) -> bool {
    inner.x >= outer.x
        && inner.y >= outer.y
        && inner.x.saturating_add(inner.width) <= outer.x.saturating_add(outer.width)
        && inner.y.saturating_add(inner.height) <= outer.y.saturating_add(outer.height)
}

/// Toast box width inside `content`.
pub fn toast_box_width(content_width: u16) -> u16 {
    content_width.min(TOAST_MAX_WIDTH)
}

/// Inner text width inside a bordered toast box (left/right borders).
pub fn toast_inner_text_width(box_width: u16) -> u16 {
    box_width.saturating_sub(2)
}

/// Bordered body height for `message` (min one inner row, max four).
pub fn toast_body_height_for_message(message: &str, inner_width: u16, max_inner_lines: u16) -> u16 {
    let width = inner_width as usize;
    if width == 0 {
        return TOAST_BODY_MIN_HEIGHT;
    }
    let wrapped = wrap_toast_lines(message, width);
    let line_count = wrapped.len().max(1).min(max_inner_lines as usize);
    let inner_lines = line_count as u16;
    inner_lines
        .saturating_add(TOAST_BODY_BORDER_ROWS)
        .clamp(TOAST_BODY_MIN_HEIGHT, TOAST_BODY_HEIGHT)
}

/// Wrap `message` to `inner_width` and keep at most `max_lines` rows. When more
/// text would fit, the last row ends with `…`.
pub fn format_toast_body_text(message: &str, inner_width: u16, max_lines: u16) -> String {
    let width = inner_width as usize;
    let max_lines = max_lines as usize;
    if width == 0 || max_lines == 0 {
        return String::new();
    }

    let wrapped = wrap_toast_lines(message, width);
    if wrapped.is_empty() {
        return String::new();
    }
    if wrapped.len() <= max_lines {
        return wrapped.join("\n");
    }

    let mut lines: Vec<String> = wrapped.into_iter().take(max_lines).collect();
    if let Some(last) = lines.last_mut() {
        mark_toast_line_clipped(last, width);
    }
    lines.join("\n")
}

fn mark_toast_line_clipped(line: &mut String, max_width: usize) {
    if max_width == 0 {
        line.clear();
        return;
    }
    if max_width == 1 {
        line.clear();
        line.push(TOAST_ELLIPSIS);
        return;
    }
    let keep = max_width - 1;
    let len = line.chars().count();
    if len > keep {
        let mut out = String::new();
        for ch in line.chars().take(keep) {
            out.push(ch);
        }
        out.push(TOAST_ELLIPSIS);
        *line = out;
    } else {
        line.push(TOAST_ELLIPSIS);
    }
}

fn wrap_toast_lines(text: &str, width: usize) -> Vec<String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }

    let mut lines = Vec::new();
    let mut current = String::new();

    for word in trimmed.split_whitespace() {
        if word.chars().count() > width {
            if !current.is_empty() {
                lines.push(current);
                current = String::new();
            }
            let mut chunk = String::new();
            for ch in word.chars() {
                if chunk.chars().count() == width {
                    lines.push(chunk);
                    chunk = String::new();
                }
                chunk.push(ch);
            }
            if !chunk.is_empty() {
                current = chunk;
            }
            continue;
        }

        if current.is_empty() {
            current = word.to_string();
        } else if current.chars().count() + 1 + word.chars().count() <= width {
            current.push(' ');
            current.push_str(word);
        } else {
            lines.push(current);
            current = word.to_string();
        }
    }

    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

/// Maximum inner text rows in a full-size toast body.
pub fn toast_max_inner_lines() -> u16 {
    toast_body_inner_lines(TOAST_BODY_HEIGHT)
}

/// Inner text rows available in a bordered toast body of `body_height`.
pub fn toast_body_inner_lines(body_height: u16) -> u16 {
    body_height.saturating_sub(TOAST_BODY_BORDER_ROWS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn success(msg: &str, expires_at: Instant) -> Toast {
        Toast {
            kind: ToastKind::Success,
            message: msg.to_string(),
            expires_at: Some(expires_at),
        }
    }

    fn error(msg: &str) -> Toast {
        Toast {
            kind: ToastKind::Error,
            message: msg.to_string(),
            expires_at: None,
        }
    }

    #[test]
    fn toast_queue_peek() {
        let mut q = ToastQueue::default();
        let t0 = Instant::now() + Duration::from_secs(3);
        q.push(success("first", t0));
        q.push(success("second", t0));
        assert_eq!(q.current().map(|t| t.message.as_str()), Some("first"));
        assert_eq!(q.peek().map(|t| t.message.as_str()), Some("second"));
    }

    #[test]
    fn toast_queue_expire_due() {
        let mut q = ToastQueue::default();
        let past = Instant::now() - Duration::from_secs(1);
        let future = Instant::now() + Duration::from_secs(10);
        q.push(success("old", past));
        q.push(error("sticky"));
        q.push(success("later", future));
        q.expire_due(Instant::now());
        assert_eq!(q.current().map(|t| t.message.as_str()), Some("sticky"));
        assert_eq!(q.peek().map(|t| t.message.as_str()), Some("later"));
        q.expire_due(Instant::now() + Duration::from_secs(20));
        assert_eq!(q.current().map(|t| t.message.as_str()), Some("sticky"));
        assert_eq!(q.len(), 2);
    }

    #[test]
    fn toast_queue_drops_newest_at_cap() {
        let mut q = ToastQueue::default();
        let t0 = Instant::now() + Duration::from_secs(3);
        q.push(error("visible"));
        for i in 0..9 {
            assert!(q.push(success(&format!("n{i}"), t0)));
        }
        assert_eq!(q.len(), 10);
        assert!(!q.push(success("too-new", t0)));
        assert_eq!(q.len(), 10);
        assert_eq!(q.dropped_newest, 1);
        assert_eq!(q.current().map(|t| t.message.as_str()), Some("visible"));
    }

    #[test]
    fn toast_area_stays_inside_content() {
        let contents = [
            Rect::new(0, 0, 80, 24),
            Rect::new(10, 5, 40, 10),
            Rect::new(0, 20, 20, 8),
            Rect::new(0, 0, 10, 2),
            Rect::new(0, 0, 1, 1),
        ];
        for content in contents {
            for has_peek in [false, true] {
                if let Some((body, peek)) = toast_area(content, has_peek, TOAST_BODY_HEIGHT) {
                    assert!(
                        rect_inside(body, content),
                        "body {body:?} not in {content:?}"
                    );
                    if let Some(peek) = peek {
                        assert!(
                            rect_inside(peek, content),
                            "peek {peek:?} not in {content:?}"
                        );
                    }
                }
            }
        }
        assert!(toast_area(Rect::new(0, 0, 0, 10), false, TOAST_BODY_HEIGHT).is_none());
        assert!(toast_area(Rect::new(0, 0, 10, 0), false, TOAST_BODY_HEIGHT).is_none());
    }

    #[test]
    fn toast_area_uses_roomier_defaults_when_content_allows() {
        let content = Rect::new(0, 0, 80, 24);
        let (body, peek) = toast_area(content, false, TOAST_BODY_HEIGHT).expect("body");
        assert_eq!(body.width, TOAST_MAX_WIDTH);
        assert_eq!(body.height, TOAST_BODY_HEIGHT);
        assert!(peek.is_none());
        assert_eq!(body.x, content.width - TOAST_MAX_WIDTH);

        let (body, peek) = toast_area(content, true, TOAST_BODY_HEIGHT).expect("body+peek");
        assert_eq!(body.width, TOAST_MAX_WIDTH);
        assert_eq!(body.height, TOAST_BODY_HEIGHT);
        let peek = peek.expect("peek");
        assert_eq!(peek.height, TOAST_PEEK_HEIGHT);
        assert_eq!(peek.y + peek.height, body.y);
    }

    #[test]
    fn toast_body_height_fits_short_messages() {
        assert_eq!(
            toast_body_height_for_message("Copied link", 58, 4),
            TOAST_BODY_MIN_HEIGHT
        );
        let msg = "Failed: Could not start Spotify desktop: Connection refused (os error 111) while waking preferred device after Connect timeout";
        assert_eq!(toast_body_height_for_message(msg, 40, 4), TOAST_BODY_HEIGHT);
    }

    #[test]
    fn toast_area_uses_compact_height_for_short_message() {
        let content = Rect::new(0, 0, 80, 24);
        let height = toast_body_height_for_message("Copied link", 58, 4);
        let (body, _) = toast_area(content, false, height).expect("body");
        assert_eq!(body.height, TOAST_BODY_MIN_HEIGHT);
    }

    #[test]
    fn toast_body_height_uses_intermediate_rows_for_multi_line_messages() {
        const INNER_WIDTH: u16 = 58;
        let two_line =
            "Failed: Could not start Spotify desktop client because the connection timed out waiting";
        let three_line = "Failed: Could not start Spotify desktop: Connection refused (os error 111) while waking preferred device after Connect";

        assert_eq!(wrap_toast_lines(two_line, INNER_WIDTH as usize).len(), 2);
        assert_eq!(wrap_toast_lines(three_line, INNER_WIDTH as usize).len(), 3);
        assert_eq!(toast_body_height_for_message(two_line, INNER_WIDTH, 4), 4);
        assert_eq!(toast_body_height_for_message(three_line, INNER_WIDTH, 4), 5);
    }

    #[test]
    fn toast_area_uses_intermediate_body_height() {
        let content = Rect::new(0, 0, 80, 24);
        const INNER_WIDTH: u16 = 58;
        let two_line =
            "Failed: Could not start Spotify desktop client because the connection timed out waiting";
        let two_h = toast_body_height_for_message(two_line, INNER_WIDTH, 4);
        let (two_body, _) = toast_area(content, false, two_h).expect("two-line body");
        assert_eq!(two_h, 4);
        assert_eq!(two_body.height, 4);

        let three_line = "Failed: Could not start Spotify desktop: Connection refused (os error 111) while waking preferred device after Connect";
        let three_h = toast_body_height_for_message(three_line, INNER_WIDTH, 4);
        let (three_body, peek) = toast_area(content, true, three_h).expect("three-line body");
        assert_eq!(three_h, 5);
        assert_eq!(three_body.height, 5);
        let peek = peek.expect("peek sliver");
        assert_eq!(peek.y + peek.height, three_body.y);
    }

    #[test]
    fn toast_peek_title_clips_overflow_with_ellipsis() {
        let title = format_toast_body_text(
            "Failed: Could not start Spotify desktop: Connection refused while waking",
            40,
            1,
        );
        assert!(!title.contains('\n'));
        assert!(title.ends_with('…'));
        assert!(title.chars().count() <= 40);
    }

    #[test]
    fn toast_body_text_clips_overflow_with_ellipsis() {
        let msg = "Failed: Could not start Spotify desktop: Connection refused (os error 111) while waking preferred device after Connect timeout";
        let body = format_toast_body_text(msg, 40, 3);
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(lines[2].ends_with('…'));
        assert!(lines[2].chars().count() <= 40);
    }

    #[test]
    fn toast_body_text_wraps_without_clip_when_short() {
        let body = format_toast_body_text("Copied link", 58, 4);
        assert_eq!(body, "Copied link");
    }

    #[test]
    fn toast_body_inner_lines_matches_border_box() {
        assert_eq!(toast_body_inner_lines(TOAST_BODY_HEIGHT), 4);
        assert_eq!(toast_body_inner_lines(3), 1);
    }

    #[test]
    fn close_popup_or_dismiss_toast_search_still_open() {
        let mut q = ToastQueue::default();
        q.push(error("api failed"));
        let mut popup = Some("search");
        close_popup_or_dismiss_toast(&mut popup, &mut q);
        assert!(popup.is_none());
        assert_eq!(q.current().map(|t| t.message.as_str()), Some("api failed"));
    }

    #[test]
    fn close_popup_or_dismiss_toast_dismisses_when_no_popup() {
        let mut q = ToastQueue::default();
        q.push(error("api failed"));
        q.push(error("next"));
        let mut popup: Option<&str> = None;
        close_popup_or_dismiss_toast(&mut popup, &mut q);
        assert!(popup.is_none());
        assert_eq!(q.current().map(|t| t.message.as_str()), Some("next"));
    }
}
