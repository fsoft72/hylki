//! In-app Contacts view, shown in the main content area (like the attachments
//! gallery): a searchable, sortable list of every GNOME Contacts entry beside
//! the selected contact's full card, with editing, creation and deletion done
//! right here (writes go through EDS, so GNOME Contacts and CardDAV stay in
//! sync). Anything Hylki doesn't do — linking, photos — is one click away via
//! a contact's "Open in GNOME Contacts".

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use adw::prelude::*;
use relm4::prelude::*;

use crate::contacts::{ContactDetails, ContactEdit, GroupEdit, GroupFormat, Labeled, Member};
use crate::ui::context_menu::{show_context_menu, MenuEntry};
use crate::i18n::{i18n, i18n_f, i18n_noop, ni18n_f};

/// Where the people found in the LDAP directories (#307) start in the
/// index space `selected` and `row_map` share with `contacts`: an index this
/// high or higher is `directory_hits[index - DIRECTORY]`.
const DIRECTORY: usize = usize::MAX / 2;

/// At most this many directory entries are listed for one search.
const DIRECTORY_LIMIT: usize = 50;

/// What the list shows, picked in the sidebar: every contact, one address
/// book's, or one group's people (the group first).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum ContactScope {
    #[default]
    All,
    Book(String),
    Group { book: String, uid: String },
    /// Every book of one account, by its [`TreeBook::key`].
    Account(String),
}

/// The address books and their groups, as the sidebar lists them while
/// Contacts is open.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContactTree {
    /// Every contact, groups left out.
    pub total: usize,
    pub books: Vec<TreeBook>,
    pub scope: ContactScope,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeBook {
    pub uid: String,
    /// The account the book is in, for the line under its name ("CardDAV —
    /// a@b.org", or the service of a book that is its account alone);
    /// empty for the Hylki book.
    pub account: String,
    /// What the books of one account share: the books with one key are
    /// listed under their account.
    pub key: String,
    pub name: String,
    pub count: usize,
    /// Hylki can write to it, so contacts can be copied or moved in.
    pub writable: bool,
    pub takes_groups: bool,
    /// An LDAP directory, searched rather than listed.
    pub directory: bool,
    pub groups: Vec<TreeGroup>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeGroup {
    pub book: String,
    pub uid: String,
    pub name: String,
    pub count: usize,
    pub editable: bool,
}

/// Where contacts dragged to the sidebar land.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DropOn {
    Book(String),
    Group { book: String, uid: String },
}

/// What a right-click on the sidebar's book or group asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TreeAction {
    NewContact(String),
    NewGroup(String),
    EditGroup { book: String, uid: String },
    DeleteGroup { book: String, uid: String },
    ComposeGroup { book: String, uid: String },
    /// Give the book a name of its own in Hylki.
    RenameBook(String),
}

/// The start of what a contact row carries when dragged, before a line of
/// book and UID per contact.
pub const DRAG_MARK: &str = "hylki-contacts";

/// The "Show only" filters of the sort and filter button, as the message
/// list has for mail. They narrow the people; groups are not filtered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContactFilter {
    Photo,
    Email,
    Phone,
    Address,
    Birthday,
    Company,
    NoGroup,
}

impl ContactFilter {
    pub const ALL: [ContactFilter; 7] = [
        ContactFilter::Photo,
        ContactFilter::Email,
        ContactFilter::Phone,
        ContactFilter::Address,
        ContactFilter::Birthday,
        ContactFilter::Company,
        ContactFilter::NoGroup,
    ];

    fn label(self) -> String {
        match self {
            ContactFilter::Photo => i18n("Has Photo"),
            ContactFilter::Email => i18n("Has Email"),
            ContactFilter::Phone => i18n("Has Phone"),
            ContactFilter::Address => i18n("Has Address"),
            ContactFilter::Birthday => i18n("Has Birthday"),
            ContactFilter::Company => i18n("Has Company"),
            ContactFilter::NoGroup => i18n("Not in a Group"),
        }
    }

    fn bit(self) -> u8 {
        1 << ContactFilter::ALL.iter().position(|f| *f == self).unwrap_or(0)
    }

    /// Whether `c` passes; `grouped` holds every group member's address,
    /// lower case, and the cards members are named by.
    fn passes(self, c: &ContactDetails, grouped: &std::collections::HashSet<String>) -> bool {
        match self {
            ContactFilter::Photo => c.has_photo(),
            ContactFilter::Email => !c.emails.is_empty(),
            ContactFilter::Phone => !c.phones.is_empty(),
            ContactFilter::Address => !c.addresses.is_empty(),
            ContactFilter::Birthday => !c.birthday.is_empty(),
            ContactFilter::Company => !c.org.is_empty(),
            ContactFilter::NoGroup => {
                !grouped.contains(&c.grouped_key()) && !c.emails.iter().any(|e| grouped.contains(&e.value.to_lowercase()))
            }
        }
    }
}

/// How the contact list is ordered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContactSort {
    FirstName,
    LastName,
    Email,
}

/// Live handles into the open contact form.
struct ContactForm {
    name: adw::EntryRow,
    nickname: adw::EntryRow,
    org: adw::EntryRow,
    title: adw::EntryRow,
    note: gtk::TextView,
    emails: ValueRows,
    phones: ValueRows,
    urls: ValueRows,
    addresses: Rc<RefCell<Vec<AddressForm>>>,
    birthday: BirthdayForm,
    /// What happens to the photo: kept, removed or a new one.
    photo: Rc<RefCell<crate::contacts::PhotoEdit>>,
    /// The book a new contact goes to; `None` when editing one (it stays in
    /// its book) or before the books are known.
    book: Option<adw::ComboRow>,
}

/// Live handles into the open group form.
struct GroupForm {
    name: adw::EntryRow,
    hide: adw::SwitchRow,
    /// The members as the form has them, in order.
    members: Rc<RefCell<Vec<Member>>>,
    /// A new group's address book, picked from `books`, those that keep
    /// groups.
    book: Option<adw::ComboRow>,
    books: Vec<crate::contacts::Book>,
}

/// The form open in the detail pane.
enum Editor {
    Contact(ContactForm),
    Group(GroupForm),
}

pub struct ContactsPage {
    contacts: Vec<ContactDetails>,
    query: String,
    sort: ContactSort,
    /// What the list shows, picked in the sidebar.
    scope: ContactScope,
    /// The tree last handed to the sidebar, so an unchanged one is not.
    tree: Option<ContactTree>,
    /// What a row being dragged carries: each contact's book and UID, by
    /// index, and the indexes selected together.
    drag: Rc<RefCell<DragKeys>>,
    /// The "Show only" filters on, by [`ContactFilter::bit`].
    filters: u8,
    /// The sort and filter button's popover, kept in step with `sort`,
    /// `filters` and `hidden_books`.
    filter_menu: Option<FilterMenu>,
    /// The search bar is shown: opened, or kept open by the setting the
    /// message list's search has too.
    search_open: bool,
    search_pinned: bool,
    /// When the bar last closed: the click on the search button that took
    /// focus from an empty search, and closed the bar, must not open it
    /// again (as in the message list).
    search_closed_at: Option<std::time::Instant>,
    /// Index into `contacts` of the contact shown in the detail pane.
    selected: Option<usize>,
    /// The contacts selected together, as indexes into `contacts`, while
    /// more than one is: the pane then offers what can be done to them all.
    multi: Vec<usize>,
    /// List position → index into `contacts` (or a directory hit, see
    /// [`DIRECTORY`]); `None` for a line that is nobody, such as a heading.
    row_map: Vec<Option<usize>>,
    /// The list's lines, shown by a list view that builds rows only for the
    /// lines on screen: a thousand contacts took most of a second to build
    /// as rows, at every visit and every key typed in the search (#477).
    store: gtk::gio::ListStore,
    selection: gtk::MultiSelection,
    /// The row widgets the list view has made, by their list item.
    rows: Rc<RefCell<HashMap<usize, Rc<ListRow>>>>,
    /// One list item per contact, in step with `contacts`. The same item
    /// keeps its row through a search, a sort or a re-read that leaves it
    /// as it was: the view makes rows only for items it has not shown.
    items: Vec<gtk::glib::BoxedAnyObject>,
    /// A load is underway (the page was just opened).
    loading: bool,
    /// The editor form, while open. `editing_target` is the contact or group
    /// being edited, `None` for a brand-new one.
    editor: Option<Editor>,
    editing_target: Option<usize>,
    /// EDS UIDs deleted here but possibly still in EDS's local cache — a
    /// re-read races the CardDAV flush, and without these tombstones a
    /// deleted contact pops right back into the list. A tombstone lives
    /// until a fresh read no longer contains its contact. Keyed by book and
    /// UID: an imported card keeps the UID of the book it came from.
    deleted: std::collections::HashSet<(String, String)>,
    /// The books new contacts can go to, the chosen default first; the
    /// Hylki book is always among them.
    books: Vec<crate::contacts::Book>,
    /// The books the sidebar lists: (book UID, name).
    filter_books: Vec<(String, String)>,
    /// The people the LDAP directories hold for the search (#307), listed
    /// after the contacts under their own heading. Read-only: they can be
    /// written to and added to Contacts, not edited.
    directory_hits: Vec<ContactDetails>,
    /// Bumped at every change to the search, so an answer to an older one
    /// is dropped.
    directory_generation: u64,
    /// The directories switched on, as (UID, name): the list shows, with
    /// its search box, even with no contacts of its own, and the book
    /// filter offers each after the address books.
    directories: Vec<(String, String)>,
    /// The directories that did not answer the search, listed under the
    /// people found, so a search that finds nobody says why (#307).
    directory_failures: Vec<crate::directory::DirectoryFailure>,
    /// The composer's recipient suggestions (Contacts and mail history),
    /// shared with an open group form, which offers them as people to add.
    /// Read again each time a group form opens; until the first read lands
    /// the form offers the contacts listed here.
    suggestions: Rc<RefCell<Vec<crate::contacts::Suggestion>>>,
    /// A read that came in while a form was open, applied once it closes:
    /// a sync in the background must not throw away what is being typed.
    pending: Option<Vec<ContactDetails>>,
    /// The photos handed to a decoding thread that has not finished them.
    decoding: std::collections::HashSet<u64>,
}

#[derive(Debug)]
pub enum ContactsPageInput {
    /// Fresh EDS read: replace everything (keeps the selection when possible).
    SetContacts(Vec<ContactDetails>),
    /// The page was just opened; a read is on its way.
    SetLoading,
    Query(String),
    SetSort(ContactSort),
    /// The sidebar picked what the list shows.
    SetScope(ContactScope),
    /// Contacts, by book and UID, dropped on a book or group in the
    /// sidebar: copied there, or moved (Ctrl held) after a question.
    DropContacts { keys: Vec<(String, String)>, on: DropOn, moving: bool },
    /// A right-click choice on the sidebar's book or group.
    Tree(TreeAction),
    /// Take these people (indexes into `contacts`) out of the group shown.
    RemoveFromGroup(Vec<usize>),
    /// A "Show only" filter turned on or off.
    SetFilter(ContactFilter, bool),
    ClearFilters,
    /// The search button, Ctrl+F or `/`: open the search, or close it.
    FocusSearch,
    /// Esc in the search, or focus left it empty: close it and clear it.
    CloseSearch,
    /// The setting that keeps the search field open.
    SetSearchPinned(bool),
    /// The list's selection changed.
    SelectionChanged,
    /// Select the first `n` entries of the list (a showcase).
    SelectFirst(u32),
    /// A right-click on the line for this entry, at (x, y) on the list.
    RowMenu { idx: usize, x: f64, y: f64 },
    Compose(String),
    OpenUrl(String),
    /// Edit the selected contact / a specific one (context menu).
    Edit,
    EditIndex(usize),
    /// The header's "+": a blank editor for a new contact, or a new group
    /// (always in the Hylki book).
    NewContact,
    NewGroup,
    /// Write to everyone in the group at this index.
    ComposeGroup(usize),
    /// Put a person's first address in a group (both indexes into
    /// `contacts`).
    AddToGroup { group: usize, person: usize },
    /// The header's import button: choose `.vcf` files for the Hylki book.
    ImportContacts,
    CancelEdit,
    SaveEdit,
    OpenInGnome(usize),
    DeleteRequest(usize),
    /// The Delete key on the list: ask to delete the contact shown.
    DeleteSelected,
    /// The contacts to delete, by book and UID: the list may have been
    /// re-read and re-sorted while the dialog was open.
    DeleteConfirmed(Vec<(String, String)>),
    /// The entries selected together: put the people in the group at this
    /// index, ask to move them to this book or to delete them all.
    BulkAddToGroup(usize),
    BulkMoveRequest(String),
    /// Ask to move the contact at this index to the book `to`.
    MoveRequest { idx: usize, to: String },
    /// Move the contact being edited, by book and UID, to the book `to`
    /// as `vcard` (its card with the edits, made for the new book),
    /// confirmed.
    EditMoveConfirmed { to: String, key: (String, String), vcard: String },
    /// Move these people, by book and UID, to this book, confirmed.
    BulkMoveConfirmed { to: String, keys: Vec<(String, String)> },
    BulkDeleteRequest,
    /// A write failed: what was deleted or moved here may still be in its
    /// book, so the next read lists whatever is there.
    WriteFailed,
    /// The card's photo was clicked: expand it in the app lightbox.
    OpenPhoto { name: String, data: Vec<u8> },
    /// The books new contacts can go to, freshly read, default first.
    SetBooks(Vec<crate::contacts::Book>),
    /// The LDAP directories switched on, as (UID, name).
    DirectoriesKnown(Vec<(String, String)>),
    /// The search paused: ask the directories, unless it changed since.
    DirectoryLookup { generation: u64, query: String },
    DirectoryResults { generation: u64, hits: Vec<ContactDetails>, failed: Vec<crate::directory::DirectoryFailure> },
    /// Copy a directory entry into the default address book.
    AddDirectoryHit(usize),
    /// Put this in the search box, as if typed.
    SetSearch(String),
    /// The composer's suggestions, freshly read for a group form.
    SuggestionsLoaded(Vec<crate::contacts::Suggestion>),
    /// Show the book at this place in the sidebar (1 is the first), or
    /// every book (0).
    PickBook(u32),
    /// More list photos are decoded and cached.
    PhotosDecoded,
}

#[derive(Debug)]
pub enum ContactsPageOutput {
    /// Start a message to this address, or these (a group).
    Compose(String),
    /// Start a message with these recipients in Bcc: a group that hides
    /// its members' addresses.
    ComposeBcc(String),
    /// The header's sidebar button (same spot as the message list's).
    ToggleSidebar,
    /// Persist an edit: the patched vCard back to its book.
    SaveContact { book_uid: String, vcard: String },
    /// Create a brand-new contact in the book picked in the editor
    /// (`None`: the default book).
    CreateContact { book_uid: Option<String>, vcard: String },
    /// Write a group: a change to `original`, its stored card, or a new
    /// one when there is none.
    SaveGroup { book_uid: String, format: GroupFormat, original: Option<String>, edit: GroupEdit },
    /// Move a group: made in the book `to` from `edit`, then the one with
    /// UID `uid` deleted from the book `from`.
    MoveGroup { from: String, uid: String, to: String, format: GroupFormat, edit: GroupEdit },
    /// Delete these, by book and UID, already confirmed by the user.
    DeleteContacts(Vec<(String, String)>),
    /// Move these people to the book `to`: (book, UID, the card for the
    /// new book), confirmed.
    MoveContacts { to: String, cards: Vec<(String, String, String)> },
    /// Copy these cards (made for the book) into the book `to`.
    CopyContacts { to: String, cards: Vec<String> },
    /// The books and groups changed: the sidebar lists them anew.
    Tree(ContactTree),
    /// Name an address book in Hylki (`None`: its own name again).
    RenameBook { uid: String, name: Option<String> },
    /// `.vcf` files chosen for import into the local Hylki book.
    ImportContacts(Vec<std::path::PathBuf>),
    /// Show the contact's photo in the app lightbox.
    ShowPhoto { name: String, data: Vec<u8> },
    /// Read the composer's suggestions for a group form
    /// ([`ContactsPageInput::SuggestionsLoaded`] brings them).
    WantSuggestions,
}

#[relm4::component(pub)]
impl Component for ContactsPage {
    /// The app's compose revealer, mounted over the detail pane.
    type Init = gtk::Revealer;
    type Input = ContactsPageInput;
    type Output = ContactsPageOutput;
    type CommandOutput = ();

    view! {
        #[name = "page_stack"]
        gtk::Stack {
            set_transition_type: gtk::StackTransitionType::Crossfade,

            // No contacts at all (or still loading the first read).
            add_named[Some("none")] = &adw::ToolbarView {
                add_top_bar = &adw::HeaderBar {
                    add_css_class: "flat",
                    #[wrap(Some)]
                    set_title_widget = &gtk::Label {
                        set_label: &i18n("Contacts"),
                        add_css_class: "pane-title",
                    },
                    pack_start = &gtk::Button {
                        set_icon_name: "sidebar-show-symbolic",
                        set_tooltip_text: Some(i18n("Toggle sidebar").as_str()),
                        add_css_class: "flat",
                        connect_clicked[sender] => move |_| {
                            let _ = sender.output(ContactsPageOutput::ToggleSidebar);
                        },
                    },
                    pack_end = &gtk::Button {
                        set_icon_name: "document-open-symbolic",
                        set_tooltip_text: Some(i18n("Import vCard files into the Hylki address book").as_str()),
                        add_css_class: "flat",
                        connect_clicked => ContactsPageInput::ImportContacts,
                    },
                    pack_end = &gtk::Button {
                        set_icon_name: "list-add-symbolic",
                        set_tooltip_text: Some(i18n("New contact or group").as_str()),
                        add_css_class: "flat",
                        connect_clicked[sender] => move |b| new_menu(b, &sender),
                    },
                },

                #[wrap(Some)]
                set_content = &adw::StatusPage {
                    set_icon_name: Some("x-office-address-book-symbolic"),
                    #[watch]
                    set_title: &if model.loading { i18n("Loading Contacts…") } else { i18n("No Contacts") },
                    #[watch]
                    set_description: Some(if model.loading {
                        String::new()
                    } else {
                        i18n("Add a contact with the + button, or import vCard files. Contacts are kept in Hylki's own address book, and the address books of GNOME Contacts are listed here too.")
                    }.as_str()),
                },
            },

            // Each pane carries its own header (like the mail view), so the
            // divider between them runs the full height of the window; the
            // paned handle lets the list grow, with its launch width as the
            // floor.
            add_named[Some("browser")] = &gtk::Paned {
                set_orientation: gtk::Orientation::Horizontal,
                set_wide_handle: false,
                set_position: crate::config::load_contacts_pane_width(),
                set_resize_start_child: false,
                set_shrink_start_child: false,
                set_resize_end_child: true,
                set_shrink_end_child: false,
                // Remember the width the user drags to. Debounced: one write
                // once the drag settles (see the mail view's list pane).
                connect_position_notify[
                    pending = std::rc::Rc::new(std::cell::RefCell::new(
                        None::<gtk::glib::SourceId>,
                    ))
                ] => move |p| {
                    let pos = p.position();
                    if let Some(id) = pending.borrow_mut().take() {
                        id.remove();
                    }
                    let armed = pending.clone();
                    *pending.borrow_mut() = Some(gtk::glib::timeout_add_local_once(
                        std::time::Duration::from_millis(600),
                        move || {
                            *armed.borrow_mut() = None;
                            crate::config::save_contacts_pane_width(pos);
                        },
                    ));
                },

                #[wrap(Some)]
                set_start_child = &adw::ToolbarView {
                    add_top_bar = &adw::HeaderBar {
                        add_css_class: "flat",
                        // Left pane: no window controls (the detail pane's
                        // header carries the window's close button).
                        set_show_start_title_buttons: false,
                        set_show_end_title_buttons: false,
                        #[wrap(Some)]
                        set_title_widget = &gtk::Label {
                            set_label: "",
                        },
                        // Leftmost, mirroring the pane it acts on — same spot
                        // as in the message list's header.
                        pack_start = &gtk::Button {
                            set_icon_name: "sidebar-show-symbolic",
                            set_tooltip_text: Some(i18n("Toggle sidebar").as_str()),
                            add_css_class: "flat",
                            connect_clicked[sender] => move |_| {
                                let _ = sender.output(ContactsPageOutput::ToggleSidebar);
                            },
                        },
                        // Beside it, as in the message list's header.
                        pack_start = &gtk::Button {
                            set_icon_name: "system-search-symbolic",
                            set_tooltip_text: Some(i18n("Search contacts (Ctrl+F)").as_str()),
                            add_css_class: "flat",
                            connect_clicked => ContactsPageInput::FocusSearch,
                        },
                        // pack_end packs right-to-left: sort and filter at the
                        // far right (matching the message list), then the
                        // count, then import, then "+", first on the right.
                        #[name = "filter_btn"]
                        pack_end = &gtk::MenuButton {
                            set_icon_name: "filter-symbolic",
                            set_tooltip_text: Some(i18n("Sort and filter contacts").as_str()),
                            set_valign: gtk::Align::Center,
                            add_css_class: "flat",
                        },
                        #[name = "count_label"]
                        pack_end = &gtk::Label {
                            set_valign: gtk::Align::Center,
                            add_css_class: "list-count",
                        },
                        pack_end = &gtk::Button {
                            set_icon_name: "document-open-symbolic",
                            set_tooltip_text: Some(i18n("Import vCard files into the Hylki address book").as_str()),
                            add_css_class: "flat",
                            connect_clicked => ContactsPageInput::ImportContacts,
                        },
                        pack_end = &gtk::Button {
                            set_icon_name: "list-add-symbolic",
                            set_tooltip_text: Some(i18n("New contact or group").as_str()),
                            add_css_class: "flat",
                            connect_clicked[sender] => move |b| new_menu(b, &sender),
                        },
                    },

                    #[wrap(Some)]
                    set_content = &gtk::Box {
                        set_orientation: gtk::Orientation::Vertical,
                        set_width_request: 280,

                        // The search, shown by the header's search button:
                        // the message list's search bar.
                        gtk::Revealer {
                            set_transition_type: gtk::RevealerTransitionType::SlideDown,
                            #[watch]
                            set_reveal_child: model.search_open || model.search_pinned,

                            gtk::Box {
                                set_spacing: 6,
                                add_css_class: "list-search-row",

                                #[name = "search"]
                                gtk::SearchEntry {
                                    set_hexpand: true,
                                    set_width_chars: 3,
                                    set_placeholder_text: Some(i18n("Search contacts").as_str()),
                                    connect_search_changed[sender] => move |e| {
                                        sender.input(ContactsPageInput::Query(e.text().to_string()));
                                    },
                                    connect_stop_search[sender] => move |_| {
                                        sender.input(ContactsPageInput::CloseSearch);
                                    },
                                    // Leaving it empty closes it too.
                                    add_controller = gtk::EventControllerFocus {
                                        connect_leave[sender] => move |ctl| {
                                            let Some(entry) = ctl.widget().and_downcast::<gtk::SearchEntry>() else { return };
                                            let sender = sender.clone();
                                            crate::ui::close_if_focus_left_row(&entry, move || {
                                                sender.input(ContactsPageInput::CloseSearch);
                                            });
                                        },
                                    },
                                },

                            },
                        },

                        gtk::ScrolledWindow {
                            set_vexpand: true,
                            set_hscrollbar_policy: gtk::PolicyType::Never,

                            #[name = "list"]
                            gtk::ListView {
                                add_css_class: "navigation-sidebar",
                                add_css_class: "contacts-listbox",
                            },
                        },
                    },
                },

                #[wrap(Some)]
                #[name = "detail_overlay"]
                set_end_child = &gtk::Overlay {
                    // The app mounts its contacts compose slot over this in
                    // init, so "New message" slides down right here.
                    #[wrap(Some)]
                    set_child = &adw::ToolbarView {
                    set_hexpand: true,

                    add_top_bar = &adw::HeaderBar {
                        add_css_class: "flat",
                        set_show_start_title_buttons: false,
                        // Top-left of the card pane: edit the shown contact.
                        #[name = "edit_btn"]
                        pack_start = &gtk::Button {
                            set_icon_name: "document-edit-symbolic",
                            set_tooltip_text: Some(i18n("Edit contact").as_str()),
                            add_css_class: "flat",
                            connect_clicked => ContactsPageInput::Edit,
                        },
                        #[wrap(Some)]
                        set_title_widget = &gtk::Label {
                            set_label: &i18n("Contacts"),
                            add_css_class: "pane-title",
                        },
                    },

                    #[wrap(Some)]
                    set_content = &gtk::ScrolledWindow {
                        set_hexpand: true,
                        set_vexpand: true,
                        set_hscrollbar_policy: gtk::PolicyType::Never,
                        add_css_class: "contact-detail-pane",

                        adw::Clamp {
                            set_maximum_size: 560,
                            set_tightening_threshold: 420,

                            #[name = "detail_box"]
                            gtk::Box {
                                set_orientation: gtk::Orientation::Vertical,
                                set_margin_top: 24,
                                set_margin_bottom: 32,
                                set_margin_start: 18,
                                set_margin_end: 18,
                            },
                        },
                    },
                    },
                },
            },
        }
    }

    fn init(
        compose_slot: Self::Init,
        root: Self::Root,
        sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        let mut model = ContactsPage {
            contacts: Vec::new(),
            query: String::new(),
            sort: ContactSort::FirstName,
            scope: ContactScope::All,
            tree: None,
            drag: Rc::new(RefCell::new(DragKeys::default())),
            filters: 0,
            filter_menu: None,
            search_open: false,
            search_pinned: false,
            search_closed_at: None,
            selected: None,
            multi: Vec::new(),
            row_map: Vec::new(),
            store: gtk::gio::ListStore::new::<gtk::glib::BoxedAnyObject>(),
            selection: gtk::MultiSelection::new(None::<gtk::gio::ListStore>),
            rows: Rc::new(RefCell::new(HashMap::new())),
            items: Vec::new(),
            loading: true,
            editor: None,
            editing_target: None,
            deleted: std::collections::HashSet::new(),
            books: Vec::new(),
            filter_books: Vec::new(),
            directory_hits: Vec::new(),
            directory_generation: 0,
            directories: Vec::new(),
            directory_failures: Vec::new(),
            suggestions: Rc::new(RefCell::new(Vec::new())),
            pending: None,
            decoding: std::collections::HashSet::new(),
        };
        let widgets = view_output!();
        widgets.detail_overlay.add_overlay(&compose_slot);

        // Delete on the list asks to remove the contact being looked at. The
        // controller sits on the list, not the page, so Delete inside the
        // search box or the editor still edits text.
        {
            let keys = gtk::EventControllerKey::new();
            let s = sender.input_sender().clone();
            keys.connect_key_pressed(move |_, keyval, _, _| {
                if !matches!(keyval, gtk::gdk::Key::Delete | gtk::gdk::Key::KP_Delete) {
                    return gtk::glib::Propagation::Proceed;
                }
                let _ = s.send(ContactsPageInput::DeleteSelected);
                gtk::glib::Propagation::Stop
            });
            widgets.list.add_controller(keys);
        }

        model.filter_menu = Some(FilterMenu::new(&widgets.filter_btn, sender.input_sender()));

        model.selection.set_model(Some(&model.store));
        {
            let s = sender.input_sender().clone();
            model.selection.connect_selection_changed(move |_, _, _| {
                let _ = s.send(ContactsPageInput::SelectionChanged);
            });
        }
        widgets.list.set_model(Some(&model.selection));
        widgets.list.set_factory(Some(&row_factory(&model.rows, sender.input_sender(), &model.drag)));

        ComponentParts { model, widgets }
    }

    fn update_with_view(
        &mut self,
        widgets: &mut Self::Widgets,
        message: Self::Input,
        sender: ComponentSender<Self>,
        _root: &Self::Root,
    ) {
        match message {
            ContactsPageInput::SetLoading => {
                self.loading = true;
                if self.contacts.is_empty() && self.directories.is_empty() {
                    widgets.page_stack.set_visible_child_name("none");
                }
                let s = sender.input_sender().clone();
                std::thread::spawn(move || {
                    let _ = s.send(ContactsPageInput::DirectoriesKnown(crate::directory::enabled_names()));
                });
            }

            ContactsPageInput::SetContacts(contacts) if self.editor.is_some() => {
                self.pending = Some(contacts);
            }

            ContactsPageInput::SetContacts(mut contacts) => {
                self.pending = None;
                // Drop tombstoned (locally deleted) entries the read still
                // carries; a tombstone retires once a read comes back clean.
                let incoming: std::collections::HashSet<(String, String)> =
                    contacts.iter().map(|c| (c.book_uid.clone(), c.eds_uid.clone())).collect();
                self.deleted.retain(|key| incoming.contains(key));
                contacts.retain(|c| !self.deleted.contains(&(c.book_uid.clone(), c.eds_uid.clone())));
                // Every visit re-reads the books, and a sync behind it reads
                // them again; unchanged, the list on screen stays (#477).
                if !contacts.is_empty() && same_contacts(&self.contacts, &contacts) {
                    // In the same order, so every index still holds: the
                    // fresh cards (an edit patches them) without new rows.
                    self.contacts = contacts;
                    self.loading = false;
                    // Their photos too, which the rows have as thumbnails.
                    self.decode_photos(Default::default(), &sender);
                    self.update_view(widgets, sender);
                    return;
                }
                // Keep the same person selected across a refresh, by identity.
                let on_hit = self.selected.filter(|&i| i >= DIRECTORY);
                let keep = self
                    .selected
                    .and_then(|i| self.contacts.get(i))
                    .map(|c| (c.name.clone(), c.primary_email().map(str::to_string)));
                // And those selected together, the same way.
                let identity = |c: &ContactDetails| {
                    (c.book_uid.clone(), c.eds_uid.clone(), c.name.clone(), c.primary_email().map(str::to_string))
                };
                let multi: Vec<_> = self.multi.iter().filter_map(|&i| self.contacts.get(i)).map(identity).collect();
                self.multi = contacts
                    .iter()
                    .enumerate()
                    .filter(|(_, c)| multi.contains(&identity(c)))
                    .map(|(i, _)| i)
                    .collect();
                self.items = self.items_for(&contacts);
                self.contacts = contacts;
                self.loading = false;
                self.editor = None;
                self.editing_target = None;
                self.selected = keep
                    .and_then(|(name, email)| {
                        self.contacts.iter().position(|c| {
                            c.name == name && c.primary_email().map(str::to_string) == email
                        })
                    })
                    .or(on_hit);
                // One just added from a directory is a contact now.
                self.drop_known_hits();
                if self.selected.is_some_and(|i| self.entry(i).is_none()) {
                    self.selected = None;
                }
                self.refresh_book_filter();
                self.rebuild(widgets, &sender);
            }

            ContactsPageInput::Query(q) if q.to_lowercase() == self.query => {}

            ContactsPageInput::Query(q) => {
                self.query = q.to_lowercase();
                self.directory_generation = self.directory_generation.wrapping_add(1);
                let typed = q.trim().to_string();
                if typed.chars().count() < crate::directory::MIN_QUERY {
                    self.directory_hits.clear();
                    self.directory_failures.clear();
                } else {
                    // What the directories found before narrows with the
                    // search until they answer this one.
                    let query = self.query.clone();
                    self.directory_hits.retain(|c| matches_query(c, query.trim()));
                    // Asked once the typing pauses, not per key.
                    let generation = self.directory_generation;
                    let s = sender.input_sender().clone();
                    gtk::glib::timeout_add_local_once(std::time::Duration::from_millis(300), move || {
                        let _ = s.send(ContactsPageInput::DirectoryLookup { generation, query: typed });
                    });
                }
                if self.selected.is_some_and(|i| self.entry(i).is_none()) {
                    self.selected = None;
                }
                self.rebuild(widgets, &sender);
            }

            ContactsPageInput::SetSearch(text) => {
                self.search_open = true;
                widgets.search.set_text(&text);
            }

            ContactsPageInput::FocusSearch => {
                if self.search_pinned {
                    widgets.search.grab_focus();
                } else if self.search_open {
                    sender.input(ContactsPageInput::CloseSearch);
                } else if self.search_closed_at.is_none_or(|t| t.elapsed() > std::time::Duration::from_millis(300)) {
                    self.search_open = true;
                    // Once the revealer has mapped it.
                    let entry = widgets.search.clone();
                    gtk::glib::idle_add_local_once(move || {
                        entry.grab_focus();
                    });
                }
            }

            ContactsPageInput::CloseSearch => {
                let had_focus = widgets.search.has_focus() || widgets.search.focus_child().is_some();
                if !self.search_pinned {
                    self.search_open = false;
                    self.search_closed_at = Some(std::time::Instant::now());
                }
                // A hidden search narrows nothing: its text goes.
                let narrowed = !self.query.is_empty();
                self.query.clear();
                widgets.search.set_text("");
                self.directory_generation = self.directory_generation.wrapping_add(1);
                self.directory_hits.clear();
                self.directory_failures.clear();
                if narrowed {
                    self.rebuild(widgets, &sender);
                }
                if had_focus {
                    widgets.list.grab_focus();
                }
            }

            ContactsPageInput::SetSearchPinned(on) => {
                self.search_pinned = on;
            }

            ContactsPageInput::SetFilter(filter, on) => {
                let filters = if on { self.filters | filter.bit() } else { self.filters & !filter.bit() };
                if filters != self.filters {
                    self.filters = filters;
                    self.sync_filter_menu();
                    self.rebuild(widgets, &sender);
                }
            }

            ContactsPageInput::ClearFilters => {
                if self.filters != 0 {
                    self.filters = 0;
                    self.sync_filter_menu();
                    self.rebuild(widgets, &sender);
                }
            }

            ContactsPageInput::SuggestionsLoaded(suggestions) => {
                *self.suggestions.borrow_mut() = suggestions;
            }

            ContactsPageInput::PickBook(n) => {
                let scope = match (n as usize).checked_sub(1).and_then(|i| self.filter_books.get(i)) {
                    Some((uid, _)) => ContactScope::Book(uid.clone()),
                    None => ContactScope::All,
                };
                sender.input(ContactsPageInput::SetScope(scope));
            }

            ContactsPageInput::DirectoriesKnown(directories) => {
                if directories == self.directories {
                    return;
                }
                self.directories = directories;
                self.refresh_book_filter();
                self.publish_tree(&sender);
                if !self.directories.is_empty() && self.contacts.is_empty() && !self.loading {
                    widgets.page_stack.set_visible_child_name("browser");
                }
            }

            ContactsPageInput::DirectoryLookup { generation, query } => {
                if generation != self.directory_generation {
                    return;
                }
                let s = sender.input_sender().clone();
                std::thread::spawn(move || {
                    let (hits, failed) = crate::directory::search_contacts(&query, DIRECTORY_LIMIT);
                    let _ = s.send(ContactsPageInput::DirectoryResults { generation, hits, failed });
                });
            }

            ContactsPageInput::DirectoryResults { generation, hits, failed } => {
                if generation != self.directory_generation {
                    return;
                }
                self.directory_failures = failed;
                // The entry shown may be at another place in the new list.
                let shown = self
                    .selected
                    .filter(|&i| i >= DIRECTORY)
                    .and_then(|i| self.entry(i))
                    .map(|c| (c.name.clone(), c.primary_email().map(str::to_string)));
                self.directory_hits = hits;
                self.drop_known_hits();
                if let Some((name, email)) = shown {
                    self.selected = self
                        .directory_hits
                        .iter()
                        .position(|c| c.name == name && c.primary_email().map(str::to_string) == email)
                        .map(|i| DIRECTORY + i);
                }
                // An open editor stays as it is; the list catches up with
                // the next change.
                if self.editor.is_none() {
                    self.rebuild(widgets, &sender);
                }
            }

            ContactsPageInput::AddDirectoryHit(idx) => {
                let Some(c) = idx.checked_sub(DIRECTORY).and_then(|i| self.directory_hits.get(i)) else {
                    return;
                };
                // A card of its own, not the directory's, whose UID is the
                // directory's name for the entry.
                let edit = ContactEdit {
                    name: c.name.clone(),
                    nickname: c.nickname.clone(),
                    org: c.org.clone(),
                    title: c.title.clone(),
                    note: c.note.clone(),
                    emails: c.emails.clone(),
                    phones: c.phones.clone(),
                    urls: c.urls.clone(),
                    addresses: c.postal.clone(),
                    birthday: c.birthday_raw.clone(),
                    photo: c.photo_bytes().map_or(crate::contacts::PhotoEdit::Keep, crate::contacts::PhotoEdit::Set),
                };
                let _ = sender.output(ContactsPageOutput::CreateContact {
                    book_uid: None,
                    vcard: crate::contacts::new_vcard(&edit),
                });
            }

            ContactsPageInput::SetSort(sort) => {
                if self.sort != sort {
                    self.sort = sort;
                    self.sync_filter_menu();
                    self.rebuild(widgets, &sender);
                }
            }

            ContactsPageInput::SetScope(scope) => {
                if self.scope != scope {
                    self.scope = scope;
                    // Another list abandons an open editor and what was
                    // selected in this one.
                    self.editor = None;
                    self.editing_target = None;
                    self.selected = None;
                    self.multi.clear();
                    self.rebuild(widgets, &sender);
                }
            }

            ContactsPageInput::DropContacts { keys, on, moving } => {
                let people: Vec<usize> = keys
                    .iter()
                    .filter_map(|(book, uid)| {
                        self.contacts.iter().position(|c| !c.is_group() && c.book_uid == *book && c.eds_uid == *uid)
                    })
                    .collect();
                match on {
                    DropOn::Book(to) if moving => {
                        let keys: Vec<(String, String)> = people
                            .iter()
                            .map(|&i| &self.contacts[i])
                            .filter(|c| self.can_move(c, &to))
                            .map(|c| (c.book_uid.clone(), c.eds_uid.clone()))
                            .collect();
                        self.ask_move(widgets, &sender, keys, to, None);
                    }
                    // A copy leaves the contact where it was: no question.
                    DropOn::Book(to) => {
                        let writable = self.books.iter().any(|b| b.uid == to);
                        let cards: Vec<String> = people
                            .iter()
                            .map(|&i| &self.contacts[i])
                            .filter(|c| writable && c.book_uid != to)
                            .map(crate::contacts::moved_vcard)
                            .collect();
                        if !cards.is_empty() {
                            let _ = sender.output(ContactsPageOutput::CopyContacts { to, cards });
                        }
                    }
                    DropOn::Group { book, uid } => {
                        let Some(g) = self.contacts.iter().position(|c| c.is_group() && c.book_uid == book && c.eds_uid == uid)
                        else {
                            return;
                        };
                        self.add_to_group(&sender, g, &people);
                    }
                }
            }

            ContactsPageInput::Tree(action) => {
                let group = |book: &str, uid: &str| {
                    self.contacts.iter().position(|c| c.is_group() && c.book_uid == book && c.eds_uid == uid)
                };
                match action {
                    TreeAction::NewContact(book) => {
                        sender.input(ContactsPageInput::SetScope(ContactScope::Book(book)));
                        sender.input(ContactsPageInput::NewContact);
                    }
                    TreeAction::NewGroup(book) => {
                        sender.input(ContactsPageInput::SetScope(ContactScope::Book(book)));
                        sender.input(ContactsPageInput::NewGroup);
                    }
                    TreeAction::EditGroup { book, uid } => {
                        if let Some(idx) = group(&book, &uid) {
                            sender.input(ContactsPageInput::SetScope(ContactScope::Group { book, uid }));
                            sender.input(ContactsPageInput::EditIndex(idx));
                        }
                    }
                    TreeAction::DeleteGroup { book, uid } => {
                        if let Some(idx) = group(&book, &uid) {
                            sender.input(ContactsPageInput::DeleteRequest(idx));
                        }
                    }
                    TreeAction::RenameBook(uid) => self.ask_book_name(widgets, &sender, uid),
                    TreeAction::ComposeGroup { book, uid } => {
                        if let Some(idx) = group(&book, &uid) {
                            sender.input(ContactsPageInput::ComposeGroup(idx));
                        }
                    }
                }
            }

            ContactsPageInput::RemoveFromGroup(people) => {
                let ContactScope::Group { book, uid } = &self.scope else { return };
                let Some(g) = self.contacts.iter().find(|c| c.is_group() && c.book_uid == *book && c.eds_uid == *uid)
                else {
                    return;
                };
                let Some(format) = g.group else { return };
                let gone: Vec<&ContactDetails> = people.iter().filter_map(|&i| self.contacts.get(i)).collect();
                let members: Vec<Member> =
                    g.members.iter().filter(|m| !gone.iter().any(|p| is_member(g, m, p))).cloned().collect();
                if members.len() == g.members.len() {
                    return;
                }
                let edit = GroupEdit {
                    name: g.name.clone(),
                    members,
                    hide_addresses: g.hide_addresses,
                    kept: g.hidden_members.clone(),
                };
                let _ = sender.output(ContactsPageOutput::SaveGroup {
                    book_uid: g.book_uid.clone(),
                    format,
                    original: Some(g.raw_vcard.clone()),
                    edit,
                });
            }

            ContactsPageInput::SelectionChanged => {
                // Read as the selection is now: the change that sent this
                // may have been followed by others.
                let picked = self.picked_entries();
                match picked.as_slice() {
                    [] => {}
                    [idx] => {
                        let idx = *idx;
                        if self.multi.is_empty()
                            && self.selected == Some(idx)
                            && (self.editor.is_none() || self.editing_target == Some(idx))
                        {
                            return;
                        }
                        // Moving away abandons an open editor.
                        self.multi.clear();
                        self.editor = None;
                        self.editing_target = None;
                        self.selected = Some(idx);
                        self.render_detail(widgets, &sender);
                    }
                    many => {
                        // A directory's entry is not a contact to act on.
                        let keys: Vec<usize> = many.iter().copied().filter(|&i| i < self.contacts.len()).collect();
                        if keys == self.multi && self.editor.is_none() {
                            return;
                        }
                        self.multi = keys;
                        self.editor = None;
                        self.editing_target = None;
                        self.selected = None;
                        self.render_detail(widgets, &sender);
                    }
                }
            }

            ContactsPageInput::SelectFirst(n) => {
                let entries: Vec<u32> = self
                    .row_map
                    .iter()
                    .enumerate()
                    .filter(|(_, i)| i.is_some())
                    .map(|(pos, _)| pos as u32)
                    .take(n as usize)
                    .collect();
                self.selection.unselect_all();
                for pos in entries {
                    self.selection.select_item(pos, false);
                }
            }

            ContactsPageInput::BulkAddToGroup(g) => {
                let chosen = self.chosen();
                self.add_to_group(&sender, g, &chosen);
            }

            ContactsPageInput::BulkMoveRequest(to) => {
                let keys: Vec<(String, String)> = self
                    .movable(&to)
                    .into_iter()
                    .map(|i| (self.contacts[i].book_uid.clone(), self.contacts[i].eds_uid.clone()))
                    .collect();
                self.ask_move(widgets, &sender, keys, to, None);
            }

            ContactsPageInput::MoveRequest { idx, to } => {
                let Some(c) = self.contacts.get(idx).filter(|c| self.can_move(c, &to)) else { return };
                let key = (c.book_uid.clone(), c.eds_uid.clone());
                self.ask_move(widgets, &sender, vec![key], to, None);
            }

            ContactsPageInput::EditMoveConfirmed { to, key, vcard } => {
                self.forget(std::slice::from_ref(&key));
                let _ = sender.output(ContactsPageOutput::MoveContacts { to, cards: vec![(key.0, key.1, vcard)] });
                self.editor = None;
                self.editing_target = None;
                self.rebuild(widgets, &sender);
            }

            ContactsPageInput::BulkMoveConfirmed { to, keys } => {
                let cards: Vec<(String, String, String)> = keys
                    .iter()
                    .filter_map(|(book, uid)| self.contacts.iter().find(|c| c.book_uid == *book && c.eds_uid == *uid))
                    .map(|c| (c.book_uid.clone(), c.eds_uid.clone(), crate::contacts::moved_vcard(c)))
                    .collect();
                if cards.is_empty() {
                    return;
                }
                // They leave their old books' lists now; the re-read after
                // the move shows them in the new one.
                self.forget(&keys);
                let _ = sender.output(ContactsPageOutput::MoveContacts { to, cards });
                self.rebuild(widgets, &sender);
            }

            ContactsPageInput::WriteFailed => {
                self.deleted.clear();
            }

            ContactsPageInput::BulkDeleteRequest => {
                let chosen = self.chosen();
                if chosen.is_empty() {
                    return;
                }
                let keys: Vec<(String, String)> = chosen
                    .iter()
                    .map(|&i| (self.contacts[i].book_uid.clone(), self.contacts[i].eds_uid.clone()))
                    .collect();
                let synced = chosen.iter().any(|&i| self.synced(&self.contacts[i].book_uid));
                let groups = chosen.iter().all(|&i| self.contacts[i].is_group());
                let n = chosen.len() as u32;
                let count = n.to_string();
                let args = [("n", count.as_str())];
                let (heading, body) = match (groups, synced) {
                    (true, true) => (
                        ni18n_f("Delete {n} Group?", "Delete {n} Groups?", n, &args),
                        ni18n_f(
                            "This will remove {n} group from your address books locally and on the server. The contacts are not removed.",
                            "This will remove {n} groups from your address books locally and on the server. The contacts are not removed.",
                            n,
                            &args,
                        ),
                    ),
                    (true, false) => (
                        ni18n_f("Delete {n} Group?", "Delete {n} Groups?", n, &args),
                        ni18n_f(
                            "This will remove {n} group from your address book. The contacts are not removed.",
                            "This will remove {n} groups from your address books. The contacts are not removed.",
                            n,
                            &args,
                        ),
                    ),
                    (false, true) => (
                        ni18n_f("Delete {n} Contact?", "Delete {n} Contacts?", n, &args),
                        ni18n_f(
                            "This will remove {n} contact from your address books locally and on the server.",
                            "This will remove {n} contacts from your address books locally and on the server.",
                            n,
                            &args,
                        ),
                    ),
                    (false, false) => (
                        ni18n_f("Delete {n} Contact?", "Delete {n} Contacts?", n, &args),
                        ni18n_f(
                            "This will remove {n} contact from your address book.",
                            "This will remove {n} contacts from your address books.",
                            n,
                            &args,
                        ),
                    ),
                };
                let s = sender.input_sender().clone();
                confirm(widgets, &heading, &body, &i18n("Delete"), true, move || {
                    let _ = s.send(ContactsPageInput::DeleteConfirmed(keys.clone()));
                });
            }

            ContactsPageInput::Compose(email) => {
                let _ = sender.output(ContactsPageOutput::Compose(email));
            }

            ContactsPageInput::OpenUrl(url) => {
                // Bare "example.org" URLs are common in vCards.
                let full = if url.contains("://") { url } else { format!("https://{url}") };
                crate::ui::launch::open_link(&full, None);
            }

            ContactsPageInput::Edit => {
                if let Some(idx) = self.selected.filter(|&i| i < DIRECTORY) {
                    self.edit(widgets, &sender, idx);
                }
            }

            ContactsPageInput::EditIndex(idx) => {
                if idx < self.contacts.len() {
                    self.selected = Some(idx);
                    self.select_row_for(idx);
                    self.edit(widgets, &sender, idx);
                }
            }

            ContactsPageInput::NewContact => {
                self.new_form(widgets);
                self.render_editor(widgets, &sender);
            }

            ContactsPageInput::NewGroup => {
                self.new_form(widgets);
                self.render_group_editor(widgets, &sender);
            }

            ContactsPageInput::ComposeGroup(idx) => {
                let Some(g) = self.contacts.get(idx).filter(|g| g.reachable().next().is_some()) else { return };
                let field = g.members_field();
                let _ = sender.output(if g.hide_addresses {
                    ContactsPageOutput::ComposeBcc(field)
                } else {
                    ContactsPageOutput::Compose(field)
                });
            }

            ContactsPageInput::AddToGroup { group, person } => {
                let (Some(g), Some(p)) = (self.contacts.get(group), self.contacts.get(person)) else { return };
                let Some(member) = member_for(g, p, true) else { return };
                let mut members = g.members.clone();
                members.push(member);
                let edit = GroupEdit {
                    name: g.name.clone(),
                    members,
                    hide_addresses: g.hide_addresses,
                    kept: g.hidden_members.clone(),
                };
                let Some(format) = g.group else { return };
                let _ = sender.output(ContactsPageOutput::SaveGroup {
                    book_uid: g.book_uid.clone(),
                    format,
                    original: Some(g.raw_vcard.clone()),
                    edit,
                });
            }

            ContactsPageInput::ImportContacts => {
                let parent = relm4::main_application().active_window();
                let s = sender.clone();
                choose_vcf_files(parent.as_ref(), move |paths| {
                    let _ = s.output(ContactsPageOutput::ImportContacts(paths));
                });
            }

            ContactsPageInput::CancelEdit => {
                self.editor = None;
                self.editing_target = None;
                if let Some(contacts) = self.pending.take() {
                    sender.input(ContactsPageInput::SetContacts(contacts));
                }
                if self.contacts.is_empty() && self.directories.is_empty() {
                    widgets.page_stack.set_visible_child_name("none");
                }
                self.render_detail(widgets, &sender);
            }

            ContactsPageInput::SaveEdit => {
                let editor = match &self.editor {
                    Some(Editor::Contact(form)) => form,
                    Some(Editor::Group(form)) => {
                        let mut edit = GroupEdit {
                            name: form.name.text().trim().to_string(),
                            members: form.members.borrow().clone(),
                            hide_addresses: form.hide.is_active(),
                            ..GroupEdit::default()
                        };
                        // A group is known by its name alone.
                        if edit.name.is_empty() {
                            form.name.add_css_class("error");
                            form.name.grab_focus();
                            return;
                        }
                        let picked = form.book.as_ref().and_then(|row| form.books.get(row.selected() as usize)).cloned();
                        let output = match self.editing_target.and_then(|i| self.contacts.get(i)) {
                            // Saved in another book: made there, then removed
                            // from its own.
                            Some(g) if picked.as_ref().is_some_and(|b| b.uid != g.book_uid) => {
                                let to = picked.unwrap_or_else(crate::contacts::local_book);
                                let format = to.groups.unwrap_or(GroupFormat::Evolution);
                                edit.members = self.members_for(edit.members, Some(&g.book_uid), &to.uid, format);
                                let key = (g.book_uid.clone(), g.eds_uid.clone());
                                self.forget(&[key.clone()]);
                                let _ = sender.output(ContactsPageOutput::MoveGroup {
                                    from: key.0,
                                    uid: key.1,
                                    to: to.uid,
                                    format,
                                    edit,
                                });
                                self.editor = None;
                                self.editing_target = None;
                                self.rebuild(widgets, &sender);
                                return;
                            }
                            Some(g) => {
                                let format = g.group.unwrap_or(GroupFormat::Evolution);
                                edit.members = self.members_for(edit.members, Some(&g.book_uid), &g.book_uid, format);
                                edit.kept = g.hidden_members.clone();
                                ContactsPageOutput::SaveGroup {
                                    book_uid: g.book_uid.clone(),
                                    format,
                                    original: Some(g.raw_vcard.clone()),
                                    edit,
                                }
                            }
                            None => {
                                let book = picked.unwrap_or_else(crate::contacts::local_book);
                                let format = book.groups.unwrap_or(GroupFormat::Evolution);
                                edit.members = self.members_for(edit.members, None, &book.uid, format);
                                ContactsPageOutput::SaveGroup { book_uid: book.uid, format, original: None, edit }
                            }
                        };
                        let _ = sender.output(output);
                        self.editor = None;
                        self.editing_target = None;
                        self.render_detail(widgets, &sender);
                        return;
                    }
                    None => return,
                };
                let edit = collect_edit(editor);
                // Anything at all is a contact; an empty form is nothing.
                let empty = [&edit.name, &edit.nickname, &edit.org, &edit.title, &edit.note, &edit.birthday]
                    .iter()
                    .all(|v| v.is_empty())
                    && edit.emails.is_empty()
                    && edit.phones.is_empty()
                    && edit.urls.is_empty()
                    && edit.addresses.is_empty()
                    && !matches!(edit.photo, crate::contacts::PhotoEdit::Set(_));
                if empty {
                    return;
                }
                let picked = editor.book.as_ref().and_then(|row| self.books.get(row.selected() as usize));
                match self.editing_target.and_then(|i| self.contacts.get(i)) {
                    // Saved in another book: moved there, with the edits,
                    // once the move is confirmed. The form stays till then.
                    Some(c) if picked.is_some_and(|b| b.uid != c.book_uid) => {
                        let to = picked.map(|b| b.uid.clone()).unwrap_or_default();
                        if !self.can_move(c, &to) {
                            return;
                        }
                        let mut moved = c.clone();
                        moved.raw_vcard = crate::contacts::patched_vcard(&c.raw_vcard, &edit);
                        moved.photo = match &edit.photo {
                            crate::contacts::PhotoEdit::Keep => c.photo_bytes(),
                            crate::contacts::PhotoEdit::Remove => None,
                            crate::contacts::PhotoEdit::Set(bytes) => Some(bytes.clone()),
                        };
                        moved.photo_key = moved.photo.as_deref().map_or(0, crate::contacts::photo_key);
                        let vcard = crate::contacts::moved_vcard(&moved);
                        let key = (c.book_uid.clone(), c.eds_uid.clone());
                        self.ask_move(widgets, &sender, vec![key], to, Some(vcard));
                        return;
                    }
                    Some(c) => {
                        let vcard = crate::contacts::patched_vcard(&c.raw_vcard, &edit);
                        let _ = sender.output(ContactsPageOutput::SaveContact {
                            book_uid: c.book_uid.clone(),
                            vcard,
                        });
                    }
                    None => {
                        let vcard = crate::contacts::new_vcard(&edit);
                        let book_uid = editor
                            .book
                            .as_ref()
                            .and_then(|row| self.books.get(row.selected() as usize))
                            .map(|b| b.uid.clone());
                        let _ = sender.output(ContactsPageOutput::CreateContact { book_uid, vcard });
                    }
                }
                // The app re-reads EDS and pushes SetContacts; until then the
                // card shows the pre-edit state rather than a half-saved one.
                self.editor = None;
                self.editing_target = None;
                self.render_detail(widgets, &sender);
            }

            ContactsPageInput::OpenInGnome(idx) => {
                if let Some(c) = self.contacts.get(idx) {
                    crate::ui::contacts_browser::launch_gnome_contacts_for(&c.eds_uid);
                }
            }

            ContactsPageInput::DeleteSelected => {
                // Not while an editor is open: that contact is being changed.
                if self.editor.is_some() {
                    return;
                }
                if !self.multi.is_empty() {
                    sender.input(ContactsPageInput::BulkDeleteRequest);
                } else if let Some(idx) = self.selected {
                    sender.input(ContactsPageInput::DeleteRequest(idx));
                }
            }

            ContactsPageInput::DeleteRequest(idx) => {
                let Some(c) = self.contacts.get(idx) else { return };
                let synced = self.synced(&c.book_uid);
                let name = [("name", c.name.as_str())];
                let (heading, body) = if c.is_group() {
                    let body = if synced {
                        i18n_f(
                            "This will remove the group {name} from your address book locally and on the server. The contacts are not removed.",
                            &name,
                        )
                    } else {
                        i18n_f("This will remove the group {name} from your address book. The contacts are not removed.", &name)
                    };
                    (i18n("Delete Group?"), body)
                } else {
                    let body = if synced {
                        i18n_f(
                            "This will remove the contact {name} from your address book locally and on the server.",
                            &name,
                        )
                    } else {
                        i18n_f("This will remove the contact {name} from your address book.", &name)
                    };
                    (i18n("Delete Contact?"), body)
                };
                let s = sender.input_sender().clone();
                let keys = vec![(c.book_uid.clone(), c.eds_uid.clone())];
                confirm(widgets, &heading, &body, &i18n("Delete"), true, move || {
                    let _ = s.send(ContactsPageInput::DeleteConfirmed(keys.clone()));
                });
            }

            ContactsPageInput::SetBooks(books) => {
                self.books = books;
                self.refresh_book_filter();
                self.publish_tree(&sender);
            }

            ContactsPageInput::PhotosDecoded => {
                for row in self.rows.borrow().values() {
                    row.show_photo();
                }
                self.decoding.retain(|key| crate::avatar::photo_thumbnail(*key).is_none());
            }

            ContactsPageInput::RowMenu { idx, x, y } => {
                let entries = self.row_menu(idx, &sender);
                if !entries.is_empty() {
                    show_context_menu(&widgets.list, x, y, entries);
                }
            }


            ContactsPageInput::OpenPhoto { name, data } => {
                let _ = sender.output(ContactsPageOutput::ShowPhoto { name, data });
            }

            ContactsPageInput::DeleteConfirmed(keys) => {
                let gone = self.forget(&keys);
                if !gone.is_empty() {
                    let _ = sender.output(ContactsPageOutput::DeleteContacts(gone));
                }
                // The entries leave the list right now; the EDS delete and
                // the reconciling re-read follow behind.
                self.editor = None;
                self.editing_target = None;
                self.rebuild(widgets, &sender);
            }
        }
        // Overriding `update_with_view` replaces the generated body that ran
        // `update_view` after every message; without this the `#[watch]`
        // bindings above (the "Loading Contacts…" / "No Contacts" face) only
        // ever hold their `init` values, and an empty address book looked
        // like a load that never finished.
        self.update_view(widgets, sender);
    }
}

impl ContactsPage {
    /// The entries held, their bytes (the floor: strings, vCards and photos)
    /// and how much of that is photos, for the memory report.
    pub fn memory_stats(&self) -> (usize, usize, usize) {
        let entries = self.contacts.iter().chain(self.pending.iter().flatten()).chain(&self.directory_hits);
        let (mut n, mut bytes, mut photos) = (0, 0, 0);
        for c in entries {
            let photo = c.photo.as_ref().map_or(0, Vec::len);
            let labeled = |l: &[crate::contacts::Labeled]| l.iter().map(|x| 48 + x.label.len() + x.value.len()).sum::<usize>();
            n += 1;
            photos += photo;
            bytes += std::mem::size_of::<ContactDetails>()
                + photo
                + c.raw_vcard.len()
                + c.name.len()
                + c.note.len()
                + labeled(&c.emails)
                + labeled(&c.phones)
                + labeled(&c.addresses);
        }
        (n, bytes, photos)
    }

    /// Whether a contact matches the active search.
    fn matches(&self, c: &ContactDetails) -> bool {
        self.in_scope(c) && matches_query(c, &self.query)
    }

    /// Whether the list's scope takes `c`, the search aside.
    fn in_scope(&self, c: &ContactDetails) -> bool {
        match &self.scope {
            ContactScope::All => !c.is_group(),
            // A book lists its groups as well as its people, the groups
            // first; so does an account of several books.
            ContactScope::Book(book) => c.book_uid == *book,
            ContactScope::Account(key) => self.book_key(&c.book_uid) == *key,
            // The group itself, first, then its people.
            ContactScope::Group { book, uid } => match self.scope_group() {
                Some(g) if c.is_group() => c.book_uid == *book && c.eds_uid == *uid && std::ptr::eq(g, c),
                Some(g) => g.members.iter().any(|m| is_member(g, m, c)),
                None => false,
            },
        }
    }

    /// The key the books of `uid`'s account share ([`TreeBook::key`]).
    fn book_key(&self, uid: &str) -> String {
        let directory = self.directories.iter().any(|(d, _)| d == uid);
        self.filter_books
            .iter()
            .find(|(u, _)| u == uid)
            .map(|(_, name)| split_book_name(name, directory).0)
            .unwrap_or_default()
    }

    /// The group the list shows, in group scope.
    fn scope_group(&self) -> Option<&ContactDetails> {
        let ContactScope::Group { book, uid } = &self.scope else { return None };
        self.contacts.iter().find(|c| c.is_group() && c.book_uid == *book && c.eds_uid == *uid)
    }

    /// The book a new contact or group starts in: the one the list shows,
    /// when it can take it, else the default (first).
    fn scope_book(&self, books: &[crate::contacts::Book]) -> usize {
        let shown = match &self.scope {
            ContactScope::All | ContactScope::Account(_) => None,
            ContactScope::Book(book) | ContactScope::Group { book, .. } => Some(book),
        };
        shown.and_then(|b| books.iter().position(|x| x.uid == *b)).unwrap_or(0)
    }

    /// Put these people (indexes into `contacts`) in the group at `g`,
    /// those it can take and does not have.
    fn add_to_group(&self, sender: &ComponentSender<Self>, g: usize, people: &[usize]) {
        let Some(group) = self.contacts.get(g) else { return };
        let Some(format) = group.group else { return };
        let mut members = group.members.clone();
        for &p in people {
            if let Some(m) = self.contacts.get(p).and_then(|p| member_for(group, p, true)) {
                if !members.iter().any(|x| x.same(&m)) {
                    members.push(m);
                }
            }
        }
        if members.len() == group.members.len() {
            return;
        }
        let edit = GroupEdit {
            name: group.name.clone(),
            members,
            hide_addresses: group.hide_addresses,
            kept: group.hidden_members.clone(),
        };
        let _ = sender.output(ContactsPageOutput::SaveGroup {
            book_uid: group.book_uid.clone(),
            format,
            original: Some(group.raw_vcard.clone()),
            edit,
        });
    }

    /// Ask for a name for the book `uid`, Hylki's own: GNOME and Evolution
    /// keep the book's. One given before can be dropped again.
    fn ask_book_name(&self, widgets: &ContactsPageWidgets, sender: &ComponentSender<Self>, uid: String) {
        let Some((_, name)) = self.filter_books.iter().find(|(u, _)| *u == uid) else { return };
        let own = name.split_once(" · ").map_or(name.as_str(), |(_, own)| own).to_string();
        let given = crate::contact_sources::names().contains_key(&uid);
        let entry = gtk::Entry::new();
        entry.set_text(&own);
        entry.set_activates_default(true);
        let dialog = adw::AlertDialog::new(
            Some(&i18n("Rename Address Book")),
            Some(&i18n("The name is used in Hylki only; GNOME Contacts and Evolution keep the book's own.")),
        );
        dialog.set_extra_child(Some(&entry));
        dialog.add_response("cancel", &i18n("Cancel"));
        if given {
            dialog.add_response("reset", &i18n("Use Its Own Name"));
        }
        dialog.add_response("rename", &i18n("Rename"));
        dialog.set_response_appearance("rename", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("rename"));
        dialog.set_close_response("cancel");
        let s = sender.output_sender().clone();
        let field = entry.clone();
        dialog.connect_response(None, move |_, response| {
            let name = match response {
                "rename" => Some(field.text().trim().to_string()).filter(|n| !n.is_empty()),
                "reset" => None,
                _ => return,
            };
            let _ = s.send(ContactsPageOutput::RenameBook { uid: uid.clone(), name });
        });
        dialog.present(widgets.page_stack.root().as_ref());
        entry.grab_focus();
    }

    /// The books and groups for the sidebar, handed over when they change.
    fn publish_tree(&mut self, sender: &ComponentSender<Self>) {
        let directories: std::collections::HashSet<&String> = self.directories.iter().map(|(uid, _)| uid).collect();
        let mut counts: HashMap<&str, usize> = HashMap::new();
        for c in self.contacts.iter().filter(|c| !c.is_group()) {
            *counts.entry(c.book_uid.as_str()).or_default() += 1;
        }
        let books = self
            .filter_books
            .iter()
            .map(|(uid, name)| {
                let directory = directories.contains(uid);
                let (key, account, own) = split_book_name(name, directory);
                let book = self.books.iter().find(|b| b.uid == *uid);
                let mut groups: Vec<TreeGroup> = self
                    .contacts
                    .iter()
                    .filter(|c| c.is_group() && c.book_uid == *uid)
                    .map(|g| TreeGroup {
                        book: uid.clone(),
                        uid: g.eds_uid.clone(),
                        name: g.name.clone(),
                        count: g.members.len(),
                        editable: g.group_editable() && book.is_some(),
                    })
                    .collect();
                groups.sort_by_key(|g| g.name.to_lowercase());
                TreeBook {
                    uid: uid.clone(),
                    account,
                    key,
                    name: own,
                    count: counts.get(uid.as_str()).copied().unwrap_or(0),
                    writable: book.is_some(),
                    takes_groups: book.is_some_and(|b| b.groups.is_some()),
                    directory,
                    groups,
                }
            })
            .collect();
        let tree = ContactTree {
            total: self.contacts.iter().filter(|c| !c.is_group()).count(),
            books,
            scope: self.scope.clone(),
        };
        if self.tree.as_ref() != Some(&tree) {
            self.tree = Some(tree.clone());
            let _ = sender.output(ContactsPageOutput::Tree(tree));
        }
    }

    /// Whether `c` passes the "Show only" filters on; groups always do.
    fn passes_filters(&self, c: &ContactDetails, grouped: &std::collections::HashSet<String>) -> bool {
        c.is_group()
            || ContactFilter::ALL.iter().all(|f| self.filters & f.bit() == 0 || f.passes(c, grouped))
    }

    /// The addresses of every group's people, lower case, for "Not in a
    /// Group"; empty unless that filter is on.
    fn grouped_addresses(&self) -> std::collections::HashSet<String> {
        if self.filters & ContactFilter::NoGroup.bit() == 0 {
            return std::collections::HashSet::new();
        }
        self.contacts
            .iter()
            .filter(|c| c.is_group())
            .flat_map(|c| c.members.iter().flat_map(|m| [Some(m.email.to_lowercase()), m.card_key(&c.book_uid)]))
            .flatten()
            .collect()
    }

    fn sync_filter_menu(&self) {
        if let Some(menu) = &self.filter_menu {
            menu.sync(self.sort, self.filters);
        }
    }

    /// The list positions selected, as entries (contacts and directory
    /// hits), in list order.
    fn picked_entries(&self) -> Vec<usize> {
        let bits = self.selection.selection();
        (0..bits.size())
            .filter_map(|n| self.row_map.get(bits.nth(n as u32) as usize).copied().flatten())
            .collect()
    }

    /// A group's people as the group in book `to`, of `format`, keeps
    /// them; `from` is the book it is in now, if any. A member it already
    /// names there stays as written. Anyone else is found in Contacts: by
    /// the contact they were picked as, their card in `from`, or their
    /// address, a contact in `to` first. An Apple group names that contact
    /// in `to`, copied there when it is in another book (someone known only
    /// by an address gets a card at the save); an Evolution list holds the
    /// address, so someone with none is left out.
    fn members_for(&self, members: Vec<Member>, from: Option<&str>, to: &str, format: GroupFormat) -> Vec<Member> {
        let same = from == Some(to);
        let people = || self.contacts.iter().filter(|c| !c.is_group());
        members
            .into_iter()
            .filter_map(|m| {
                if same && !m.line.is_empty() {
                    return Some(m);
                }
                if format == GroupFormat::Evolution {
                    return (!m.email.is_empty())
                        .then(|| Member { name: m.name, email: m.email, ..Member::default() });
                }
                let person = m
                    .source
                    .as_ref()
                    .and_then(|(book, uid)| people().find(|c| c.book_uid == *book && c.eds_uid == *uid))
                    .or_else(|| {
                        let from = from?;
                        people().find(|c| !m.uid.is_empty() && c.book_uid == from && c.eds_uid == m.uid)
                    })
                    .or_else(|| {
                        people()
                            .filter(|c| !m.email.is_empty() && c.emails.iter().any(|e| e.value.eq_ignore_ascii_case(&m.email)))
                            .min_by_key(|c| c.book_uid != to)
                    });
                let base = Member { name: m.name, email: m.email, ..Member::default() };
                match person {
                    Some(p) if p.book_uid == to && !p.eds_uid.is_empty() => Some(Member { uid: p.eds_uid.clone(), ..base }),
                    Some(p) => Some(Member { card: crate::contacts::moved_vcard(p), ..base }),
                    None => (!base.email.is_empty() || !m.card.is_empty()).then(|| Member { card: m.card, ..base }),
                }
            })
            .collect()
    }

    /// The contacts selected together, as indexes into `contacts`.
    fn chosen(&self) -> Vec<usize> {
        self.multi.iter().copied().filter(|&i| i < self.contacts.len()).collect()
    }

    /// The people of the selection that can move to the book `to`: those
    /// in another book Hylki can write to, and delete from.
    fn movable(&self, to: &str) -> Vec<usize> {
        self.chosen().into_iter().filter(|&i| self.can_move(&self.contacts[i], to)).collect()
    }

    /// Whether a person can move to the book `to`: from another book Hylki
    /// can write to, and delete from.
    fn can_move(&self, c: &ContactDetails, to: &str) -> bool {
        let writable = |uid: &str| self.books.iter().any(|b| b.uid == uid);
        !c.is_group() && c.book_uid != to && !c.eds_uid.is_empty() && writable(to) && writable(&c.book_uid)
    }

    /// Ask before moving these contacts, by book and UID, to the book `to`:
    /// out of the books they are in, into that one. `edited` is the card
    /// of a contact being edited, saved with the move.
    fn ask_move(
        &self,
        widgets: &ContactsPageWidgets,
        sender: &ComponentSender<Self>,
        keys: Vec<(String, String)>,
        to: String,
        edited: Option<String>,
    ) {
        let Some(book) = self.books.iter().find(|b| b.uid == to) else { return };
        let one = match keys.as_slice() {
            [] => return,
            [(b, u)] => self.contacts.iter().find(|c| c.book_uid == *b && c.eds_uid == *u),
            _ => None,
        };
        let (heading, body) = match one {
            Some(c) => (
                i18n("Move Contact?"),
                i18n_f(
                    "This will move {name} out of {from} and into {to}, locally and on the server. A group in {from} that holds the contact will no longer have it.",
                    &[("name", &c.name), ("from", &c.book_name), ("to", &book.name)],
                ),
            ),
            None => {
                let n = keys.len() as u32;
                let count = n.to_string();
                (
                    ni18n_f("Move {n} Contact?", "Move {n} Contacts?", n, &[("n", &count)]),
                    ni18n_f(
                        "This will move {n} contact out of the address book it is in and into {to}, locally and on the server. A group there that holds it will no longer have it.",
                        "This will move {n} contacts out of the address books they are in and into {to}, locally and on the server. Groups there that hold them will no longer have them.",
                        n,
                        &[("n", &count), ("to", &book.name)],
                    ),
                )
            }
        };
        let s = sender.input_sender().clone();
        confirm(widgets, &heading, &body, &i18n("Move"), false, move || {
            let _ = s.send(match (&edited, keys.first()) {
                (Some(vcard), Some(key)) => {
                    ContactsPageInput::EditMoveConfirmed { to: to.clone(), key: key.clone(), vcard: vcard.clone() }
                }
                _ => ContactsPageInput::BulkMoveConfirmed { to: to.clone(), keys: keys.clone() },
            });
        });
    }

    /// Whether a book syncs with a server. Books on this computer keep
    /// groups as Evolution lists; the rest are a server's.
    fn synced(&self, book_uid: &str) -> bool {
        !crate::contacts::is_local_book(book_uid)
            && self
                .books
                .iter()
                .find(|b| b.uid == book_uid)
                .is_none_or(|b| b.groups != Some(GroupFormat::Evolution))
    }

    /// Take these entries (by book and UID) off the list, for good: a
    /// re-read that still has them leaves them out until EDS lets go.
    /// Returns those that are EDS's to delete.
    fn forget(&mut self, keys: &[(String, String)]) -> Vec<(String, String)> {
        let mut gone = Vec::new();
        for (book, uid) in keys {
            let Some(idx) = self.contacts.iter().position(|c| c.book_uid == *book && c.eds_uid == *uid) else {
                continue;
            };
            self.contacts.remove(idx);
            self.items.remove(idx);
            if !uid.is_empty() {
                self.deleted.insert((book.clone(), uid.clone()));
                gone.push((book.clone(), uid.clone()));
            }
        }
        for (i, item) in self.items.iter().enumerate() {
            item.borrow::<Line>().idx.set(Some(i));
        }
        self.selected = None;
        self.multi.clear();
        gone
    }

    /// The entry at `idx`, a directory entry's index included.
    fn entry(&self, idx: usize) -> Option<&ContactDetails> {
        match idx.checked_sub(DIRECTORY) {
            Some(i) => self.directory_hits.get(i),
            None => self.contacts.get(idx),
        }
    }

    /// Someone already in Contacts is not listed again. One found in two
    /// directories stays in both, for when one of them is picked; the list
    /// of every book shows the first only.
    fn drop_known_hits(&mut self) {
        let known: std::collections::HashSet<String> =
            self.contacts.iter().flat_map(|c| c.emails.iter().map(|e| e.value.to_lowercase())).collect();
        self.directory_hits.retain(|c| !c.emails.iter().any(|e| known.contains(&e.value.to_lowercase())));
    }

    /// Fill the filter popover's books from those known: the writable ones
    /// (the Hylki book always among them), any others the contacts come
    /// from, and the directories. A book unticked stays so while it is
    /// there.
    fn refresh_book_filter(&mut self) {
        let mut books: Vec<(String, String)> =
            self.books.iter().map(|b| (b.uid.clone(), b.name.clone())).collect();
        for c in &self.contacts {
            if !c.book_uid.is_empty() && !books.iter().any(|(uid, _)| *uid == c.book_uid) {
                books.push((c.book_uid.clone(), c.book_name.clone()));
            }
        }
        // A directory lists the people a search finds in it (#307).
        for (uid, name) in &self.directories {
            if !books.iter().any(|(u, _)| u == uid) {
                books.push((uid.clone(), format!("LDAP — {name}")));
            }
        }
        if books == self.filter_books {
            return;
        }
        self.filter_books = books;
        // A book that went is no longer shown alone.
        let gone = match &self.scope {
            ContactScope::All => false,
            ContactScope::Book(b) | ContactScope::Group { book: b, .. } => !self.filter_books.iter().any(|(u, _)| u == b),
            ContactScope::Account(key) => !self.filter_books.iter().any(|(u, _)| self.book_key(u) == *key),
        };
        if gone {
            self.scope = ContactScope::All;
        }
    }

    /// Ready the pane for a new contact or group: nothing selected, so the
    /// form is what it shows.
    fn new_form(&mut self, widgets: &ContactsPageWidgets) {
        self.editing_target = None;
        widgets.page_stack.set_visible_child_name("browser");
        self.selected = None;
        self.multi.clear();
        self.selection.unselect_all();
    }

    /// Move the list selection to the row showing contact `idx` (if visible).
    fn select_row_for(&self, idx: usize) {
        if let Some(pos) = self.row_map.iter().position(|&i| i == Some(idx)) {
            self.selection.select_item(pos as u32, true);
        }
    }

    /// Rebuild the list from `contacts` + `query` + `sort`, keep (or default)
    /// the selection, and render its detail card.
    fn rebuild(&mut self, widgets: &ContactsPageWidgets, sender: &ComponentSender<Self>) {
        let started = std::time::Instant::now();
        self.publish_tree(sender);
        // What a row dragged from the list carries.
        self.drag.borrow_mut().keys = self
            .contacts
            .iter()
            .map(|c| (!c.is_group() && !c.eds_uid.is_empty()).then(|| (c.book_uid.clone(), c.eds_uid.clone())))
            .collect();
        let empty = self.contacts.is_empty() && self.directories.is_empty();
        widgets.page_stack.set_visible_child_name(if empty { "none" } else { "browser" });

        let grouped = self.grouped_addresses();
        let mut people: Vec<usize> = (0..self.contacts.len())
            .filter(|&i| self.matches(&self.contacts[i]) && self.passes_filters(&self.contacts[i], &grouped))
            .collect();
        people.sort_by_key(|&i| {
            let c = &self.contacts[i];
            let name = c.name.to_lowercase();
            // Groups head the people, by name: a group shown with its
            // people, or a book's groups.
            let key = match self.sort {
                _ if c.is_group() => name,
                ContactSort::FirstName => name,
                // Sort by the final word of the name (then the whole name for
                // ties) — how an address book orders by surname.
                ContactSort::LastName => {
                    format!("{} {name}", name.rsplit(' ').next().unwrap_or(&name))
                }
                ContactSort::Email => {
                    c.primary_email().map(str::to_lowercase).unwrap_or(name)
                }
            };
            (!c.is_group(), key)
        });
        // People in what the list shows, and of them those it lists.
        let total = self.contacts.iter().filter(|c| !c.is_group() && self.in_scope(c)).count();
        let shown = people.iter().filter(|&&i| !self.contacts[i].is_group()).count();
        let narrowed = !self.query.is_empty() || self.filters != 0;
        widgets.count_label.set_label(&if !narrowed {
            total.to_string()
        } else {
            i18n_f("{shown} of {total}", &[("shown", &shown.to_string()), ("total", &total.to_string())])
        });
        let mut lines: Vec<Line> = Vec::new();
        // Directories are searched with every book, or alone; a group has
        // none of their people.
        let groups = matches!(self.scope, ContactScope::Group { .. });

        // The directories' answers, after the contacts and by name whatever
        // the sort, under a heading of their own so nobody takes them for
        // contacts. A book picked keeps those of the directory it is.
        let in_filter = |uid: &str| match &self.scope {
            ContactScope::Book(b) => b == uid,
            ContactScope::Account(key) => self.book_key(uid) == *key,
            _ => true,
        };
        let mut listed = std::collections::HashSet::new();
        let mut hits = Vec::new();
        for (i, c) in self.directory_hits.iter().enumerate() {
            if groups || !in_filter(&c.book_uid) || !self.passes_filters(c, &grouped) {
                continue;
            }
            let emails: Vec<String> = c.emails.iter().map(|e| e.value.to_lowercase()).collect();
            if emails.iter().any(|e| listed.contains(e)) {
                continue;
            }
            listed.extend(emails);
            // Which directory it is in, after the address.
            let subtitle = match c.primary_email() {
                Some(email) => format!("{email} · {}", c.book_name),
                None => c.book_name.clone(),
            };
            hits.push(Line::entry(DIRECTORY + i, c, subtitle));
        }
        if !hits.is_empty() {
            lines.push(Line::other(LineKind::Heading, i18n("In Directories"), String::new()));
            lines.extend(hits);
        }

        // A directory that did not answer, last and not selectable, so a
        // search that finds nobody in it says why.
        for f in self.directory_failures.iter().filter(|f| !groups && in_filter(&f.uid)) {
            lines.push(Line::other(LineKind::Failure, f.name.clone(), f.message.clone()));
        }

        // A directory picked in the sidebar is empty until searched.
        let directory_picked = match &self.scope {
            ContactScope::Book(b) => self.directories.iter().any(|(d, _)| d == b),
            _ => false,
        };
        if directory_picked && !groups && people.is_empty() && lines.is_empty() {
            let text = if self.query.trim().chars().count() < crate::directory::MIN_QUERY {
                i18n("Type three letters or more to search the directory.")
            } else {
                i18n("Nobody in the directory matches.")
            };
            lines.push(Line::other(LineKind::Note, text, String::new()));
        }

        let items: Vec<gtk::glib::BoxedAnyObject> = people
            .iter()
            .filter_map(|&idx| self.items.get(idx).cloned())
            .chain(lines.into_iter().map(gtk::glib::BoxedAnyObject::new))
            .collect();
        self.row_map = items.iter().map(|item| item.borrow::<Line>().idx.get()).collect();
        let missing: std::collections::HashSet<u64> = items
            .iter()
            .filter_map(|item| match item.borrow::<Line>().kind {
                LineKind::Entry { photo_key, .. } if photo_key != 0 => Some(photo_key),
                _ => None,
            })
            .filter(|key| !self.decoding.contains(key) && crate::avatar::photo_thumbnail(*key).is_none())
            .collect();
        // Unselected first: the selection would otherwise follow the old
        // lines through the swap and report a position in the new ones.
        self.selection.unselect_all();
        self.store.splice(0, self.store.n_items(), &items);

        // Several selected together stay so while two of them are listed.
        let multi_pos: Vec<u32> = self
            .multi
            .iter()
            .filter_map(|&idx| self.row_map.iter().position(|&i| i == Some(idx)))
            .map(|pos| pos as u32)
            .collect();
        if multi_pos.len() > 1 {
            for &pos in &multi_pos {
                self.selection.select_item(pos, false);
            }
            self.multi.retain(|idx| self.row_map.contains(&Some(*idx)));
            self.selected = None;
            self.render_detail(widgets, sender);
            self.decode_photos(missing, sender);
            return;
        }
        self.multi.clear();

        // Selection: keep it if it survived the filter, else the first match.
        let shown_pos = self
            .selected
            .and_then(|sel| self.row_map.iter().position(|&i| i == Some(sel)))
            .or_else(|| self.row_map.iter().position(Option::is_some));
        match shown_pos {
            Some(pos) => {
                self.selected = self.row_map[pos];
                self.selection.select_item(pos as u32, true);
            }
            None => self.selected = None,
        }
        self.render_detail(widgets, sender);
        self.decode_photos(missing, sender);
        tracing::debug!("contacts: listed {} in {:?}", self.row_map.len(), started.elapsed());
    }

    /// The list items for a fresh read: an unchanged line keeps the item it
    /// had, and with it its row.
    fn items_for(&self, contacts: &[ContactDetails]) -> Vec<gtk::glib::BoxedAnyObject> {
        let mut had: HashMap<(&str, &str), &gtk::glib::BoxedAnyObject> = self
            .contacts
            .iter()
            .zip(&self.items)
            .map(|(c, item)| ((c.book_uid.as_str(), c.eds_uid.as_str()), item))
            .collect();
        contacts
            .iter()
            .enumerate()
            .map(|(idx, c)| {
                let subtitle = if c.is_group() {
                    group_byline(c)
                } else {
                    // An address, else a phone number, else the company:
                    // whatever tells the card apart, as an address book does.
                    c.primary_email()
                        .or_else(|| c.phones.first().map(|p| p.value.as_str()))
                        .or_else(|| (!c.org.is_empty()).then_some(c.org.as_str()))
                        .filter(|v| *v != c.name)
                        .unwrap_or_default()
                        .to_string()
                };
                let line = Line::entry(idx, c, subtitle);
                match had.remove(&(c.book_uid.as_str(), c.eds_uid.as_str())) {
                    Some(item) if *item.borrow::<Line>() == line => {
                        item.borrow::<Line>().idx.set(Some(idx));
                        item.clone()
                    }
                    _ => gtk::glib::BoxedAnyObject::new(line),
                }
            })
            .collect()
    }

    /// Decode these photos on other threads; the rows waiting for them show
    /// each as it lands.
    fn decode_photos(&mut self, mut keys: std::collections::HashSet<u64>, sender: &ComponentSender<Self>) {
        // Each photo once, however many people share it. The address books'
        // photos go to the thread, or are let go when not wanted now: the
        // list draws thumbnails, and the rest read a photo again from its
        // card (`photo_bytes`) when they need it (#504).
        let mut photos: Vec<(u64, Vec<u8>)> = Vec::new();
        for c in &mut self.contacts {
            let key = line_photo_key(c);
            c.photo_key = key;
            if let Some(bytes) = c.photo.take() {
                if keys.remove(&key) {
                    photos.push((key, bytes));
                }
            }
        }
        for c in &self.directory_hits {
            let key = line_photo_key(c);
            if keys.remove(&key) {
                photos.extend(c.photo.clone().map(|bytes| (key, bytes)));
            }
        }
        // A photo let go earlier and wanted now (a search or filter brought
        // its contact in) is read from its card on the thread.
        let cards: Vec<ContactDetails> = self
            .contacts
            .iter()
            .filter(|c| keys.remove(&c.photo_key))
            .map(|c| ContactDetails {
                raw_vcard: c.raw_vcard.clone(),
                book_uid: c.book_uid.clone(),
                photo_key: c.photo_key,
                ..Default::default()
            })
            .collect();
        if photos.is_empty() && cards.is_empty() {
            return;
        }
        self.decoding.extend(photos.iter().map(|(key, _)| *key));
        self.decoding.extend(cards.iter().map(|c| c.photo_key));
        let s = sender.input_sender().clone();
        std::thread::spawn(move || {
            photos.extend(cards.iter().filter_map(|c| Some((c.photo_key, c.photo_bytes()?))));
            crate::avatar::decode_photo_thumbnails(photos, || {
                let _ = s.send(ContactsPageInput::PhotosDecoded);
            });
        });
    }

    /// The right-click menu for an entry of the list.
    fn row_menu(&self, idx: usize, sender: &ComponentSender<Self>) -> Vec<Vec<MenuEntry>> {
        let s = sender.input_sender();
        if idx >= DIRECTORY {
            let Some(c) = self.entry(idx) else { return Vec::new() };
            let mut entries = Vec::new();
            if let Some(email) = c.primary_email().map(str::to_string) {
                let sc = s.clone();
                entries.push(
                    MenuEntry::new(i18n("New message"), move || {
                        let _ = sc.send(ContactsPageInput::Compose(email.clone()));
                    })
                    .icon("mail-message-new-symbolic"),
                );
            }
            let sa = s.clone();
            entries.push(
                MenuEntry::new(i18n("Add to Contacts"), move || {
                    let _ = sa.send(ContactsPageInput::AddDirectoryHit(idx));
                })
                .icon("contact-new-symbolic"),
            );
            return vec![entries];
        }
        let Some(c) = self.contacts.get(idx) else { return Vec::new() };
        // On one of several selected: what can be done to them all.
        if self.multi.contains(&idx) {
            let choices = self.bulk_choices();
            let mut first = Vec::new();
            if let Some(g) = self.scope_group().filter(|g| g.group_editable()) {
                let chosen = self.chosen();
                if chosen.iter().any(|&i| g.members.iter().any(|m| is_member(g, m, &self.contacts[i]))) {
                    let sr = s.clone();
                    first.push(
                        MenuEntry::new(i18n("Remove from Group"), move || {
                            let _ = sr.send(ContactsPageInput::RemoveFromGroup(chosen.clone()));
                        })
                        .icon("list-remove-symbolic"),
                    );
                }
            }
            if !choices.groups.is_empty() {
                first.push(MenuEntry::submenu(i18n("Add to Group"), vec![choices.group_entries(s)]).icon("system-users-symbolic"));
            }
            if !choices.books.is_empty() {
                first.push(
                    MenuEntry::submenu(i18n("Move to Address Book"), vec![choices.book_entries(s)])
                        .icon("x-office-address-book-symbolic"),
                );
            }
            let sd = s.clone();
            let delete = vec![MenuEntry::new(i18n("Delete…"), move || {
                let _ = sd.send(ContactsPageInput::BulkDeleteRequest);
            })
            .icon("user-trash-symbolic")];
            return if first.is_empty() { vec![delete] } else { vec![first, delete] };
        }
        let local = crate::contacts::is_local_book(&c.book_uid);
        let group = c.is_group();
        let (se, sd, sg) = (s.clone(), s.clone(), s.clone());
        let mut first = vec![MenuEntry::new(i18n("Edit"), move || {
            let _ = se.send(ContactsPageInput::EditIndex(idx));
        })
        .icon("document-edit-symbolic")
        .enabled(!group || c.group_editable())];
        if group {
            let sc = s.clone();
            first.push(
                MenuEntry::new(i18n("New message"), move || {
                    let _ = sc.send(ContactsPageInput::ComposeGroup(idx));
                })
                .icon("mail-message-new-symbolic")
                .enabled(c.reachable().next().is_some()),
            );
        }
        // A person can go in a group they are not in, by name.
        if !group {
            let mut groups: Vec<(usize, &str)> = (0..self.contacts.len())
                .filter(|&g| member_for(&self.contacts[g], c, false).is_some())
                .map(|g| (g, self.contacts[g].name.as_str()))
                .collect();
            groups.sort_by_key(|(_, name)| name.to_lowercase());
            if !groups.is_empty() {
                let entries = groups
                    .into_iter()
                    .map(|(g, name)| {
                        let sa = s.clone();
                        MenuEntry::new(name.to_string(), move || {
                            let _ = sa.send(ContactsPageInput::AddToGroup { group: g, person: idx });
                        })
                        .icon("system-users-symbolic")
                    })
                    .collect();
                first.push(MenuEntry::submenu(i18n("Add to Group"), vec![entries]).icon("system-users-symbolic"));
            }
        }
        // Out of the group the list shows.
        if let Some(g) = self.scope_group().filter(|g| !group && g.group_editable()) {
            if g.members.iter().any(|m| is_member(g, m, c)) {
                let sr = s.clone();
                first.push(
                    MenuEntry::new(i18n("Remove from Group"), move || {
                        let _ = sr.send(ContactsPageInput::RemoveFromGroup(vec![idx]));
                    })
                    .icon("list-remove-symbolic"),
                );
            }
        }
        // Another book, with a question first.
        let books: Vec<(String, String)> = self
            .books
            .iter()
            .filter(|b| self.can_move(c, &b.uid))
            .map(|b| (b.uid.clone(), b.name.clone()))
            .collect();
        if !books.is_empty() {
            let entries = books
                .into_iter()
                .map(|(uid, name)| {
                    let sm = s.clone();
                    MenuEntry::new(name, move || {
                        let _ = sm.send(ContactsPageInput::MoveRequest { idx, to: uid.clone() });
                    })
                    .icon("x-office-address-book-symbolic")
                })
                .collect();
            first.push(MenuEntry::submenu(i18n("Move to Address Book"), vec![entries]).icon("x-office-address-book-symbolic"));
        }
        // GNOME Contacts knows nothing of the Hylki book, nor of groups.
        if !local && !group {
            first.push(
                MenuEntry::new(i18n("Open in GNOME Contacts"), move || {
                    let _ = sg.send(ContactsPageInput::OpenInGnome(idx));
                })
                .icon("adw-external-link-symbolic"),
            );
        }
        vec![
            first,
            vec![MenuEntry::new(i18n("Delete…"), move || {
                let _ = sd.send(ContactsPageInput::DeleteRequest(idx));
            })
            .icon("user-trash-symbolic")],
        ]
    }

    /// What the entries selected together can be given: the groups any of
    /// them can join, and the books any of them can move to.
    fn bulk_choices(&self) -> BulkChoices {
        let chosen = self.chosen();
        let mut groups: Vec<(usize, String)> = (0..self.contacts.len())
            .filter(|&g| chosen.iter().any(|&p| member_for(&self.contacts[g], &self.contacts[p], false).is_some()))
            .map(|g| (g, self.contacts[g].name.clone()))
            .collect();
        groups.sort_by_key(|(_, name)| name.to_lowercase());
        let books = self
            .books
            .iter()
            .filter(|b| !self.movable(&b.uid).is_empty())
            .map(|b| (b.uid.clone(), b.name.clone()))
            .collect();
        BulkChoices { groups, books }
    }

    /// The pane for several entries selected together: how many, what
    /// can be done to them all, and who they are.
    fn render_multi(&self, widgets: &ContactsPageWidgets, sender: &ComponentSender<Self>) {
        let detail = &widgets.detail_box;
        widgets.edit_btn.set_sensitive(false);
        let chosen = self.chosen();
        let n = chosen.len() as u32;
        let count = n.to_string();
        let groups = !chosen.is_empty() && chosen.iter().all(|&i| self.contacts[i].is_group());

        let icon = gtk::Image::from_icon_name(if groups { "system-users-symbolic" } else { "edit-select-all-symbolic" });
        icon.set_pixel_size(64);
        icon.add_css_class("dim-label");
        icon.set_margin_top(24);
        detail.append(&icon);
        let title = gtk::Label::new(Some(&if groups {
            ni18n_f("{n} Group Selected", "{n} Groups Selected", n, &[("n", &count)])
        } else {
            ni18n_f("{n} Contact Selected", "{n} Contacts Selected", n, &[("n", &count)])
        }));
        title.add_css_class("title-1");
        title.set_wrap(true);
        title.set_justify(gtk::Justification::Center);
        title.set_margin_top(12);
        detail.append(&title);

        let actions = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        actions.set_halign(gtk::Align::Center);
        actions.set_margin_top(18);
        let choices = Rc::new(self.bulk_choices());
        let pill = |label: &str| {
            let b = gtk::Button::with_label(label);
            b.add_css_class("pill");
            b
        };
        if let Some(g) = self.scope_group().filter(|g| !groups && g.group_editable()) {
            let remove = pill(&i18n("Remove from Group"));
            remove.set_sensitive(chosen.iter().any(|&i| g.members.iter().any(|m| is_member(g, m, &self.contacts[i]))));
            let (s, people) = (sender.input_sender().clone(), chosen.clone());
            remove.connect_clicked(move |_| {
                let _ = s.send(ContactsPageInput::RemoveFromGroup(people.clone()));
            });
            actions.append(&remove);
        }
        if !groups {
            let add = pill(&i18n("Add to Group"));
            add.set_sensitive(!choices.groups.is_empty());
            let (c, s) = (choices.clone(), sender.input_sender().clone());
            add.connect_clicked(move |b| {
                show_context_menu(b, (b.width() / 2) as f64, b.height() as f64, vec![c.group_entries(&s)]);
            });
            actions.append(&add);
            let move_btn = pill(&i18n("Move to…"));
            move_btn.set_tooltip_text(Some(&i18n("Move to another address book")));
            move_btn.set_sensitive(!choices.books.is_empty());
            let (c, s) = (choices.clone(), sender.input_sender().clone());
            move_btn.connect_clicked(move |b| {
                show_context_menu(b, (b.width() / 2) as f64, b.height() as f64, vec![c.book_entries(&s)]);
            });
            actions.append(&move_btn);
        }
        let delete = pill(&i18n("Delete…"));
        delete.add_css_class("destructive-action");
        delete.set_sensitive(!chosen.is_empty());
        let s = sender.input_sender().clone();
        delete.connect_clicked(move |_| {
            let _ = s.send(ContactsPageInput::BulkDeleteRequest);
        });
        actions.append(&delete);
        detail.append(&actions);

        let list = group(detail, &i18n("Selected"));
        for &i in &chosen {
            let c = &self.contacts[i];
            let row = adw::ActionRow::new();
            row.set_title(&gtk::glib::markup_escape_text(&c.name));
            row.set_subtitle(&gtk::glib::markup_escape_text(&c.book_name));
            row.add_prefix(&avatar_for(c, 32));
            list.append(&row);
        }
    }

    /// Fill the right-hand pane with the selected contact's card.
    fn render_detail(&mut self, widgets: &ContactsPageWidgets, sender: &ComponentSender<Self>) {
        self.drag.borrow_mut().multi = self.multi.clone();
        self.editor = None;
        let detail = &widgets.detail_box;
        while let Some(child) = detail.first_child() {
            detail.remove(&child);
        }
        if !self.multi.is_empty() {
            self.render_multi(widgets, sender);
            return;
        }
        let Some(c) = self.selected.and_then(|i| self.entry(i)) else {
            return;
        };
        let hit = self.selected.filter(|&i| i >= DIRECTORY);
        widgets.edit_btn.set_sensitive(hit.is_none() && (!c.is_group() || c.group_editable()));
        if let Some(idx) = self.selected.filter(|_| c.is_group()) {
            render_group_card(detail, sender, c, idx);
            return;
        }

        // ---- identity header: photo, name, who they are ----
        let photo = c.photo_bytes();
        let avatar = avatar_with(c, 96, photo.as_deref());
        if let Some(photo) = &photo {
            // A real photo expands to the lightbox on click.
            let btn = gtk::Button::new();
            btn.set_child(Some(&avatar));
            btn.add_css_class("flat");
            btn.add_css_class("circular");
            btn.set_halign(gtk::Align::Center);
            btn.set_tooltip_text(Some(i18n("View photo").as_str()));
            let s = sender.input_sender().clone();
            let name = c.name.clone();
            let data = photo.clone();
            btn.connect_clicked(move |_| {
                let _ = s.send(ContactsPageInput::OpenPhoto {
                    name: name.clone(),
                    data: data.clone(),
                });
            });
            detail.append(&btn);
        } else {
            avatar.set_halign(gtk::Align::Center);
            detail.append(&avatar);
        }

        let name = gtk::Label::new(Some(&c.name));
        name.add_css_class("title-1");
        name.set_wrap(true);
        name.set_justify(gtk::Justification::Center);
        name.set_margin_top(12);
        detail.append(&name);

        let mut byline = Vec::new();
        if !c.nickname.is_empty() && c.nickname != c.name {
            byline.push(format!("“{}”", c.nickname));
        }
        match (!c.title.is_empty(), !c.org.is_empty()) {
            (true, true) => byline.push(format!("{} · {}", c.title, c.org)),
            (true, false) => byline.push(c.title.clone()),
            (false, true) => byline.push(c.org.clone()),
            (false, false) => {}
        }
        if !byline.is_empty() {
            let sub = gtk::Label::new(Some(&byline.join("  —  ")));
            sub.add_css_class("dim-label");
            sub.set_wrap(true);
            sub.set_justify(gtk::Justification::Center);
            sub.set_margin_top(4);
            detail.append(&sub);
        }

        // From a directory: it can be kept as a contact, not edited here.
        if let Some(idx) = hit {
            let add = gtk::Button::with_label(&i18n("Add to Contacts"));
            add.add_css_class("pill");
            add.set_halign(gtk::Align::Center);
            add.set_margin_top(12);
            let s = sender.input_sender().clone();
            add.connect_clicked(move |b| {
                b.set_sensitive(false);
                let _ = s.send(ContactsPageInput::AddDirectoryHit(idx));
            });
            detail.append(&add);
        }

        // ---- field groups ----
        if !c.emails.is_empty() {
            let list = group(detail, &i18n("Email"));
            for e in &c.emails {
                let row = labeled_row(e);
                let compose = flat_button(
                    "mail-message-new-symbolic",
                    &i18n("New message"),
                );
                let s = sender.input_sender().clone();
                let addr = e.value.clone();
                compose.connect_clicked(move |_| {
                    let _ = s.send(ContactsPageInput::Compose(addr.clone()));
                });
                row.add_suffix(&copy_button(&e.value));
                row.add_suffix(&compose);
                row.set_activatable(true);
                let s = sender.input_sender().clone();
                let addr = e.value.clone();
                row.connect_activated(move |_| {
                    let _ = s.send(ContactsPageInput::Compose(addr.clone()));
                });
                list.append(&row);
            }
        }

        if !c.phones.is_empty() {
            let list = group(detail, &i18n("Phone"));
            for p in &c.phones {
                let row = labeled_row(p);
                row.add_suffix(&copy_button(&p.value));
                list.append(&row);
            }
        }

        if !c.addresses.is_empty() {
            let list = group(detail, &i18n("Address"));
            for a in &c.addresses {
                let row = labeled_row(a);
                row.add_suffix(&copy_button(&a.value));
                list.append(&row);
            }
        }

        if !c.urls.is_empty() {
            let list = group(detail, &i18n("Website"));
            for url in &c.urls {
                let row = adw::ActionRow::new();
                row.set_title(&gtk::glib::markup_escape_text(url));
                row.set_activatable(true);
                let icon =
                    gtk::Image::from_icon_name("adw-external-link-symbolic");
                icon.add_css_class("dim-label");
                row.add_suffix(&icon);
                let s = sender.input_sender().clone();
                let url = url.clone();
                row.connect_activated(move |_| {
                    let _ = s.send(ContactsPageInput::OpenUrl(url.clone()));
                });
                list.append(&row);
            }
        }

        let mut extras: Vec<Labeled> = Vec::new();
        if !c.birthday.is_empty() {
            extras.push(Labeled { label: i18n("Birthday"), value: c.birthday.clone() });
        }
        if !c.note.is_empty() {
            extras.push(Labeled { label: i18n("Note"), value: c.note.clone() });
        }
        // Where this entry lives: the local book, a synced account, or the
        // directory it was found in.
        if hit.is_some() {
            extras.push(Labeled { label: i18n("Directory"), value: c.book_name.clone() });
        } else if !c.book_name.is_empty() {
            extras.push(Labeled { label: i18n("Address Book"), value: c.book_name.clone() });
        }
        if !extras.is_empty() {
            let list = group(detail, &i18n("Details"));
            for x in &extras {
                let row = adw::ActionRow::new();
                row.set_title(&gtk::glib::markup_escape_text(&x.label));
                row.set_subtitle(&gtk::glib::markup_escape_text(&x.value));
                row.add_css_class("property");
                list.append(&row);
            }
        }
    }

    /// Open the form for the entry at `idx`: a group's, or a contact's. A
    /// group Hylki cannot write stays as it is.
    fn edit(&mut self, widgets: &ContactsPageWidgets, sender: &ComponentSender<Self>, idx: usize) {
        let Some(c) = self.contacts.get(idx) else { return };
        if c.is_group() && !c.group_editable() {
            return;
        }
        let group = c.is_group();
        self.editing_target = Some(idx);
        if group {
            self.render_group_editor(widgets, sender);
        } else {
            self.render_editor(widgets, sender);
        }
    }

    /// Replace the detail pane with the group form for `editing_target`
    /// (blank for a new group, in a book picked from those that keep
    /// groups).
    fn render_group_editor(&mut self, widgets: &ContactsPageWidgets, sender: &ComponentSender<Self>) {
        let detail = &widgets.detail_box;
        while let Some(child) = detail.first_child() {
            detail.remove(&child);
        }
        let blank = ContactDetails::default();
        let c = self.editing_target.and_then(|i| self.contacts.get(i)).unwrap_or(&blank);
        let heading = if self.editing_target.is_some() { i18n("Edit Group") } else { i18n("New Group") };
        editor_bar(detail, sender, &heading);

        let identity = group(detail, &i18n("Group"));
        // Any book that keeps groups: a new group goes in one, a group
        // being edited can move to another. The Hylki book when none is
        // listed yet.
        let mut books: Vec<crate::contacts::Book> = self.books.iter().filter(|b| b.groups.is_some()).cloned().collect();
        let editing = self.editing_target.is_some();
        if editing && !books.iter().any(|b| b.uid == c.book_uid) {
            books.insert(0, crate::contacts::Book { uid: c.book_uid.clone(), name: c.book_name.clone(), groups: c.group });
        }
        if books.is_empty() {
            books.push(crate::contacts::local_book());
        }
        let row = adw::ComboRow::new();
        row.set_title(&i18n("Address book"));
        let labels: Vec<&str> = books.iter().map(|b| b.name.as_str()).collect();
        row.set_model(Some(&gtk::StringList::new(&labels)));
        // Its own book, or for a new one the first book the list shows,
        // else the default (first).
        let at = if editing {
            books.iter().position(|b| b.uid == c.book_uid).unwrap_or(0)
        } else {
            self.scope_book(&books)
        };
        row.set_selected(at as u32);
        row.set_sensitive(books.len() > 1);
        let apple = books[at].groups == Some(GroupFormat::Apple);
        identity.append(&row);
        // What saving in another book does, said while one is picked.
        let move_note = gtk::Label::new(None);
        move_note.add_css_class("dim-label");
        move_note.add_css_class("caption");
        move_note.set_wrap(true);
        move_note.set_xalign(0.0);
        move_note.set_margin_top(6);
        move_note.set_visible(false);
        detail.append(&move_note);
        let book_row = Some(row);
        let name = entry_row(&i18n("Name"), &c.name);
        name.connect_changed(|row| row.remove_css_class("error"));
        identity.append(&name);
        let hide = adw::SwitchRow::new();
        hide.set_title(&i18n("Send as Bcc"));
        hide.set_subtitle(&i18n("Members do not see each other's addresses."));
        hide.set_active(c.hide_addresses);
        identity.append(&hide);

        // People to add are offered as the composer offers recipients: from
        // Contacts and from the mail sent and received. Until that is read,
        // the contacts here stand in.
        if self.suggestions.borrow().is_empty() {
            let contacts = self.contacts.iter().filter(|p| !p.is_group()).flat_map(|p| {
                p.emails.iter().map(|e| crate::contacts::Suggestion {
                    name: p.name.clone(),
                    email: e.value.clone(),
                    from_contacts: true,
                    from_directory: false,
                    score: 0,
                    own: false,
                    group: None,
                })
            });
            self.suggestions.borrow_mut().extend(contacts);
        }
        let _ = sender.output(ContactsPageOutput::WantSuggestions);
        // The contacts with no address, who can join a group that names
        // its members by contact.
        let no_address: Vec<(Member, String)> = self
            .contacts
            .iter()
            .filter(|p| !p.is_group() && p.emails.is_empty() && !p.eds_uid.is_empty())
            .map(|p| {
                let m = Member {
                    name: p.name.clone(),
                    source: Some((p.book_uid.clone(), p.eds_uid.clone())),
                    // Their card, when it is in the group's book already.
                    uid: if p.book_uid == c.book_uid { p.eds_uid.clone() } else { String::new() },
                    ..Member::default()
                };
                (m, p.book_name.clone())
            })
            .collect();
        let cards = Rc::new(Cell::new(apple));
        let members = member_editor(detail, &c.members, self.suggestions.clone(), Rc::new(no_address), cards.clone());
        // An Apple group (iCloud's) holds cards of its own book.
        let note = gtk::Label::new(Some(&i18n(
            "Groups in this address book hold its own contacts: anyone added who is not in it yet is added to it.",
        )));
        note.add_css_class("dim-label");
        note.add_css_class("caption");
        note.set_wrap(true);
        note.set_xalign(0.0);
        note.set_margin_top(6);
        note.set_visible(apple);
        detail.append(&note);
        if let Some(row) = &book_row {
            let formats: Vec<Option<GroupFormat>> = books.iter().map(|b| b.groups).collect();
            let (note, move_note) = (note.downgrade(), move_note.downgrade());
            let (own, old_name) = (editing.then_some(at), c.book_name.clone());
            row.connect_selected_notify(move |row| {
                let at = row.selected() as usize;
                let format = formats.get(at).copied().flatten();
                cards.set(format == Some(GroupFormat::Apple));
                if let Some(note) = note.upgrade() {
                    note.set_visible(format == Some(GroupFormat::Apple));
                }
                let Some(move_note) = move_note.upgrade() else { return };
                let moving = own.is_some_and(|own| own != at);
                move_note.set_visible(moving);
                if moving {
                    let mut text = i18n_f(
                        "Saving moves the group to this address book, with its people, and removes it from {book}.",
                        &[("book", &old_name)],
                    );
                    if format == Some(GroupFormat::Evolution) {
                        text.push(' ');
                        text.push_str(&i18n("Groups here hold addresses, so people with no email address are left out."));
                    }
                    move_note.set_label(&text);
                }
            });
        }
        if self.editing_target.is_none() {
            name.grab_focus();
        }
        self.editor = Some(Editor::Group(GroupForm { name, hide, members, book: book_row, books }));
    }

    /// Replace the detail pane with the editor form for `editing_target`
    /// (blank for a new contact).
    fn render_editor(&mut self, widgets: &ContactsPageWidgets, sender: &ComponentSender<Self>) {
        let detail = &widgets.detail_box;
        while let Some(child) = detail.first_child() {
            detail.remove(&child);
        }
        let blank = ContactDetails::default();
        let c = self
            .editing_target
            .and_then(|i| self.contacts.get(i))
            .unwrap_or(&blank);

        let heading = if self.editing_target.is_some() { i18n("Edit Contact") } else { i18n("New Contact") };
        editor_bar(detail, sender, &heading);

        let photo = Rc::new(RefCell::new(crate::contacts::PhotoEdit::Keep));
        photo_editor(detail, c, photo.clone());

        // ---- identity fields ----
        let identity = group(detail, &i18n("Identity"));
        let name = entry_row(&i18n("Name"), &c.name);
        let nickname = entry_row(&i18n("Nickname"), &c.nickname);
        let org = entry_row(&i18n("Organization"), &c.org);
        let title = entry_row(&i18n("Title"), &c.title);
        identity.append(&name);
        identity.append(&nickname);
        identity.append(&org);
        identity.append(&title);
        let (birthday_row, birthday) = BirthdayForm::new(&c.birthday_raw);
        identity.append(&birthday_row);

        // ---- the address book: chosen for a new contact; for one being
        // edited, its own, or another to move it to (when Hylki can write
        // to its book, and so delete from it) ----
        let mut book = None;
        let editing = self.editing_target.is_some();
        let own = self.books.iter().position(|b| b.uid == c.book_uid).filter(|_| !c.eds_uid.is_empty());
        if (!editing || own.is_some()) && !self.books.is_empty() {
            let row = adw::ComboRow::new();
            row.set_title(&i18n("Address book"));
            let labels: Vec<&str> = self.books.iter().map(|b| b.name.as_str()).collect();
            row.set_model(Some(&gtk::StringList::new(&labels)));
            // Its own book, or for a new one the first book the list shows,
            // else the default (first).
            let at = own.filter(|_| editing).unwrap_or_else(|| self.scope_book(&self.books));
            row.set_selected(at as u32);
            row.set_sensitive(self.books.len() > 1);
            if editing {
                row.set_subtitle(&i18n("Another book moves the contact there when saved."));
            }
            identity.prepend(&row);
            book = Some(row);
        } else if !c.book_name.is_empty() {
            let row = adw::ActionRow::new();
            row.set_title(&i18n("Address book"));
            row.set_subtitle(&gtk::glib::markup_escape_text(&c.book_name));
            row.add_css_class("property");
            identity.prepend(&row);
        }

        // ---- repeating value groups ----
        let emails = editable_values(
            detail,
            &i18n("Email"),
            &i18n("Add email"),
            &c.emails.iter().map(|l| (l.value.clone(), l.label.clone())).collect::<Vec<_>>(),
            EMAIL_LABELS,
        );
        let phones = editable_values(
            detail,
            &i18n("Phone"),
            &i18n("Add phone"),
            &c.phones.iter().map(|l| (l.value.clone(), l.label.clone())).collect::<Vec<_>>(),
            PHONE_LABELS,
        );
        let addresses = address_editor(detail, &c.postal);
        let urls = editable_values(
            detail,
            &i18n("Website"),
            &i18n("Add website"),
            &c.urls.iter().map(|u| (u.clone(), String::new())).collect::<Vec<_>>(),
            &[],
        );

        // ---- note ----
        let note_list = group(detail, &i18n("Note"));
        let note = gtk::TextView::new();
        note.set_wrap_mode(gtk::WrapMode::WordChar);
        note.buffer().set_text(&c.note);
        note.set_top_margin(8);
        note.set_bottom_margin(8);
        note.set_left_margin(10);
        note.set_right_margin(10);
        note.set_height_request(72);
        let note_row = gtk::ListBoxRow::new();
        note_row.set_activatable(false);
        note_row.set_child(Some(&note));
        note_list.append(&note_row);

        self.editor = Some(Editor::Contact(ContactForm {
            name,
            nickname,
            org,
            title,
            note,
            emails,
            phones,
            urls,
            addresses,
            birthday,
            photo,
            book,
        }));
    }
}

/// A group's card: who is in it, and a message to them all.
fn render_group_card(detail: &gtk::Box, sender: &ComponentSender<ContactsPage>, c: &ContactDetails, idx: usize) {
    let avatar = avatar_for(c, 96);
    avatar.set_halign(gtk::Align::Center);
    detail.append(&avatar);

    let name = gtk::Label::new(Some(&c.name));
    name.add_css_class("title-1");
    name.set_wrap(true);
    name.set_justify(gtk::Justification::Center);
    name.set_margin_top(12);
    detail.append(&name);

    let sub = gtk::Label::new(Some(&group_byline(c)));
    sub.add_css_class("dim-label");
    sub.set_wrap(true);
    sub.set_justify(gtk::Justification::Center);
    sub.set_margin_top(4);
    detail.append(&sub);

    if c.reachable().next().is_some() {
        let write = gtk::Button::with_label(&i18n("New Message"));
        write.add_css_class("pill");
        write.add_css_class("suggested-action");
        write.set_halign(gtk::Align::Center);
        write.set_margin_top(12);
        let s = sender.input_sender().clone();
        write.connect_clicked(move |_| {
            let _ = s.send(ContactsPageInput::ComposeGroup(idx));
        });
        detail.append(&write);
    }

    let list = group(detail, &i18n("Members"));
    for m in &c.members {
        let row = adw::ActionRow::new();
        // Someone known by their phone number only: in the group, but
        // not written to.
        if m.email.is_empty() {
            row.set_title(&gtk::glib::markup_escape_text(&m.name));
            row.set_subtitle(&i18n("No email address"));
            list.append(&row);
            continue;
        }
        let named = !m.name.trim().is_empty() && m.name != m.email;
        row.set_title(&gtk::glib::markup_escape_text(if named { &m.name } else { &m.email }));
        if named {
            row.set_subtitle(&gtk::glib::markup_escape_text(&m.email));
        }
        let recipient = crate::worker::format_recipient(&m.name, &m.email);
        let compose = flat_button("mail-message-new-symbolic", &i18n("New message"));
        let s = sender.input_sender().clone();
        let to = recipient.clone();
        compose.connect_clicked(move |_| {
            let _ = s.send(ContactsPageInput::Compose(to.clone()));
        });
        row.add_suffix(&copy_button(&m.email));
        row.add_suffix(&compose);
        row.set_activatable(true);
        let s = sender.input_sender().clone();
        row.connect_activated(move |_| {
            let _ = s.send(ContactsPageInput::Compose(recipient.clone()));
        });
        list.append(&row);
    }
    if c.members.is_empty() {
        let row = adw::ActionRow::new();
        row.set_title(&i18n("No one is in this group yet."));
        row.add_css_class("dim-label");
        list.append(&row);
    }

    let details = group(detail, &i18n("Details"));
    if !c.book_name.is_empty() {
        let row = adw::ActionRow::new();
        row.set_title(&i18n("Address Book"));
        row.set_subtitle(&gtk::glib::markup_escape_text(&c.book_name));
        row.add_css_class("property");
        details.append(&row);
    }
    if !c.group_editable() {
        let row = adw::ActionRow::new();
        row.set_title(&i18n("Changed in the app it came from"));
        row.set_subtitle(&i18n("This group names its members by contact, which Hylki does not edit."));
        row.add_css_class("property");
        details.append(&row);
    }
}

/// `person` as a new member of `group`, when the group can take them and
/// does not have them. An Apple group names its members by their card in
/// its own book, so someone of another book is copied there, whole, when
/// they join (unless the book has them by address already); an Evolution
/// list holds addresses, so it takes only someone with one. The copy, with
/// its photo, is made only `with_card`, for the save: a menu only asks.
fn member_for(group: &ContactDetails, person: &ContactDetails, with_card: bool) -> Option<Member> {
    if !group.group_editable() || person.is_group() {
        return None;
    }
    let apple = group.group == Some(GroupFormat::Apple);
    let own_card = apple && person.book_uid == group.book_uid && !person.eds_uid.is_empty();
    let member = Member {
        name: person.name.clone(),
        email: person.primary_email().unwrap_or_default().to_string(),
        uid: if own_card { person.eds_uid.clone() } else { String::new() },
        card: if apple && !own_card && with_card { crate::contacts::moved_vcard(person) } else { String::new() },
        ..Member::default()
    };
    if member.email.is_empty() && member.uid.is_empty() && !(apple && !own_card) {
        return None;
    }
    let present = group.members.iter().any(|m| m.same(&member))
        || person.emails.iter().any(|e| group.members.iter().any(|m| m.email.eq_ignore_ascii_case(&e.value)))
        || (member.email.is_empty()
            && !own_card
            && group.members.iter().any(|m| m.email.is_empty() && m.name.eq_ignore_ascii_case(&member.name)));
    (!present).then_some(member)
}

fn group_byline(c: &ContactDetails) -> String {
    crate::contacts::group_reach(c.members.len(), c.hide_addresses)
}

/// The sort and filter button's popover, laid out as the message list's
/// (#441): the orders in one column, the "Show only" filters in the other.
/// It stays open as rows are clicked, so an order and several filters can
/// be set in one visit.
struct FilterMenu {
    button: gtk::MenuButton,
    sorts: Vec<(ContactSort, gtk::CheckButton)>,
    filters: Vec<(ContactFilter, gtk::CheckButton)>,
    clear: gtk::Button,
}

impl FilterMenu {
    fn new(button: &gtk::MenuButton, input: &relm4::Sender<ContactsPageInput>) -> Self {
        let column = |title: &str| {
            let col = gtk::Box::new(gtk::Orientation::Vertical, 0);
            col.add_css_class("sort-filter-column");
            let caption = gtk::Label::new(Some(title));
            caption.set_xalign(0.0);
            caption.add_css_class("dim-label");
            caption.add_css_class("caption-heading");
            col.append(&caption);
            col
        };
        let sort_col = column(&i18n("Sort by"));
        let mut sorts: Vec<(ContactSort, gtk::CheckButton)> = Vec::new();
        for (label, sort) in [
            (i18n_noop("First name"), ContactSort::FirstName),
            (i18n_noop("Last name"), ContactSort::LastName),
            (i18n_noop("Email"), ContactSort::Email),
        ] {
            let radio = gtk::CheckButton::with_label(&i18n(label));
            if let Some((_, first)) = sorts.first() {
                radio.set_group(Some(first));
            }
            radio.set_active(sort == ContactSort::FirstName);
            let s = input.clone();
            radio.connect_toggled(move |r| {
                if r.is_active() {
                    let _ = s.send(ContactsPageInput::SetSort(sort));
                }
            });
            sort_col.append(&radio);
            sorts.push((sort, radio));
        }
        let filter_col = column(&i18n("Show only"));
        let mut filters = Vec::new();
        for f in ContactFilter::ALL {
            let check = gtk::CheckButton::with_label(&f.label());
            let s = input.clone();
            check.connect_toggled(move |c| {
                let _ = s.send(ContactsPageInput::SetFilter(f, c.is_active()));
            });
            filter_col.append(&check);
            filters.push((f, check));
        }
        let clear = gtk::Button::with_label(&i18n("Clear Filters"));
        clear.add_css_class("flat");
        clear.set_halign(gtk::Align::Start);
        clear.set_sensitive(false);
        let s = input.clone();
        clear.connect_clicked(move |_| {
            let _ = s.send(ContactsPageInput::ClearFilters);
        });
        filter_col.append(&clear);

        let columns = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        columns.add_css_class("sort-filter-popover");
        columns.append(&sort_col);
        columns.append(&gtk::Separator::new(gtk::Orientation::Vertical));
        columns.append(&filter_col);
        let popover = gtk::Popover::new();
        popover.set_child(Some(&columns));
        button.set_popover(Some(&popover));
        FilterMenu { button: button.clone(), sorts, filters, clear }
    }

    /// Show the order and the filters on the rows, and tint the button
    /// while any filter is on, so a narrowed list never passes for a short
    /// one.
    fn sync(&self, sort: ContactSort, on: u8) {
        for (s, radio) in &self.sorts {
            if *s == sort && !radio.is_active() {
                radio.set_active(true);
            }
        }
        for (f, check) in &self.filters {
            let active = on & f.bit() != 0;
            if check.is_active() != active {
                check.set_active(active);
            }
        }
        self.clear.set_sensitive(on != 0);
        if on == 0 {
            self.button.remove_css_class("list-filter-active");
            self.button.set_tooltip_text(Some(&i18n("Sort and filter contacts")));
        } else {
            self.button.add_css_class("list-filter-active");
            let names: Vec<String> =
                ContactFilter::ALL.iter().filter(|f| on & f.bit() != 0).map(|f| f.label()).collect();
            self.button.set_tooltip_text(Some(&i18n_f("Filtered: {names}", &[("names", &names.join(", "))])));
        }
    }
}

/// The groups and books a selection of several can go to, as (index into
/// `contacts`, name) and (book UID, name).
struct BulkChoices {
    groups: Vec<(usize, String)>,
    books: Vec<(String, String)>,
}

impl BulkChoices {
    fn group_entries(&self, s: &relm4::Sender<ContactsPageInput>) -> Vec<MenuEntry> {
        self.groups
            .iter()
            .map(|(g, name)| {
                let (s, g) = (s.clone(), *g);
                MenuEntry::new(name.clone(), move || {
                    let _ = s.send(ContactsPageInput::BulkAddToGroup(g));
                })
                .icon("system-users-symbolic")
            })
            .collect()
    }

    fn book_entries(&self, s: &relm4::Sender<ContactsPageInput>) -> Vec<MenuEntry> {
        self.books
            .iter()
            .map(|(uid, name)| {
                let (s, uid) = (s.clone(), uid.clone());
                MenuEntry::new(name.clone(), move || {
                    let _ = s.send(ContactsPageInput::BulkMoveRequest(uid.clone()));
                })
                .icon("x-office-address-book-symbolic")
            })
            .collect()
    }
}

/// Ask before doing something to contacts that cannot be taken back.
fn confirm(
    widgets: &ContactsPageWidgets,
    heading: &str,
    body: &str,
    action: &str,
    destructive: bool,
    on_yes: impl Fn() + 'static,
) {
    let win = widgets.page_stack.root().and_downcast::<gtk::Window>();
    let dialog = adw::MessageDialog::new(win.as_ref(), Some(heading), Some(body));
    dialog.add_response("cancel", &i18n("Cancel"));
    dialog.add_response("yes", action);
    dialog.set_response_appearance(
        "yes",
        if destructive { adw::ResponseAppearance::Destructive } else { adw::ResponseAppearance::Suggested },
    );
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");
    dialog.connect_response(None, move |_, resp| {
        if resp == "yes" {
            on_yes();
        }
    });
    dialog.present();
}

/// The header's "+": a contact or a group.
fn new_menu(button: &gtk::Button, sender: &ComponentSender<ContactsPage>) {
    let (sc, sg) = (sender.input_sender().clone(), sender.input_sender().clone());
    show_context_menu(
        button,
        (button.width() / 2) as f64,
        button.height() as f64,
        vec![vec![
            MenuEntry::new(i18n("New Contact"), move || {
                let _ = sc.send(ContactsPageInput::NewContact);
            })
            .icon("contact-new-symbolic"),
            MenuEntry::new(i18n("New Group"), move || {
                let _ = sg.send(ContactsPageInput::NewGroup);
            })
            .icon("system-users-symbolic"),
        ]],
    );
}

/// Whether a contact matches a search, already lower case; an empty one
/// matches everyone.
fn matches_query(c: &ContactDetails, q: &str) -> bool {
    if q.is_empty() {
        return true;
    }
    c.name.to_lowercase().contains(q)
        || c.nickname.to_lowercase().contains(q)
        || c.org.to_lowercase().contains(q)
        || c.emails.iter().any(|e| e.value.to_lowercase().contains(q))
        || c.phones.iter().any(|p| p.value.to_lowercase().contains(q))
        || c.members.iter().any(|m| m.name.to_lowercase().contains(q) || m.email.to_lowercase().contains(q))
}

/// Ask for `.vcf` files to import into the Hylki book, and hand their paths
/// to `done` (never called for a cancelled dialog or no files).
pub fn choose_vcf_files(parent: Option<&gtk::Window>, done: impl Fn(Vec<std::path::PathBuf>) + 'static) {
    let dialog = gtk::FileDialog::builder().title(&i18n("Import Contacts")).build();
    let filter = gtk::FileFilter::new();
    filter.set_name(Some(&i18n("vCard files")));
    filter.add_pattern("*.vcf");
    filter.add_pattern("*.vcard");
    filter.add_mime_type("text/vcard");
    filter.add_mime_type("text/x-vcard");
    let filters = gtk::gio::ListStore::new::<gtk::FileFilter>();
    filters.append(&filter);
    let any = gtk::FileFilter::new();
    any.set_name(Some(&i18n("All files")));
    any.add_pattern("*");
    filters.append(&any);
    dialog.set_filters(Some(&filters));
    dialog.open_multiple(parent, gtk::gio::Cancellable::NONE, move |res| {
        let Ok(files) = res else { return };
        let paths: Vec<std::path::PathBuf> =
            files.iter::<gtk::gio::File>().flatten().filter_map(|f| f.path()).collect();
        if !paths.is_empty() {
            done(paths);
        }
    });
}

/// The Cancel · title · Save bar over a form (dialog-header layout).
fn editor_bar(detail: &gtk::Box, sender: &ComponentSender<ContactsPage>, heading: &str) {
    let bar = gtk::CenterBox::new();
    let cancel = gtk::Button::with_label(&i18n("Cancel"));
    let s = sender.input_sender().clone();
    cancel.connect_clicked(move |_| {
        let _ = s.send(ContactsPageInput::CancelEdit);
    });
    let save = gtk::Button::with_label(&i18n("Save"));
    save.add_css_class("suggested-action");
    let s = sender.input_sender().clone();
    save.connect_clicked(move |_| {
        let _ = s.send(ContactsPageInput::SaveEdit);
    });
    let heading = gtk::Label::new(Some(heading));
    heading.add_css_class("title-4");
    bar.set_start_widget(Some(&cancel));
    bar.set_center_widget(Some(&heading));
    bar.set_end_widget(Some(&save));
    detail.append(&bar);
}

/// The group form's members: a row each with a remove button, and a row to
/// add someone, by an address or by picking from `people` (the composer's
/// suggestions) as a name or address is typed. Returns the live list read at
/// save time.
/// One of the people the group form offers for what is typed.
#[derive(Clone)]
struct Pick {
    member: Member,
    title: String,
    subtitle: String,
    /// From Contacts, not from mail history.
    contact: bool,
}

/// The group form's members, with the field that adds them. `people` are
/// the composer's suggestions, by address; `no_address` the contacts with
/// none, offered too while `cards` says the group's book names its members
/// by contact (they cannot be in a list of addresses).
fn member_editor(
    detail: &gtk::Box,
    initial: &[Member],
    people: Rc<RefCell<Vec<crate::contacts::Suggestion>>>,
    no_address: Rc<Vec<(Member, String)>>,
    cards: Rc<Cell<bool>>,
) -> Rc<RefCell<Vec<Member>>> {
    let list = group(detail, &i18n("Members"));
    let members: Rc<RefCell<Vec<Member>>> = Rc::new(RefCell::new(Vec::new()));
    let entry = adw::EntryRow::new();
    entry.set_title(&i18n("Add a person by name or address"));
    entry.set_show_apply_button(true);
    list.append(&entry);
    // The people matching what is typed, under the members.
    let found = gtk::ListBox::new();
    found.add_css_class("boxed-list");
    found.add_css_class("member-matches");
    found.set_selection_mode(gtk::SelectionMode::Single);
    found.set_can_focus(false);
    found.set_margin_top(6);
    found.set_visible(false);
    detail.append(&found);

    // Widgets are held weakly in the handlers they own, so the form is
    // freed when the pane is cleared.
    let add: Rc<dyn Fn(Member)> = {
        let (list, members, entry) = (list.downgrade(), members.clone(), entry.downgrade());
        Rc::new(move |m: Member| {
            let (Some(list), Some(entry)) = (list.upgrade(), entry.upgrade()) else { return };
            if members.borrow().iter().any(|x| x.same(&m)) {
                return;
            }
            let row = adw::ActionRow::new();
            let named = !m.name.trim().is_empty() && m.name != m.email;
            row.set_title(&gtk::glib::markup_escape_text(if named { &m.name } else { &m.email }));
            if m.email.is_empty() {
                row.set_subtitle(&i18n("No email address"));
            } else if named {
                row.set_subtitle(&gtk::glib::markup_escape_text(&m.email));
            }
            let remove = flat_button("user-trash-symbolic", &i18n("Remove"));
            {
                let (list, row, members, gone) = (list.downgrade(), row.downgrade(), members.clone(), m.clone());
                remove.connect_clicked(move |_| {
                    members.borrow_mut().retain(|x| !x.same(&gone));
                    if let (Some(list), Some(row)) = (list.upgrade(), row.upgrade()) {
                        list.remove(&row);
                    }
                });
            }
            row.add_suffix(&remove);
            list.insert(&row, entry.index());
            members.borrow_mut().push(m);
        })
    };
    for m in initial {
        add(m.clone());
    }

    // Who matches the typing, by name or address and not already in,
    // ranked as the composer ranks them; then the contacts with no address
    // whose name matches.
    let matches = {
        let (people, members) = (people.clone(), members.clone());
        move |typed: &str| -> Vec<Pick> {
            let q = typed.trim();
            if q.is_empty() {
                return Vec::new();
            }
            let taken: std::collections::HashSet<String> =
                members.borrow().iter().map(|m| m.email.to_lowercase()).collect();
            let mut found: Vec<crate::contacts::Suggestion> = people
                .borrow()
                .iter()
                .filter(|s| s.group.is_none() && !taken.contains(&s.email.to_lowercase()) && s.matches(q))
                .cloned()
                .collect();
            crate::contacts::rank(&mut found, q);
            found.truncate(6);
            let mut picks: Vec<Pick> = found
                .into_iter()
                .map(|sug| Pick {
                    // History may know an address without a name.
                    member: Member {
                        name: if sug.name == sug.email { String::new() } else { sug.name.clone() },
                        email: sug.email.clone(),
                        ..Member::default()
                    },
                    subtitle: if sug.name != sug.email { sug.email.clone() } else { String::new() },
                    title: sug.name,
                    contact: sug.from_contacts,
                })
                .collect();
            if cards.get() {
                let q = q.to_lowercase();
                let members = members.borrow();
                picks.extend(
                    no_address
                        .iter()
                        .filter(|(m, _)| m.name.to_lowercase().contains(&q) && !members.iter().any(|x| x.same(m)))
                        .take(4)
                        .map(|(m, book)| Pick {
                            member: m.clone(),
                            title: m.name.clone(),
                            subtitle: i18n_f("No email address · {book}", &[("book", book)]),
                            contact: true,
                        }),
                );
            }
            picks
        }
    };
    let matches = Rc::new(matches);
    // The matches listed, and the one Return takes: the first while a name
    // is typed, none while the text is an address (Return adds what was
    // typed), whichever the arrow keys move to.
    let shown: Rc<RefCell<Vec<Pick>>> = Rc::new(RefCell::new(Vec::new()));
    let chosen: Rc<std::cell::Cell<Option<usize>>> = Rc::new(std::cell::Cell::new(None));
    let highlight = {
        let (found, chosen) = (found.downgrade(), chosen.clone());
        move |at: Option<usize>| {
            chosen.set(at);
            let Some(found) = found.upgrade() else { return };
            match at.and_then(|i| found.row_at_index(i as i32)) {
                Some(row) => found.select_row(Some(&row)),
                None => found.unselect_all(),
            }
        }
    };
    let highlight = Rc::new(highlight);
    // Add a match as a member and start the field over.
    let take = {
        let add = add.clone();
        move |pick: &Pick, entry: &adw::EntryRow| {
            add(pick.member.clone());
            entry.set_text("");
            entry.grab_focus();
        }
    };
    let take = Rc::new(take);
    {
        let (found, matches, shown, highlight, take) =
            (found.downgrade(), matches.clone(), shown.clone(), highlight.clone(), take.clone());
        entry.connect_changed(move |e| {
            let Some(found) = found.upgrade() else { return };
            while let Some(child) = found.first_child() {
                found.remove(&child);
            }
            let text = e.text();
            *shown.borrow_mut() = matches(&text);
            for sug in shown.borrow().iter() {
                let row = adw::ActionRow::new();
                row.set_title(&gtk::glib::markup_escape_text(&sug.title));
                if !sug.subtitle.is_empty() {
                    row.set_subtitle(&gtk::glib::markup_escape_text(&sug.subtitle));
                }
                // Where the address came from, as in the composer: the
                // address book or mail history.
                let icon = gtk::Image::from_icon_name(if sug.contact {
                    "avatar-default-symbolic"
                } else {
                    "document-open-recent-symbolic"
                });
                icon.add_css_class("dim-label");
                row.add_prefix(&icon);
                row.set_activatable(true);
                // The typing stays in the field while the arrows move here.
                row.set_can_focus(false);
                let (take, entry, sug) = (take.clone(), e.downgrade(), sug.clone());
                row.connect_activated(move |_| {
                    if let Some(entry) = entry.upgrade() {
                        take(&sug, &entry);
                    }
                });
                found.append(&row);
            }
            let any = found.first_child().is_some();
            found.set_visible(any);
            let address = !typed_addresses(&text).is_empty();
            highlight((any && !address).then_some(0));
        });
    }
    // Down and Up move through the matches; Up from the first goes back to
    // what was typed. Escape puts the list away.
    {
        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        let (found, shown, chosen, highlight) = (found.downgrade(), shown.clone(), chosen.clone(), highlight.clone());
        keys.connect_key_pressed(move |_, keyval, _, _| {
            use gtk::glib::Propagation;
            let Some(found) = found.upgrade().filter(|f| f.is_visible()) else { return Propagation::Proceed };
            let last = shown.borrow().len().saturating_sub(1);
            if keyval == gtk::gdk::Key::Down || keyval == gtk::gdk::Key::KP_Down {
                highlight(Some(chosen.get().map_or(0, |i| (i + 1).min(last))));
            } else if keyval == gtk::gdk::Key::Up || keyval == gtk::gdk::Key::KP_Up {
                highlight(chosen.get().and_then(|i| i.checked_sub(1)));
            } else if keyval == gtk::gdk::Key::Escape {
                highlight(None);
                found.set_visible(false);
            } else {
                return Propagation::Proceed;
            }
            Propagation::Stop
        });
        entry.add_controller(keys);
    }
    // Return or the apply button: the match picked, else the addresses typed
    // or pasted.
    let commit = move |e: &adw::EntryRow| {
        let picked = chosen.get().and_then(|i| shown.borrow().get(i).cloned());
        if let Some(sug) = picked.filter(|_| found.is_visible()) {
            take(&sug, e);
            return;
        }
        let typed = typed_addresses(&e.text());
        if typed.is_empty() {
            e.add_css_class("error");
            return;
        }
        for (name, email) in typed {
            // An address typed bare keeps the name Contacts or mail has for it.
            let name = if name.is_empty() || name == email {
                people
                    .borrow()
                    .iter()
                    .find(|s| s.email.eq_ignore_ascii_case(&email) && s.name != s.email)
                    .map(|s| s.name.clone())
                    .unwrap_or_default()
            } else {
                name
            };
            add(Member { name, email, ..Member::default() });
        }
        e.set_text("");
    };
    let commit = Rc::new(commit);
    {
        let commit = commit.clone();
        entry.connect_apply(move |e| commit(e));
    }
    entry.connect_entry_activated(move |e| commit(e));
    entry.connect_changed(|e| e.remove_css_class("error"));
    members
}

/// The whole addresses in a typed or pasted text, as (name, address).
fn typed_addresses(text: &str) -> Vec<(String, String)> {
    crate::worker::parse_recipients(text)
        .into_iter()
        .filter(|(_, email)| email.contains('@') && crate::worker::invalid_recipient(email).is_none())
        .collect()
}

/// Read the editor form back into a `ContactEdit`.
fn collect_edit(e: &ContactForm) -> ContactEdit {
    let values = |rows: &ValueRows| -> Vec<Labeled> {
        let rows = rows.borrow();
        rows.rows
            .iter()
            .filter_map(|(entry, label)| {
                let value = entry.text().trim().to_string();
                let label = label
                    .as_ref()
                    .and_then(|d| rows.labels.get(d.selected() as usize))
                    .map_or(String::new(), |l| l.to_string());
                (!value.is_empty()).then(|| Labeled { label, value })
            })
            .collect()
    };
    let buffer = e.note.buffer();
    ContactEdit {
        addresses: e
            .addresses
            .borrow()
            .iter()
            .map(AddressForm::read)
            .filter(|a| !a.is_empty())
            .collect(),
        birthday: e.birthday.read(),
        photo: e.photo.borrow().clone(),
        name: e.name.text().trim().to_string(),
        nickname: e.nickname.text().trim().to_string(),
        org: e.org.text().trim().to_string(),
        title: e.title.text().trim().to_string(),
        note: buffer
            .text(&buffer.start_iter(), &buffer.end_iter(), false)
            .trim()
            .to_string(),
        emails: values(&e.emails),
        phones: values(&e.phones),
        urls: values(&e.urls).into_iter().map(|l| l.value).collect(),
    }
}

/// The value rows of one kind (emails, phones…), each with its label menu
/// when the kind has labels: `labels[i]` is what item `i` stands for, the
/// first being none.
struct ValueRowsInner {
    rows: Vec<(adw::EntryRow, Option<gtk::DropDown>)>,
    labels: &'static [&'static str],
}
type ValueRows = Rc<RefCell<ValueRowsInner>>;

/// The labels an email can have, as vCard TYPEs name them.
const EMAIL_LABELS: &[&str] = &["", "Home", "Work"];
const PHONE_LABELS: &[&str] = &["", "Mobile", "Home", "Work", "Fax"];
const ADDRESS_LABELS: &[&str] = &["", "Home", "Work"];

/// A label menu over `labels`, on `current`.
fn label_menu(labels: &'static [&'static str], current: &str) -> gtk::DropDown {
    let names: Vec<String> = labels
        .iter()
        .map(|l| match *l {
            "" => i18n("No label"),
            "Home" => i18n("Home"),
            "Work" => i18n("Work"),
            "Mobile" => i18n("Mobile"),
            "Fax" => i18n("Fax"),
            other => other.to_string(),
        })
        .collect();
    let refs: Vec<&str> = names.iter().map(String::as_str).collect();
    let menu = gtk::DropDown::from_strings(&refs);
    menu.add_css_class("flat");
    menu.set_valign(gtk::Align::Center);
    menu.set_tooltip_text(Some(&i18n("Label")));
    menu.set_selected(labels.iter().position(|l| *l == current).unwrap_or(0) as u32);
    menu
}

/// A titled group of editable value rows with a remove button each and an
/// "add" row at the bottom. Returns the live handle used at save time.
fn editable_values(
    detail: &gtk::Box,
    title: &str,
    add_label: &str,
    initial: &[(String, String)],
    labels: &'static [&'static str],
) -> ValueRows {
    let list = group(detail, title);
    let rows: ValueRows = Rc::new(RefCell::new(ValueRowsInner { rows: Vec::new(), labels }));

    // The "add" row sits last; value rows are inserted above it.
    let add_row = gtk::ListBoxRow::new();
    add_row.set_activatable(true);
    let add_box = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    add_box.set_margin_top(10);
    add_box.set_margin_bottom(10);
    add_box.set_margin_start(12);
    let plus = gtk::Image::from_icon_name("list-add-symbolic");
    plus.add_css_class("dim-label");
    let label = gtk::Label::new(Some(add_label));
    label.add_css_class("dim-label");
    add_box.append(&plus);
    add_box.append(&label);
    add_row.set_child(Some(&add_box));

    let field = title.to_string();
    let add_value = {
        let list = list.clone();
        let rows = rows.clone();
        let add_row = add_row.clone();
        move |value: &str, label: String, grab: bool| {
            let entry = adw::EntryRow::new();
            entry.set_title(&field);
            entry.set_text(value);
            let menu = (!labels.is_empty()).then(|| label_menu(labels, &label));
            if let Some(menu) = &menu {
                entry.add_suffix(menu);
            }
            let remove = gtk::Button::from_icon_name("user-trash-symbolic");
            remove.set_tooltip_text(Some(i18n("Remove").as_str()));
            remove.set_valign(gtk::Align::Center);
            remove.add_css_class("flat");
            {
                let list = list.clone();
                let rows = rows.clone();
                let entry = entry.clone();
                remove.connect_clicked(move |_| {
                    rows.borrow_mut().rows.retain(|(e, _)| e != &entry);
                    list.remove(&entry);
                });
            }
            entry.add_suffix(&remove);
            // Insert above the trailing "add" row.
            list.insert(&entry, add_row.index());
            rows.borrow_mut().rows.push((entry.clone(), menu));
            if grab {
                entry.grab_focus();
            }
        }
    };

    for (value, label) in initial {
        add_value(value, label.clone(), false);
    }
    {
        let add_value = add_value.clone();
        let add_row = add_row.clone();
        list.connect_row_activated(move |_, row| {
            if row == &add_row {
                add_value("", String::new(), true);
            }
        });
    }
    list.append(&add_row);
    rows
}

/// One postal address in the contact form: its parts, and its label. The
/// post office box and the extended part, which the form does not show,
/// are kept as they were.
struct AddressForm {
    base: crate::contacts::Postal,
    street: adw::EntryRow,
    locality: adw::EntryRow,
    region: adw::EntryRow,
    code: adw::EntryRow,
    country: adw::EntryRow,
    label: gtk::DropDown,
}

impl AddressForm {
    fn read(&self) -> crate::contacts::Postal {
        let text = |e: &adw::EntryRow| e.text().trim().to_string();
        crate::contacts::Postal {
            label: ADDRESS_LABELS.get(self.label.selected() as usize).map_or(String::new(), |l| l.to_string()),
            street: text(&self.street),
            locality: text(&self.locality),
            region: text(&self.region),
            code: text(&self.code),
            country: text(&self.country),
            ..self.base.clone()
        }
    }
}

/// The contact form's addresses: a block of rows for each, and a row that
/// adds one.
fn address_editor(detail: &gtk::Box, initial: &[crate::contacts::Postal]) -> Rc<RefCell<Vec<AddressForm>>> {
    let heading = gtk::Label::new(Some(&i18n("Address")));
    heading.add_css_class("heading");
    heading.set_halign(gtk::Align::Start);
    heading.set_margin_top(20);
    heading.set_margin_bottom(6);
    detail.append(&heading);
    let column = gtk::Box::new(gtk::Orientation::Vertical, 12);
    detail.append(&column);
    let forms: Rc<RefCell<Vec<AddressForm>>> = Rc::new(RefCell::new(Vec::new()));

    // The same "add" row as the other kinds of value have.
    let add_list = gtk::ListBox::new();
    add_list.add_css_class("boxed-list");
    add_list.set_selection_mode(gtk::SelectionMode::None);
    let add_row = gtk::ListBoxRow::new();
    add_row.set_activatable(true);
    let add_box = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    add_box.set_margin_top(10);
    add_box.set_margin_bottom(10);
    add_box.set_margin_start(12);
    let plus = gtk::Image::from_icon_name("list-add-symbolic");
    plus.add_css_class("dim-label");
    let add_label = gtk::Label::new(Some(&i18n("Add address")));
    add_label.add_css_class("dim-label");
    add_box.append(&plus);
    add_box.append(&add_label);
    add_row.set_child(Some(&add_box));
    add_list.append(&add_row);

    let add = {
        let (column, forms, add_list) = (column.downgrade(), forms.clone(), add_list.downgrade());
        Rc::new(move |p: &crate::contacts::Postal, grab: bool| {
            let (Some(column), Some(add_list)) = (column.upgrade(), add_list.upgrade()) else { return };
            let list = gtk::ListBox::new();
            list.add_css_class("boxed-list");
            list.set_selection_mode(gtk::SelectionMode::None);
            let head = adw::ActionRow::new();
            head.set_title(&i18n("Label"));
            let label = label_menu(ADDRESS_LABELS, &p.label);
            head.add_suffix(&label);
            let remove = gtk::Button::from_icon_name("user-trash-symbolic");
            remove.set_tooltip_text(Some(i18n("Remove").as_str()));
            remove.set_valign(gtk::Align::Center);
            remove.add_css_class("flat");
            head.add_suffix(&remove);
            list.append(&head);
            let street = entry_row(&i18n("Street"), &p.street);
            let locality = entry_row(&i18n("City"), &p.locality);
            let region = entry_row(&i18n("State or region"), &p.region);
            let code = entry_row(&i18n("Postal code"), &p.code);
            let country = entry_row(&i18n("Country"), &p.country);
            for row in [&street, &locality, &region, &code, &country] {
                list.append(row);
            }
            {
                let (forms, list_weak, street) = (forms.clone(), list.downgrade(), street.clone());
                remove.connect_clicked(move |_| {
                    forms.borrow_mut().retain(|f| f.street != street);
                    if let Some(list) = list_weak.upgrade() {
                        if let Some(parent) = list.parent().and_downcast::<gtk::Box>() {
                            parent.remove(&list);
                        }
                    }
                });
            }
            column.insert_child_after(&list, add_list.prev_sibling().as_ref());
            if grab {
                street.grab_focus();
            }
            forms.borrow_mut().push(AddressForm { base: p.clone(), street, locality, region, code, country, label });
        })
    };
    column.append(&add_list);
    for p in initial {
        add(p, false);
    }
    add_list.connect_row_activated(move |_, _| add(&crate::contacts::Postal::default(), true));
    forms
}

/// The contact form's birthday: day, month and an optional year. No month
/// is no birthday.
struct BirthdayForm {
    day: gtk::SpinButton,
    month: gtk::DropDown,
    year: gtk::Entry,
}

impl BirthdayForm {
    fn new(raw: &str) -> (adw::ActionRow, BirthdayForm) {
        let row = adw::ActionRow::new();
        row.set_title(&i18n("Birthday"));
        let parts = crate::contacts::birthday_parts(raw);
        let day = gtk::SpinButton::with_range(1.0, 31.0, 1.0);
        day.set_valign(gtk::Align::Center);
        day.set_value(parts.map_or(1, |(_, _, d)| d) as f64);
        let mut months = vec![i18n("No birthday")];
        months.extend((1..=12).map(|m| {
            gtk::glib::DateTime::from_local(2000, m, 1, 12, 0, 0.0)
                .ok()
                .and_then(|d| d.format("%B").ok())
                .map_or(m.to_string(), |s| s.to_string())
        }));
        let refs: Vec<&str> = months.iter().map(String::as_str).collect();
        let month = gtk::DropDown::from_strings(&refs);
        month.set_valign(gtk::Align::Center);
        month.set_selected(parts.map_or(0, |(_, m, _)| m));
        let year = gtk::Entry::new();
        year.set_valign(gtk::Align::Center);
        year.set_width_chars(5);
        year.set_max_width_chars(5);
        year.set_placeholder_text(Some(&i18n("Year")));
        year.set_input_purpose(gtk::InputPurpose::Digits);
        if let Some((Some(y), _, _)) = parts {
            year.set_text(&y.to_string());
        }
        let sync = {
            let (day, year) = (day.clone(), year.clone());
            move |month: &gtk::DropDown| {
                let on = month.selected() != 0;
                day.set_sensitive(on);
                year.set_sensitive(on);
            }
        };
        sync(&month);
        month.connect_selected_notify(sync);
        row.add_suffix(&day);
        row.add_suffix(&month);
        row.add_suffix(&year);
        (row, BirthdayForm { day, month, year })
    }

    /// The BDAY value, empty for none.
    fn read(&self) -> String {
        let month = self.month.selected();
        if month == 0 {
            return String::new();
        }
        let year = self.year.text().trim().parse::<i32>().ok().filter(|y| (1..=9999).contains(y));
        crate::contacts::birthday_value(year, month, self.day.value() as u32)
    }
}

/// The contact form's photo: the picture, with buttons to change it or
/// remove it.
fn photo_editor(detail: &gtk::Box, c: &ContactDetails, photo: Rc<RefCell<crate::contacts::PhotoEdit>>) {
    let avatar = avatar_for(c, 96);
    avatar.set_halign(gtk::Align::Center);
    avatar.set_margin_top(12);
    detail.append(&avatar);
    let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    buttons.set_halign(gtk::Align::Center);
    buttons.set_margin_top(8);
    let change = gtk::Button::with_label(&i18n("Change Photo…"));
    change.add_css_class("pill");
    let remove = gtk::Button::with_label(&i18n("Remove Photo"));
    remove.add_css_class("pill");
    remove.set_visible(c.has_photo());
    buttons.append(&change);
    buttons.append(&remove);
    detail.append(&buttons);
    {
        let (photo, avatar, remove_btn) = (photo.clone(), avatar.clone(), remove.clone());
        remove.connect_clicked(move |_| {
            *photo.borrow_mut() = crate::contacts::PhotoEdit::Remove;
            avatar.set_custom_image(None::<&gtk::gdk::Paintable>);
            remove_btn.set_visible(false);
        });
    }
    change.connect_clicked(move |b| {
        let dialog = gtk::FileDialog::builder().title(&i18n("Choose a Photo")).build();
        let filter = gtk::FileFilter::new();
        filter.set_name(Some(&i18n("Images")));
        filter.add_pixbuf_formats();
        let filters = gtk::gio::ListStore::new::<gtk::FileFilter>();
        filters.append(&filter);
        dialog.set_filters(Some(&filters));
        let parent = b.root().and_downcast::<gtk::Window>();
        let (photo, avatar, remove) = (photo.clone(), avatar.clone(), remove.clone());
        dialog.open(parent.as_ref(), gtk::gio::Cancellable::NONE, move |res| {
            let Some(path) = res.ok().and_then(|f| f.path()) else { return };
            let Some(bytes) = small_photo(&path) else { return };
            if let Ok(texture) = gtk::gdk::Texture::from_bytes(&gtk::glib::Bytes::from(&bytes)) {
                avatar.set_custom_image(Some(&texture));
            }
            *photo.borrow_mut() = crate::contacts::PhotoEdit::Set(bytes);
            remove.set_visible(true);
        });
    });
}

/// An image file as a contact photo: at most 512 pixels a side, a JPEG (a
/// PNG when it has see-through parts), so it goes in the card itself.
fn small_photo(path: &std::path::Path) -> Option<Vec<u8>> {
    let pixbuf = gtk::gdk_pixbuf::Pixbuf::from_file_at_scale(path, 512, 512, true).ok()?;
    let (kind, options): (&str, &[(&str, &str)]) =
        if pixbuf.has_alpha() { ("png", &[]) } else { ("jpeg", &[("quality", "90")]) };
    pixbuf.save_to_bufferv(kind, options).ok()
}

fn entry_row(title: &str, value: &str) -> adw::EntryRow {
    let row = adw::EntryRow::new();
    row.set_title(title);
    row.set_text(value);
    row
}

/// One line of the contact list.
struct Line {
    /// The entry the line shows (see `row_map`); `None` for one that is
    /// nobody and cannot be selected. Moves when a contact before it is
    /// deleted.
    idx: Cell<Option<usize>>,
    title: String,
    subtitle: String,
    kind: LineKind,
}

/// Lines are alike when they show the same thing, wherever they are.
impl PartialEq for Line {
    fn eq(&self, other: &Self) -> bool {
        self.title == other.title && self.subtitle == other.subtitle && self.kind == other.kind
    }
}

#[derive(PartialEq)]
enum LineKind {
    /// A person or a group, with their photo's key (0: none).
    Entry { photo_key: u64, group: bool },
    /// The heading above the people found in the directories.
    Heading,
    /// A directory that did not answer the search.
    Failure,
    /// A dim line in place of an empty list.
    Note,
}

impl Line {
    fn entry(idx: usize, c: &ContactDetails, subtitle: String) -> Self {
        Line {
            idx: Cell::new(Some(idx)),
            title: c.name.clone(),
            subtitle,
            kind: LineKind::Entry { photo_key: line_photo_key(c), group: c.is_group() },
        }
    }

    fn other(kind: LineKind, title: String, subtitle: String) -> Self {
        Line { idx: Cell::new(None), title, subtitle, kind }
    }
}

/// The key a contact's photo is decoded under. A read of the address books
/// works it out off the main thread; a directory entry's is worked out here.
fn line_photo_key(c: &ContactDetails) -> u64 {
    match (&c.photo, c.photo_key) {
        (Some(photo), 0) => crate::contacts::photo_key(photo),
        (_, key) => key,
    }
}

/// The widgets of one row, which the list view binds to line after line.
/// Kept light, since the view holds a couple of hundred: the face is an
/// image in a clipped disc rather than an `adw::Avatar`, and the parts only
/// a heading, a note or a group needs are made when one first shows here.
struct ListRow {
    root: gtk::Box,
    header: gtk::Box,
    /// The circle the face is drawn in; its rounded corners clip a photo.
    disc: gtk::Box,
    face: gtk::Image,
    warning: gtk::Image,
    title: gtk::Label,
    subtitle: gtk::Label,
    /// For a group, or a name with no letter to draw: libadwaita's avatar.
    avatar: RefCell<Option<adw::Avatar>>,
    /// The heading or note text, for those lines.
    banner: RefCell<Option<gtk::Label>>,
    /// The line bound now, for the right-click.
    line: RefCell<Option<gtk::glib::BoxedAnyObject>>,
    /// The photo the face waits for (0: none).
    waiting: Cell<u64>,
}

/// The size of a list face.
const FACE: i32 = 34;

impl ListRow {
    fn new() -> Self {
        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        // `header`, as in a list box row: the selected row's text turns
        // white by that class (styles.css).
        // The measures of the `adw::ActionRow` the list had before.
        let header = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        header.add_css_class("header");
        header.set_size_request(-1, 50);
        header.set_margin_start(12);
        header.set_margin_end(12);
        let disc = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        disc.add_css_class("contact-face");
        disc.set_overflow(gtk::Overflow::Hidden);
        disc.set_valign(gtk::Align::Center);
        let face = gtk::Image::new();
        face.set_pixel_size(FACE);
        disc.append(&face);
        let warning = gtk::Image::from_icon_name("dialog-warning-symbolic");
        warning.add_css_class("warning");
        // As wide as a face, so the names line up.
        warning.set_size_request(FACE, -1);
        let text = gtk::Box::new(gtk::Orientation::Vertical, 3);
        text.set_valign(gtk::Align::Center);
        text.set_hexpand(true);
        let title = gtk::Label::new(None);
        title.set_xalign(0.0);
        title.set_ellipsize(gtk::pango::EllipsizeMode::End);
        let subtitle = gtk::Label::new(None);
        subtitle.add_css_class("dim-label");
        subtitle.add_css_class("caption");
        subtitle.set_xalign(0.0);
        subtitle.set_ellipsize(gtk::pango::EllipsizeMode::End);
        text.append(&title);
        text.append(&subtitle);
        header.append(&disc);
        header.append(&warning);
        header.append(&text);
        root.append(&header);
        ListRow {
            root,
            header,
            disc,
            face,
            warning,
            title,
            subtitle,
            avatar: RefCell::new(None),
            banner: RefCell::new(None),
            line: RefCell::new(None),
            waiting: Cell::new(0),
        }
    }

    fn bind(&self, item: &gtk::glib::BoxedAnyObject) {
        let line = item.borrow::<Line>();
        self.line.replace(Some(item.clone()));
        self.waiting.set(0);
        let banner = matches!(line.kind, LineKind::Heading | LineKind::Note);
        self.header.set_visible(!banner);
        self.show_banner(banner.then_some(&line));
        if banner {
            return;
        }
        self.title.set_label(&line.title);
        self.subtitle.set_label(&line.subtitle);
        self.subtitle.set_visible(!line.subtitle.is_empty());
        let failure = matches!(line.kind, LineKind::Failure);
        // A failure says why in full; an address fits one line.
        self.subtitle.set_wrap(failure);
        self.subtitle.set_lines(if failure { 3 } else { 1 });
        self.warning.set_visible(failure);
        self.disc.set_visible(!failure);
        if let LineKind::Entry { photo_key, group } = line.kind {
            let initials = (!group).then(|| crate::ui::initials::InitialsPaintable::for_name(&line.title)).flatten();
            match initials {
                Some(paintable) => {
                    self.face.set_paintable(Some(&paintable));
                    self.face.set_visible(true);
                    self.show_avatar(None);
                }
                None => {
                    self.face.set_visible(false);
                    self.show_avatar(Some((&line.title, group)));
                }
            }
            self.waiting.set(photo_key);
            self.show_photo();
        }
    }

    /// Put the awaited photo in the face, once it is decoded.
    fn show_photo(&self) {
        let key = self.waiting.get();
        if key == 0 {
            return;
        }
        if let Some(texture) = crate::avatar::photo_thumbnail(key) {
            if let Some(texture) = texture {
                self.face.set_paintable(Some(&texture));
                self.face.set_visible(true);
                self.show_avatar(None);
            }
            self.waiting.set(0);
        }
    }

    /// libadwaita's avatar for `(name, group)`, in place of the face; `None`
    /// hides it.
    fn show_avatar(&self, who: Option<(&str, bool)>) {
        let mut avatar = self.avatar.borrow_mut();
        let Some((name, group)) = who else {
            if let Some(a) = avatar.as_ref() {
                a.set_visible(false);
            }
            return;
        };
        let a = avatar.get_or_insert_with(|| {
            let a = adw::Avatar::new(FACE, None, true);
            self.disc.append(&a);
            a
        });
        a.set_text(Some(name));
        a.set_show_initials(!group);
        a.set_icon_name(group.then_some("system-users-symbolic"));
        a.set_visible(true);
    }

    /// A heading's or a note's text in place of the row; `None` hides it.
    fn show_banner(&self, line: Option<&Line>) {
        let mut banner = self.banner.borrow_mut();
        let Some(line) = line else {
            if let Some(b) = banner.as_ref() {
                b.set_visible(false);
            }
            return;
        };
        let label = banner.get_or_insert_with(|| {
            let label = gtk::Label::new(None);
            label.add_css_class("dim-label");
            self.root.append(&label);
            label
        });
        let heading = matches!(line.kind, LineKind::Heading);
        label.set_label(&line.title);
        label.set_css_classes(if heading { &["heading", "dim-label"] } else { &["dim-label"] });
        label.set_xalign(if heading { 0.0 } else { 0.5 });
        label.set_wrap(!heading);
        label.set_justify(if heading { gtk::Justification::Left } else { gtk::Justification::Center });
        label.set_margin_top(if heading { 12 } else { 24 });
        label.set_margin_bottom(if heading { 4 } else { 0 });
        label.set_margin_start(12);
        label.set_margin_end(12);
        label.set_visible(true);
    }

    /// The entry the bound line shows.
    fn idx(&self) -> Option<usize> {
        self.line.borrow().as_ref().and_then(|item| item.borrow::<Line>().idx.get())
    }
}

/// The list view's rows: made for the lines on screen and bound to each
/// line scrolled to.
/// What a contact row carries when dragged to the sidebar: the book and
/// UID of each contact by its index (`None` for a group, or a card EDS
/// has not named), and the indexes selected together, which go as one.
#[derive(Default)]
struct DragKeys {
    keys: Vec<Option<(String, String)>>,
    multi: Vec<usize>,
}

impl DragKeys {
    /// The drag's text for the row of contact `idx`: [`DRAG_MARK`], then a
    /// line of book and UID, tab apart, per contact.
    fn payload(&self, idx: usize) -> Option<String> {
        let idxs: Vec<usize> = if self.multi.contains(&idx) { self.multi.clone() } else { vec![idx] };
        let lines: Vec<String> = idxs
            .iter()
            .filter_map(|&i| self.keys.get(i).cloned().flatten())
            .map(|(book, uid)| format!("{book}\t{uid}"))
            .collect();
        (!lines.is_empty()).then(|| format!("{DRAG_MARK}\n{}", lines.join("\n")))
    }
}

/// The contacts in a drag's text, by book and UID; `None` when it is not
/// a drag of contacts (a message, a folder).
pub fn dragged_contacts(payload: &str) -> Option<Vec<(String, String)>> {
    let mut lines = payload.lines();
    (lines.next()? == DRAG_MARK).then(|| {
        lines
            .filter_map(|l| l.split_once('\t'))
            .map(|(book, uid)| (book.to_string(), uid.to_string()))
            .collect()
    })
}

/// A book's name in its parts: the key its account's books share, the
/// account for the line under the name, and the book's own name. "Account
/// · book" is both; a book that is its account alone ("CardDAV — a@b.org")
/// is named by its address with its service under it, and an account of
/// its own; a directory is one of the directories.
fn split_book_name(name: &str, directory: bool) -> (String, String, String) {
    if directory {
        let directories = i18n("Directories");
        return (directories.clone(), directories, name.trim_start_matches("LDAP — ").to_string());
    }
    match name.split_once(" · ") {
        Some((account, own)) => (account.to_string(), account.to_string(), own.to_string()),
        None => match name.split_once(" — ") {
            Some((kind, account)) => (name.to_string(), kind.to_string(), account.to_string()),
            None => (name.to_string(), String::new(), name.to_string()),
        },
    }
}

/// Whether member `m` of group `g` is the contact `p`: the card the group
/// names, in its own book, or an address of theirs.
fn is_member(g: &ContactDetails, m: &Member, p: &ContactDetails) -> bool {
    (!m.uid.is_empty() && p.book_uid == g.book_uid && p.eds_uid == m.uid)
        || (!m.email.is_empty() && p.emails.iter().any(|e| e.value.eq_ignore_ascii_case(&m.email)))
}

fn row_factory(
    rows: &Rc<RefCell<HashMap<usize, Rc<ListRow>>>>,
    input: &relm4::Sender<ContactsPageInput>,
    drag: &Rc<RefCell<DragKeys>>,
) -> gtk::SignalListItemFactory {
    let factory = gtk::SignalListItemFactory::new();
    let key = |obj: &gtk::glib::Object| obj.as_ptr() as usize;
    {
        let (rows, input, drag) = (Rc::downgrade(rows), input.clone(), Rc::downgrade(drag));
        factory.connect_setup(move |_, obj| {
            let (Some(rows), Some(item)) = (rows.upgrade(), obj.downcast_ref::<gtk::ListItem>()) else {
                return;
            };
            let row = Rc::new(ListRow::new());
            // Right-click: the quick actions on this entry, built when asked.
            let right_click = gtk::GestureClick::new();
            right_click.set_button(3);
            let (weak, input) = (Rc::downgrade(&row), input.clone());
            right_click.connect_pressed(move |gesture, _, x, y| {
                let (Some(row), Some(widget)) = (weak.upgrade(), gesture.widget()) else { return };
                let Some(idx) = row.idx() else { return };
                let Some(list) = widget.ancestor(gtk::ListView::static_type()) else { return };
                let Some(at) = widget.compute_point(&list, &gtk::graphene::Point::new(x as f32, y as f32)) else {
                    return;
                };
                let _ = input.send(ContactsPageInput::RowMenu { idx, x: at.x().into(), y: at.y().into() });
            });
            row.root.add_controller(right_click);
            // Dragged to a book or group in the sidebar: this contact, or
            // every contact selected with it.
            let source = gtk::DragSource::new();
            source.set_actions(gtk::gdk::DragAction::COPY | gtk::gdk::DragAction::MOVE);
            {
                let (weak, drag) = (Rc::downgrade(&row), drag.clone());
                source.connect_prepare(move |_, _, _| {
                    let (row, drag) = (weak.upgrade()?, drag.upgrade()?);
                    let payload = drag.borrow().payload(row.idx()?)?;
                    Some(gtk::gdk::ContentProvider::for_value(&payload.to_value()))
                });
            }
            {
                let weak = Rc::downgrade(&row);
                source.connect_drag_begin(move |src, _| {
                    if let Some(row) = weak.upgrade() {
                        src.set_icon(Some(&gtk::WidgetPaintable::new(Some(&row.root))), 0, 0);
                    }
                });
            }
            row.root.add_controller(source);
            item.set_child(Some(&row.root));
            rows.borrow_mut().insert(key(obj), row);
        });
    }
    {
        let rows = Rc::downgrade(rows);
        factory.connect_bind(move |_, obj| {
            let Some(rows) = rows.upgrade() else { return };
            let Some(item) = obj.downcast_ref::<gtk::ListItem>() else { return };
            let Some(line) = item.item().and_downcast::<gtk::glib::BoxedAnyObject>() else { return };
            let someone = line.borrow::<Line>().idx.get().is_some();
            item.set_selectable(someone);
            item.set_activatable(someone);
            let row = rows.borrow().get(&key(obj)).cloned();
            if let Some(row) = row {
                row.bind(&line);
            }
        });
    }
    {
        let rows = Rc::downgrade(rows);
        factory.connect_teardown(move |_, obj| {
            if let Some(rows) = rows.upgrade() {
                rows.borrow_mut().remove(&key(obj));
            }
        });
    }
    factory
}

/// Whether a fresh read holds what the list already shows. The stored vCard
/// carries everything parsed from it, the photo key a photo kept in a file.
/// Its REV is left out: some books (Microsoft 365's Recent Contacts) stamp
/// every card anew at every sync.
fn same_contacts(shown: &[ContactDetails], read: &[ContactDetails]) -> bool {
    fn card(c: &ContactDetails) -> impl Iterator<Item = &str> {
        c.raw_vcard.lines().filter(|l| !l.starts_with("REV:") && !l.starts_with("REV;"))
    }
    shown.len() == read.len()
        && shown.iter().zip(read).all(|(a, b)| {
            a.book_uid == b.book_uid
                && a.book_name == b.book_name
                && a.eds_uid == b.eds_uid
                && a.name == b.name
                && a.photo_key == b.photo_key
                && card(a).eq(card(b))
        })
}

/// The contact's avatar: their GNOME Contacts photo when they have one, the
/// usual initials disc when not.
fn avatar_for(c: &ContactDetails, size: i32) -> adw::Avatar {
    // The list's thumbnail does for a small face, without reading the photo.
    if size <= 48 {
        if let Some(Some(texture)) = crate::avatar::photo_thumbnail(line_photo_key(c)) {
            let avatar = avatar_with(c, size, None);
            avatar.set_custom_image(Some(&texture));
            return avatar;
        }
    }
    avatar_with(c, size, c.photo_bytes().as_deref())
}

/// [`avatar_for`] with the photo already in hand.
fn avatar_with(c: &ContactDetails, size: i32, photo: Option<&[u8]>) -> adw::Avatar {
    let avatar = adw::Avatar::new(size, Some(&c.name), !c.is_group());
    if c.is_group() {
        avatar.set_icon_name(Some("system-users-symbolic"));
    }
    if let Some(photo) = photo {
        let bytes = gtk::glib::Bytes::from(photo);
        if let Ok(texture) = gtk::gdk::Texture::from_bytes(&bytes) {
            avatar.set_custom_image(Some(&texture));
        }
    }
    avatar
}

/// A titled boxed list appended to the detail column; returns the list.
fn group(detail: &gtk::Box, title: &str) -> gtk::ListBox {
    let heading = gtk::Label::new(Some(title));
    heading.add_css_class("heading");
    heading.set_halign(gtk::Align::Start);
    heading.set_margin_top(20);
    heading.set_margin_bottom(6);
    detail.append(&heading);
    let list = gtk::ListBox::new();
    list.add_css_class("boxed-list");
    list.set_selection_mode(gtk::SelectionMode::None);
    detail.append(&list);
    list
}

/// A value row with its (optional) "Home"/"Work" label as the subtitle.
fn labeled_row(l: &Labeled) -> adw::ActionRow {
    let row = adw::ActionRow::new();
    row.set_title(&gtk::glib::markup_escape_text(&l.value));
    if !l.label.is_empty() {
        row.set_subtitle(&gtk::glib::markup_escape_text(&l.label));
    }
    row
}

fn flat_button(icon: &str, tooltip: &str) -> gtk::Button {
    let b = gtk::Button::from_icon_name(icon);
    b.set_tooltip_text(Some(tooltip));
    b.set_valign(gtk::Align::Center);
    b.add_css_class("flat");
    b
}

/// A suffix button that puts `text` on the clipboard, confirming by briefly
/// swapping its icon for a checkmark.
fn copy_button(text: &str) -> gtk::Button {
    let b = flat_button("edit-copy-symbolic", "Copy");
    let text = text.to_string();
    b.connect_clicked(move |b| {
        b.clipboard().set_text(&text);
        b.set_icon_name("verified-checkmark-symbolic");
        let b = b.clone();
        gtk::glib::timeout_add_local_once(std::time::Duration::from_millis(1200), move || {
            b.set_icon_name("edit-copy-symbolic");
        });
    });
    b
}

#[cfg(test)]
mod tests {
    use super::same_contacts;
    use crate::contacts::ContactDetails;

    fn contact(vcard: &str) -> ContactDetails {
        ContactDetails {
            name: "Ann".into(),
            book_uid: "book".into(),
            eds_uid: "ann".into(),
            raw_vcard: vcard.into(),
            ..ContactDetails::default()
        }
    }

    /// A sync that only stamps the cards anew leaves the list as it is; any
    /// real change, a new photo among them, has it listed again (#477).
    #[test]
    fn a_reread_that_only_moves_rev_is_the_same_list() {
        let card = |rev: &str, tel: &str| format!("BEGIN:VCARD\r\nFN:Ann\r\nREV:{rev}\r\nTEL:{tel}\r\nEND:VCARD");
        let shown = vec![contact(&card("2026-10-18T12:23:02Z", "1"))];
        assert!(same_contacts(&shown, &[contact(&card("2026-10-18T12:25:15Z", "1"))]));
        assert!(!same_contacts(&shown, &[contact(&card("2026-10-18T12:25:15Z", "2"))]));
        let mut photo = contact(&card("2026-10-18T12:23:02Z", "1"));
        photo.photo_key = 7;
        assert!(!same_contacts(&shown, &[photo]));
        assert!(!same_contacts(&shown, &[]));
    }
}
