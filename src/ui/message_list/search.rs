//! Search: the scope picker, the text each message is searched by, and
//! matching (off the main thread for big folders).

use super::filters::{start_of_today, QuickFilter, QuickFilterContext};
use super::types::{MessageListInput, MessageListOutput};
use super::MessageList;
use crate::i18n::i18n;
use crate::models::Message;
use adw::prelude::*;
use relm4::prelude::*;
use std::rc::Rc;

/// A list at least this long is searched on another thread
/// ([`MessageList::start_search`]).
const SEARCH_OFF_MAIN: usize = 4000;

/// What a search looks in for one message, lowercased: its subject, its
/// sender's name and address, and its preview, apart, so a query never
/// matches across two of them.
pub(crate) fn search_text_of(m: &Message) -> Box<str> {
    [m.subject.as_str(), &m.from_name, &m.from_addr, &m.preview]
        .join("\u{0}")
        .to_lowercase()
        .into_boxed_str()
}

/// [`search_text_of`] for each message of a list, and which messages they
/// were, so that a changed one is noticed and made again.
pub(super) struct SearchText {
    pub(super) of: Vec<*const Message>,
    pub(super) text: std::sync::Arc<Vec<Box<str>>>,
}

/// What a search run on another thread matched: for `query`, a flag for
/// each entry of `text`, which says which messages they were.
pub(super) struct SearchHits {
    pub(super) query: String,
    pub(super) text: std::sync::Arc<Vec<Box<str>>>,
    pub(super) hits: Vec<bool>,
}

/// Which messages the search field filters over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SearchScope {
    /// Every folder of every account (the merged `search_pool`) but Trash
    /// and Junk, unless the list is showing one of them (#374).
    AllFolders,
    /// Every folder of every account, Trash and Junk too.
    Everywhere,
    /// Only the folder currently shown (the local `all` index).
    ThisFolder,
}

impl SearchScope {
    /// In the order the search bar's dropdown lists them.
    pub(super) const ALL: [SearchScope; 3] = [
        SearchScope::AllFolders,
        SearchScope::Everywhere,
        SearchScope::ThisFolder,
    ];

    pub(super) fn label(self) -> String {
        match self {
            SearchScope::AllFolders => i18n("All folders (except trash & junk)"),
            SearchScope::Everywhere => i18n("Everywhere"),
            SearchScope::ThisFolder => i18n("This folder"),
        }
    }

    /// The name on the picker's button, short enough for a narrow list: the
    /// popover spells the scope out.
    fn button_label(self) -> String {
        match self {
            SearchScope::AllFolders => i18n("All folders"),
            _ => self.label(),
        }
    }

    /// What the scope searches, behind the info button beside its name in
    /// the picker.
    fn description(self) -> String {
        match self {
            SearchScope::AllFolders => i18n(
                "Every folder of every account, except Trash and Junk. A search started in Trash or Junk looks there too.",
            ),
            SearchScope::Everywhere => i18n("Every folder of every account, Trash and Junk included."),
            SearchScope::ThisFolder => i18n("Only the folder or view the list is showing."),
        }
    }
}

/// The search scope's button and popover (#374): each scope by name, with
/// an info button that tells what it searches.
pub(super) struct ScopePicker {
    pub(super) button: gtk::MenuButton,
    label: gtk::Label,
    /// Follows the way the popover opens (#496).
    pub(super) arrow: gtk::Image,
    radios: Vec<(SearchScope, gtk::CheckButton)>,
    /// Each scope's info button, in the same order.
    pub(super) infos: Vec<gtk::MenuButton>,
}

impl ScopePicker {
    pub(super) fn new(button: &gtk::MenuButton, input: &relm4::Sender<MessageListInput>) -> Self {
        // An ellipsizing label, so the button, not the messages, gives way
        // first in a narrow list (#29).
        let label = gtk::Label::new(Some(&SearchScope::AllFolders.button_label()));
        label.set_ellipsize(gtk::pango::EllipsizeMode::End);
        let face = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        face.append(&label);
        let arrow = gtk::Image::from_icon_name("pan-down-symbolic");
        face.append(&arrow);
        button.set_child(Some(&face));

        let popover = gtk::Popover::new();
        let list = gtk::Box::new(gtk::Orientation::Vertical, 0);
        list.add_css_class("search-scope-popover");
        let mut radios: Vec<(SearchScope, gtk::CheckButton)> = Vec::new();
        let mut infos = Vec::new();
        for scope in SearchScope::ALL {
            let radio = gtk::CheckButton::with_label(&scope.label());
            radio.set_hexpand(true);
            // Beside the radio, not in it: a click on it must not pick the
            // scope.
            let about = gtk::Label::new(Some(&scope.description()));
            about.set_xalign(0.0);
            about.set_wrap(true);
            about.set_max_width_chars(34);
            about.add_css_class("search-scope-about");
            let about_popover = gtk::Popover::new();
            about_popover.set_child(Some(&about));
            let info = gtk::MenuButton::new();
            info.set_icon_name("info-outline-symbolic");
            info.set_valign(gtk::Align::Center);
            info.add_css_class("flat");
            info.add_css_class("circular");
            info.set_tooltip_text(Some(&i18n("What this searches")));
            info.set_popover(Some(&about_popover));
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 4);
            row.append(&radio);
            row.append(&info);
            infos.push(info);
            if let Some((_, first)) = radios.first() {
                radio.set_group(Some(first));
            }
            radio.set_active(scope == SearchScope::AllFolders);
            let input = input.clone();
            let popover = popover.clone();
            radio.connect_toggled(move |r| {
                if r.is_active() {
                    input.emit(MessageListInput::SetScope(scope));
                    popover.popdown();
                }
            });
            list.append(&row);
            radios.push((scope, radio));
        }
        popover.set_child(Some(&list));
        button.set_popover(Some(&popover));
        Self {
            button: button.clone(),
            label,
            arrow,
            radios,
            infos,
        }
    }

    pub(super) fn sync(&self, scope: SearchScope) {
        self.label.set_label(&scope.button_label());
        if let Some((_, radio)) = self.radios.iter().find(|(s, _)| *s == scope) {
            if !radio.is_active() {
                radio.set_active(true);
            }
        }
    }
}

/// A folder of the cross-folder search pool, as its results show it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PoolFolder {
    /// What the result's folder pill says.
    pub(crate) name: String,
    /// Trash or Junk: left out of an "All folders" search.
    pub(crate) waste: bool,
}

impl MessageList {
    /// Whether a search is currently active (the query is non-empty).
    pub(super) fn searching(&self) -> bool {
        !self.query.trim().is_empty()
    }

    /// The message set the search filters over: the cross-folder pool while an
    /// `AllFolders` search is active (and the pool has arrived), otherwise the
    /// current folder's own index.
    pub(super) fn active_source(&self) -> &[Rc<Message>] {
        if self.searching_pool() {
            &self.search_pool
        } else {
            &self.all
        }
    }

    /// Whether the search reaches past this folder, and so wants the pool.
    pub(super) fn wants_pool(&self) -> bool {
        self.searching() && self.scope != SearchScope::ThisFolder
    }

    pub(super) fn searching_pool(&self) -> bool {
        self.wants_pool() && !self.search_pool.is_empty()
    }

    pub(super) fn search_placeholder(&self) -> String {
        match self.scope {
            SearchScope::AllFolders => i18n("Search all folders but Trash and Junk"),
            SearchScope::Everywhere => i18n("Search all folders, Trash and Junk too"),
            SearchScope::ThisFolder => i18n("Search this folder"),
        }
    }

    /// Drop any active search: clear the query and the entry text so a folder
    /// switch doesn't leave a stale term filtering the new folder.
    pub(super) fn clear_search(&mut self, sender: &ComponentSender<Self>) {
        if self.query.is_empty() {
            return;
        }
        // The entry's own change arrives with the query already empty, too
        // late to see the search end: the cross-folder pool and the account
        // tint were left behind.
        if self.wants_pool() {
            let _ = sender.output(MessageListOutput::SearchActive(false));
        }
        self.query.clear();
        self.search_gen
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.search_hits = None;
        *self.search_text.borrow_mut() = Default::default();
        if let Some(e) = &self.search_entry {
            e.set_text("");
        }
    }

    /// What the search field and the quick filters let through.
    pub(super) fn filter(&self) -> impl Fn(&Message) -> bool {
        let q = self.query.to_lowercase();
        let besides = self.filter_besides_text();
        move |m: &Message| besides(m) && (q.is_empty() || search_text_of(m).contains(&q))
    }

    /// What the quick filters and the search's scope let through, the
    /// search's text aside.
    fn filter_besides_text(&self) -> impl Fn(&Message) -> bool {
        let quick = QuickFilterContext {
            filters: self.quick,
            own: if self.quick.has(QuickFilter::ToMe) {
                self.own_addresses.clone()
            } else {
                Default::default()
            },
            contacts: self
                .contact_addresses
                .clone()
                .filter(|_| self.quick.has(QuickFilter::Contacts)),
            tags: if self.quick.has(QuickFilter::Tagged) {
                self.shared.tags.borrow().clone()
            } else {
                Vec::new()
            },
            today: if self.quick.has(QuickFilter::Today) {
                start_of_today()
            } else {
                0
            },
        };
        // An "All folders" search leaves Trash and Junk out, but for the
        // folder the list is showing (#374).
        let left_out: std::collections::HashSet<(u32, u32)> =
            if self.searching_pool() && self.scope == SearchScope::AllFolders {
                let here: std::collections::HashSet<(u32, u32)> = self
                    .all
                    .iter()
                    .map(|m| (m.account_id, m.folder_id))
                    .collect();
                self.pool_folders
                    .iter()
                    .filter(|(k, f)| f.waste && !here.contains(k))
                    .map(|(k, _)| *k)
                    .collect()
            } else {
                Default::default()
            };
        move |m: &Message| !left_out.contains(&(m.account_id, m.folder_id)) && quick.passes(m)
    }

    /// [`search_text_of`] for the source a search goes through, made again
    /// only for the messages that changed since.
    fn source_text(&self) -> std::sync::Arc<Vec<Box<str>>> {
        let source = self.active_source();
        let mut texts = self.search_text.borrow_mut();
        let slot = &mut texts[usize::from(self.searching_pool())];
        match slot {
            Some(t) if t.of.len() == source.len() => {
                let stale: Vec<usize> = (0..source.len())
                    .filter(|&i| t.of[i] != Rc::as_ptr(&source[i]))
                    .collect();
                if !stale.is_empty() {
                    let text = std::sync::Arc::make_mut(&mut t.text);
                    for i in stale {
                        t.of[i] = Rc::as_ptr(&source[i]);
                        text[i] = search_text_of(&source[i]);
                    }
                }
            }
            _ => {
                *slot = Some(SearchText {
                    of: source.iter().map(Rc::as_ptr).collect(),
                    text: std::sync::Arc::new(source.iter().map(|m| search_text_of(m)).collect()),
                });
            }
        }
        slot.as_ref().map(|t| t.text.clone()).unwrap_or_default()
    }

    /// Search a large source on another thread, answered with
    /// [`MessageListInput::SearchMatched`]; `false` for a source small
    /// enough to search in place, or no search at all. Either way a search
    /// still running is overtaken.
    pub(super) fn start_search(&self) -> bool {
        let gen = self
            .search_gen
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + 1;
        if self.query.is_empty() || self.active_source().len() < SEARCH_OFF_MAIN {
            return false;
        }
        let text = self.source_text();
        let query = self.query.clone();
        let latest = self.search_gen.clone();
        let sender = self.input.clone();
        std::thread::spawn(move || {
            let t = std::time::Instant::now();
            let needle = query.to_lowercase();
            let mut hits = Vec::with_capacity(text.len());
            for (i, haystack) in text.iter().enumerate() {
                if i % 4096 == 0 && latest.load(std::sync::atomic::Ordering::Relaxed) != gen {
                    return;
                }
                hits.push(haystack.contains(&needle));
            }
            tracing::debug!(
                "list: searched {} messages off the main loop in {:?}",
                text.len(),
                t.elapsed()
            );
            let _ = sender.send(MessageListInput::SearchMatched {
                gen,
                query,
                text,
                hits,
            });
        });
        true
    }

    /// The messages of the source that the search and the quick filters
    /// let through, in the source's order.
    pub(super) fn source_matches(&self) -> Vec<Rc<Message>> {
        let source = self.active_source();
        let besides = self.filter_besides_text();
        if self.query.is_empty() {
            return source.iter().filter(|m| besides(m)).cloned().collect();
        }
        let text = self.source_text();
        let hits = self
            .search_hits
            .as_ref()
            .filter(|h| h.query == self.query && std::sync::Arc::ptr_eq(&h.text, &text))
            .map(|h| &h.hits);
        let query = self.query.to_lowercase();
        source
            .iter()
            .enumerate()
            .filter(|(i, m)| {
                hits.map_or_else(|| text[*i].contains(&query), |h| h[*i]) && besides(m)
            })
            .map(|(_, m)| m.clone())
            .collect()
    }
}
