//! Settings → Contacts: every place Hylki finds contacts, laid out as Mail
//! Accounts is. One row per source (the Hylki book, this computer's books,
//! each GNOME Online Account that keeps contacts, CardDAV accounts and LDAP
//! directories), each with a switch that uses it in Hylki or not, and a
//! page of its own: its address books, its sign-in, or the Hylki book's
//! import and export. **Add Address Book…** sets up CardDAV accounts and
//! LDAP directories, and says where Google and Microsoft 365 are added.

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use relm4::prelude::*;

use crate::carddav::{self, Account, Status};
use crate::contact_sources::{self as sources, Kind, Origin, Source};
use crate::directory::{self, Directory, Security};
use crate::i18n::{i18n, i18n_f, i18n_noop, ni18n_f};

/// What the editor page shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Editing {
    Source(usize),
    Add,
}

/// What can be added from **Add Address Book…**.
#[derive(Debug, Clone, Copy)]
enum AddWhat {
    /// A CardDAV account; `server` is filled in for a known service.
    CardDav { server: Option<&'static str>, hint: &'static str },
    Ldap,
    /// Added in GNOME Online Accounts: Hylki cannot sign in to it itself.
    Online,
}

struct AddKind {
    label: &'static str,
    mark: &'static str,
    what: AddWhat,
}

const APP_PW: &str = i18n_noop("Use an app-specific password, not your usual one.");

const ADD_KINDS: &[AddKind] = &[
    AddKind {
        label: i18n_noop("CardDAV Server"),
        mark: "folder-remote-symbolic",
        what: AddWhat::CardDav {
            server: None,
            hint: i18n_noop("The server's address is enough for most services; Hylki finds the address books on it. Services that sign in with two steps want an app password."),
        },
    },
    AddKind { label: "iCloud", mark: "icloud", what: AddWhat::CardDav { server: Some("https://contacts.icloud.com"), hint: APP_PW } },
    AddKind {
        label: "Nextcloud",
        mark: "nextcloud",
        what: AddWhat::CardDav {
            server: None,
            hint: i18n_noop("Your Nextcloud's address, such as cloud.example.org. With two-factor sign-in, make an app password in Nextcloud under Settings → Security."),
        },
    },
    AddKind {
        label: "Fastmail",
        mark: "fastmail",
        what: AddWhat::CardDav {
            server: Some("https://carddav.fastmail.com"),
            hint: i18n_noop("Use an app password with access to contacts, made in Fastmail under Settings → Privacy & Security."),
        },
    },
    AddKind { label: "Yahoo", mark: "yahoo", what: AddWhat::CardDav { server: Some("https://carddav.address.yahoo.com"), hint: APP_PW } },
    AddKind { label: i18n_noop("LDAP Directory"), mark: "network-server-symbolic", what: AddWhat::Ldap },
    AddKind { label: "Google", mark: "gmail", what: AddWhat::Online },
    AddKind { label: "Microsoft 365 / Outlook", mark: "outlook", what: AddWhat::Online },
];

/// A brand mark, or the icon of that name when there is none.
fn mark(id: &str, px: i32) -> gtk::Widget {
    crate::brand::mark(id, px, id)
}

pub struct ContactSources {
    sources: Vec<Source>,
    /// The books new contacts can go to, the chosen default first.
    books: Vec<crate::contacts::Book>,
    local_count: usize,
    list: gtk::ListBox,
    add_btn: gtk::Button,
    no_eds: gtk::Label,
    book_row: adw::ComboRow,
    /// Set while the book row's model is replaced, which fires its notify.
    book_guard: Rc<std::cell::Cell<bool>>,
    toasts: adw::ToastOverlay,
    nav: adw::NavigationView,
    editor_page: adw::NavigationPage,
    editor_slot: adw::Bin,
    save_btn: gtk::Button,
    remove_btn: gtk::Button,
    editing: Option<Editing>,
    /// What the editor's Save does: reads the form and sends the save;
    /// answers whether it accepted the form. `None` on a page with nothing
    /// to save.
    save_action: Rc<RefCell<Option<Rc<dyn Fn() -> bool>>>>,
}

#[derive(Debug)]
pub enum ContactSourcesInput {
    /// Ask EDS, GNOME Online Accounts and the keyring again.
    Load,
    Loaded { sources: Vec<Source>, eds: bool },
    /// The books new contacts can go to, and the Hylki book's size, from
    /// the app.
    SetBooks(Vec<crate::contacts::Book>),
    SetLocalCount(usize),
    ChangeBook(u32),
    Open(usize),
    Add,
    /// The kind picked in the add page's dropdown.
    AddKind(u32),
    Toggle { index: usize, on: bool },
    ToggleBook { book: String, on: bool },
    SaveClicked,
    CloseEditor,
    RemoveCurrent,
    ConfirmRemove(usize),
    Remove(usize),
    SaveCardDav { account: Account, password: String },
    SaveLdap { dir: Directory, password: String },
    /// A save, switch or removal went through (or not). `found` is a saved
    /// CardDAV account once EDS had looked at the server, `checked` a
    /// saved directory's first answer.
    Done {
        error: Option<String>,
        found: Option<(Account, Status)>,
        checked: Option<(String, Result<(), String>)>,
    },
    ImportLocal,
    ExportLocal,
    DeleteAllLocal,
}

#[derive(Debug)]
pub enum ContactSourcesOutput {
    /// A page is pushed over the list (or gone); `form` when it holds
    /// changes Save keeps, so leaving it asks first.
    PageOpen { open: bool, form: bool },
    /// What Hylki reads contacts from changed: read them again.
    Changed,
    ImportLocal,
    ExportLocal,
    DeleteAllLocal,
    SetContactBook(String),
    /// The page is showing: read the books new contacts can go to.
    LoadBooks,
}

#[relm4::component(pub)]
impl SimpleComponent for ContactSources {
    type Init = ();
    type Input = ContactSourcesInput;
    type Output = ContactSourcesOutput;

    view! {
        adw::Bin {
            #[wrap(Some)]
            #[name = "nav"]
            set_child = &adw::NavigationView {
                add = &adw::NavigationPage {
                    set_title: &i18n("Contacts"),
                    set_tag: Some("list"),

                    #[wrap(Some)]
                    set_child = &adw::ToolbarView {
                        #[wrap(Some)]
                        #[name = "toasts"]
                        set_content = &adw::ToastOverlay {
                            #[wrap(Some)]
                            set_child = &adw::PreferencesPage {
                                add = &adw::PreferencesGroup {
                                    set_title: &i18n("Address Books"),
                                    set_description: Some(&i18n("Where Hylki finds contacts: for Contacts, the composer's suggestions, sender pictures and the filters. Switch one off to leave it out of Hylki; GNOME and Evolution keep using it.")),
                                    #[wrap(Some)]
                                    set_header_suffix = &gtk::Box {
                                        set_orientation: gtk::Orientation::Horizontal,
                                        set_spacing: 6,
                                        set_valign: gtk::Align::Start,
                                        set_halign: gtk::Align::End,
                                        set_margin_start: 24,
                                        // One just added in GNOME Settings,
                                        // without leaving the page.
                                        gtk::Button {
                                            set_icon_name: "view-refresh-symbolic",
                                            set_tooltip_text: Some(&i18n("Look again for address books")),
                                            connect_clicked => ContactSourcesInput::Load,
                                        },
                                        #[name = "add_btn"]
                                        gtk::Button {
                                            set_label: &i18n("Add Address Book…"),
                                            connect_clicked => ContactSourcesInput::Add,
                                        },
                                    },
                                    #[name = "list"]
                                    gtk::ListBox {
                                        add_css_class: "boxed-list",
                                        set_selection_mode: gtk::SelectionMode::None,
                                        connect_row_activated[sender] => move |_, row| {
                                            sender.input(ContactSourcesInput::Open(row.index() as usize));
                                        },
                                    },
                                    #[name = "no_eds"]
                                    gtk::Label {
                                        set_label: &i18n("The address books of GNOME Contacts, CardDAV accounts and LDAP directories are kept by Evolution Data Server, which is not installed. Install the evolution-data-server package, which runs without GNOME, and open this page again."),
                                        add_css_class: "dim-label",
                                        set_wrap: true,
                                        set_xalign: 0.0,
                                        set_margin_top: 12,
                                        set_visible: false,
                                    },
                                },
                                add = &adw::PreferencesGroup {
                                    set_title: &i18n("New Contacts"),
                                    #[name = "book_row"]
                                    adw::ComboRow {
                                        set_title: &i18n("Save new contacts in"),
                                        set_subtitle: &i18n("The address book a contact made in Hylki goes to, unless another is picked when it is made."),
                                        connect_selected_notify[sender] => move |row| {
                                            sender.input(ContactSourcesInput::ChangeBook(row.selected()));
                                        },
                                    },
                                },
                            },
                        },
                    },
                },

                #[name = "editor_page"]
                add = &adw::NavigationPage {
                    set_title: &i18n("Address Book"),
                    set_tag: Some("editor"),

                    #[wrap(Some)]
                    set_child = &adw::ToolbarView {
                        add_top_bar = &adw::HeaderBar {
                            set_show_end_title_buttons: true,
                            #[name = "save_btn"]
                            pack_end = &gtk::Button {
                                set_label: &i18n("Save"),
                                add_css_class: "suggested-action",
                                connect_clicked => ContactSourcesInput::SaveClicked,
                            },
                            #[name = "remove_btn"]
                            pack_end = &gtk::Button {
                                set_label: &i18n("Remove"),
                                add_css_class: "destructive-action",
                                set_visible: false,
                                connect_clicked => ContactSourcesInput::RemoveCurrent,
                            },
                        },

                        #[wrap(Some)]
                        set_content = &gtk::ScrolledWindow {
                            set_hscrollbar_policy: gtk::PolicyType::Never,
                            #[wrap(Some)]
                            set_child = &adw::Clamp {
                                set_maximum_size: 640,
                                set_tightening_threshold: 480,
                                set_margin_top: 24,
                                set_margin_bottom: 24,
                                set_margin_start: 24,
                                set_margin_end: 24,
                                #[wrap(Some)]
                                #[name = "editor_slot"]
                                set_child = &adw::Bin {},
                            },
                        },
                    },
                },
            },
        }
    }

    fn init(_init: (), root: Self::Root, sender: ComponentSender<Self>) -> ComponentParts<Self> {
        let widgets = view_output!();
        let model = ContactSources {
            sources: Vec::new(),
            books: Vec::new(),
            local_count: 0,
            list: widgets.list.clone(),
            add_btn: widgets.add_btn.clone(),
            no_eds: widgets.no_eds.clone(),
            book_row: widgets.book_row.clone(),
            book_guard: Rc::new(std::cell::Cell::new(false)),
            toasts: widgets.toasts.clone(),
            nav: widgets.nav.clone(),
            editor_page: widgets.editor_page.clone(),
            editor_slot: widgets.editor_slot.clone(),
            save_btn: widgets.save_btn.clone(),
            remove_btn: widgets.remove_btn.clone(),
            editing: None,
            save_action: Rc::new(RefCell::new(None)),
        };
        {
            let s = sender.output_sender().clone();
            let save_action = model.save_action.clone();
            let slot = widgets.editor_slot.clone();
            widgets.nav.connect_visible_page_notify(move |nav| {
                let open = nav.visible_page().and_then(|p| p.tag()).is_some_and(|t| t == "editor");
                let form = open && save_action.borrow().is_some();
                if !open {
                    *save_action.borrow_mut() = None;
                    slot.set_child(None::<&gtk::Widget>);
                }
                let _ = s.send(ContactSourcesOutput::PageOpen { open, form });
            });
        }
        // Read each time the page is shown: an account may have been added
        // in GNOME Settings, or a sign-in gone through, since.
        {
            let (s, out) = (sender.input_sender().clone(), sender.output_sender().clone());
            root.connect_map(move |_| {
                let _ = s.send(ContactSourcesInput::Load);
                let _ = out.send(ContactSourcesOutput::LoadBooks);
            });
        }
        // HYLKI_SHOWCASE_EDIT_CONTACTS=<index>|add|add<kind> opens that
        // source's page or the add page (on a kind), for a capture (demo
        // only), once the page has read its sources.
        if let Ok(what) = std::env::var("HYLKI_SHOWCASE_EDIT_CONTACTS") {
            if std::env::var_os("HYLKI_DEMO").is_some() {
                let s = sender.input_sender().clone();
                gtk::glib::timeout_add_seconds_local_once(7, move || {
                    if let Some(kind) = what.strip_prefix("add") {
                        let _ = s.send(ContactSourcesInput::Add);
                        if let Ok(k) = kind.parse() {
                            let _ = s.send(ContactSourcesInput::AddKind(k));
                        }
                    } else {
                        let _ = s.send(ContactSourcesInput::Open(what.parse().unwrap_or(0)));
                    }
                });
            }
        }
        ComponentParts { model, widgets }
    }

    fn update(&mut self, message: Self::Input, sender: ComponentSender<Self>) {
        match message {
            ContactSourcesInput::Load => {
                let s = sender.input_sender().clone();
                std::thread::spawn(move || {
                    let (sources, eds) = sources::list_with_eds();
                    let _ = s.send(ContactSourcesInput::Loaded { sources, eds });
                });
            }
            ContactSourcesInput::Loaded { sources, eds } => {
                self.sources = sources;
                // Every kind that can be added is kept by EDS.
                self.add_btn.set_sensitive(eds);
                self.no_eds.set_visible(!eds);
                self.rebuild(&sender);
                // An open page about a source follows it, unless it holds a
                // form being typed in.
                if let Some(Editing::Source(i)) = self.editing {
                    if self.save_action.borrow().is_none() && i < self.sources.len() {
                        self.open_source(i, &sender);
                    }
                }
            }
            ContactSourcesInput::SetBooks(books) => {
                self.books = books;
                let labels: Vec<&str> = self.books.iter().map(|b| b.name.as_str()).collect();
                self.book_guard.set(true);
                self.book_row.set_model(Some(&gtk::StringList::new(&labels)));
                self.book_row.set_selected(0);
                self.book_row.set_sensitive(self.books.len() > 1);
                self.book_guard.set(false);
            }
            ContactSourcesInput::SetLocalCount(n) => self.local_count = n,
            ContactSourcesInput::ChangeBook(i) => {
                if self.book_guard.get() {
                    return;
                }
                if let Some(b) = self.books.get(i as usize) {
                    let _ = sender.output(ContactSourcesOutput::SetContactBook(b.uid.clone()));
                }
            }
            ContactSourcesInput::Open(i) => self.open_source(i, &sender),
            ContactSourcesInput::Add => self.open_add(0, &sender),
            ContactSourcesInput::AddKind(k) => {
                if self.editing == Some(Editing::Add) {
                    self.open_add(k, &sender);
                }
            }
            ContactSourcesInput::Toggle { index, on } => {
                let Some(source) = self.sources.get_mut(index) else { return };
                if source.on == on {
                    return;
                }
                source.on = on;
                let source = source.clone();
                let s = sender.input_sender().clone();
                std::thread::spawn(move || {
                    let error = sources::switch_source(&source, on).err();
                    let _ = s.send(ContactSourcesInput::Done { error, found: None, checked: None });
                });
            }
            ContactSourcesInput::ToggleBook { book, on } => {
                let s = sender.input_sender().clone();
                std::thread::spawn(move || {
                    let error = sources::set_off(&book, !on).err();
                    let _ = s.send(ContactSourcesInput::Done { error, found: None, checked: None });
                });
            }
            ContactSourcesInput::SaveClicked => {
                let action = self.save_action.borrow().clone();
                if let Some(f) = action {
                    if f() {
                        *self.save_action.borrow_mut() = None;
                        self.close_editor();
                    }
                }
            }
            ContactSourcesInput::CloseEditor => {
                *self.save_action.borrow_mut() = None;
                self.close_editor();
            }
            ContactSourcesInput::RemoveCurrent => {
                if let Some(Editing::Source(i)) = self.editing {
                    sender.input(ContactSourcesInput::ConfirmRemove(i));
                }
            }
            ContactSourcesInput::ConfirmRemove(i) => {
                let Some(source) = self.sources.get(i) else { return };
                let body = match source.kind {
                    Kind::Ldap(_) => i18n("The directory is removed from Evolution Data Server, so Evolution and other apps that use it stop seeing it too. Nothing on the server changes."),
                    _ => i18n("The account and its address books are removed from this computer, so Evolution and other apps that use them stop seeing them too. Nothing on the server changes."),
                };
                let parent = relm4::main_application().active_window();
                let dialog = adw::MessageDialog::new(
                    parent.as_ref(),
                    Some(&i18n_f("Remove {name}?", &[("name", &source.title)])),
                    Some(&body),
                );
                dialog.add_response("cancel", &i18n("Cancel"));
                dialog.add_response("remove", &i18n("Remove"));
                dialog.set_response_appearance("remove", adw::ResponseAppearance::Destructive);
                dialog.set_default_response(Some("cancel"));
                dialog.set_close_response("cancel");
                let s = sender.input_sender().clone();
                dialog.connect_response(None, move |_, response| {
                    if response == "remove" {
                        let _ = s.send(ContactSourcesInput::Remove(i));
                    }
                });
                dialog.present();
            }
            ContactSourcesInput::Remove(i) => {
                let Some(source) = self.sources.get(i).cloned() else { return };
                *self.save_action.borrow_mut() = None;
                self.close_editor();
                let s = sender.input_sender().clone();
                std::thread::spawn(move || {
                    let error = match &source.kind {
                        Kind::CardDav(a) => carddav::remove(a).err(),
                        Kind::Ldap(d) => directory::remove(d).err(),
                        _ => None,
                    };
                    // Its switch goes with it.
                    let _ = sources::set_off(&source.id, false);
                    let _ = s.send(ContactSourcesInput::Done { error, found: None, checked: None });
                });
            }
            ContactSourcesInput::SaveCardDav { account, password } => {
                self.toast(&i18n_f("Looking for address books on {name}…", &[("name", &account.title())]));
                let s = sender.input_sender().clone();
                std::thread::spawn(move || {
                    let password = (!password.is_empty()).then_some(password);
                    let saved = carddav::save(&account, password.as_deref());
                    report_carddav(&s, saved, password.is_some());
                });
            }
            ContactSourcesInput::SaveLdap { dir, password } => {
                let s = sender.input_sender().clone();
                std::thread::spawn(move || {
                    let password = (!password.is_empty()).then_some(password);
                    let msg = match directory::save(&dir, password.as_deref()) {
                        Err(e) => ContactSourcesInput::Done { error: Some(e), found: None, checked: None },
                        Ok(uid) => {
                            // A new source takes a moment to appear in the
                            // registry; check it once it has.
                            let mut found = None;
                            for _ in 0..10 {
                                found = directory::list().ok().and_then(|l| l.into_iter().find(|d| d.uid == uid));
                                if found.is_some() {
                                    break;
                                }
                                std::thread::sleep(std::time::Duration::from_millis(200));
                            }
                            let _ = s.send(ContactSourcesInput::Load);
                            let checked = found.map(|d| (d.title(), check_within(d, CHECK_LIMIT)));
                            ContactSourcesInput::Done { error: None, found: None, checked }
                        }
                    };
                    let _ = s.send(msg);
                });
            }
            ContactSourcesInput::Done { error, found, checked } => {
                if let Some(e) = error {
                    self.toast(&e);
                }
                if let Some((a, status)) = found {
                    let n = a.books.len() as u32;
                    match status.problem() {
                        Some(problem) => self.toast(&i18n_f("{name}: {problem}", &[("name", &a.title()), ("problem", &problem)])),
                        None if n == 0 => self.toast(&i18n_f("No address books were found on {name} yet.", &[("name", &a.title())])),
                        None => self.toast(&ni18n_f(
                            "Found {n} address book on {name}",
                            "Found {n} address books on {name}",
                            n,
                            &[("n", &n.to_string()), ("name", &a.title())],
                        )),
                    }
                }
                match checked {
                    Some((name, Ok(()))) => self.toast(&i18n_f("Connected to {name}", &[("name", &name)])),
                    Some((name, Err(e))) => {
                        self.toast(&i18n_f("{name} did not answer: {error}", &[("name", &name), ("error", &e)]))
                    }
                    None => {}
                }
                let _ = sender.output(ContactSourcesOutput::Changed);
                sender.input(ContactSourcesInput::Load);
            }
            ContactSourcesInput::ImportLocal => {
                let _ = sender.output(ContactSourcesOutput::ImportLocal);
            }
            ContactSourcesInput::ExportLocal => {
                let _ = sender.output(ContactSourcesOutput::ExportLocal);
            }
            ContactSourcesInput::DeleteAllLocal => {
                let _ = sender.output(ContactSourcesOutput::DeleteAllLocal);
            }
        }
    }
}

/// After a CardDAV save: show the account at once, wait for EDS to look at
/// the server, and say what it found. Blocking; `signing_in` when a new
/// password was given.
fn report_carddav(s: &relm4::Sender<ContactSourcesInput>, saved: Result<String, String>, signing_in: bool) {
    let uid = match saved {
        Ok(uid) => uid,
        Err(e) => {
            let _ = s.send(ContactSourcesInput::Done { error: Some(e), found: None, checked: None });
            return;
        }
    };
    let _ = s.send(ContactSourcesInput::Load);
    let found = carddav::await_books(&uid, std::time::Duration::from_secs(30), signing_in).map(|a| {
        let status = carddav::status(&a);
        (a, status)
    });
    let _ = s.send(ContactSourcesInput::Done { error: None, found, checked: None });
}

/// How long Settings waits on a directory's first answer. EDS sets no
/// timeout of its own on the connection, and holds the book while it waits.
const CHECK_LIMIT: std::time::Duration = std::time::Duration::from_secs(30);

/// [`directory::check`], given up on after `limit`; the check itself runs on
/// to its end in the background.
fn check_within(dir: Directory, limit: std::time::Duration) -> Result<(), String> {
    let (tx, rx) = std::sync::mpsc::channel();
    let name = dir.title();
    std::thread::spawn(move || {
        let _ = tx.send(directory::check(&dir));
    });
    rx.recv_timeout(limit).unwrap_or_else(|_| {
        tracing::warn!(directory = %name, "LDAP directory check gave no answer in {}s", limit.as_secs());
        Err(i18n("The server took too long to answer."))
    })
}

impl ContactSources {
    fn toast(&self, text: &str) {
        let toast = adw::Toast::new(text);
        toast.set_timeout(6);
        self.toasts.add_toast(toast);
    }

    fn push_editor(&self) {
        if !self.nav.visible_page().and_then(|p| p.tag()).is_some_and(|t| t == "editor") {
            self.nav.push_by_tag("editor");
        }
    }

    fn close_editor(&mut self) {
        self.editing = None;
        if self.nav.visible_page().and_then(|p| p.tag()).is_some_and(|t| t == "editor") {
            self.nav.pop();
        }
    }

    /// Tell the window whether a form with changes to keep is up, which the
    /// save action decides.
    fn announce(&self, sender: &ComponentSender<Self>) {
        let form = self.save_action.borrow().is_some();
        let _ = sender.output(ContactSourcesOutput::PageOpen { open: true, form });
    }

    /// The page about one source.
    fn open_source(&mut self, index: usize, sender: &ComponentSender<Self>) {
        let Some(source) = self.sources.get(index).cloned() else { return };
        self.editing = Some(Editing::Source(index));
        *self.save_action.borrow_mut() = None;
        self.editor_page.set_title(&source.title);
        let removable = matches!(source.kind, Kind::CardDav(_) | Kind::Ldap(_));
        self.remove_btn.set_visible(removable);
        let page = gtk::Box::new(gtk::Orientation::Vertical, 24);
        page.append(&source_header(&source));
        if let Some(why) = &source.paused {
            let note = adw::PreferencesGroup::new();
            note.set_description(Some(why));
            page.append(&note);
        }
        let form: Option<(gtk::Widget, Rc<dyn Fn() -> bool>)> = match &source.kind {
            Kind::Hylki => {
                page.append(&self.hylki_group(sender));
                None
            }
            Kind::CardDav(a) => Some(carddav_form(a.clone(), None, ADD_KINDS[0].what, sender.input_sender().clone())),
            Kind::Ldap(d) => Some(ldap_form(d.clone(), sender.input_sender().clone())),
            Kind::ThisComputer => {
                page.append(&info_group(
                    &i18n("Kept by Evolution Data Server"),
                    &i18n("The address books GNOME Contacts and Evolution keep on this computer. They are changed there, and in Hylki's Contacts."),
                    &i18n("Open GNOME Contacts"),
                    crate::ui::contacts_browser::launch_gnome_contacts,
                ));
                None
            }
            Kind::Online => {
                page.append(&info_group(
                    &i18n("Set up in GNOME Online Accounts"),
                    &i18n("The account's sign-in, and whether it keeps contacts at all, are changed in GNOME Settings."),
                    &i18n("Open Online Accounts"),
                    crate::ui::accounts::open_online_accounts,
                ));
                None
            }
            Kind::Other => None,
        };
        if let Some((widget, save)) = form {
            page.append(&widget);
            *self.save_action.borrow_mut() = Some(save);
        }
        if !source.books.is_empty() {
            page.append(&books_group(&source, sender.input_sender().clone()));
        }
        self.save_btn.set_visible(self.save_action.borrow().is_some());
        self.editor_slot.set_child(Some(&page));
        self.push_editor();
        self.announce(sender);
    }

    /// The add page, on the kind at `kind` in the dropdown.
    fn open_add(&mut self, kind: u32, sender: &ComponentSender<Self>) {
        self.editing = Some(Editing::Add);
        self.editor_page.set_title(&i18n("Add Address Book"));
        self.remove_btn.set_visible(false);
        let what = ADD_KINDS.get(kind as usize).unwrap_or(&ADD_KINDS[0]).what;
        match what {
            AddWhat::CardDav { server, .. } => {
                let mut account = Account::empty();
                account.url = server.unwrap_or_default().to_string();
                self.show_carddav_form(account, kind, what, sender);
            }
            AddWhat::Ldap => {
                let page = gtk::Box::new(gtk::Orientation::Vertical, 24);
                page.append(&kind_picker(kind, sender.input_sender().clone()));
                let (form, save) = ldap_form(Directory::empty(), sender.input_sender().clone());
                page.append(&form);
                *self.save_action.borrow_mut() = Some(save);
                self.save_btn.set_visible(true);
                self.editor_slot.set_child(Some(&page));
            }
            AddWhat::Online => {
                let page = gtk::Box::new(gtk::Orientation::Vertical, 24);
                page.append(&kind_picker(kind, sender.input_sender().clone()));
                page.append(&info_group(
                    &i18n("Added in GNOME Online Accounts"),
                    &i18n("Google and Microsoft 365 sign in through the browser, which Evolution Data Server leaves to GNOME Online Accounts. Add the account there with Contacts switched on, and it is listed here."),
                    &i18n("Open Online Accounts"),
                    crate::ui::accounts::open_online_accounts,
                ));
                *self.save_action.borrow_mut() = None;
                self.save_btn.set_visible(false);
                self.editor_slot.set_child(Some(&page));
            }
        }
        self.push_editor();
        self.announce(sender);
    }

    /// The add page's CardDAV form, under the kind dropdown.
    fn show_carddav_form(&mut self, account: Account, kind: u32, what: AddWhat, sender: &ComponentSender<Self>) {
        let page = gtk::Box::new(gtk::Orientation::Vertical, 24);
        page.append(&kind_picker(kind, sender.input_sender().clone()));
        let (form, save) = carddav_form(account, Some(kind), what, sender.input_sender().clone());
        page.append(&form);
        *self.save_action.borrow_mut() = Some(save);
        self.save_btn.set_visible(true);
        self.editor_slot.set_child(Some(&page));
    }

    /// The Hylki book's own rows: its size, import, export, emptying.
    fn hylki_group(&self, sender: &ComponentSender<Self>) -> adw::PreferencesGroup {
        let group = adw::PreferencesGroup::new();
        group.set_title(&i18n("Hylki Address Book"));
        group.set_description(Some(&i18n("Contacts Hylki keeps itself, in its own data folder. They work without GNOME.")));
        let count = adw::ActionRow::new();
        count.set_title(&i18n("Contacts in the Hylki book"));
        let n = gtk::Label::new(Some(&self.local_count.to_string()));
        n.add_css_class("dim-label");
        count.add_suffix(&n);
        group.add(&count);
        let action = |title: String, subtitle: String, msg: fn() -> ContactSourcesInput, enabled: bool| {
            let row = adw::ActionRow::new();
            row.set_title(&title);
            row.set_subtitle(&subtitle);
            row.set_activatable(true);
            row.set_sensitive(enabled);
            row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
            let s = sender.input_sender().clone();
            row.connect_activated(move |_| {
                let _ = s.send(msg());
            });
            row
        };
        group.add(&action(
            i18n("Import vCard files"),
            i18n("Adds the contacts in .vcf files to the Hylki book. Importing a file again updates its contacts."),
            || ContactSourcesInput::ImportLocal,
            true,
        ));
        group.add(&action(
            i18n("Export as a vCard file"),
            i18n("Saves every contact in the Hylki book in one .vcf file, which other apps can import."),
            || ContactSourcesInput::ExportLocal,
            self.local_count > 0,
        ));
        let delete = action(
            i18n("Delete all contacts"),
            i18n("Empties the Hylki book. The other address books are not touched."),
            || ContactSourcesInput::DeleteAllLocal,
            self.local_count > 0,
        );
        delete.add_css_class("error");
        group.add(&delete);
        group
    }

    fn rebuild(&mut self, sender: &ComponentSender<Self>) {
        while let Some(child) = self.list.first_child() {
            self.list.remove(&child);
        }
        // Marks and badges each get a column, held to one width down the
        // list, as on Mail Accounts.
        let mark_widths = gtk::SizeGroup::new(gtk::SizeGroupMode::Horizontal);
        let badge_widths = gtk::SizeGroup::new(gtk::SizeGroupMode::Horizontal);
        let slot = |child: &gtk::Widget, group: &gtk::SizeGroup| {
            let slot = gtk::Box::new(gtk::Orientation::Horizontal, 0);
            slot.set_valign(gtk::Align::Center);
            child.set_halign(gtk::Align::Center);
            child.set_hexpand(true);
            slot.append(child);
            slot.set_hexpand(false);
            group.add_widget(&slot);
            slot
        };
        for (i, s) in self.sources.iter().enumerate() {
            let row = gtk::ListBoxRow::new();
            row.set_activatable(true);
            let hbox = gtk::Box::new(gtk::Orientation::Horizontal, 12);
            hbox.add_css_class("account-list-row");
            hbox.append(&slot(&mark(s.mark, 24), &mark_widths));
            let vbox = gtk::Box::new(gtk::Orientation::Vertical, 0);
            vbox.set_hexpand(true);
            vbox.set_valign(gtk::Align::Center);
            let title = gtk::Label::new(Some(&s.title));
            title.set_halign(gtk::Align::Start);
            title.set_ellipsize(gtk::pango::EllipsizeMode::End);
            title.add_css_class("account-name");
            let detail = gtk::Label::new(Some(s.paused.as_deref().unwrap_or(&s.detail)));
            detail.set_halign(gtk::Align::Start);
            detail.set_ellipsize(gtk::pango::EllipsizeMode::End);
            detail.add_css_class("account-email");
            detail.set_tooltip_text(Some(s.paused.as_deref().unwrap_or(&s.detail)));
            if s.status.as_ref().and_then(Status::problem).is_some() && s.on {
                detail.add_css_class("error");
            }
            vbox.append(&title);
            vbox.append(&detail);
            hbox.append(&vbox);
            let (label, tip) = match s.origin {
                Origin::Hylki => (i18n("Hylki"), i18n("Set up in Hylki")),
                Origin::Online => (i18n("GOA"), i18n("From GNOME Online Accounts")),
                Origin::Evolution => (i18n("Evolution"), i18n("Kept by Evolution Data Server, set up in Evolution or GNOME Contacts")),
            };
            let badge = gtk::Label::new(Some(&label));
            badge.add_css_class("account-source-badge");
            if s.origin == Origin::Online {
                badge.add_css_class("goa");
            }
            badge.set_tooltip_text(Some(&tip));
            hbox.append(&slot(badge.upcast_ref(), &badge_widths));
            let toggle = gtk::Switch::new();
            toggle.set_valign(gtk::Align::Center);
            toggle.set_active(s.on);
            toggle.set_sensitive(s.paused.is_none());
            toggle.set_tooltip_text(Some(&i18n("Use in Hylki")));
            let input = sender.input_sender().clone();
            toggle.connect_state_set(move |_, on| {
                let _ = input.send(ContactSourcesInput::Toggle { index: i, on });
                gtk::glib::Propagation::Proceed
            });
            hbox.append(&toggle);
            let next = gtk::Image::from_icon_name("go-next-symbolic");
            next.add_css_class("dim-label");
            hbox.append(&next);
            row.set_child(Some(&hbox));
            self.list.append(&row);
        }
    }
}

/// The big mark and name over a source's page, as over a mail account's.
fn source_header(source: &Source) -> gtk::Widget {
    let b = gtk::Box::new(gtk::Orientation::Vertical, 6);
    b.set_halign(gtk::Align::Center);
    b.append(&mark(source.mark, 56));
    let detail = gtk::Label::new(Some(&source.detail));
    detail.add_css_class("dim-label");
    detail.set_wrap(true);
    detail.set_justify(gtk::Justification::Center);
    b.append(&detail);
    b.upcast()
}

/// A note with a button to where the thing is changed.
fn info_group(title: &str, text: &str, button: &str, open: fn()) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::new();
    let row = adw::ActionRow::new();
    row.set_title(&gtk::glib::markup_escape_text(title));
    row.set_subtitle(&gtk::glib::markup_escape_text(text));
    row.set_subtitle_lines(4);
    let b = gtk::Button::with_label(button);
    b.set_valign(gtk::Align::Center);
    b.connect_clicked(move |_| open());
    row.add_suffix(&b);
    group.add(&row);
    group
}

/// A source's address books, each with its own switch.
fn books_group(source: &Source, sender: relm4::Sender<ContactSourcesInput>) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::new();
    group.set_title(&i18n("Address books"));
    group.set_description(Some(&i18n("Switch off an address book to leave it out of Hylki.")));
    for book in &source.books {
        let row = adw::SwitchRow::new();
        row.set_title(&gtk::glib::markup_escape_text(if book.name.is_empty() { &book.uid } else { &book.name }));
        row.set_active(book.on);
        // The account's own switch decides first.
        row.set_sensitive(source.on);
        let (s, uid) = (sender.clone(), book.uid.clone());
        row.connect_active_notify(move |r| {
            let _ = s.send(ContactSourcesInput::ToggleBook { book: uid.clone(), on: r.is_active() });
        });
        group.add(&row);
    }
    group
}

/// The add page's dropdown of what can be added, with its mark.
fn kind_picker(selected: u32, sender: relm4::Sender<ContactSourcesInput>) -> gtk::Widget {
    let b = gtk::Box::new(gtk::Orientation::Vertical, 18);
    let what = ADD_KINDS.get(selected as usize).unwrap_or(&ADD_KINDS[0]);
    let m = mark(what.mark, 56);
    m.set_halign(gtk::Align::Center);
    b.append(&m);
    let group = adw::PreferencesGroup::new();
    let row = adw::ComboRow::new();
    row.set_title(&i18n("Address book"));
    let labels: Vec<String> = ADD_KINDS.iter().map(|k| i18n(k.label)).collect();
    let refs: Vec<&str> = labels.iter().map(String::as_str).collect();
    row.set_model(Some(&gtk::StringList::new(&refs)));
    row.set_selected(selected);
    row.connect_selected_notify(move |r| {
        let _ = sender.send(ContactSourcesInput::AddKind(r.selected()));
    });
    group.add(&row);
    b.append(&group);
    b.upcast()
}

/// The CardDAV form: a new account for the add page's `kind`, or the
/// account itself; and what Save does with it.
fn carddav_form(
    account: Account,
    kind: Option<u32>,
    what: AddWhat,
    sender: relm4::Sender<ContactSourcesInput>,
) -> (gtk::Widget, Rc<dyn Fn() -> bool>) {
    let (preset, hint) = match what {
        AddWhat::CardDav { server, hint } => (server.filter(|_| kind.is_some()), hint),
        _ => (None, ""),
    };
    let new = account.uid.is_empty();
    let server = adw::PreferencesGroup::new();
    server.set_title(&i18n("Server"));
    if !hint.is_empty() && new {
        server.set_description(Some(&i18n(hint)));
    }
    let name = adw::EntryRow::new();
    name.set_title(&i18n("Name (optional)"));
    name.set_text(&account.name);
    let url = adw::EntryRow::new();
    url.set_title(&i18n("Server"));
    url.set_text(&account.url);
    url.set_input_purpose(gtk::InputPurpose::Url);
    // A known service's server is not the user's to type.
    url.set_visible(preset.is_none());
    server.add(&name);
    server.add(&url);

    let signin = adw::PreferencesGroup::new();
    signin.set_title(&i18n("Sign-in"));
    let user = adw::EntryRow::new();
    user.set_title(&i18n("User name"));
    user.set_text(&account.user);
    let pass = adw::PasswordEntryRow::new();
    pass.set_title(&i18n("Password"));
    // Asked off the main thread: the keyring may wait on its user (#466).
    if !new {
        let (a, pass) = (account.clone(), pass.clone());
        crate::config::keyring_then(
            move || carddav::has_password(&a),
            move |saved| {
                if saved {
                    pass.set_title(&i18n("Password (saved; type to replace it)"));
                }
            },
        );
    }
    signin.add(&user);
    signin.add(&pass);
    // Return in the last field saves, as the button does.
    {
        let sender = sender.clone();
        pass.connect_entry_activated(move |_| {
            let _ = sender.send(ContactSourcesInput::SaveClicked);
        });
    }
    // A new account starts at the first field it still needs.
    if new {
        let (url, user, pass) = (url.clone(), user.clone(), pass.clone());
        gtk::glib::idle_add_local_once(move || {
            if url.is_visible() && url.text().is_empty() {
                url.grab_focus();
            } else if user.text().is_empty() {
                user.grab_focus();
            } else {
                pass.grab_focus();
            }
        });
    }

    let error = gtk::Label::new(None);
    error.add_css_class("error");
    error.set_wrap(true);
    error.set_xalign(0.0);
    error.set_visible(false);

    let form = gtk::Box::new(gtk::Orientation::Vertical, 24);
    form.append(&server);
    form.append(&signin);
    form.append(&error);

    let save: Rc<dyn Fn() -> bool> = Rc::new(move || {
        let fail = |text: &str, row: &adw::EntryRow| {
            error.set_label(text);
            error.set_visible(true);
            row.grab_focus();
            false
        };
        let address = preset.map(str::to_string).unwrap_or_else(|| url.text().trim().to_string());
        if let Err(e) = carddav::parse_url(&address) {
            return fail(&e, &url);
        }
        if user.text().trim().is_empty() {
            return fail(&i18n("Enter the user name."), &user);
        }
        if new && pass.text().is_empty() {
            return fail(&i18n("Enter the password."), pass.upcast_ref());
        }
        let mut a = account.clone();
        a.name = name.text().trim().to_string();
        a.url = address;
        a.user = user.text().trim().to_string();
        let _ = sender.send(ContactSourcesInput::SaveCardDav { account: a, password: pass.text().to_string() });
        true
    });
    (form.upcast(), save)
}

const SECURITY: [Security; 3] = [Security::StartTls, Security::Ldaps, Security::None];

/// The LDAP directory form, and what Save does with it.
fn ldap_form(dir: Directory, sender: relm4::Sender<ContactSourcesInput>) -> (gtk::Widget, Rc<dyn Fn() -> bool>) {
    let server = adw::PreferencesGroup::new();
    server.set_title(&i18n("Server"));
    server.set_description(Some(&i18n("The composer looks up recipients in the directory as you type an address, and a search in Contacts looks in it too.")));
    let name = adw::EntryRow::new();
    name.set_title(&i18n("Name (optional)"));
    name.set_text(&dir.name);
    let host = adw::EntryRow::new();
    host.set_title(&i18n("Server"));
    host.set_text(&dir.host);
    host.set_input_purpose(gtk::InputPurpose::Url);
    let security = adw::ComboRow::new();
    security.set_title(&i18n("Encryption"));
    security.set_model(Some(&gtk::StringList::new(&[&i18n("StartTLS"), &i18n("TLS (LDAPS)"), &i18n("None")])));
    security.set_selected(SECURITY.iter().position(|s| *s == dir.security).unwrap_or(0) as u32);
    let port = adw::SpinRow::with_range(1.0, 65535.0, 1.0);
    port.set_title(&i18n("Port"));
    port.set_value(f64::from(dir.port));
    // Follow the encryption with the port, unless it was set to something
    // other than the usual one.
    {
        let port = port.clone();
        security.connect_selected_notify(move |row| {
            let chosen = SECURITY.get(row.selected() as usize).copied().unwrap_or(Security::StartTls);
            if SECURITY.iter().any(|s| f64::from(s.default_port()) == port.value()) {
                port.set_value(f64::from(chosen.default_port()));
            }
        });
    }
    let base = adw::EntryRow::new();
    base.set_title(&i18n("Search base, such as dc=example,dc=com"));
    base.set_text(&dir.base_dn);
    let scope = adw::ComboRow::new();
    scope.set_title(&i18n("Search"));
    scope.set_model(Some(&gtk::StringList::new(&[&i18n("Whole tree"), &i18n("First level")])));
    scope.set_selected(if dir.subtree { 0 } else { 1 });
    for w in [name.upcast_ref::<gtk::Widget>(), host.upcast_ref(), security.upcast_ref(), port.upcast_ref(), base.upcast_ref(), scope.upcast_ref()] {
        server.add(w);
    }

    let signin = adw::PreferencesGroup::new();
    signin.set_title(&i18n("Sign-in"));
    signin.set_description(Some(&i18n("Leave these empty for a directory that can be searched without signing in.")));
    let bind = adw::EntryRow::new();
    bind.set_title(&i18n("Sign in as, such as cn=jane,ou=people,dc=example,dc=com"));
    bind.set_text(&dir.bind_dn);
    let pass = adw::PasswordEntryRow::new();
    pass.set_title(&i18n("Password"));
    if !dir.uid.is_empty() {
        let (dir, pass) = (dir.clone(), pass.clone());
        crate::config::keyring_then(
            move || directory::has_password(&dir),
            move |saved| {
                if saved {
                    pass.set_title(&i18n("Password (saved; type to replace it)"));
                }
            },
        );
    }
    signin.add(&bind);
    signin.add(&pass);

    let error = gtk::Label::new(None);
    error.add_css_class("error");
    error.set_wrap(true);
    error.set_xalign(0.0);
    error.set_visible(false);

    let form = gtk::Box::new(gtk::Orientation::Vertical, 24);
    form.append(&server);
    form.append(&signin);
    form.append(&error);

    let save: Rc<dyn Fn() -> bool> = Rc::new(move || {
        // A server pasted as a URL names its encryption and port too.
        let (host_text, typed_security, typed_port) = directory::parse_server(&host.text());
        if host_text.is_empty() {
            error.set_label(&i18n("Enter the directory's server."));
            error.set_visible(true);
            host.grab_focus();
            return false;
        }
        let mut d = dir.clone();
        d.name = name.text().trim().to_string();
        d.host = host_text;
        let chosen = SECURITY.get(security.selected() as usize).copied().unwrap_or(Security::StartTls);
        d.security = typed_security.unwrap_or(chosen);
        d.port = match typed_port {
            Some(p) => p,
            // The usual port of the encryption picked, when the URL changed it.
            None if SECURITY.iter().any(|s| f64::from(s.default_port()) == port.value()) => d.security.default_port(),
            None => port.value() as u16,
        };
        d.base_dn = base.text().trim().to_string();
        d.subtree = scope.selected() == 0;
        d.bind_dn = bind.text().trim().to_string();
        let _ = sender.send(ContactSourcesInput::SaveLdap { dir: d, password: pass.text().to_string() });
        true
    });
    (form.upcast(), save)
}
