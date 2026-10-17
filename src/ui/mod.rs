pub mod accounts;
pub mod attachment_drawer;
pub mod attachments_gallery;
pub mod carry_over;
pub mod chip_flow;
pub mod column_bin;
pub mod command_palette;
pub mod fade_label;
pub mod cloud_accounts;
pub mod translation_page;
pub mod compose;
pub mod contacts_browser;
pub mod contacts_page;
pub mod context_menu;
pub mod directories;
pub mod drop_zones;
pub mod emoji;
pub mod folder_picker;
pub mod grab_pill;
pub mod icon_picker;
pub mod initials;
pub mod launch;
pub mod lightbox;
pub mod message_list;
pub mod message_row;
pub mod message_view;
pub mod message_window;
pub mod notifications;
pub mod pgp_keys;
pub mod preferences;
pub mod print_preview;
pub mod recipient_row;
pub mod rich_editor;
pub mod send_morph;
pub mod sidebar;
pub mod theme_picker;
pub mod tool_flow;
pub mod web_signin;
pub mod welcome;

/// How long Focus Mode's parts take to slide and fade away (and back), in
/// milliseconds: the reader toolbar, the list header, the sidebar's
/// accounts and the list's avatars all move on this one clock.
pub const FOCUS_ANIM_MS: u32 = 320;

/// Give a Refresh button its face for the network's state: orange, with a
/// tooltip that says why, while the network is down (#442). The header's, the
/// peek's and the rail's buttons all wear it.
pub fn show_refresh_offline(button: &gtk::Button, offline: bool) {
    use gtk::prelude::*;
    let tip = if offline {
        crate::i18n::i18n(
            "Offline: there is no network connection. Mail syncs again once the network is back. Click to try now, or long-press for Status Bar",
        )
    } else {
        crate::i18n::i18n("Refresh or long-press for Status Bar")
    };
    button.set_tooltip_text(Some(tip.as_str()));
    if offline {
        button.add_css_class("refresh-offline");
    } else {
        button.remove_css_class("refresh-offline");
    }
}

/// Run `close` when `entry` lost focus, was left empty, and focus did not
/// move to another widget of its own row (the scope dropdown, a button, the
/// popover a dropdown opens). The check waits for an idle turn because the
/// new focus widget is not set yet while the leave event runs.
pub fn close_if_focus_left_row(entry: &gtk::SearchEntry, close: impl Fn() + 'static) {
    use gtk::prelude::*;
    if !entry.text().trim().is_empty() {
        return;
    }
    let entry = entry.clone();
    gtk::glib::idle_add_local_once(move || {
        let Some(row) = entry.parent() else { return };
        let Some(root) = entry.root() else { return };
        let focus = root.focus();
        let entry_widget: &gtk::Widget = entry.upcast_ref();
        let in_row = focus.is_some_and(|f| f != *entry_widget && (f == row || f.is_ancestor(&row)));
        if !in_row {
            close();
        }
    });
}

/// A style provider for the whole display that is loaded only when what it
/// holds changes. Loading one restyles every widget in every window, the
/// message list's rows included, and with hundreds of rows that takes longer
/// than the work the reload was for (#323).
pub struct DisplayCss {
    provider: gtk::CssProvider,
    css: std::cell::RefCell<String>,
}

impl DisplayCss {
    pub fn new() -> Self {
        let provider = gtk::CssProvider::new();
        if let Some(display) = gtk::gdk::Display::default() {
            gtk::style_context_add_provider_for_display(
                &display,
                &provider,
                gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
            );
        }
        DisplayCss { provider, css: Default::default() }
    }

    pub fn load(&self, css: String) {
        if *self.css.borrow() != css {
            self.provider.load_from_string(&css);
            *self.css.borrow_mut() = css;
        }
    }
}

/// Ctrl + scroll wheel as zoom, the way browsers have it: `step` gets 1 for
/// a step in (wheel up) and -1 for a step out. Without Ctrl, or while
/// `zooms` says no, the scroll goes on to the page. Capture phase, so a
/// WebView underneath never scrolls the same notch it zooms.
///
/// A touchpad reports pixels in small bites, so those add up to a step
/// rather than each being one: otherwise a short swipe ran the whole scale.
pub fn ctrl_scroll_zoom(
    zooms: impl Fn() -> bool + 'static,
    step: impl Fn(i8) + 'static,
) -> gtk::EventControllerScroll {
    use gtk::prelude::*;
    let scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::VERTICAL);
    scroll.set_propagation_phase(gtk::PropagationPhase::Capture);
    let pending = std::rc::Rc::new(std::cell::Cell::new(0.0f64));
    let p = pending.clone();
    scroll.connect_scroll_begin(move |_| p.set(0.0));
    scroll.connect_scroll(move |c, _, dy| {
        if !c.current_event_state().contains(gtk::gdk::ModifierType::CONTROL_MASK) || !zooms() {
            pending.set(0.0);
            return gtk::glib::Propagation::Proceed;
        }
        let per_step = match c.unit() {
            gtk::gdk::ScrollUnit::Wheel => 1.0,
            _ => 40.0,
        };
        let mut sum = pending.get();
        // Turning back starts afresh rather than first undoing what was owed.
        if sum * dy < 0.0 {
            sum = 0.0;
        }
        sum += dy;
        while sum.abs() >= per_step {
            step(if sum < 0.0 { 1 } else { -1 });
            sum -= per_step.copysign(sum);
        }
        pending.set(sum);
        gtk::glib::Propagation::Stop
    });
    scroll
}
