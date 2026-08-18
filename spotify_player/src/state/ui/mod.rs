use crate::{
    config::{self, Theme},
    key,
    ui::{self, Orientation},
    utils::filtered_items_from_query,
};

#[cfg(feature = "image")]
use crate::ui::cover_image::CoverImage;
#[cfg(feature = "image")]
use ratatui_image::picker::Picker;

pub type UIStateGuard<'a> = parking_lot::MutexGuard<'a, UIState>;

mod page;
mod popup;
mod toast;

pub use page::*;
pub use popup::*;
pub use toast::*;

#[cfg(feature = "image")]
#[derive(Default)]
pub struct ImageRenderInfo {
    pub url: String,
    pub render_area: ratatui::layout::Rect,
    pub state: Option<CoverImage>,
}

#[cfg(feature = "image")]
impl std::fmt::Debug for ImageRenderInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImageRenderInfo")
            .field("url", &self.url)
            .field("render_area", &self.render_area)
            .field("state", &self.state.is_some())
            .finish()
    }
}

/// Application's UI state
#[derive(Debug)]
pub struct UIState {
    pub is_running: bool,
    pub theme: config::Theme,
    pub input_key_sequence: key::KeySequence,
    pub orientation: ui::Orientation,

    pub history: Vec<PageState>,
    pub popup: Option<PopupState>,
    pub toasts: ToastQueue,

    /// The rectangle representing the playback progress bar,
    /// which is mainly used to handle mouse click events (for seeking command)
    pub playback_progress_bar_rect: ratatui::layout::Rect,

    /// Count prefix for vim-style navigation (e.g., 5j, 10k)
    pub count_prefix: Option<usize>,

    #[cfg(feature = "image")]
    pub last_cover_image_render_info: ImageRenderInfo,

    #[cfg(feature = "image")]
    pub picker: Picker,
}

impl UIState {
    pub fn current_page(&self) -> &PageState {
        self.history.last().expect("non-empty history")
    }

    pub fn current_page_mut(&mut self) -> &mut PageState {
        self.history.last_mut().expect("non-empty history")
    }

    pub fn new_search_popup(&mut self) {
        self.current_page_mut().select(0);
        self.popup = Some(PopupState::Search {
            query: String::new(),
        });
    }

    pub fn new_page(&mut self, page: PageState) {
        self.popup = None;
        if let Some(current_page) = self.history.last() {
            if &page == current_page {
                return;
            }
        }
        self.history.push(page);
    }

    pub fn close_popup_or_dismiss_toast(&mut self) {
        close_popup_or_dismiss_toast(&mut self.popup, &mut self.toasts);
    }

    pub fn push_success_toast(&mut self, message: impl Into<String>) {
        self.push_success_toast_with_config(&config::get_config().app_config, message);
    }

    pub fn push_error_toast(&mut self, message: impl Into<String>) {
        self.push_error_toast_with_config(&config::get_config().app_config, message);
    }

    fn push_success_toast_with_config(
        &mut self,
        app_config: &config::AppConfig,
        message: impl Into<String>,
    ) {
        if !should_enqueue_toast(app_config.enable_toast, false) {
            return;
        }
        let timeout = std::time::Duration::from_secs(app_config.toast_success_timeout_secs);
        self.toasts.push(Toast::success(message, timeout));
    }

    fn push_error_toast_with_config(
        &mut self,
        app_config: &config::AppConfig,
        message: impl Into<String>,
    ) {
        if !should_enqueue_toast(app_config.enable_toast, false) {
            return;
        }
        let timeout = std::time::Duration::from_secs(app_config.toast_success_timeout_secs);
        self.toasts.push(Toast::error(message, timeout));
    }

    /// Return whether there exists a focused popup.
    ///
    /// Currently, only search popup is not focused when it's opened.
    pub fn has_focused_popup(&self) -> bool {
        match self.popup.as_ref() {
            None => false,
            Some(popup) => !matches!(popup, PopupState::Search { .. }),
        }
    }

    /// Get a list of items possibly filtered by a search query if exists a search popup
    pub fn search_filtered_items<'a, T: std::fmt::Display>(&self, items: &'a [T]) -> Vec<&'a T> {
        match self.popup {
            Some(PopupState::Search { ref query }) => filtered_items_from_query(query, items),
            _ => items.iter().collect::<Vec<_>>(),
        }
    }
}

use ratatui::layout::Rect;

impl Default for UIState {
    fn default() -> Self {
        Self {
            is_running: true,
            theme: Theme::default(),
            input_key_sequence: key::KeySequence { keys: vec![] },
            orientation: match crossterm::terminal::size() {
                Ok((columns, rows)) => ui::Orientation::from_size(columns, rows),
                Err(err) => {
                    tracing::warn!("Unable to get terminal size, error: {err:#}");
                    Orientation::default()
                }
            },

            history: vec![PageState::Library {
                state: LibraryPageUIState::new(),
            }],
            popup: None,
            toasts: ToastQueue::default(),

            playback_progress_bar_rect: Rect::default(),

            count_prefix: None,

            #[cfg(feature = "image")]
            last_cover_image_render_info: ImageRenderInfo::default(),

            // Will be reinitialize later in ui/mod.rs after init_ui()
            #[cfg(feature = "image")]
            picker: Picker::halfblocks(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toast_queue_respects_enable_flag() {
        let defaults = config::AppConfig::default();
        assert!(defaults.enable_toast);
        assert_eq!(defaults.toast_success_timeout_secs, 3);

        let mut disabled = config::AppConfig::default();
        disabled.enable_toast = false;

        let mut ui = UIState::default();
        ui.push_success_toast_with_config(&disabled, "ok");
        ui.push_error_toast_with_config(&disabled, "err");
        assert!(
            ui.toasts.is_empty(),
            "enable_toast=false must not enqueue success or error toasts"
        );

        assert!(!should_enqueue_toast(false, false));
        assert!(should_enqueue_toast(true, false));
    }

    #[test]
    fn new_page_does_not_clear_toasts() {
        let mut ui = UIState::default();
        ui.toasts.push(Toast::error(
            "api failed",
            std::time::Duration::from_secs(3),
        ));
        ui.new_page(PageState::Queue { scroll_offset: 0 });
        assert_eq!(ui.toasts.len(), 1);
        assert_eq!(
            ui.toasts.visible().next().map(|t| t.message.as_str()),
            Some("api failed")
        );
        assert!(ui.popup.is_none());
    }
}
