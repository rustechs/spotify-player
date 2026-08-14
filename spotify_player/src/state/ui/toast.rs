use std::collections::VecDeque;
use std::time::Instant;

use ratatui::layout::Rect;

/// Maximum number of toasts stored (current + waiting).
pub const TOAST_QUEUE_CAP: usize = 10;

const TOAST_MAX_WIDTH: u16 = 48;
const TOAST_BODY_HEIGHT: u16 = 3;
const TOAST_PEEK_HEIGHT: u16 = 1;

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
/// `None` when the content rect is too small to draw anything.
pub fn toast_area(content: Rect, has_peek: bool) -> Option<(Rect, Option<Rect>)> {
    if content.width == 0 || content.height == 0 {
        return None;
    }

    let width = content.width.min(TOAST_MAX_WIDTH);
    let peek_h = if has_peek { TOAST_PEEK_HEIGHT } else { 0 };
    let total_h = TOAST_BODY_HEIGHT.saturating_add(peek_h);
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
                if let Some((body, peek)) = toast_area(content, has_peek) {
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
        assert!(toast_area(Rect::new(0, 0, 0, 10), false).is_none());
        assert!(toast_area(Rect::new(0, 0, 10, 0), false).is_none());
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
