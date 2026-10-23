//! The row's actions palette, its menus, and the card a drag carries.

use super::headings::tag_menu_entries;
use super::row::{PaletteButtons, Row};
use super::shared::{RowLook, DRAG_CARD_WIDTH, DRAG_STACK, DRAG_STACK_STEP};
use super::{RowAction, RowData};
use crate::i18n::i18n;
use crate::models::Message;
use crate::ui::context_menu::show_context_menu;
use crate::ui::message_list::MessageListInput;
use adw::prelude::*;
use gtk::glib;
use relm4::prelude::*;
use std::rc::Rc;

impl Row {
    pub(crate) fn toggle_palette(&self) {
        if self.data().is_none() {
            return;
        }
        if self.st.borrow().palette_open {
            self.open_palette(false);
            self.cancel_collapse();
        } else {
            self.open_palette(true);
            // Persist briefly; moving onto the palette cancels this.
            self.arm_collapse();
        }
    }

    pub(super) fn open_palette(&self, open: bool) {
        if open {
            // One palette at a time.
            if let Some(shared) = self.shared() {
                let previous = shared.open_palette.borrow().upgrade();
                if let Some(prev) = previous {
                    if !std::ptr::eq(Rc::as_ptr(&prev), self) {
                        prev.open_palette(false);
                        prev.cancel_collapse();
                    }
                }
                shared.open_palette.replace(self.me.clone());
            }
            self.ensure_palette();
        }
        self.st.borrow_mut().palette_open = open;
        self.sync_palette_classes();
        self.slide_palette();
    }

    pub(super) fn cancel_collapse(&self) {
        if let Some(id) = self.st.borrow_mut().collapse_timer.take() {
            id.remove();
        }
    }

    /// (Re)start the auto-collapse countdown from the preference (min 1s).
    pub(super) fn arm_collapse(&self) {
        self.cancel_collapse();
        let Some(shared) = self.shared() else { return };
        let secs = shared.palette_collapse_secs.get().max(1);
        let weak = self.me.clone();
        let id = glib::timeout_add_seconds_local_once(secs as u32, move || {
            if let Some(row) = weak.upgrade() {
                row.st.borrow_mut().collapse_timer = None;
                row.open_palette(false);
            }
        });
        self.st.borrow_mut().collapse_timer = Some(id);
    }

    /// The ⋯ shows while the row is hovered or its palette open; open, the
    /// whole line sits on one card surface.
    pub(super) fn sync_palette_classes(&self) {
        let st = self.st.borrow();
        let revealed = st.hovered || st.palette_open;
        let open = st.palette_open;
        drop(st);
        self.w.chevron.set_css_classes(if revealed {
            &["flat", "palette-toggle", "revealed"]
        } else {
            &["flat", "palette-toggle"]
        });
        self.w.actions_line.set_css_classes(if open {
            &["actions-line", "open"]
        } else {
            &["actions-line"]
        });
    }

    /// Build the palette's buttons on its first open: most rows never open
    /// theirs, and building them for every row was most of a row's cost.
    /// Mirrors the reader toolbar's order.
    fn ensure_palette(&self) {
        let Some(shared) = self.shared() else { return };
        let built_for = {
            let look = shared.look.borrow();
            (look.in_junk, look.in_drafts)
        };
        if self
            .st
            .borrow()
            .palette
            .as_ref()
            .is_some_and(|p| p.built_for == built_for)
        {
            return;
        }
        let inner = &self.w.palette_inner;
        while let Some(child) = inner.first_child() {
            inner.remove(&child);
        }
        let weak = self.me.clone();
        let button = |icon: &str, tip: String| {
            let b = gtk::Button::from_icon_name(icon);
            b.set_tooltip_text(Some(tip.as_str()));
            b.add_css_class("flat");
            b
        };
        let action = |b: &gtk::Button, a: RowAction| {
            let weak = weak.clone();
            b.connect_clicked(move |_| {
                if let Some(row) = weak.upgrade() {
                    row.act(a);
                }
            });
        };
        let (in_junk, in_drafts) = built_for;
        let reply = button("mail-reply-sender-symbolic", i18n("Reply"));
        action(&reply, RowAction::Reply);
        let reply_all = button("mail-reply-all-symbolic", i18n("Reply All"));
        action(&reply_all, RowAction::ReplyAll);
        let forward = button("mail-forward-symbolic", i18n("Forward"));
        action(&forward, RowAction::Forward);
        // A draft is neither read nor unread, so it gets no toggle.
        let read = (!in_drafts).then(|| {
            let b = button("hylki-mail-read-symbolic", i18n("Mark as read"));
            action(&b, RowAction::ToggleRead);
            b
        });
        let star = button("hylki-non-starred-symbolic", i18n("Star"));
        action(&star, RowAction::ToggleStar);
        let tag = button("tag-outline-symbolic", i18n("Tags"));
        {
            let weak = weak.clone();
            tag.connect_clicked(move |b| {
                if let Some(row) = weak.upgrade() {
                    row.tag_menu(b);
                }
            });
        }
        let moveto = button("folder-symbolic", i18n("Move to…"));
        {
            let weak = weak.clone();
            moveto.connect_clicked(move |b| {
                if let Some(row) = weak.upgrade() {
                    row.move_menu(b);
                }
            });
        }
        let archive = button("mail-archive-symbolic", i18n("Archive"));
        action(&archive, RowAction::Archive);
        let delete = button("user-trash-symbolic", i18n("Delete"));
        action(&delete, RowAction::Delete);
        let spam = if in_junk {
            let b = button("mail-mark-notjunk-symbolic", i18n("Not spam"));
            action(&b, RowAction::NotSpam);
            b
        } else {
            let b = button("mail-mark-junk-symbolic", i18n("Mark as spam"));
            action(&b, RowAction::Spam);
            b
        };
        let contact = button("contact-new-symbolic", i18n("Add sender to Contacts"));
        action(&contact, RowAction::AddContact);
        let source = button("code-symbolic", i18n("View Source"));
        action(&source, RowAction::ViewSource);
        for b in [
            Some(&reply),
            Some(&reply_all),
            Some(&forward),
            read.as_ref(),
            Some(&star),
            Some(&tag),
            Some(&moveto),
            Some(&archive),
            Some(&delete),
            Some(&spam),
            Some(&contact),
            Some(&source),
        ]
        .into_iter()
        .flatten()
        {
            inner.append(b);
        }
        self.st.borrow_mut().palette = Some(PaletteButtons {
            read,
            star,
            tag,
            built_for,
        });
        self.sync_palette_buttons();
    }

    /// Keep the built palette's state-carrying buttons in step with the
    /// message.
    pub(super) fn sync_palette_buttons(&self) {
        let Some(data) = self.data() else { return };
        let Some(shared) = self.shared() else { return };
        let st = self.st.borrow();
        let Some(b) = st.palette.as_ref() else { return };
        // Action-showing icon (read envelope = "mark as read"), like the
        // menus and toolbar.
        if let Some(read) = &b.read {
            if data.msg.unread {
                read.set_icon_name("hylki-mail-read-symbolic");
                read.set_tooltip_text(Some(i18n("Mark as read").as_str()));
            } else {
                read.set_icon_name("mail-unread-symbolic");
                read.set_tooltip_text(Some(i18n("Mark as unread").as_str()));
            }
        }
        let starred = data.msg.starred || data.meta.starred;
        b.star.set_css_classes(if starred {
            &["flat", "star-active"]
        } else {
            &["flat"]
        });
        b.star.set_tooltip_text(Some(
            if starred {
                i18n("Remove star")
            } else {
                i18n("Star")
            }
            .as_str(),
        ));
        // Only once there is a tag to give.
        b.tag.set_visible(!shared.tags.borrow().is_empty());
    }

    /// Slide the palette open or shut by animating the spacer that gives
    /// the clip its width — not a GtkRevealer, whose transitions don't
    /// repaint inside an Overlay's overlay child.
    fn slide_palette(&self) {
        let open = self.st.borrow().palette_open;
        let w = &self.w;
        if open {
            w.palette_inner.set_visible(true);
        }
        let (inner_w, inner_h) = (
            w.palette_inner.measure(gtk::Orientation::Horizontal, -1).1,
            w.palette_inner.measure(gtk::Orientation::Vertical, -1).1,
        );
        w.palette_spacer.set_height_request(inner_h);
        let target = if open { inner_w } else { 0 };
        if self.st.borrow().palette_target == target {
            return;
        }
        self.st.borrow_mut().palette_target = target;
        let spacer = w.palette_spacer.clone();
        let from = spacer.width() as f64;
        let setter = {
            let spacer = spacer.clone();
            adw::CallbackAnimationTarget::new(move |v| spacer.set_width_request(v as i32))
        };
        // Bound to the line (mapped: it holds the visible toggle), NOT the
        // spacer — adw skips animations on unmapped widgets, and the
        // zero-width spacer counts as one.
        let anim = adw::TimedAnimation::new(&w.actions_line, from, target as f64, 180, setter);
        anim.set_easing(adw::Easing::EaseOutCubic);
        if target == 0 {
            // Sliding shut: hide only once fully back in the button.
            let inner = w.palette_inner.downgrade();
            anim.connect_done(move |_| {
                if let Some(inner) = inner.upgrade() {
                    inner.set_visible(false);
                }
            });
        }
        let old = self.st.borrow_mut().palette_anim.replace(anim.clone());
        if let Some(old) = old {
            old.pause();
        }
        anim.play();
    }

    pub(super) fn act(&self, action: RowAction) {
        if let Some(data) = self.data() {
            self.send(MessageListInput::RowAction {
                action,
                message: Box::new(Message::clone(&data.msg)),
            });
        }
    }

    fn tag_menu(&self, btn: &gtk::Button) {
        let (Some(data), Some(shared)) = (self.data(), self.shared()) else {
            return;
        };
        let tags = shared.tags.borrow().clone();
        let target = Message::clone(&data.msg);
        let input = shared.input.clone();
        let entries = tag_menu_entries(&tags, &data.msg, move |keyword, add| {
            let _ = input.send(MessageListInput::SetTagFor {
                message: Box::new(target.clone()),
                keyword,
                add,
            });
        });
        show_context_menu(
            btn,
            (btn.width() / 2) as f64,
            btn.height() as f64,
            vec![entries],
        );
    }

    fn move_menu(&self, btn: &gtk::Button) {
        let Some(data) = self.data() else { return };
        // Under the button's middle, in window coordinates: the picker is
        // anchored on the window by the app.
        let point = btn.root().and_then(|root| {
            let root: gtk::Widget = root.upcast();
            btn.compute_point(
                &root,
                &gtk::graphene::Point::new(btn.width() as f32 / 2.0, btn.height() as f32),
            )
        });
        let (x, y) = point.map_or((0.0, 0.0), |p| (p.x() as f64, p.y() as f64));
        self.send(MessageListInput::RowMoveTo {
            message: Box::new(Message::clone(&data.msg)),
            x,
            y,
        });
    }

    pub(super) fn context_menu(&self, x: f64, y: f64) {
        let (Some(data), Some(shared)) = (self.data(), self.shared()) else {
            return;
        };
        let Some(view) = shared.view.upgrade() else {
            return;
        };
        let Some(point) = self
            .w
            .host
            .compute_point(&view, &gtk::graphene::Point::new(x as f32, y as f32))
        else {
            return;
        };
        self.send(MessageListInput::ContextMenu {
            x: point.x() as f64,
            y: point.y() as f64,
            key: (data.msg.account_id, data.msg.id),
        });
    }

    /// What a drag of this row carries under the pointer: the row's card,
    /// and when the row is one of a selection, the cards of the others
    /// stacked behind it, at most [`DRAG_STACK`] deep, with the full count.
    pub(super) fn drag_card(&self) -> Option<gtk::Widget> {
        let (data, shared) = (self.data()?, self.shared()?);
        let look = shared.look.borrow().clone();
        let me = (data.msg.account_id, data.msg.id);
        let selected = shared.selected_rows();
        if selected.len() < 2 || !selected.iter().any(|r| (r.msg.account_id, r.msg.id) == me) {
            let card = self.message_card(&data, &look, self.w.avatar.custom_image());
            return Some(card.upcast());
        }
        let front = self.message_card(&data, &look, self.w.avatar.custom_image());
        let count = gtk::Label::new(Some(&selected.len().to_string()));
        count.add_css_class("message-drag-count");
        count.set_valign(gtk::Align::Center);
        front.append(&count);
        let behind: Vec<_> = selected
            .iter()
            .filter(|r| (r.msg.account_id, r.msg.id) != me)
            .take(DRAG_STACK - 1)
            .map(|r| self.message_card(r, &look, None))
            .collect();
        // One width for every card, so the edges behind run parallel.
        let stack = gtk::Fixed::new();
        for (depth, card) in behind.iter().enumerate().rev() {
            card.set_size_request(DRAG_CARD_WIDTH, -1);
            let at = ((depth + 1) as i32 * DRAG_STACK_STEP) as f64;
            stack.put(card, at, at);
        }
        front.set_size_request(DRAG_CARD_WIDTH, -1);
        stack.put(&front, 0.0, 0.0);
        Some(stack.upcast())
    }

    /// One message as a drag card: its circle, the subject over who it is
    /// from (to, in Sent).
    fn message_card(
        &self,
        data: &RowData,
        look: &RowLook,
        image: Option<gtk::gdk::Paintable>,
    ) -> gtk::Box {
        let card = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        card.add_css_class("message-drag-card");
        let name = self.face_name(data, look);
        let email = self.face_email(data, look);
        let avatar = adw::Avatar::new(34, Some(&name), true);
        avatar.set_custom_image(image.as_ref());
        avatar.set_valign(gtk::Align::Center);
        card.append(&avatar);
        let text = gtk::Box::new(gtk::Orientation::Vertical, 3);
        text.set_valign(gtk::Align::Center);
        text.set_hexpand(true);
        let subject = if data.msg.subject.trim().is_empty() {
            i18n("(no subject)")
        } else {
            data.msg.subject.clone()
        };
        let title = gtk::Label::new(Some(&subject));
        title.set_xalign(0.0);
        title.set_ellipsize(gtk::pango::EllipsizeMode::End);
        title.set_max_width_chars(40);
        let who = match (name.trim(), email.trim()) {
            (n, e) if n.is_empty() || n.eq_ignore_ascii_case(e) => e.to_string(),
            (n, "") => n.to_string(),
            (n, e) => format!("{n} <{e}>"),
        };
        let subtitle = gtk::Label::new(Some(&who));
        subtitle.add_css_class("dim-label");
        subtitle.add_css_class("caption");
        subtitle.set_xalign(0.0);
        subtitle.set_ellipsize(gtk::pango::EllipsizeMode::End);
        subtitle.set_max_width_chars(40);
        text.append(&title);
        text.append(&subtitle);
        card.append(&text);
        card
    }

    pub(super) fn drag_payload(&self) -> Option<gtk::gdk::ContentProvider> {
        let (data, shared) = (self.data()?, self.shared()?);
        let me = (
            data.msg.account_id,
            data.msg.folder_id,
            data.msg.uid,
            data.msg.id,
        );
        let mut items = shared.selected_drag_keys();
        // Dragging a row outside the selection moves just that row.
        if !items.iter().any(|k| k.0 == me.0 && k.3 == me.3) {
            items = vec![me];
        }
        // A conversation row stands for its whole thread (#171).
        let threads = shared.thread_drag.borrow();
        let items: Vec<_> = items
            .into_iter()
            .flat_map(|k| threads.get(&(k.0, k.3)).cloned().unwrap_or_else(|| vec![k]))
            .collect();
        let mut payload = String::from("vireo-move");
        for (a, f, u, i) in items {
            payload.push_str(&format!("\t{a}\t{f}\t{u}\t{i}"));
        }
        Some(gtk::gdk::ContentProvider::for_value(&payload.to_value()))
    }
}
