//! Find in the message (Ctrl+F): the bar and the page's search functions.

use super::types::MessageViewInput;
use super::MessageView;
use adw::prelude::*;
use relm4::prelude::*;

impl MessageView {
    pub(super) fn on_open_find(&mut self, sender: &ComponentSender<Self>) {
        // The toolbar button toggles: a second press collapses the bar.
        if self.find_open {
            sender.input_sender().emit(MessageViewInput::CloseFind);
            return;
        }
        if self
            .find_closed_at
            .take()
            .is_some_and(|t| t.elapsed() < std::time::Duration::from_millis(300))
        {
            return;
        }
        self.find_open = true;
        if let Some(entry) = self.find_entry.clone() {
            gtk::glib::idle_add_local_once(move || {
                entry.grab_focus();
            });
        }
    }

    pub(super) fn on_close_find(&mut self) {
        self.find_open = false;
        self.find_closed_at = Some(std::time::Instant::now());
        self.find_matches = None;
        if let Some(entry) = &self.find_entry {
            entry.set_text("");
        }
        self.eval_find("hylkiFindClear()");
    }

    pub(super) fn on_find_changed(&mut self, text: String) {
        let text = text.trim();
        if text.is_empty() {
            self.find_matches = None;
            self.eval_find("hylkiFindClear()");
            return;
        }
        // Into a JS single-quoted string: backslashes and quotes only.
        let quoted = text.replace('\\', "\\\\").replace('\'', "\\'");
        self.eval_find(&format!("hylkiFind('{quoted}')"));
    }
}

impl MessageView {
    /// Run one of the in-message find engine's calls (#103), logging failures.
    pub(super) fn eval_find(&self, js: &str) {
        self.run_js_or_warn(js, "find js");
    }
}
