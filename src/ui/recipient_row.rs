//! A recipient field of the composer (To, Cc, Bcc): every address is a pill,
//! and the one being typed is text after them. Space after a whole address,
//! Return or a comma turns the text into a pill, so the field never ends in
//! a comma. Backspace on an empty line turns the last pill back into text to
//! edit, until the next space, Return or comma.
//!
//! Pills are selected like files: a click, Ctrl+click and Shift+click, or
//! the arrow keys from the start of the text, with Shift to extend, across
//! To, Cc and Bcc; or like text, by sweeping the pointer over them. The selection drags to another place in its field or to
//! another field, and its menu (right click, the menu key) moves it to
//! another field too; Delete removes it (#492).
//!
//! Built from libadwaita's own entry-row markup and class names (`row.entry`,
//! `.editable-area`, `.title`, `.subtitle`, `.edit-icon`, `.focused`), so the
//! row looks, focuses and turns red on an error the way the Subject row under
//! it does.

use std::cell::{Cell, RefCell};
use std::sync::OnceLock;

use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::{gdk, glib};
use gtk::glib::subclass::Signal;

use crate::i18n::{i18n, i18n_f};

/// Space between pills, and between lines of them.
const SPACING: i32 = 4;
/// The narrowest the text after the pills gets before it takes a line of
/// its own.
const TEXT_MIN: i32 = 96;

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct RecipientRow {
        pub header: gtk::Box,
        pub empty_title: gtk::Label,
        pub title: gtk::Label,
        pub flow: super::PillFlow,
        pub text: gtk::Text,
        pub edit_icon: gtk::Image,
        pub suffixes: gtk::Box,
        /// The pills in order, each with the recipient it stands for as it
        /// goes into the field ("Name <address>").
        pub pills: RefCell<Vec<(gtk::Widget, String)>>,
        /// Set while the row rewrites its own text, so that is not taken
        /// for typing.
        pub busy: Cell<bool>,
        pub focused: Cell<bool>,
        /// Every recipient field of the composer, this one included, in
        /// order: a selection spans them, and a pill's menu moves to them.
        pub fields: RefCell<Vec<glib::WeakRef<super::RecipientRow>>>,
        /// The pill a Shift+click or Shift+arrow selects from.
        pub anchor: RefCell<Option<gtk::Widget>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for RecipientRow {
        const NAME: &'static str = "HylkiRecipientRow";
        type Type = super::RecipientRow;
        type ParentType = adw::PreferencesRow;
    }

    impl ObjectImpl for RecipientRow {
        fn signals() -> &'static [Signal] {
            static SIGNALS: OnceLock<Vec<Signal>> = OnceLock::new();
            SIGNALS.get_or_init(|| {
                vec![
                    Signal::builder("changed").build(),
                    // A pill of this row starts (true) or ends (false) a drag.
                    Signal::builder("pill-drag").param_types([bool::static_type()]).build(),
                ]
            })
        }

        fn constructed(&self) {
            self.parent_constructed();
            let obj = self.obj();
            obj.add_css_class("entry");
            obj.add_css_class("recipient-row");
            obj.set_activatable(false);
            // The text takes the focus, never the row (see `focus`).
            obj.set_focusable(false);

            self.header.add_css_class("header");
            self.header.set_valign(gtk::Align::Center);

            let area = gtk::Box::new(gtk::Orientation::Horizontal, 0);
            area.add_css_class("editable-area");
            area.set_hexpand(true);

            for label in [&self.empty_title, &self.title] {
                label.set_halign(gtk::Align::Start);
                label.set_xalign(0.0);
                label.set_ellipsize(gtk::pango::EllipsizeMode::End);
                label.set_can_target(false);
                label.set_use_markup(false);
                obj.bind_property("title", label, "label").sync_create().build();
            }
            self.empty_title.add_css_class("title");
            self.empty_title.add_css_class("dimmed");
            self.empty_title.set_valign(gtk::Align::Center);
            self.title.add_css_class("subtitle");

            self.text.set_hexpand(true);
            self.text.set_valign(gtk::Align::Center);
            self.text.set_enable_undo(true);
            self.text.set_accessible_role(gtk::AccessibleRole::TextBox);
            self.text.update_relation(&[gtk::accessible::Relation::LabelledBy(&[self.title.upcast_ref::<gtk::Accessible>()])]);
            self.flow.add_css_class("recipient-flow");
            self.flow.set_text_child(&self.text);

            let column = gtk::Box::new(gtk::Orientation::Vertical, 0);
            column.set_hexpand(true);
            column.set_valign(gtk::Align::Center);
            column.append(&self.title);
            column.append(&self.flow);
            // The title sits where the text would be until there is some, as
            // in an entry row; it overlays the column so the row keeps its
            // height either way.
            let overlay = gtk::Overlay::new();
            overlay.set_hexpand(true);
            overlay.set_child(Some(&column));
            overlay.add_overlay(&self.empty_title);
            area.append(&overlay);

            self.edit_icon.set_icon_name(Some("adw-entry-edit-symbolic"));
            self.edit_icon.add_css_class("edit-icon");
            self.edit_icon.set_valign(gtk::Align::Center);
            self.edit_icon.set_can_target(false);
            self.edit_icon.set_accessible_role(gtk::AccessibleRole::Presentation);
            area.append(&self.edit_icon);
            obj.wire_focus(&area);

            self.header.append(&area);
            self.suffixes.add_css_class("suffixes");
            self.suffixes.set_spacing(6);
            self.suffixes.set_visible(false);
            self.header.append(&self.suffixes);
            obj.set_child(Some(&self.header));

            obj.wire();
            obj.update_state();
        }
    }

    impl WidgetImpl for RecipientRow {
        fn grab_focus(&self) -> bool {
            self.text.grab_focus()
        }

        fn focus(&self, direction: gtk::DirectionType) -> bool {
            self.header.child_focus(direction)
        }
    }

    impl ListBoxRowImpl for RecipientRow {}
    impl PreferencesRowImpl for RecipientRow {}
}

glib::wrapper! {
    pub struct RecipientRow(ObjectSubclass<imp::RecipientRow>)
        @extends adw::PreferencesRow, gtk::ListBoxRow, gtk::Widget,
        @implements gtk::Accessible, gtk::Actionable, gtk::Buildable, gtk::ConstraintTarget;
}

impl Default for RecipientRow {
    fn default() -> Self {
        glib::Object::new()
    }
}

impl RecipientRow {
    /// The whole field as one recipient list: the pills, then whatever is
    /// still being typed, comma separated.
    pub fn text(&self) -> String {
        let imp = self.imp();
        let mut all: Vec<String> = imp.pills.borrow().iter().map(|(_, r)| r.clone()).collect();
        let typed = self.typed();
        if !typed.is_empty() {
            all.push(typed);
        }
        all.join(", ")
    }

    /// Replace the field with these recipients, every one a pill.
    pub fn set_text(&self, field: &str) {
        let imp = self.imp();
        for (pill, _) in imp.pills.take() {
            imp.flow.remove(&pill);
        }
        self.set_typed("");
        self.push(field);
        self.emit_changed();
    }

    /// What is being typed after the pills, which suggestions are for.
    pub fn typed(&self) -> String {
        self.imp().text.text().trim().to_string()
    }

    /// Put a recipient in place of what is being typed (a suggestion
    /// picked from the list).
    pub fn complete(&self, recipient: &str) {
        self.set_typed("");
        self.push(recipient);
        self.emit_changed();
    }

    /// Add a recipient after the others, leaving what is being typed alone.
    pub fn add_recipient(&self, recipient: &str) {
        self.push(recipient);
        self.emit_changed();
    }

    pub fn add_suffix(&self, widget: &impl IsA<gtk::Widget>) {
        let imp = self.imp();
        imp.suffixes.append(widget);
        imp.suffixes.set_visible(true);
    }

    pub fn set_input_purpose(&self, purpose: gtk::InputPurpose) {
        self.imp().text.set_input_purpose(purpose);
    }

    /// Called after every change to the field: typing, a pill made or
    /// taken away.
    pub fn connect_changed<F: Fn(&Self) + 'static>(&self, f: F) -> glib::SignalHandlerId {
        self.connect_local("changed", false, move |values| {
            let row = values[0].get::<Self>().expect("RecipientRow");
            f(&row);
            None
        })
    }

    fn emit_changed(&self) {
        self.update_state();
        self.emit_by_name::<()>("changed", &[]);
    }

    fn wire(&self) {
        let imp = self.imp();
        let text = &imp.text;

        // Keys are not enough: an input method or a paste inserts text
        // without them, so separators are caught as the text goes in.
        let weak = self.downgrade();
        text.connect_insert_text(move |text, new, pos| {
            let Some(row) = weak.upgrade() else { return };
            if row.imp().busy.get() {
                return;
            }
            let current = text.text().to_string();
            let at = current.char_indices().nth((*pos).max(0) as usize).map_or(current.len(), |(i, _)| i);
            let (before, after) = current.split_at(at);
            if let Some(action) = on_insert(before, new, after) {
                text.stop_signal_emission_by_name("insert-text");
                match action {
                    Insert::Swallow => {}
                    Insert::Commit { done, rest } => {
                        row.push(&done);
                        row.set_typed(&rest);
                        row.emit_changed();
                    }
                }
            }
        });

        let weak = self.downgrade();
        text.connect_changed(move |_| {
            let Some(row) = weak.upgrade() else { return };
            if !row.imp().busy.get() {
                row.emit_changed();
            }
        });

        // Return makes a pill of whatever was typed. The composer catches
        // Return first while its suggestion list is open, to pick one.
        let weak = self.downgrade();
        text.connect_activate(move |_| {
            let Some(row) = weak.upgrade() else { return };
            let typed = row.typed();
            if !typed.is_empty() {
                row.complete(&typed);
            }
        });

        // Backspace on an empty line: the last pill becomes text again.
        let weak = self.downgrade();
        text.connect_backspace(move |text| {
            let Some(row) = weak.upgrade() else { return };
            if !text.text().is_empty() {
                return;
            }
            let Some((pill, recipient)) = row.imp().pills.borrow_mut().pop() else { return };
            text.stop_signal_emission_by_name("backspace");
            row.imp().flow.remove(&pill);
            row.set_typed(&recipient);
            row.emit_changed();
        });

        // Left from the start of the text selects the last pill, to go
        // through them with the keyboard, and Ctrl+A on an empty line
        // selects them all.
        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        let weak = self.downgrade();
        keys.connect_key_pressed(move |_, key, _, state| {
            let Some(row) = weak.upgrade() else { return glib::Propagation::Proceed };
            let text = &row.imp().text;
            let Some(last) = row.pill_list().pop() else { return glib::Propagation::Proceed };
            let ctrl = state.contains(gdk::ModifierType::CONTROL_MASK);
            if ctrl && matches!(key, gdk::Key::a | gdk::Key::A) && text.text().is_empty() {
                row.select_all();
                last.grab_focus();
                return glib::Propagation::Stop;
            }
            let towards = if row.direction() == gtk::TextDirection::Rtl { gdk::Key::Right } else { gdk::Key::Left };
            let other = state.intersects(gdk::ModifierType::CONTROL_MASK | gdk::ModifierType::ALT_MASK);
            if key != towards || other || text.position() != 0 || text.selection_bounds().is_some() {
                return glib::Propagation::Proceed;
            }
            row.select_only(&last);
            last.grab_focus();
            glib::Propagation::Stop
        });
        text.add_controller(keys);

        // Going back to typing lets go of the selection.
        let focus = gtk::EventControllerFocus::new();
        let weak = self.downgrade();
        focus.connect_enter(move |_| {
            if let Some(row) = weak.upgrade() {
                row.clear_selection();
            }
        });
        text.add_controller(focus);

        // Swept with the pointer from anywhere but a pill, the pills are
        // selected the way dragging over text selects it, on into the other
        // fields; Ctrl keeps those already selected. A press on the text
        // where there is some is left to it, to select text with; one
        // anywhere else is taken from the text at once, which would claim
        // it, and is a click into the text when it does not move. In the
        // capture phase, to see the press before the text does.
        let sweep = gtk::GestureDrag::new();
        sweep.set_propagation_phase(gtk::PropagationPhase::Capture);
        // From where among all the pills it started, the selection Ctrl
        // kept, and whether it has moved enough to be a sweep.
        let swept: std::rc::Rc<RefCell<Option<(usize, Vec<gtk::Widget>, bool)>>> = Default::default();
        let weak = self.downgrade();
        let s = swept.clone();
        sweep.connect_drag_begin(move |gesture, x, y| {
            let Some(row) = weak.upgrade() else { return };
            let text = &row.imp().text;
            let on = row.pick(x, y, gtk::PickFlags::DEFAULT);
            let on_text = on.as_ref().is_some_and(|w| w.is_ancestor(text) || w == text.upcast_ref::<gtk::Widget>()) && !text.text().is_empty();
            if on_text || on.is_none_or(|w| is_in_pill(&w) || w.ancestor(gtk::Button::static_type()).is_some()) {
                gesture.set_state(gtk::EventSequenceState::Denied);
                return;
            }
            gesture.set_state(gtk::EventSequenceState::Claimed);
            let kept = if gesture.current_event_state().contains(gdk::ModifierType::CONTROL_MASK) {
                row.all_pills().into_iter().filter(is_selected).collect()
            } else {
                Vec::new()
            };
            s.replace(Some((row.sweep_index(x, y), kept, false)));
        });
        let weak = self.downgrade();
        let s = swept.clone();
        sweep.connect_drag_update(move |gesture, dx, dy| {
            let (Some(row), Some((x, y))) = (weak.upgrade(), gesture.start_point()) else { return };
            let mut swept = s.borrow_mut();
            let Some((from, kept, moved)) = swept.as_mut() else { return };
            *moved = *moved || dx.hypot(dy) >= row.settings().gtk_dnd_drag_threshold() as f64;
            if !*moved {
                return;
            }
            let to = row.sweep_index(x + dx, y + dy);
            let range = (*from).min(to)..(*from).max(to);
            for (i, p) in row.all_pills().iter().enumerate() {
                set_selected(p, range.contains(&i) || kept.contains(p));
            }
        });
        let weak = self.downgrade();
        sweep.connect_drag_end(move |gesture, dx, dy| {
            let Some((from, kept, moved)) = swept.take() else { return };
            let (Some(row), Some((x, y))) = (weak.upgrade(), gesture.start_point()) else { return };
            let to = row.sweep_index(x + dx, y + dy);
            if !moved || (from == to && kept.is_empty()) {
                row.imp().text.grab_focus();
                return;
            }
            // Shift+click goes on from where the sweep started, and the
            // focus is on the pill it ended at.
            let pills = row.all_pills();
            let (start, end) = if to < from { (from - 1, to) } else { (from, to.max(1) - 1) };
            let end = if from == to { kept.last() } else { pills.get(end) };
            if let Some(start) = pills.get(start).filter(|_| from != to) {
                if let Some(field) = start.ancestor(RecipientRow::static_type()).and_downcast::<RecipientRow>() {
                    field.imp().anchor.replace(Some(start.clone()));
                }
            }
            if let Some(end) = end {
                end.grab_focus();
            }
        });
        self.add_controller(sweep);

        // A pill dragged here, from this field or another. In the capture
        // phase, ahead of the text's own target, which would take the pill
        // as typed text.
        let target = gtk::DropTarget::new(gtk::Widget::static_type(), gdk::DragAction::MOVE);
        target.set_propagation_phase(gtk::PropagationPhase::Capture);
        target.set_preload(true);
        let weak = self.downgrade();
        target.connect_accept(move |_, drop| {
            weak.upgrade().is_some()
                && drop.formats().contains_type(gtk::Widget::static_type())
                && drop.actions().contains(gdk::DragAction::MOVE)
        });
        let weak = self.downgrade();
        target.connect_motion(move |target, x, y| {
            let Some(row) = weak.upgrade() else { return gdk::DragAction::empty() };
            let dragged = target.value().and_then(|v| v.get::<gtk::Widget>().ok());
            let at = row.drop_index(x, y);
            // No marker where letting go leaves the pills as they are, nor
            // in a field with no pills, where its own highlight says enough.
            let moving = dragged.as_ref().map(|p| acting_on(p)).unwrap_or_default();
            let stays = row.would_stay(&moving, at);
            let empty = row.imp().pills.borrow().is_empty();
            row.imp().flow.set_marker((!stays && !empty).then_some(at));
            gdk::DragAction::MOVE
        });
        let weak = self.downgrade();
        target.connect_leave(move |_| {
            if let Some(row) = weak.upgrade() {
                row.imp().flow.set_marker(None);
            }
        });
        let weak = self.downgrade();
        target.connect_drop(move |_, value, x, y| {
            let Some(row) = weak.upgrade() else { return false };
            row.imp().flow.set_marker(None);
            let Ok(pill) = value.get::<gtk::Widget>() else { return false };
            row.take_pills(acting_on(&pill), row.drop_index(x, y));
            true
        });
        self.add_controller(target);
    }

    /// The field looks focused while its text or one of its pills has the
    /// focus, as an entry row does while its text has.
    fn wire_focus(&self, area: &gtk::Box) {
        let focus = gtk::EventControllerFocus::new();
        let weak = self.downgrade();
        focus.connect_enter(move |_| {
            if let Some(row) = weak.upgrade() {
                row.imp().focused.set(true);
                row.update_state();
            }
        });
        let weak = self.downgrade();
        focus.connect_leave(move |_| {
            let Some(row) = weak.upgrade() else { return };
            row.imp().focused.set(false);
            row.update_state();
            // A whole address left behind becomes a pill like the rest, once
            // the focus has gone to another field (a pill of this one keeps
            // it here). Not when the window loses it: someone who switched
            // away to look the rest of an address up comes back to the text
            // as they left it.
            let weak = row.downgrade();
            glib::idle_add_local_once(move || {
                let Some(row) = weak.upgrade() else { return };
                // The selection goes when the focus goes somewhere else:
                // not to a pill of another field, nor to the pills' menu,
                // nor nowhere for a moment as that menu closes.
                let focus = row.root().and_then(|r| r.focus());
                let gone = focus.is_some_and(|w| !is_in_pill(&w) && w.ancestor(gtk::Popover::static_type()).is_none());
                if gone {
                    row.clear_selection();
                }
                let active = row.root().and_downcast::<gtk::Window>().is_some_and(|w| w.is_active());
                let typed = row.typed();
                if active && !row.imp().focused.get() && is_whole_address(&typed) {
                    row.complete(&typed);
                }
            });
        });
        area.add_controller(focus);
    }

    /// Make pills of every recipient in `field`, after the others.
    fn push(&self, field: &str) {
        for (name, addr) in crate::worker::parse_recipients(&normalise_separators(field)) {
            if addr.is_empty() {
                continue;
            }
            let recipient = crate::worker::format_recipient(&name, &addr);
            let at = self.imp().pills.borrow().len();
            self.insert_pill(at, &name, &addr, &recipient);
        }
    }

    /// Make a pill of one recipient and put it `at` that place among the
    /// others.
    fn insert_pill(&self, at: usize, name: &str, addr: &str, recipient: &str) -> gtk::Widget {
        let imp = self.imp();
        let pill = self.pill(name, addr, recipient).upcast::<gtk::Widget>();
        let next = imp.pills.borrow().get(at).map(|(p, _)| p.clone());
        imp.flow.place(&pill, next.as_ref());
        let mut pills = imp.pills.borrow_mut();
        let at = at.min(pills.len());
        pills.insert(at, (pill.clone(), recipient.to_string()));
        pill
    }

    fn pill(&self, name: &str, addr: &str, recipient: &str) -> gtk::Box {
        let pill = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        pill.add_css_class("recipient-pill");
        // Focusable for the keyboard (see `pill_key`), but Tab passes over
        // the pills (see the flow's `focus`).
        pill.set_focusable(true);
        let valid = crate::worker::invalid_recipient(recipient).is_none();
        if !valid {
            pill.add_css_class("invalid");
        }
        let shown = if name.is_empty() { addr } else { name };
        pill.update_property(&[gtk::accessible::Property::Label(shown)]);
        let label = gtk::Label::new(Some(shown));
        label.set_ellipsize(gtk::pango::EllipsizeMode::End);
        label.set_max_width_chars(32);
        label.set_single_line_mode(true);
        pill.append(&label);
        pill.set_tooltip_text(Some(if valid {
            addr.to_string()
        } else {
            i18n_f("“{addr}” is not an email address.", &[("addr", addr)])
        }
        .as_str()));

        let remove = gtk::Button::from_icon_name("window-close-symbolic");
        remove.add_css_class("flat");
        remove.add_css_class("circular");
        remove.set_focusable(false);
        remove.set_focus_on_click(false);
        remove.set_valign(gtk::Align::Center);
        remove.set_tooltip_text(Some(i18n("Remove").as_str()));
        remove.update_property(&[gtk::accessible::Property::Label(&i18n_f("Remove {name}", &[("name", shown)]))]);
        let weak = self.downgrade();
        let weak_pill = pill.downgrade();
        remove.connect_clicked(move |_| {
            let (Some(row), Some(pill)) = (weak.upgrade(), weak_pill.upgrade()) else { return };
            row.remove_pill(pill.upcast_ref());
        });
        pill.append(&remove);

        // A click selects the pill, Ctrl+click adds it to the selection or
        // takes it out, and Shift+click selects from the last one clicked.
        // A plain click on a pill already selected keeps the others until
        // the button is up, so they can all be dragged. A double click
        // edits the pill: it becomes the text, and what was being typed is
        // kept as a pill of its own first.
        let click = gtk::GestureClick::new();
        let collapse = std::rc::Rc::new(Cell::new(false));
        let weak = self.downgrade();
        let weak_pill = pill.downgrade();
        let c = collapse.clone();
        click.connect_pressed(move |gesture, n, x, y| {
            let (Some(row), Some(pill)) = (weak.upgrade(), weak_pill.upgrade()) else { return };
            let pill = pill.upcast::<gtk::Widget>();
            let on_remove = pill.pick(x, y, gtk::PickFlags::DEFAULT).is_some_and(|w| w.ancestor(gtk::Button::static_type()).is_some());
            if on_remove {
                return;
            }
            let state = gesture.current_event_state();
            c.set(false);
            if state.contains(gdk::ModifierType::CONTROL_MASK) {
                set_selected(&pill, !is_selected(&pill));
                row.imp().anchor.replace(Some(pill.clone()));
            } else if state.contains(gdk::ModifierType::SHIFT_MASK) {
                row.select_range(&pill);
            } else if n == 2 {
                gesture.set_state(gtk::EventSequenceState::Claimed);
                row.edit_pill(&pill);
                return;
            } else if is_selected(&pill) {
                c.set(true);
            } else {
                row.select_only(&pill);
            }
            pill.grab_focus();
        });
        let weak = self.downgrade();
        let weak_pill = pill.downgrade();
        let c = collapse.clone();
        click.connect_released(move |_, _, _, _| {
            let (Some(row), Some(pill)) = (weak.upgrade(), weak_pill.upgrade()) else { return };
            if c.take() {
                row.select_only(pill.upcast_ref());
            }
        });
        // Past the click's distance it is a drag, which takes them all.
        click.connect_stopped(move |_| collapse.set(false));
        pill.add_controller(click);

        let menu = gtk::GestureClick::new();
        menu.set_button(gdk::BUTTON_SECONDARY);
        let weak = self.downgrade();
        let weak_pill = pill.downgrade();
        menu.connect_pressed(move |gesture, _, x, y| {
            let (Some(row), Some(pill)) = (weak.upgrade(), weak_pill.upgrade()) else { return };
            gesture.set_state(gtk::EventSequenceState::Claimed);
            let pill = pill.upcast::<gtk::Widget>();
            if !is_selected(&pill) {
                row.select_only(&pill);
            }
            pill.grab_focus();
            row.pill_menu(&pill, x, y);
        });
        pill.add_controller(menu);

        let keys = gtk::EventControllerKey::new();
        let weak = self.downgrade();
        let weak_pill = pill.downgrade();
        keys.connect_key_pressed(move |_, key, _, state| {
            let (Some(row), Some(pill)) = (weak.upgrade(), weak_pill.upgrade()) else {
                return glib::Propagation::Proceed;
            };
            row.pill_key(pill.upcast_ref(), key, state)
        });
        pill.add_controller(keys);

        // Dragged to another place in this field or into another, with the
        // rest of the selection when it is part of one. The drag carries
        // the pill itself, which only a recipient field takes.
        let drag = gtk::DragSource::new();
        drag.set_actions(gdk::DragAction::MOVE);
        let weak = self.downgrade();
        let weak_pill = pill.downgrade();
        drag.connect_prepare(move |source, x, y| {
            let row = weak.upgrade()?;
            let pill = weak_pill.upgrade()?.upcast::<gtk::Widget>();
            if !is_selected(&pill) {
                row.select_only(&pill);
            }
            // The pill as it looks now, before it dims for the drag.
            let image = gtk::WidgetPaintable::new(Some(&pill)).current_image();
            source.set_icon(Some(&drag_icon(&image, acting_on(&pill).len())), x as i32, y as i32);
            Some(gdk::ContentProvider::for_value(&pill.to_value()))
        });
        let weak = self.downgrade();
        let weak_pill = pill.downgrade();
        drag.connect_drag_begin(move |_, _| {
            if let Some(pill) = weak_pill.upgrade() {
                for (_, p) in acting_on(pill.upcast_ref()) {
                    p.add_css_class("dragged");
                }
            }
            if let Some(row) = weak.upgrade() {
                row.emit_by_name::<()>("pill-drag", &[&true]);
            }
        });
        let weak = self.downgrade();
        drag.connect_drag_end(move |_, _, _| {
            let Some(row) = weak.upgrade() else { return };
            for field in row.fields() {
                for p in field.pill_list() {
                    p.remove_css_class("dragged");
                }
            }
            row.emit_by_name::<()>("pill-drag", &[&false]);
        });
        pill.add_controller(drag);
        pill
    }

    /// Every recipient field of the composer, this one among them, in
    /// order, for a selection to span and a pill's menu to move it to.
    pub fn set_fields(&self, fields: &[&RecipientRow]) {
        *self.imp().fields.borrow_mut() = fields.iter().map(|f| f.downgrade()).collect();
    }

    fn fields(&self) -> Vec<RecipientRow> {
        let fields: Vec<_> = self.imp().fields.borrow().iter().filter_map(|f| f.upgrade()).collect();
        if fields.is_empty() {
            vec![self.clone()]
        } else {
            fields
        }
    }

    /// Called as a pill of this field starts (true) and ends (false) a
    /// drag.
    pub fn connect_pill_drag<F: Fn(&Self, bool) + 'static>(&self, f: F) -> glib::SignalHandlerId {
        self.connect_local("pill-drag", false, move |values| {
            let row = values[0].get::<Self>().expect("RecipientRow");
            f(&row, values[1].get::<bool>().unwrap_or(false));
            None
        })
    }

    fn pill_list(&self) -> Vec<gtk::Widget> {
        self.imp().pills.borrow().iter().map(|(p, _)| p.clone()).collect()
    }

    /// The pills of every field shown, in order.
    fn all_pills(&self) -> Vec<gtk::Widget> {
        self.fields().iter().filter(|f| f.is_mapped()).flat_map(|f| f.pill_list()).collect()
    }

    /// Where a sweep at `(x, y)` in this field is, as a place among
    /// `all_pills`: above a field is before its pills, and below the last
    /// is after all of them.
    fn sweep_index(&self, x: f64, y: f64) -> usize {
        let fields: Vec<_> = self.fields().into_iter().filter(|f| f.is_mapped()).collect();
        let mut before = 0;
        for field in &fields {
            let Some(p) = self.compute_point(field, &gtk::graphene::Point::new(x as f32, y as f32)) else { continue };
            if p.y() < 0.0 {
                break;
            }
            if (p.y() as i32) < field.height() {
                return before + field.drop_index(p.x() as f64, p.y() as f64);
            }
            before += field.imp().pills.borrow().len();
        }
        before
    }

    fn index_of(&self, pill: &gtk::Widget) -> Option<usize> {
        self.imp().pills.borrow().iter().position(|(p, _)| p == pill)
    }

    /// Unselect every pill of every field.
    fn clear_selection(&self) {
        for field in self.fields() {
            for p in field.pill_list().iter().filter(|p| is_selected(p)) {
                set_selected(p, false);
            }
        }
    }

    /// Select `pill` and nothing else, in any field.
    fn select_only(&self, pill: &gtk::Widget) {
        self.clear_selection();
        set_selected(pill, true);
        self.imp().anchor.replace(Some(pill.clone()));
    }

    /// Select every pill of this field, and none of the others.
    fn select_all(&self) {
        self.clear_selection();
        let pills = self.pill_list();
        for p in &pills {
            set_selected(p, true);
        }
        self.imp().anchor.replace(pills.first().cloned());
    }

    /// Select the pills of this field from the anchor to `pill`, and none
    /// of the others.
    fn select_range(&self, pill: &gtk::Widget) {
        let pills = self.pill_list();
        let Some(to) = pills.iter().position(|p| p == pill) else { return };
        let anchor = self.imp().anchor.borrow().clone();
        let from = anchor.and_then(|a| pills.iter().position(|p| *p == a));
        if from.is_none() {
            self.imp().anchor.replace(Some(pill.clone()));
        }
        let from = from.unwrap_or(to);
        self.clear_selection();
        for p in &pills[from.min(to)..=from.max(to)] {
            set_selected(p, true);
        }
    }

    /// Where among the pills a pill let go at `(x, y)` goes: before the
    /// first pill on the pointer's line that is past it, after those on
    /// the lines above.
    fn drop_index(&self, x: f64, y: f64) -> usize {
        let imp = self.imp();
        let flow = &imp.flow;
        let Some(p) = self.compute_point(flow, &gtk::graphene::Point::new(x as f32, y as f32)) else { return 0 };
        let rtl = flow.direction() == gtk::TextDirection::Rtl;
        let y = (p.y() as f64).clamp(0.0, (flow.height() - 1).max(0) as f64);
        imp.pills
            .borrow()
            .iter()
            .filter_map(|(pill, _)| pill.compute_bounds(flow))
            .filter(|b| {
                let (top, bottom) = (b.y() as f64, (b.y() + b.height()) as f64);
                let middle = (b.x() + b.width() / 2.0) as f64;
                let past = if rtl { (p.x() as f64) > middle } else { (p.x() as f64) < middle };
                bottom <= y || (top <= y && !past)
            })
            .count()
    }

    /// The order this field's pills would have with `moving` put at place
    /// `at`, or `None` when some come from another field.
    fn order_after(&self, moving: &[(RecipientRow, gtk::Widget)], at: usize) -> Option<Vec<gtk::Widget>> {
        if moving.iter().any(|(f, _)| f != self) {
            return None;
        }
        let pills = self.pill_list();
        let is_moving = |p: &gtk::Widget| moving.iter().any(|(_, m)| m == p);
        let next = pills.iter().skip(at).find(|p| !is_moving(p));
        let mut order: Vec<_> = pills.iter().filter(|p| !is_moving(p)).cloned().collect();
        let i = next.and_then(|n| order.iter().position(|p| p == n)).unwrap_or(order.len());
        order.splice(i..i, moving.iter().map(|(_, p)| p.clone()));
        Some(order)
    }

    /// Whether putting `moving` at place `at` leaves this field as it is.
    fn would_stay(&self, moving: &[(RecipientRow, gtk::Widget)], at: usize) -> bool {
        !moving.is_empty() && self.order_after(moving, at).is_some_and(|o| o == self.pill_list())
    }

    /// Move `moving`, pills of any field this one included, to place `at`
    /// among this field's pills, in their order. An address this field
    /// already has is not added twice. The pills stay selected in their new
    /// place, and the one that had the focus keeps it. Returns them in
    /// their new place, in order.
    fn take_pills(&self, moving: Vec<(RecipientRow, gtk::Widget)>, at: usize) -> Vec<gtk::Widget> {
        if moving.is_empty() || self.would_stay(&moving, at) {
            return moving.into_iter().map(|(_, p)| p).collect();
        }
        let imp = self.imp();
        let next = self.pill_list().into_iter().skip(at).find(|p| !moving.iter().any(|(_, m)| m == p));
        let mut taken = Vec::new();
        let mut changed = vec![self.clone()];
        for (field, pill) in &moving {
            let Some(i) = field.index_of(pill) else { continue };
            let recipient = field.imp().pills.borrow()[i].1.clone();
            // One that does not read as an address stays where it is
            // rather than be lost.
            let Some((name, addr)) = crate::worker::parse_recipients(&recipient).into_iter().next() else { continue };
            let focused = pill.is_focus();
            field.imp().pills.borrow_mut().remove(i);
            field.imp().flow.remove(pill);
            if !changed.contains(field) {
                changed.push(field.clone());
            }
            taken.push((name, addr, recipient, focused));
        }
        let mut at = next.and_then(|n| self.index_of(&n)).unwrap_or(imp.pills.borrow().len());
        let mut focus = None;
        let mut first = None;
        let mut placed = Vec::new();
        for (name, addr, recipient, focused) in taken {
            let have = imp.pills.borrow().iter().find_map(|(p, r)| {
                let other = crate::worker::parse_recipients(r).into_iter().next().map(|(_, a)| a);
                other.is_some_and(|a| a.eq_ignore_ascii_case(&addr)).then(|| p.clone())
            });
            let pill = have.unwrap_or_else(|| {
                let p = self.insert_pill(at, &name, &addr, &recipient);
                at += 1;
                p
            });
            set_selected(&pill, true);
            first.get_or_insert_with(|| pill.clone());
            placed.push(pill.clone());
            if focused {
                focus = Some(pill);
            }
        }
        imp.anchor.replace(first);
        for field in changed {
            field.emit_changed();
        }
        if let Some(p) = focus {
            p.grab_focus();
        }
        placed
    }

    /// Keys on a focused pill: the arrows, Home and End select another
    /// (Shift to extend the selection) or go back to the text, Ctrl+A
    /// selects the field's pills, Delete or Backspace removes the
    /// selection, Return edits the pill, the menu key opens its menu, and a
    /// letter goes to the text.
    fn pill_key(&self, pill: &gtk::Widget, key: gdk::Key, state: gdk::ModifierType) -> glib::Propagation {
        let Some(i) = self.index_of(pill) else { return glib::Propagation::Proceed };
        let pills = self.pill_list();
        let rtl = self.direction() == gtk::TextDirection::Rtl;
        let shift = state.contains(gdk::ModifierType::SHIFT_MASK);
        let ctrl = state.contains(gdk::ModifierType::CONTROL_MASK);
        let (back, forward) = if rtl { (gdk::Key::Right, gdk::Key::Left) } else { (gdk::Key::Left, gdk::Key::Right) };
        // Select the pill at `j`, alone or from the anchor with Shift.
        let go = |j: usize| {
            let p = &pills[j];
            if shift {
                if self.imp().anchor.borrow().is_none() {
                    self.imp().anchor.replace(Some(pill.clone()));
                }
                self.select_range(p);
            } else {
                self.select_only(p);
            }
            p.grab_focus();
        };
        match key {
            gdk::Key::a | gdk::Key::A if ctrl => self.select_all(),
            _ if ctrl || state.contains(gdk::ModifierType::ALT_MASK) => return glib::Propagation::Proceed,
            k if k == back => {
                if i > 0 {
                    go(i - 1);
                }
            }
            k if k == forward => {
                if i + 1 < pills.len() {
                    go(i + 1);
                } else if !shift {
                    self.focus_text();
                }
            }
            gdk::Key::Home => go(0),
            gdk::Key::End if shift => go(pills.len() - 1),
            gdk::Key::End => self.focus_text(),
            gdk::Key::BackSpace | gdk::Key::Delete | gdk::Key::KP_Delete => {
                let gone = acting_on(pill);
                let stays = |p: &&gtk::Widget| !gone.iter().any(|(_, g)| g == *p);
                // Backspace leaves the focus on the pill before, Delete on
                // the one after, as they would on letters.
                let before = pills[..i].iter().rev().find(stays).cloned();
                let after = pills[i + 1..].iter().find(stays).cloned();
                let next = if key == gdk::Key::BackSpace { before.or(after) } else { after.or(before) };
                for (field, p) in &gone {
                    field.remove_pill(p);
                }
                match next {
                    Some(p) => {
                        self.select_only(&p);
                        p.grab_focus();
                    }
                    None => self.focus_text(),
                }
            }
            gdk::Key::Return | gdk::Key::KP_Enter | gdk::Key::ISO_Enter | gdk::Key::F2 => self.edit_pill(pill),
            gdk::Key::Menu => self.pill_menu(pill, pill.width() as f64 / 2.0, pill.height() as f64),
            gdk::Key::F10 if shift => self.pill_menu(pill, pill.width() as f64 / 2.0, pill.height() as f64),
            // Typing goes on in the text, after the pills.
            _ => match key.to_unicode().filter(|c| !c.is_control()) {
                Some(c) => {
                    self.focus_text();
                    let text = &self.imp().text;
                    let mut end = text.text().chars().count() as i32;
                    text.insert_text(&c.to_string(), &mut end);
                    text.set_position(end);
                }
                None => return glib::Propagation::Proceed,
            },
        }
        glib::Propagation::Stop
    }

    /// The menu of `pill` and the rest of the selection, at `(x, y)` in the
    /// pill: move them to another field, edit the pill, remove them.
    fn pill_menu(&self, pill: &gtk::Widget, x: f64, y: f64) {
        use crate::ui::context_menu::{show_context_menu, MenuEntry};
        let Some(p) = pill.compute_point(self, &gtk::graphene::Point::new(x as f32, y as f32)) else { return };
        let acting = acting_on(pill);
        let mut moves = Vec::new();
        for field in self.fields().into_iter().filter(|f| f != self) {
            let label = i18n_f("Move to {name}", &[("name", &field.title())]);
            let acting = acting.clone();
            let which = acting.iter().position(|(_, p)| p == pill).unwrap_or(0);
            moves.push(MenuEntry::new(label, move || {
                field.set_visible(true);
                let at = field.imp().pills.borrow().len();
                let placed = field.take_pills(acting.clone(), at);
                // The menu hands the focus back to the field's text as it
                // closes, which lets go of the selection: once it has, the
                // moved pills are selected again, and the one the menu was
                // opened on takes the focus.
                let Some(focus) = placed.get(which).or(placed.first()).cloned() else { return };
                glib::idle_add_local_once(move || {
                    focus.grab_focus();
                    for p in &placed {
                        set_selected(p, true);
                    }
                });
            }));
        }
        let (row, weak) = (self.downgrade(), pill.downgrade());
        let edit = MenuEntry::new(i18n("Edit"), move || {
            if let (Some(row), Some(pill)) = (row.upgrade(), weak.upgrade()) {
                row.edit_pill(&pill);
            }
        })
        .enabled(acting.len() == 1);
        let row = self.downgrade();
        let remove = MenuEntry::new(i18n("Remove"), move || {
            for (field, p) in &acting {
                field.remove_pill(p);
            }
            if let Some(row) = row.upgrade() {
                row.focus_text();
            }
        });
        show_context_menu(self, p.x() as f64, p.y() as f64, vec![moves, vec![edit, remove]]);
    }

    fn focus_text(&self) {
        let text = &self.imp().text;
        text.grab_focus();
        text.set_position(-1);
    }

    fn remove_pill(&self, pill: &gtk::Widget) {
        let imp = self.imp();
        imp.pills.borrow_mut().retain(|(p, _)| p != pill);
        imp.flow.remove(pill);
        self.emit_changed();
    }

    fn edit_pill(&self, pill: &gtk::Widget) {
        let imp = self.imp();
        let typed = self.typed();
        if !typed.is_empty() {
            self.push(&typed);
        }
        let recipient = {
            let mut pills = imp.pills.borrow_mut();
            let Some(i) = pills.iter().position(|(p, _)| p == pill) else { return };
            pills.remove(i).1
        };
        imp.flow.remove(pill);
        self.set_typed(&recipient);
        imp.text.grab_focus();
        imp.text.set_position(-1);
        self.emit_changed();
    }

    /// Set the typed text without it counting as typing.
    fn set_typed(&self, s: &str) {
        let imp = self.imp();
        imp.busy.set(true);
        imp.text.set_text(s.trim_start());
        imp.text.set_position(-1);
        imp.busy.set(false);
    }

    /// The floating title, the focus ring and the pencil, as an entry row
    /// shows them.
    fn update_state(&self) {
        let imp = self.imp();
        let focused = imp.focused.get();
        let empty = imp.pills.borrow().is_empty() && imp.text.text().is_empty();
        let floating = focused || !empty;
        imp.empty_title.set_visible(!floating);
        imp.title.set_opacity(if floating { 1.0 } else { 0.0 });
        imp.edit_icon.set_visible(!focused);
        if focused {
            self.add_css_class("focused");
        } else {
            self.remove_css_class("focused");
        }
    }
}

fn is_selected(pill: &gtk::Widget) -> bool {
    pill.has_css_class("selected")
}

fn set_selected(pill: &gtk::Widget, on: bool) {
    if on {
        pill.add_css_class("selected");
    } else {
        pill.remove_css_class("selected");
    }
    pill.update_state(&[gtk::accessible::State::Selected(Some(on))]);
}

/// The pills an action on `pill` takes: every selected pill of every
/// field, in order, when `pill` is one of them, or else `pill` alone.
fn acting_on(pill: &gtk::Widget) -> Vec<(RecipientRow, gtk::Widget)> {
    let Some(row) = pill.ancestor(RecipientRow::static_type()).and_downcast::<RecipientRow>() else { return Vec::new() };
    if !is_selected(pill) {
        return vec![(row, pill.clone())];
    }
    let mut out = Vec::new();
    for field in row.fields() {
        for p in field.pill_list().into_iter().filter(is_selected) {
            out.push((field.clone(), p));
        }
    }
    out
}

/// The drag icon: the pill dragged, drawn as a stack when others go with
/// it.
fn drag_icon(pill: &gdk::Paintable, count: usize) -> gdk::Paintable {
    if count < 2 {
        return pill.clone();
    }
    let (w, h) = (pill.intrinsic_width() as f32, pill.intrinsic_height() as f32);
    let snapshot = gtk::Snapshot::new();
    // The others as a pill's edge behind it.
    snapshot.save();
    snapshot.translate(&gtk::graphene::Point::new(4.0, 4.0));
    snapshot.push_opacity(0.5);
    pill.snapshot(&snapshot, w as f64, h as f64);
    snapshot.pop();
    snapshot.restore();
    pill.snapshot(&snapshot, w as f64, h as f64);
    snapshot.to_paintable(Some(&gtk::graphene::Size::new(w + 4.0, h + 4.0))).unwrap_or_else(|| pill.clone())
}

/// Whether `widget` is a pill or inside one.
fn is_in_pill(widget: &gtk::Widget) -> bool {
    let mut w = Some(widget.clone());
    while let Some(c) = w {
        if c.has_css_class("recipient-pill") {
            return true;
        }
        w = c.parent();
    }
    false
}

/// What to do with text about to go into the field, when it is more than
/// plain typing.
#[derive(Debug, PartialEq)]
enum Insert {
    /// Leave it out: a space before anything is typed, or a lone comma.
    Swallow,
    /// Make pills of `done` and leave `rest` as the text.
    Commit { done: String, rest: String },
}

/// Decide what inserting `new` between `before` and `after` does. `None`
/// lets it go in as typed.
fn on_insert(before: &str, new: &str, after: &str) -> Option<Insert> {
    let joined = format!("{before}{new}{after}");
    // A separator outside quotes and angle brackets ends a recipient: typed
    // or pasted, everything up to the last one becomes pills.
    if let Some(cut) = last_separator(&joined) {
        let (done, rest) = joined.split_at(cut);
        let rest = rest[1..].trim_start().to_string();
        if done.trim().is_empty() {
            return Some(if rest.is_empty() {
                Insert::Swallow
            } else {
                Insert::Commit { done: String::new(), rest }
            });
        }
        return Some(Insert::Commit { done: done.to_string(), rest });
    }
    if new.chars().all(char::is_whitespace) {
        // Nothing to separate yet.
        if before.trim().is_empty() && after.trim().is_empty() {
            return Some(Insert::Swallow);
        }
        // A space at the end of a whole address finishes it; anywhere else,
        // or after a name, it is part of what is being typed.
        if after.trim().is_empty() && is_whole_address(before) {
            return Some(Insert::Commit { done: before.to_string(), rest: String::new() });
        }
        return None;
    }
    // Several addresses pasted with spaces between them.
    if new.chars().any(char::is_whitespace) {
        let words: Vec<&str> = joined.split_whitespace().collect();
        let ended = words.len() > 1 || (after.trim().is_empty() && new.ends_with(char::is_whitespace));
        if ended && !words.is_empty() && words.iter().all(|w| is_whole_address(w)) {
            return Some(Insert::Commit { done: words.join(", "), rest: String::new() });
        }
    }
    None
}

/// The byte index of the last comma, semicolon or line break that separates
/// two recipients, outside a quoted name and an address's angle brackets.
fn last_separator(s: &str) -> Option<usize> {
    let (mut quoted, mut escaped, mut angle) = (false, false, false);
    let mut last = None;
    for (i, ch) in s.char_indices() {
        match ch {
            _ if escaped => escaped = false,
            '\\' if quoted => escaped = true,
            '"' => quoted = !quoted,
            '<' if !quoted => angle = true,
            '>' if !quoted => angle = false,
            ',' | ';' | '\n' | '\r' if !quoted && !angle => last = Some(i),
            _ => {}
        }
    }
    last
}

/// Outlook's semicolons and pasted line breaks read as commas.
fn normalise_separators(s: &str) -> String {
    let (mut quoted, mut escaped, mut angle) = (false, false, false);
    s.chars()
        .map(|ch| {
            match ch {
                _ if escaped => escaped = false,
                '\\' if quoted => escaped = true,
                '"' => quoted = !quoted,
                '<' if !quoted => angle = true,
                '>' if !quoted => angle = false,
                ';' | '\n' | '\r' if !quoted && !angle => return ',',
                _ => {}
            }
            ch
        })
        .collect()
}

/// Whether the text is one complete recipient that can be sent to: an
/// address, or a name with its address closed in angle brackets.
fn is_whole_address(s: &str) -> bool {
    let s = s.trim();
    if !s.contains('@') || (s.contains('<') && !s.ends_with('>')) {
        return false;
    }
    if s.chars().filter(|&c| c == '"').count() % 2 == 1 {
        return false;
    }
    crate::worker::parse_recipients(s).len() == 1 && crate::worker::invalid_recipient(s).is_none()
}

mod flow_imp {
    use super::*;

    /// Lays the pills out left to right, wrapping, with the text after the
    /// last one taking what is left of its line.
    #[derive(Default)]
    pub struct PillFlow {
        pub text: RefCell<Option<gtk::Widget>>,
        /// The line showing where a dragged pill would go, outside the
        /// layout.
        pub marker: gtk::Box,
        /// The pill the marker stands before; the number of pills puts it
        /// after the last.
        pub marker_at: Cell<Option<usize>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for PillFlow {
        const NAME: &'static str = "HylkiPillFlow";
        type Type = super::PillFlow;
        type ParentType = gtk::Widget;
    }

    impl ObjectImpl for PillFlow {
        fn constructed(&self) {
            self.parent_constructed();
            self.marker.add_css_class("pill-drop-marker");
            self.marker.set_can_target(false);
            self.marker.set_visible(false);
            self.marker.set_parent(&*self.obj());
        }

        fn dispose(&self) {
            while let Some(c) = self.obj().first_child() {
                c.unparent();
            }
        }
    }

    impl WidgetImpl for PillFlow {
        fn request_mode(&self) -> gtk::SizeRequestMode {
            gtk::SizeRequestMode::HeightForWidth
        }

        fn measure(&self, orientation: gtk::Orientation, for_size: i32) -> (i32, i32, i32, i32) {
            let obj = self.obj();
            if orientation == gtk::Orientation::Horizontal {
                let mut min = TEXT_MIN;
                let mut nat = 0;
                for child in obj.children() {
                    let (cmin, cnat, _, _) = child.measure(orientation, -1);
                    min = min.max(cmin);
                    nat += cnat.max(if obj.is_text(&child) { TEXT_MIN } else { 0 }) + SPACING;
                }
                (min, (nat - SPACING).max(min), -1, -1)
            } else {
                let width = if for_size < 0 { i32::MAX / 2 } else { for_size };
                let lines = obj.lines(width);
                let h = lines.last().map_or(0, |l| l.y + l.h);
                // Wrapped lines get room under the last one, which a single
                // line has from the row's own height.
                let h = if lines.len() > 1 { h + SPACING } else { h };
                (h, h, -1, -1)
            }
        }

        fn size_allocate(&self, width: i32, _height: i32, _baseline: i32) {
            let obj = self.obj();
            let rtl = obj.direction() == gtk::TextDirection::Rtl;
            // Each child's place before mirroring, for the marker.
            let mut placed = Vec::new();
            for line in obj.lines(width) {
                for (child, x, w, h) in line.items {
                    let y = line.y + (line.h - h) / 2;
                    placed.push((obj.is_text(&child), x, y, w, h));
                    let x = if rtl { width - x - w } else { x };
                    child.size_allocate(&gtk::Allocation::new(x, y, w, h), -1);
                }
            }
            if let Some(at) = self.marker_at.get() {
                let pills: Vec<_> = placed.iter().filter(|p| !p.0).collect();
                let (w, _, _, _) = self.marker.measure(gtk::Orientation::Horizontal, -1);
                let w = w.max(2);
                // In the gap before the pill, or after the last one, or at
                // the start of the text when there is none.
                let (x, y, h) = match (pills.get(at), pills.last(), placed.iter().find(|p| p.0)) {
                    (Some(p), _, _) => (p.1 - (SPACING + w) / 2, p.2, p.4),
                    (None, Some(p), _) => (p.1 + p.3 + (SPACING - w) / 2, p.2, p.4),
                    (None, None, Some(t)) => (t.1, t.2, t.4),
                    _ => (0, 0, 0),
                };
                let x = x.clamp(0, (width - w).max(0));
                let x = if rtl { width - x - w } else { x };
                self.marker.size_allocate(&gtk::Allocation::new(x, y, w, h), -1);
            }
        }

        /// Tab goes to the text and on out of the field, never through the
        /// pills, which the arrow keys reach from the text.
        fn focus(&self, direction: gtk::DirectionType) -> bool {
            match direction {
                gtk::DirectionType::TabForward | gtk::DirectionType::TabBackward => {
                    if self.obj().focus_child().is_some() {
                        return false;
                    }
                    self.text.borrow().as_ref().is_some_and(|t| t.grab_focus())
                }
                _ => self.parent_focus(direction),
            }
        }
    }
}

glib::wrapper! {
    pub struct PillFlow(ObjectSubclass<flow_imp::PillFlow>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl Default for PillFlow {
    fn default() -> Self {
        glib::Object::new()
    }
}

/// One line of the flow: each child with its x, width and height.
struct Line {
    y: i32,
    h: i32,
    items: Vec<(gtk::Widget, i32, i32, i32)>,
}

impl PillFlow {
    /// The text, always the last child.
    fn set_text_child(&self, text: &impl IsA<gtk::Widget>) {
        text.set_parent(self);
        self.imp().text.replace(Some(text.clone().upcast()));
    }

    /// Put a pill before `next`, or after the others; a pill already here
    /// moves.
    fn place(&self, pill: &impl IsA<gtk::Widget>, next: Option<&gtk::Widget>) {
        let text = self.imp().text.borrow().clone();
        pill.insert_before(self, next.or(text.as_ref()));
        self.queue_resize();
    }

    /// Show the drop marker before pill `at`, or hide it.
    fn set_marker(&self, at: Option<usize>) {
        let imp = self.imp();
        if imp.marker_at.get() == at {
            return;
        }
        imp.marker_at.set(at);
        imp.marker.set_visible(at.is_some());
        self.queue_allocate();
    }

    fn remove(&self, child: &impl IsA<gtk::Widget>) {
        if child.parent().as_ref() == Some(self.upcast_ref()) {
            child.unparent();
            self.queue_resize();
        }
    }

    fn is_text(&self, child: &gtk::Widget) -> bool {
        self.imp().text.borrow().as_ref() == Some(child)
    }

    fn children(&self) -> Vec<gtk::Widget> {
        let mut out = Vec::new();
        let mut child = self.first_child();
        while let Some(c) = child {
            child = c.next_sibling();
            if c.is_visible() && c != *self.imp().marker.upcast_ref::<gtk::Widget>() {
                out.push(c);
            }
        }
        out
    }

    fn lines(&self, width: i32) -> Vec<Line> {
        let mut lines: Vec<Line> = Vec::new();
        let mut cur = Line { y: 0, h: 0, items: Vec::new() };
        let mut x = 0;
        for child in self.children() {
            let (_, nat, _, _) = child.measure(gtk::Orientation::Horizontal, -1);
            let text = self.is_text(&child);
            // The text needs room to type in; a pill as much as its name,
            // cut short at the full width.
            let need = if text { TEXT_MIN } else { nat.min(width) };
            if x > 0 && x + need > width {
                let y = cur.y + cur.h + SPACING;
                lines.push(std::mem::replace(&mut cur, Line { y, h: 0, items: Vec::new() }));
                x = 0;
            }
            let w = if text { (width - x).max(need) } else { need };
            let (_, h, _, _) = child.measure(gtk::Orientation::Vertical, w);
            cur.h = cur.h.max(h);
            cur.items.push((child, x, w, h));
            x += w + SPACING;
        }
        lines.push(cur);
        lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn commit(done: &str, rest: &str) -> Option<Insert> {
        Some(Insert::Commit { done: done.into(), rest: rest.into() })
    }

    #[test]
    fn a_space_after_a_whole_address_makes_a_pill() {
        assert_eq!(on_insert("ann@example.com", " ", ""), commit("ann@example.com", ""));
        assert_eq!(on_insert("Ann Lee <ann@example.com>", " ", ""), commit("Ann Lee <ann@example.com>", ""));
    }

    #[test]
    fn a_space_in_a_name_or_a_half_address_is_typed() {
        assert_eq!(on_insert("Ann", " ", ""), None);
        assert_eq!(on_insert("Ann <ann@exa", " ", ""), None);
        assert_eq!(on_insert("\"Lee", " ", ""), None);
        // Inside the text rather than at its end.
        assert_eq!(on_insert("ann@example.com", " ", "x"), None);
    }

    #[test]
    fn a_comma_makes_a_pill_and_never_stays() {
        assert_eq!(on_insert("ann@example.com", ",", ""), commit("ann@example.com", ""));
        assert_eq!(on_insert("Ann", ",", ""), commit("Ann", ""));
        assert_eq!(on_insert("", ",", ""), Some(Insert::Swallow));
        assert_eq!(on_insert("", " ", ""), Some(Insert::Swallow));
        assert_eq!(on_insert("bob@example.com", ";", ""), commit("bob@example.com", ""));
    }

    #[test]
    fn a_comma_in_a_quoted_name_is_part_of_it() {
        assert_eq!(on_insert("\"Lee", ",", ""), None);
        assert_eq!(on_insert("\"Lee, Ann\" <ann@exa", ",", ""), None);
    }

    #[test]
    fn a_pasted_list_becomes_pills_and_keeps_its_tail() {
        assert_eq!(
            on_insert("", "ann@example.com, Bob <bob@example.com>, car", ""),
            commit("ann@example.com, Bob <bob@example.com>", "car"),
        );
        assert_eq!(
            on_insert("", "ann@example.com bob@example.com", ""),
            commit("ann@example.com, bob@example.com", ""),
        );
        assert_eq!(on_insert("", "Ann Lee", ""), None);
    }

    #[test]
    fn separators_outside_quotes_read_as_commas() {
        assert_eq!(normalise_separators("a@x.com; b@y.com\nc@z.com"), "a@x.com, b@y.com,c@z.com");
        assert_eq!(normalise_separators("\"Lee; Ann\" <a@x.com>"), "\"Lee; Ann\" <a@x.com>");
    }

    #[test]
    fn whole_addresses() {
        assert!(is_whole_address("ann@example.com"));
        assert!(is_whole_address("\"Lee, Ann\" <ann@example.com>"));
        assert!(!is_whole_address("ann"));
        assert!(!is_whole_address("Ann <ann@example.com"));
        assert!(!is_whole_address("ann@"));
    }
}
