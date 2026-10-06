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

mod login;
mod page;
mod popup;
mod toast;

pub use login::*;
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
    /// Why the app stops, when the user has to see it: the UI thread prints
    /// it to stderr after restoring the terminal and exits with a failure code.
    pub exit_message: Option<String>,

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

    /// Stop the application and print `message` once the terminal is restored.
    pub fn quit_with_message(&mut self, message: impl Into<String>) {
        self.is_running = false;
        self.exit_message = Some(message.into());
    }

    pub fn show_login_popup(&mut self, login: PendingLogin) {
        self.popup = Some(PopupState::Login(login));
    }

    pub fn mark_login_approved(&mut self) {
        if let Some(PopupState::Login(login)) = &mut self.popup {
            login.phase = LoginPhase::Approved;
        }
    }

    /// Close the login popup; any other popup is left alone.
    pub fn close_login_popup(&mut self) {
        if matches!(self.popup, Some(PopupState::Login(_))) {
            self.popup = None;
        }
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
        if !app_config.enable_toast {
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
        if !app_config.enable_toast {
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
            exit_message: None,

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

    fn pending_login() -> PendingLogin {
        PendingLogin::new(
            "Web API access for the configured client",
            "client-id",
            "https://accounts.spotify.com/authorize?x=1",
            "http://127.0.0.1:8989/login",
        )
    }

    #[test]
    fn login_popup_follows_the_login() {
        let mut ui = UIState::default();
        ui.mark_login_approved();
        ui.close_login_popup();
        assert!(ui.popup.is_none(), "no-ops without a login popup");

        ui.show_login_popup(pending_login());
        assert!(ui.has_focused_popup(), "the login popup takes the focus");
        assert!(matches!(
            &ui.popup,
            Some(PopupState::Login(login)) if login.phase == LoginPhase::WaitingForBrowser
        ));

        ui.mark_login_approved();
        assert!(matches!(
            &ui.popup,
            Some(PopupState::Login(login)) if login.phase == LoginPhase::Approved
        ));

        ui.close_login_popup();
        assert!(ui.popup.is_none());
    }

    #[test]
    fn closing_the_login_popup_leaves_other_popups_alone() {
        let mut ui = UIState::default();
        ui.new_search_popup();
        ui.mark_login_approved();
        ui.close_login_popup();
        assert!(matches!(ui.popup, Some(PopupState::Search { .. })));
    }

    #[test]
    fn quit_with_message_stops_the_ui_and_keeps_the_message() {
        let mut ui = UIState::default();
        assert!(ui.is_running);
        assert!(ui.exit_message.is_none());
        ui.quit_with_message("Spotify login cancelled");
        assert!(!ui.is_running);
        assert_eq!(ui.exit_message.as_deref(), Some("Spotify login cancelled"));
    }
}
