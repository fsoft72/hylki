//! Where Hylki finds contacts, for Settings → Contacts: the Hylki book, the
//! Evolution Data Server books on this computer, the GNOME Online Accounts
//! that keep contacts, CardDAV accounts and LDAP directories, each one row,
//! grouped by the account its books belong to.
//!
//! Each can be switched off for Hylki alone. The switch is Hylki's own,
//! kept in `contacts.toml`, and leaves GNOME and Evolution as they are: a
//! book switched off is left out of Contacts, the composer's suggestions,
//! avatars, the filters and syncing. An account switched off takes all its
//! books with it, and the books of one switched on can be turned off one by
//! one.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::i18n::{i18n, i18n_f, ni18n_f};

/// The ID of the Hylki book in the switches, and its book UID.
pub const HYLKI: &str = crate::local_contacts::LOCAL_BOOK_UID;
/// The ID that stands for the EDS books on this computer, which no source
/// of their own groups.
pub const THIS_COMPUTER: &str = "eds-local";

#[derive(Debug, Default, Serialize, Deserialize)]
struct SwitchFile {
    /// The sources and books switched off for Hylki, by ID.
    #[serde(default)]
    off: Vec<String>,
    /// The names given to address books in Hylki, by book UID, in place of
    /// the book's own.
    #[serde(default)]
    names: std::collections::BTreeMap<String, String>,
}

fn read_file() -> SwitchFile {
    switch_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|text| toml::from_str::<SwitchFile>(&text).ok())
        .unwrap_or_default()
}

fn write_file(file: &SwitchFile) -> Result<(), String> {
    let path = switch_path().ok_or("no configuration folder")?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let text = toml::to_string_pretty(file).map_err(|e| e.to_string())?;
    crate::config::write_private_file(&path, &text).map_err(|e| e.to_string())
}

/// The names given to address books in Hylki, by book UID.
pub fn names() -> HashMap<String, String> {
    read_file().names.into_iter().collect()
}

/// Give an address book a name of its own in Hylki, or (`None`, or a
/// blank one) go back to the book's own name. GNOME and Evolution keep
/// theirs.
pub fn set_name(uid: &str, name: Option<&str>) -> Result<(), String> {
    let mut file = read_file();
    match name.map(str::trim).filter(|n| !n.is_empty()) {
        Some(name) => file.names.insert(uid.to_string(), name.to_string()),
        None => file.names.remove(uid),
    };
    write_file(&file)
}

fn switch_path() -> Option<PathBuf> {
    Some(crate::config::config_base()?.join("hylki").join("contacts.toml"))
}

/// The IDs switched off for Hylki: sources (an account, a directory, the
/// Hylki book, [`THIS_COMPUTER`]) and single books.
pub fn off() -> HashSet<String> {
    read_file().off.into_iter().collect()
}

/// Switch a source or a book on or off for Hylki.
pub fn set_off(id: &str, is_off: bool) -> Result<(), String> {
    let mut ids = off();
    if is_off {
        ids.insert(id.to_string());
    } else {
        ids.remove(id);
    }
    let mut list: Vec<String> = ids.into_iter().collect();
    list.sort();
    let mut file = read_file();
    file.off = list;
    write_file(&file)
}

/// Whether the Hylki book is used.
pub fn hylki_used() -> bool {
    !off().contains(HYLKI)
}

/// The source an EDS book is grouped under: the collection (account) it
/// belongs to, [`THIS_COMPUTER`] for a local book, or the book itself.
pub fn group_of(uid: &str, sources: &HashMap<String, String>) -> String {
    let mut current = uid.to_string();
    for _ in 0..4 {
        let Some(data) = sources.get(&current) else { break };
        let Some(kf) = crate::platform::keyfile(data) else { break };
        if kf.has_group("Collection") {
            return current;
        }
        let parent = kf.string("Data Source", "Parent").map(|s| s.to_string()).unwrap_or_default();
        if parent.is_empty() || !sources.contains_key(&parent) {
            break;
        }
        current = parent;
    }
    let local = sources
        .get(uid)
        .and_then(|d| crate::platform::keyfile_value(d, "Address Book", "BackendName"))
        .is_some_and(|b| b == "local");
    if local { THIS_COMPUTER.to_string() } else { uid.to_string() }
}

/// Whether an EDS book is used by Hylki: neither it nor its account is
/// switched off. `sources` is the registry's UID → key file.
pub fn book_used(uid: &str, sources: &HashMap<String, String>, off: &HashSet<String>) -> bool {
    !off.contains(uid) && !off.contains(&group_of(uid, sources))
}

/// What kind of source a row is.
#[derive(Debug, Clone)]
pub enum Kind {
    Hylki,
    ThisComputer,
    /// An account in GNOME Online Accounts: changed in GNOME Settings.
    Online,
    /// A CardDAV collection made in Hylki or Evolution.
    CardDav(crate::carddav::Account),
    Ldap(crate::directory::Directory),
    /// Any other EDS account or book (Evolution's Exchange, a lone CardDAV
    /// book): shown and switched, changed in the app that made it.
    Other,
}

/// One address book of a source.
#[derive(Debug, Clone)]
pub struct Book {
    pub uid: String,
    /// The book's own name; [`Book::shown`] is the one Hylki uses.
    pub name: String,
    /// The name given to it in Hylki, if any.
    pub given: Option<String>,
    /// Switched on for Hylki (its own switch; the source's is apart).
    pub on: bool,
}

/// One row of Settings → Contacts.
#[derive(Debug, Clone)]
pub struct Source {
    /// The ID its switch is kept under.
    pub id: String,
    pub kind: Kind,
    /// The name it is listed by: the one given to it in Hylki, else its own.
    pub title: String,
    /// Its own name, which the given one stands in for.
    pub own_title: String,
    /// The name given to it in Hylki, if any.
    pub given: Option<String>,
    /// The second line: what it is and holds.
    pub detail: String,
    /// The brand mark, or an icon name when there is none.
    pub mark: &'static str,
    /// Where it is set up: "Hylki", "GOA" or "Evolution".
    pub origin: Origin,
    pub books: Vec<Book>,
    /// The source's switch.
    pub on: bool,
    /// Why the switch cannot be turned on here, when it cannot.
    pub paused: Option<String>,
    /// A CardDAV account's sign-in, for its row.
    pub status: Option<crate::carddav::Status>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    Hylki,
    Online,
    Evolution,
}

/// The mark of a GNOME Online Accounts provider.
fn online_mark(provider_type: &str, host: &str) -> &'static str {
    match provider_type {
        "google" => "gmail",
        "ms_graph" | "exchange" | "windows_live" => "outlook",
        "owncloud" => "nextcloud",
        // GOA not answering: the books' server says who it is.
        _ if host.ends_with("googleapis.com") => "gmail",
        _ if host.contains("icloud.com") => "icloud",
        _ => "x-office-address-book-symbolic",
    }
}

/// The mark of a CardDAV account, by its server.
fn carddav_mark(url: &str) -> &'static str {
    let host = crate::carddav::parse_url(url).map(|u| u.host.to_ascii_lowercase()).unwrap_or_default();
    if host.ends_with("icloud.com") {
        "icloud"
    } else if host.ends_with("fastmail.com") {
        "fastmail"
    } else if host.ends_with("yahoo.com") {
        "yahoo"
    } else {
        "folder-remote-symbolic"
    }
}

/// "3 address books", or the books' names when there are few.
fn books_line(books: &[Book]) -> String {
    match books.len() {
        0 => String::new(),
        1..=3 => books.iter().map(|b| b.name.as_str()).collect::<Vec<_>>().join(", "),
        n => ni18n_f("{n} address book", "{n} address books", n as u32, &[("n", &n.to_string())]),
    }
}

/// Every source Hylki can read contacts from, the Hylki book first, then
/// this computer's books, then the accounts by name, and whether Evolution
/// Data Server answered. Blocking: it asks EDS, GNOME Online Accounts and
/// the keyring. Without EDS, only the Hylki book.
pub fn list_with_eds() -> (Vec<Source>, bool) {
    let sources = list();
    let eds = crate::contacts::registry_sources().is_some();
    (sources, eds)
}

impl Book {
    /// The name Hylki uses: the one given to it, else its own.
    pub fn shown(&self) -> &str {
        self.given.as_deref().unwrap_or(if self.name.is_empty() { &self.uid } else { &self.name })
    }
}

/// [`list_with_eds`] with the names given in Hylki to the accounts and
/// books in place of their own.
pub fn list() -> Vec<Source> {
    let names = names();
    let mut sources = list_own();
    for s in &mut sources {
        s.own_title = s.title.clone();
        s.given = names.get(&s.id).cloned();
        if let Some(given) = &s.given {
            s.title = given.clone();
        }
        for b in &mut s.books {
            b.given = names.get(&b.uid).cloned();
        }
    }
    sources
}

/// The sources as EDS and GNOME Online Accounts name them.
fn list_own() -> Vec<Source> {
    let off = off();
    let count = crate::local_contacts::count();
    let mut out = vec![Source {
                own_title: String::new(),
                given: None,
        id: HYLKI.to_string(),
        kind: Kind::Hylki,
        title: i18n("Hylki Address Book"),
        detail: ni18n_f("{n} contact, kept by Hylki", "{n} contacts, kept by Hylki", count as u32, &[("n", &count.to_string())]),
        mark: "x-office-address-book-symbolic",
        origin: Origin::Hylki,
        books: Vec::new(),
        on: !off.contains(HYLKI),
        paused: None,
        status: None,
    }];
    let Some(registry) = crate::contacts::registry_sources() else { return out };
    let sources: HashMap<String, String> = registry.iter().map(|(_, uid, data)| (uid.clone(), data.clone())).collect();
    let online = crate::goa::contacts_accounts();
    let carddav: HashMap<String, crate::carddav::Account> =
        crate::carddav::list().unwrap_or_default().into_iter().map(|a| (a.uid.clone(), a)).collect();

    // The address books by the source they are grouped under, LDAP aside.
    let mut groups: HashMap<String, Vec<(Book, bool)>> = HashMap::new();
    for (uid, data) in &sources {
        let Some(kf) = crate::platform::keyfile(data) else { continue };
        let Ok(backend) = kf.string("Address Book", "BackendName") else { continue };
        if backend == "ldap" {
            continue;
        }
        let name = kf.string("Data Source", "DisplayName").map(|s| s.to_string()).unwrap_or_default();
        let eds_on = kf.boolean("Data Source", "Enabled").unwrap_or(true);
        let book = Book { uid: uid.clone(), name, given: None, on: !off.contains(uid) };
        groups.entry(group_of(uid, &sources)).or_default().push((book, eds_on));
    }
    // A CardDAV account still looking for its books is listed all the same.
    for uid in carddav.keys() {
        groups.entry(uid.clone()).or_default();
    }

    let mut accounts = Vec::new();
    for (id, mut books) in groups {
        books.sort_by_key(|(b, _)| b.name.to_lowercase());
        let all_eds_off = !books.is_empty() && books.iter().all(|(_, eds_on)| !eds_on);
        let books: Vec<Book> = books.into_iter().map(|(b, _)| b).collect();
        let on = !off.contains(&id);
        if id == THIS_COMPUTER {
            out.push(Source {
                own_title: String::new(),
                given: None,
                id,
                kind: Kind::ThisComputer,
                title: i18n("On This Computer"),
                detail: i18n_f("{books} · kept by GNOME Contacts and Evolution", &[("books", &books_line(&books))]),
                mark: "computer-symbolic",
                origin: Origin::Evolution,
                books,
                on,
                paused: None,
                status: None,
            });
            continue;
        }
        let data = sources.get(&id).cloned().unwrap_or_default();
        let kf = crate::platform::keyfile(&data);
        let get = |group: &str, key: &str| {
            kf.as_ref().and_then(|k| k.string(group, key).ok()).map(|s| s.to_string()).unwrap_or_default()
        };
        let display = get("Data Source", "DisplayName");
        let goa_id = get("GNOME Online Accounts", "AccountId");
        if !goa_id.is_empty() {
            let goa = online.get(&goa_id);
            let host = books
                .iter()
                .find_map(|b| sources.get(&b.uid).and_then(|d| crate::platform::keyfile_value(d, "Authentication", "Host")))
                .unwrap_or_default();
            let mark = online_mark(goa.map_or("", |g| g.provider_type.as_str()), &host);
            // GNOME names an iCloud account by how it connects (WebDAV).
            let provider = if mark == "icloud" {
                "iCloud".to_string()
            } else {
                goa.map(|g| g.provider_name.clone()).filter(|p| !p.is_empty()).unwrap_or_else(|| i18n("Online account"))
            };
            // An address over the provider's name ("Microsoft 365").
            let identity = [goa.map(|g| g.identity.clone()).unwrap_or_default(), get("Collection", "Identity"), display]
                .into_iter()
                .filter(|i| !i.is_empty())
                .find(|i| i.contains('@'))
                .or_else(|| goa.map(|g| g.identity.clone()).filter(|i| !i.is_empty()))
                .unwrap_or_default();
            let paused = (goa.is_some_and(|g| g.contacts_disabled) || all_eds_off)
                .then(|| i18n("Contacts is switched off for this account in GNOME Online Accounts."));
            accounts.push(Source {
                own_title: String::new(),
                given: None,
                detail: format!("{provider} · {}", books_line(&books)),
                mark,
                id,
                kind: Kind::Online,
                title: identity,
                origin: Origin::Online,
                on: on && paused.is_none(),
                books,
                paused,
                status: None,
            });
            continue;
        }
        if let Some(account) = carddav.get(&id) {
            let status = crate::carddav::status(account);
            let detail = match status.problem() {
                Some(problem) if account.enabled => problem,
                _ if books.is_empty() => i18n("Looking for address books…"),
                _ => format!("CardDAV · {}", books_line(&books)),
            };
            let origin = if is_hylki_made(&data) { Origin::Hylki } else { Origin::Evolution };
            accounts.push(Source {
                own_title: String::new(),
                given: None,
                title: account.title(),
                detail,
                mark: carddav_mark(&account.url),
                kind: Kind::CardDav(account.clone()),
                origin,
                on: on && account.enabled,
                books,
                paused: None,
                status: Some(status),
                id,
            });
            continue;
        }
        let title = if display.is_empty() { books.first().map(|b| b.name.clone()).unwrap_or_default() } else { display };
        accounts.push(Source {
                own_title: String::new(),
                given: None,
            title,
            detail: books_line(&books),
            mark: "x-office-address-book-symbolic",
            kind: Kind::Other,
            origin: Origin::Evolution,
            on: on && !all_eds_off,
            paused: all_eds_off.then(|| i18n("This address book is switched off in Evolution.")),
            books,
            status: None,
            id,
        });
    }
    for dir in crate::directory::list().unwrap_or_default() {
        let failed = if dir.enabled { crate::directory::last_error(&dir.uid) } else { None };
        let mut detail = format!("LDAP · {}", dir.host);
        if !dir.base_dn.is_empty() {
            detail.push_str(&format!(" · {}", dir.base_dn));
        }
        accounts.push(Source {
                own_title: String::new(),
                given: None,
            id: dir.uid.clone(),
            title: dir.title(),
            detail: failed.unwrap_or(detail),
            mark: "network-server-symbolic",
            origin: if is_hylki_made_ldap(&dir) { Origin::Hylki } else { Origin::Evolution },
            on: !off.contains(&dir.uid) && dir.enabled,
            kind: Kind::Ldap(dir),
            books: Vec::new(),
            paused: None,
            status: None,
        });
    }
    // By name, and two of one name (an address used for iCloud and for
    // Microsoft 365) always in the same order: the books are read from a
    // map, whose order changes from one read to the next.
    accounts.sort_by(|a, b| a.title.to_lowercase().cmp(&b.title.to_lowercase()).then_with(|| a.id.cmp(&b.id)));
    out.extend(accounts);
    out
}

/// Whether Hylki made this collection: Evolution's carry keys Hylki never
/// writes, and nothing else tells them apart. Hylki's ask for address books
/// only, with no calendar or mail address.
fn is_hylki_made(data: &str) -> bool {
    crate::platform::keyfile(data).is_some_and(|kf| {
        kf.boolean("Collection", "CalendarEnabled").ok() == Some(false)
            && kf.boolean("Collection", "MailEnabled").ok() == Some(false)
    })
}

/// A directory Hylki set up keeps its password in Hylki's keyring entry.
fn is_hylki_made_ldap(dir: &crate::directory::Directory) -> bool {
    crate::directory::has_password(dir)
}

/// Switch a source on or off for Hylki. Turning on one EDS itself has off
/// (a CardDAV account or directory switched off before this page kept its
/// own switch) switches it on there too. Blocking.
pub fn switch_source(source: &Source, on: bool) -> Result<(), String> {
    set_off(&source.id, !on)?;
    if on {
        match &source.kind {
            Kind::CardDav(a) if !a.enabled => crate::carddav::set_enabled(a, true)?,
            Kind::Ldap(d) if !d.enabled => crate::directory::set_enabled(d, true)?,
            _ => {}
        }
    }
    crate::directory::list_changed();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{book_used, group_of, THIS_COMPUTER};
    use std::collections::{HashMap, HashSet};

    fn sources() -> HashMap<String, String> {
        HashMap::from([
            ("goa1".to_string(), "[Data Source]\nDisplayName=Google\n\n[Collection]\nBackendName=google\n".to_string()),
            ("b1".to_string(), "[Data Source]\nParent=goa1\n\n[Address Book]\nBackendName=carddav\n".to_string()),
            ("b2".to_string(), "[Data Source]\nParent=goa1\n\n[Address Book]\nBackendName=carddav\n".to_string()),
            ("system-address-book".to_string(), "[Data Source]\nParent=local-stub\n\n[Address Book]\nBackendName=local\n".to_string()),
            ("lone".to_string(), "[Data Source]\nParent=webdav-stub\n\n[Address Book]\nBackendName=carddav\n".to_string()),
        ])
    }

    #[test]
    fn books_are_grouped_under_their_account() {
        let s = sources();
        assert_eq!(group_of("b1", &s), "goa1");
        assert_eq!(group_of("system-address-book", &s), THIS_COMPUTER);
        assert_eq!(group_of("lone", &s), "lone");
    }

    #[test]
    fn an_account_switched_off_takes_its_books_with_it() {
        let s = sources();
        let off: HashSet<String> = ["goa1".to_string()].into();
        assert!(!book_used("b1", &s, &off) && !book_used("b2", &s, &off));
        assert!(book_used("lone", &s, &off));
        let off: HashSet<String> = ["b2".to_string(), THIS_COMPUTER.to_string()].into();
        assert!(book_used("b1", &s, &off));
        assert!(!book_used("b2", &s, &off));
        assert!(!book_used("system-address-book", &s, &off));
    }

    /// The switches against a real server, on the sealed bus of the EDS rig
    /// with a keyring of its own: `CARDDAV_LIVE` as for the CardDAV tests,
    /// for a server with a contact on it.
    #[test]
    #[ignore]
    fn live_switches() {
        let Ok(spec) = std::env::var("CARDDAV_LIVE") else { return };
        let mut f = spec.splitn(3, '|').map(str::to_string);
        let (url, user, pw) = (f.next().unwrap(), f.next().unwrap(), f.next().unwrap());
        let mut a = crate::carddav::Account::empty();
        a.url = url;
        a.user = user;
        let uid = crate::carddav::save(&a, Some(&pw)).unwrap();
        let got = crate::carddav::await_books(&uid, std::time::Duration::from_secs(30), true).unwrap();
        crate::carddav::sync_books(true);
        std::thread::sleep(std::time::Duration::from_secs(2));
        let from_server = || {
            crate::contacts::read_contact_details().into_iter().filter(|c| c.book_name.contains(&got.books[0])).count()
        };
        let shown = from_server();
        println!("contacts from {}: {shown}", got.books[0]);
        assert!(shown > 0);
        let row = super::list().into_iter().find(|s| s.id == uid).expect("the account has a row");
        let book = row.books.iter().find(|b| b.name == got.books[0]).unwrap().uid.clone();
        super::switch_source(&row, false).unwrap();
        assert_eq!(from_server(), 0, "account off");
        super::switch_source(&row, true).unwrap();
        super::set_off(&book, true).unwrap();
        assert_eq!(from_server(), 0, "book off");
        super::set_off(&book, false).unwrap();
        assert_eq!(from_server(), shown, "both on");
        crate::carddav::remove(&got).unwrap();
    }
}
