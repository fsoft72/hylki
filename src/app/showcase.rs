//! The screenshot showcase: environment-driven hooks that stage the window
//! for the site and store screenshots and capture it. Demo and debug use only.
//!
//! The probes (`install_showcase_probes`) and the overlays work on real
//! profiles too; the staging (`stage_showcase`) runs only in the demo with
//! HYLKI_SHOWCASE set. Each HYLKI_SHOWCASE_* hook is one function in the
//! child module for what it stages, called in the order the hooks were
//! always armed (timers due at the same second fire in that order).

use relm4::prelude::*;

use super::demo::demo_mode;
use super::model::AppModel;
use super::msg::AppMsg;
use super::AppModelWidgets;

mod capture;
mod compose;
mod list;
mod messages;
mod probes;
mod settings;

pub(super) use capture::showcase_pgp;
use capture::{input_after, showcase_at, showcase_log_focus};

impl AppModel {
    /// Arms the probes that work on real profiles as much as the demo.
    pub(super) fn install_showcase_probes(&self, sender: &ComponentSender<Self>) {
        probes::probe_memory(sender);
        probes::probe_zoom(sender);
        probes::probe_delete_attachment(sender);
        self.probe_select();
        self.probe_translate();
        self.probe_card_delete();
        probes::probe_inbox(sender);
        self.probe_list_search();
        probes::probe_notify(sender);
    }

    /// Screenshot showcase (HYLKI_DEMO + HYLKI_SHOWCASE=/path.png): stage
    /// the demo the way the marketing shots want it — first row (the demo
    /// conversation, expanded via the threads_expanded preference) selected,
    /// one mid-thread card highlighted — then render the window to a PNG.
    /// Timers leave room for the WebViews to load and settle between steps.
    pub(super) fn stage_showcase(
        &self,
        root: &adw::ApplicationWindow,
        widgets: &AppModelWidgets,
        sender: &ComponentSender<Self>,
    ) {
        if demo_mode() {
            if let Ok(shot) = std::env::var("HYLKI_SHOWCASE") {
                // HYLKI_SHOWCASE_STAGE=0 skips the staging (capture-only, for
                // testing arbitrary states); HYLKI_SHOWCASE_DELAY overrides the
                // capture time (seconds, default 9).
                let stage = std::env::var("HYLKI_SHOWCASE_STAGE").as_deref() != Ok("0");
                let delay: u32 = std::env::var("HYLKI_SHOWCASE_DELAY")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(9);
                self.stage_heights(delay);
                if stage {
                    self.stage_select_first_row();
                    self.stage_palette(root);
                    self.stage_open_card();
                }
                self.stage_swipe(root);
                self.stage_row();
                self.stage_unsubscribe();
                self.stage_invite();
                self.stage_unified();
                self.stage_row_menu();
                list::stage_apply_filters(sender);
                list::stage_toolbar_gap(sender);
                list::stage_toolbar_menu(widgets, sender);
                list::stage_reader_menu(sender);
                list::stage_print(sender);
                list::stage_popout(sender);
                list::stage_edit_as_new(sender);
                list::stage_move(sender);
                self.stage_gallery(sender);
                self.stage_rail();
                list::stage_peek(root, sender);
                self.stage_toggle();
                self.stage_fold_filtered();
                compose::stage_focus(sender);
                compose::stage_folder(sender);
                compose::stage_send(sender);
                compose::stage_undo(sender);
                compose::stage_copy(sender);
                compose::stage_reply(root, sender);
                compose::stage_new(root, sender);
                compose::stage_forward(sender);
                compose::stage_compose_menu(sender);
                compose::stage_compose_preview(sender);
                compose::stage_compose_from(sender);
                compose::stage_compose_close(sender);
                compose::stage_compose_format(sender);
                compose::stage_compose_undo(sender);
                compose::stage_files(sender);
                compose::stage_flip(sender);
                compose::stage_theme(sender);
                settings::stage_shortcuts(sender);
                settings::stage_new_account(sender);
                settings::stage_account_settings(sender);
                settings::stage_editor_dirty(sender);
                settings::stage_settings_go(sender);
                settings::stage_add_contact(sender);
                settings::stage_dialog();
                self.stage_contacts(sender);
                // HYLKI_SHOWCASE_SETTINGS names a Settings panel to open
                // and to capture instead of the main window.
                let settings = std::env::var("HYLKI_SHOWCASE_SETTINGS").ok();
                capture::stage_settings_window(sender, settings.clone());
                capture::capture_window(root, delay, shot, settings);
            }
        }
    }

    /// HYLKI_SHOWCASE_ABOUT=1 opens the About window a beat after
    /// launch; pair it with HYLKI_SHOWCASE_TOP=1 to capture that window
    /// instead of the main one.
    pub(super) fn install_showcase_overlays(&self, sender: &ComponentSender<Self>) {
        if std::env::var("HYLKI_SHOWCASE_ABOUT").is_ok() {
            input_after(sender, 3, AppMsg::OpenAbout);
        }
        // Screenshot/dev hook: open the status bar console shortly after
        // launch (pref permitting) so captures can show it.
        if std::env::var("HYLKI_SHOWCASE_CONSOLE").is_ok() {
            input_after(sender, 3, AppMsg::OpenConsole);
        }
    }
}
