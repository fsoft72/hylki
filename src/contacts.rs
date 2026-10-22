//! GNOME Contacts integration via Evolution Data Server (EDS).
//!
//! Reading is done straight from the EDS address-book SQLite caches (covers
//! both the local address book and CardDAV books, which EDS mirrors locally).
//! Writing goes through the EDS D-Bus API so changes are tracked by EDS and
//! synced back to CardDAV servers.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime};
use crate::i18n::{i18n, ni18n_f};

/// A name + email pair from the address book.
#[derive(Debug, Clone)]
pub struct Contact {
    pub name: String,
    pub email: String,
}

/// A recipient autocomplete suggestion (from Contacts or mail history).
#[derive(Debug, Clone)]
pub struct Suggestion {
    pub name: String,
    pub email: String,
    /// True when this came from the address book.
    pub from_contacts: bool,
    /// Found in an LDAP or organisation directory: marked as such even when
    /// mail history knows the address too (#307).
    pub from_directory: bool,
    /// How often this address appears in mail history (0 for contacts-only).
    pub score: u32,
    /// One of the user's own account addresses: offered, since people do
    /// mail themselves, but ranked after everyone else so it does not sit
    /// on top of every list (it is on nearly every message received).
    pub own: bool,
    /// Set for a contact group: picking it puts in its members, and `email`
    /// is empty.
    pub group: Option<Arc<SuggestedGroup>>,
}

/// "3 people", and "in Bcc" for a group whose people go there.
pub fn group_reach(people: usize, hide_addresses: bool) -> String {
    let n = people as u32;
    let count = n.to_string();
    if hide_addresses {
        ni18n_f("{n} person, in Bcc", "{n} people, in Bcc", n, &[("n", &count)])
    } else {
        ni18n_f("{n} person", "{n} people", n, &[("n", &count)])
    }
}

/// What a group suggestion stands for.
#[derive(Debug, Clone)]
pub struct SuggestedGroup {
    pub members: Vec<Contact>,
    /// The members go in Bcc whichever field the group was picked in.
    pub hide_addresses: bool,
}

impl Suggestion {
    /// "Name <email>" for inserting into a recipient field (email alone if no name).
    pub fn display(&self) -> String {
        crate::worker::format_recipient(&self.name, &self.email)
    }

    /// Whether the suggestion matches a typed fragment (name or email).
    pub fn matches(&self, q: &str) -> bool {
        let q = q.to_lowercase();
        self.email.to_lowercase().contains(&q) || self.name.to_lowercase().contains(&q)
    }
}

/// Order the suggestions matching `typed` best first: one whose name or
/// address starts with it, then the most used, then by name. The user's own
/// addresses come after everyone else's, prefix match or not.
pub fn rank(matches: &mut [Suggestion], typed: &str) {
    let q = typed.to_lowercase();
    matches.sort_by(|a, b| {
        let pa = a.email.to_lowercase().starts_with(&q) || a.name.to_lowercase().starts_with(&q);
        let pb = b.email.to_lowercase().starts_with(&q) || b.name.to_lowercase().starts_with(&q);
        a.own
            .cmp(&b.own)
            .then(pb.cmp(&pa))
            .then(b.score.cmp(&a.score))
            .then(a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
}

/// Combined, de-duplicated recipient suggestions from Contacts and mail history
/// (addresses sent to / received from). History frequency becomes each
/// suggestion's `score` so frequently used addresses — contact or not — can
/// rank ahead. The user's `own` (name, email) account addresses are in the
/// list too, flagged and with no score, so they come last.
pub fn suggestions(own: &[(String, String)]) -> Vec<Suggestion> {
    let own_keys: HashSet<String> = own.iter().map(|(_, e)| e.to_lowercase()).collect();
    let mut map: std::collections::HashMap<String, Suggestion> = std::collections::HashMap::new();

    // The registry is asked once for both reads: this runs as a composer opens.
    let active = registry_books();
    for c in read_contacts_in(&active, false).0 {
        let key = c.email.to_lowercase();
        if own_keys.contains(&key) {
            continue;
        }
        map.entry(key).or_insert(Suggestion {
            name: c.name,
            email: c.email,
            from_contacts: true,
            from_directory: false,
            score: 0,
            own: false,
            group: None,
        });
    }

    if let Ok(cache) = crate::cache::Cache::open() {
        for (name, email, count) in cache.address_history() {
            let key = email.to_lowercase();
            if own_keys.contains(&key) {
                continue;
            }
            map.entry(key)
                .and_modify(|s| s.score = s.score.max(count))
                .or_insert(Suggestion {
                    name,
                    email,
                    from_contacts: false,
                    from_directory: false,
                    score: count,
                    own: false,
                    group: None,
                });
        }
    }

    for (name, email) in own {
        let key = email.to_lowercase();
        if key.is_empty() || !key.contains('@') {
            continue;
        }
        map.entry(key).or_insert(Suggestion {
            name: if name.trim().is_empty() { email.clone() } else { name.clone() },
            email: email.clone(),
            from_contacts: false,
            from_directory: false,
            score: 0,
            own: true,
            group: None,
        });
    }

    let mut out: Vec<Suggestion> = map.into_values().collect();
    out.extend(group_suggestions(read_groups_in(&active)));
    // Default order: most-used first, then name (the field filter re-ranks live).
    out.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then(a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    out
}

/// The groups that have someone to send to, as suggestions; the same group
/// in two books is offered once.
fn group_suggestions(groups: Vec<ContactDetails>) -> Vec<Suggestion> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for g in groups {
        let members: Vec<Contact> =
            g.reachable().map(|m| Contact { name: m.name.clone(), email: m.email.clone() }).collect();
        let mut key: Vec<String> = members.iter().map(|m| m.email.to_lowercase()).collect();
        key.sort();
        key.push(g.name.to_lowercase());
        if members.is_empty() || !seen.insert(key) {
            continue;
        }
        out.push(Suggestion {
            name: g.name,
            email: String::new(),
            from_contacts: true,
            from_directory: false,
            score: 0,
            own: false,
            group: Some(Arc::new(SuggestedGroup { members, hide_addresses: g.hide_addresses })),
        });
    }
    out
}

/// A writable address book the user can add contacts to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Book {
    /// EDS source UID (what `OpenAddressBook` expects).
    pub uid: String,
    pub name: String,
    /// How a new group is written to this book; `None` where the server
    /// keeps no groups (Google stores them as odd people, Microsoft 365
    /// refuses them).
    pub groups: Option<GroupFormat>,
}

// Inside a Flatpak sandbox `dirs::{data,cache,config}_dir()` are redirected into
// ~/.var/app/<app-id>/, but EDS's books live in the *host's* real XDG dirs (made
// visible by the `--filesystem=xdg-{data,cache,config}/evolution:ro` grants). So
// under Flatpak resolve them from the real home instead of the redirected XDG env.
fn host_xdg(sandbox: fn() -> Option<PathBuf>, subdir: &str) -> Option<PathBuf> {
    if crate::platform::is_flatpak() {
        dirs::home_dir().map(|h| h.join(subdir))
    } else {
        sandbox()
    }
}

fn data_dir() -> Option<PathBuf> {
    host_xdg(dirs::data_dir, ".local/share").map(|d| d.join("evolution/addressbook"))
}

fn cache_dir() -> Option<PathBuf> {
    host_xdg(dirs::cache_dir, ".cache").map(|d| d.join("evolution/addressbook"))
}

fn config_dir() -> Option<PathBuf> {
    host_xdg(dirs::config_dir, ".config").map(|d| d.join("evolution"))
}

/// Read every address email from the local + cached (CardDAV) EDS books.
pub fn read_contacts() -> Vec<Contact> {
    read_contacts_from(false).0
}

/// Like [`read_contacts`], but only the contacts someone saved: the books
/// that fill themselves from mail ("Collected Addresses", "Recently
/// contacted", "Other contacts") are left out, so a filter on "sender is in
/// Contacts" does not trust an address just because you once mailed it.
/// `None` when any book could not be read: a filter then cannot tell a
/// stranger from a contact whose book failed, and must match neither way.
pub fn read_saved_contacts() -> Option<Vec<Contact>> {
    let (contacts, complete) = read_contacts_from(true);
    complete.then_some(contacts)
}

/// The saved contacts' addresses, lower-cased, for the filter rules that ask
/// whether a sender is in Contacts (PR #384). The filter pass runs on the main
/// thread and a read goes to D-Bus and every book, so after the first read
/// the set is refreshed in the background once it is a minute old, and the
/// pass uses the last one. `None` inside means unknown: unreadable or empty.
#[derive(Clone, Default)]
pub struct SavedAddresses(std::sync::Arc<std::sync::Mutex<SavedAddressesState>>);

#[derive(Default)]
struct SavedAddressesState {
    read_at: Option<std::time::Instant>,
    set: Option<std::sync::Arc<HashSet<String>>>,
    refreshing: bool,
}

impl SavedAddresses {
    const FRESH_FOR: std::time::Duration = std::time::Duration::from_secs(60);

    /// The addresses for this filter pass. Only the very first call reads
    /// on the caller's thread: mail filtered before any read would never be
    /// looked at again.
    pub fn get(&self) -> Option<std::sync::Arc<HashSet<String>>> {
        let (never, stale) = {
            let st = self.0.lock().unwrap_or_else(|e| e.into_inner());
            (st.read_at.is_none(), st.read_at.is_some_and(|t| t.elapsed() > Self::FRESH_FOR))
        };
        if never {
            let set = read_saved_addresses();
            let mut st = self.0.lock().unwrap_or_else(|e| e.into_inner());
            st.read_at = Some(std::time::Instant::now());
            st.set = set;
        } else if stale {
            self.refresh();
        }
        self.0.lock().unwrap_or_else(|e| e.into_inner()).set.clone()
    }

    /// Read the addresses again in the background (after a contact changed,
    /// or a rule that needs them was added).
    pub fn refresh(&self) {
        {
            let mut st = self.0.lock().unwrap_or_else(|e| e.into_inner());
            if st.refreshing {
                return;
            }
            st.refreshing = true;
        }
        let shared = self.0.clone();
        std::thread::spawn(move || {
            let set = read_saved_addresses();
            let mut st = shared.lock().unwrap_or_else(|e| e.into_inner());
            st.read_at = Some(std::time::Instant::now());
            st.set = set;
            st.refreshing = false;
        });
    }
}

fn read_saved_addresses() -> Option<std::sync::Arc<HashSet<String>>> {
    let set: HashSet<String> =
        read_saved_contacts()?.into_iter().map(|c| c.email.trim().to_lowercase()).collect();
    (!set.is_empty()).then(|| std::sync::Arc::new(set))
}

/// Whether a book fills itself from mail rather than being kept by hand.
/// Judged by the source UID and display name, the only trace EDS leaves.
fn is_auto_collected(uid: &str, name: &str) -> bool {
    const MARKERS: [&str; 3] = ["collected", "recently contacted", "other contacts"];
    let (uid, name) = (uid.to_lowercase(), name.to_lowercase());
    MARKERS.iter().any(|m| uid.contains(m) || name.contains(m))
}

/// Shared body of [`read_contacts`] and [`read_saved_contacts`], and
/// whether every book it wanted could be read.
fn read_contacts_from(saved_only: bool) -> (Vec<Contact>, bool) {
    read_contacts_in(&registry_books(), saved_only)
}

/// [`read_contacts_from`] in the books `active` names.
fn read_contacts_in(active: &Option<HashMap<String, String>>, saved_only: bool) -> (Vec<Contact>, bool) {
    let mut complete = true;
    let mut out = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    // Local books: table `folder_id` + `folder_id_email_list`. Read the vCard
    // (not `full_name`, which EDS stores case-folded for search) so the display
    // name keeps its original capitalisation.
    if let Some(dir) = data_dir() {
        for db in find_dbs(&dir, "contacts.db") {
            if !db_book_wanted(&db, active, saved_only) {
                continue;
            }
            complete &= read_book_db(
                &db,
                "SELECT f.vcard, e.value FROM folder_id f \
                 JOIN folder_id_email_list e ON f.uid = e.uid",
                &mut out,
                &mut seen,
            );
        }
    }
    // Cached books (CardDAV etc.): table `ECacheObjects` + `attrlist_email_list`.
    if let Some(dir) = cache_dir() {
        for db in find_dbs(&dir, "cache.db") {
            if !db_book_wanted(&db, active, saved_only) {
                continue;
            }
            complete &= read_book_db(
                &db,
                "SELECT o.ECacheOBJ, e.value FROM ECacheObjects o \
                 JOIN attrlist_email_list e ON o.ECacheUID = e.uid",
                &mut out,
                &mut seen,
            );
        }
    }
    // The local Hylki book, last: an address EDS already has keeps EDS's name.
    complete &= add_local_contacts(&mut out, &mut seen);
    (out, complete)
}

/// Every contact group in the address books the suggestions read, with its
/// members' addresses: Evolution's lists, vCard 4 and Apple groups, and the
/// groups of the Hylki book, in the EDS books `active` names. Only the
/// groups' cards are read, not the books.
fn read_groups_in(active: &Option<HashMap<String, String>>) -> Vec<ContactDetails> {
    const WHERE: &str = "LIKE '%X-EVOLUTION-LIST%' OR {col} LIKE '%KIND:group%'";
    let mut out = Vec::new();
    let mut read = |db: &std::path::Path, table: &str, col: &str, uid_col: &str| {
        let Ok(conn) = open_book_db(db) else { return };
        let query = format!("SELECT {col} FROM {table} WHERE {col} {}", WHERE.replace("{col}", col));
        let Ok(mut stmt) = conn.prepare(&query) else { return };
        let Ok(rows) = stmt.query_map([], |r| r.get::<_, String>(0)) else { return };
        let lookup_sql = format!("SELECT {col} FROM {table} WHERE {uid_col} = ?1");
        let lookup = |uid: &str| -> Option<(String, String)> {
            let vcard: String = conn.query_row(&lookup_sql, [uid], |r| r.get(0)).ok()?;
            person_address(&vcard)
        };
        for vcard in rows.flatten() {
            let Some(mut c) = parse_vcard_fields(&vcard).filter(ContactDetails::is_group) else { continue };
            resolve_members(&mut c, &lookup);
            c.raw_vcard = vcard;
            out.push(c);
        }
    };
    if let Some(dir) = data_dir() {
        for db in find_dbs(&dir, "contacts.db") {
            if db_book_wanted(&db, active, false) {
                read(&db, "folder_id", "vcard", "uid");
            }
        }
    }
    if let Some(dir) = cache_dir() {
        for db in find_dbs(&dir, "cache.db") {
            if db_book_wanted(&db, active, false) {
                read(&db, "ECacheObjects", "ECacheOBJ", "ECacheUID");
            }
        }
    }
    if crate::local_contacts::exists() && crate::contact_sources::hylki_used() {
        match crate::local_contacts::group_vcards() {
            Ok(vcards) => {
                for vcard in vcards {
                    let Some(mut c) = parse_vcard_fields(&vcard).filter(ContactDetails::is_group) else { continue };
                    resolve_members(&mut c, |uid| {
                        crate::local_contacts::vcard_by_uid(uid).ok().flatten().as_deref().and_then(person_address)
                    });
                    c.book_uid = crate::local_contacts::LOCAL_BOOK_UID.to_string();
                    c.raw_vcard = vcard;
                    out.push(c);
                }
            }
            Err(e) => tracing::warn!("contacts: cannot read the local address book's groups: {e}"),
        }
    }
    out
}

/// Append the local book's addresses, skipping the ones already in `seen`.
/// False when the book is there but could not be read.
fn add_local_contacts(out: &mut Vec<Contact>, seen: &mut HashSet<String>) -> bool {
    if !crate::local_contacts::exists() || !crate::contact_sources::hylki_used() {
        return true;
    }
    let rows = match crate::local_contacts::emails() {
        Ok(rows) => rows,
        Err(e) => {
            tracing::warn!("contacts: cannot read the local address book: {e}");
            return false;
        }
    };
    for (name, email) in rows {
        let email = email.trim().to_string();
        if email.is_empty() || !email.contains('@') || !seen.insert(email.to_lowercase()) {
            continue;
        }
        let name = if name.trim().is_empty() { email.clone() } else { name };
        out.push(Contact { name, email });
    }
    true
}

/// Whether `book_uid` is the local Hylki book rather than an EDS one.
pub fn is_local_book(book_uid: &str) -> bool {
    book_uid == crate::local_contacts::LOCAL_BOOK_UID
}

/// The local Hylki book as a [`Book`].
pub fn local_book() -> Book {
    Book {
        uid: crate::local_contacts::LOCAL_BOOK_UID.to_string(),
        name: crate::contact_sources::names()
            .remove(crate::local_contacts::LOCAL_BOOK_UID)
            .unwrap_or_else(|| i18n("Hylki Address Book")),
        groups: Some(GroupFormat::Evolution),
    }
}

/// Parse stored local vCards into the Contacts view's entries. A local
/// vCard may come from any `.vcf` file, so its PHOTO is read only when
/// embedded, never from a `file://` path.
fn details_from_local_vcards(vcards: Vec<String>, book_name: &str) -> Vec<ContactDetails> {
    vcards
        .into_iter()
        .filter_map(|vcard| {
            let mut c = parse_vcard_fields(&vcard)?;
            c.book_uid = crate::local_contacts::LOCAL_BOOK_UID.to_string();
            c.book_name = book_name.to_string();
            c.photo = vcard_photo_with(&vcard, false);
            c.raw_vcard = vcard;
            Some(c)
        })
        .collect()
}

/// The local book's contacts, parsed (empty when it was never written).
fn local_details() -> Vec<ContactDetails> {
    if !crate::local_contacts::exists() || !crate::contact_sources::hylki_used() {
        return Vec::new();
    }
    match crate::local_contacts::list_vcards() {
        Ok(vcards) => {
            let mut book = details_from_local_vcards(vcards, &local_book().name);
            resolve_book_members(&mut book);
            book
        }
        Err(e) => {
            tracing::warn!("contacts: cannot read the local address book: {e}");
            Vec::new()
        }
    }
}

/// `book_uid_active` keyed off a book database's directory name, minus the
/// auto-collected books when `saved_only` is set.
fn db_book_wanted(db: &std::path::Path, active: &Option<HashMap<String, String>>, saved_only: bool) -> bool {
    let Some(folder) = db.parent().and_then(|p| p.file_name()) else {
        return true;
    };
    let folder = folder.to_string_lossy();
    let uid = if folder == "system" { "system-address-book" } else { folder.as_ref() };
    if !book_uid_active(uid, active) {
        return false;
    }
    let name = active.as_ref().and_then(|m| m.get(uid)).map_or("", String::as_str);
    !(saved_only && is_auto_collected(uid, name))
}

/// Photo bytes and the EDS database state they came from. The inexpensive
/// fingerprint check lets a long-running Hylki notice CardDAV synchronizations
/// without subscribing to backend-specific EDS signals.
#[derive(Clone)]
struct PhotoLocation {
    db: PathBuf,
    uid: String,
    cached_book: bool,
    /// The photo lives in the local Hylki book (`db` is unused).
    local: bool,
}

/// (path, size, mtime-nanos) per EDS database file — the cheap change signal.
type DbFingerprint = Vec<(PathBuf, u64, u128)>;

struct ContactPhotoIndex {
    photos: HashMap<String, Vec<PhotoLocation>>,
    fingerprint: DbFingerprint,
    generation: u64,
    ready: bool,
}

static CONTACT_PHOTOS: OnceLock<Arc<Mutex<ContactPhotoIndex>>> = OnceLock::new();
static PHOTO_LOAD_STARTED: OnceLock<()> = OnceLock::new();
static PHOTO_WATCH_STARTED: OnceLock<()> = OnceLock::new();
type PhotoChangeCallback = Box<dyn Fn() + Send + Sync + 'static>;
static PHOTO_CHANGE_CALLBACKS: OnceLock<Mutex<Vec<PhotoChangeCallback>>> = OnceLock::new();
const PHOTO_REFRESH_INTERVAL: Duration = Duration::from_secs(30);

fn new_photo_index() -> Arc<Mutex<ContactPhotoIndex>> {
    Arc::new(Mutex::new(ContactPhotoIndex {
        photos: HashMap::new(),
        fingerprint: Vec::new(),
        generation: 1,
        ready: false,
    }))
}

fn load_stable_photo_index() -> (HashMap<String, Vec<PhotoLocation>>, DbFingerprint) {
    let mut last = (HashMap::new(), Vec::new());
    for _ in 0..3 {
        let before = photo_db_fingerprint();
        let photos = load_contact_photo_index();
        let after = photo_db_fingerprint();
        last = (photos, after.clone());
        if before == after {
            break;
        }
    }
    last
}

fn notify_photo_change() {
    if let Some(callbacks) = PHOTO_CHANGE_CALLBACKS.get() {
        for callback in callbacks.lock().unwrap_or_else(|p| p.into_inner()).iter() {
            callback();
        }
    }
}

fn start_photo_load() {
    PHOTO_LOAD_STARTED.get_or_init(|| {
        let index = CONTACT_PHOTOS.get_or_init(new_photo_index).clone();
        let _ = std::thread::Builder::new()
            .name("eds-photo-load".into())
            .spawn(move || {
                let (photos, fingerprint) = load_stable_photo_index();
                let mut current = index.lock().unwrap_or_else(|p| p.into_inner());
                current.photos = photos;
                current.fingerprint = fingerprint;
                current.generation = current.generation.wrapping_add(1).max(1);
                current.ready = true;
                drop(current);
                notify_photo_change();
            });
    });
}

fn start_photo_watcher() {
    PHOTO_WATCH_STARTED.get_or_init(|| {
        let index = CONTACT_PHOTOS.get_or_init(new_photo_index).clone();
        let _ = std::thread::Builder::new()
            .name("eds-photo-watch".into())
            .spawn(move || loop {
                std::thread::sleep(PHOTO_REFRESH_INTERVAL);
                let fingerprint = photo_db_fingerprint();
                let changed = {
                    let current = index.lock().unwrap_or_else(|p| p.into_inner());
                    fingerprint != current.fingerprint
                };
                if !changed {
                    continue;
                }
                // Scan outside the lock. UI lookups continue using the previous
                // immutable snapshot until the replacement is ready.
                let (photos, stable_fingerprint) = load_stable_photo_index();
                let mut current = index.lock().unwrap_or_else(|p| p.into_inner());
                current.photos = photos;
                current.fingerprint = stable_fingerprint;
                current.generation = current.generation.wrapping_add(1).max(1);
                drop(current);
                notify_photo_change();
            });
    });
}

pub fn watch_photo_changes<F: Fn() + Send + Sync + 'static>(callback: F) {
    PHOTO_CHANGE_CALLBACKS
        .get_or_init(|| Mutex::new(Vec::new()))
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .push(Box::new(callback));
    start_photo_load();
    start_photo_watcher();
}

/// Current index generation. Avatar textures and negative results use this to
/// invalidate themselves after an EDS/CardDAV database change.
pub fn photo_generation() -> u64 {
    start_photo_load();
    start_photo_watcher();
    CONTACT_PHOTOS
        .get_or_init(new_photo_index)
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .generation
}

pub fn photos_ready() -> bool {
    start_photo_load();
    CONTACT_PHOTOS
        .get_or_init(new_photo_index)
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .ready
}

/// Find contact-photo candidates by email in the local EDS caches. Multiple
/// books can contain the same address; the avatar loader tries each candidate.
/// CardDAV backends such as Nextcloud keep PHOTO in the cached vCard, so this
/// does not contact the address-book server or disclose the sender address to a third
/// party.
pub fn contact_photos(email: &str) -> Vec<Arc<[u8]>> {
    let key = email.trim().to_lowercase();
    if key.is_empty() {
        return Vec::new();
    }
    start_photo_load();
    start_photo_watcher();
    let locations = CONTACT_PHOTOS
        .get_or_init(new_photo_index)
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .photos
        .get(&key)
        .cloned()
        .unwrap_or_default();
    locations
        .iter()
        .filter_map(read_photo_at)
        .map(Arc::from)
        .collect()
}

fn photo_db_fingerprint() -> DbFingerprint {
    let mut files = Vec::new();
    for db in photo_db_paths() {
        files.push(db.clone());
        for suffix in ["-journal", "-wal"] {
            files.push(PathBuf::from(format!("{}{suffix}", db.to_string_lossy())));
        }
    }
    files.extend(crate::local_contacts::db_paths());
    files.sort();
    files
        .into_iter()
        .filter_map(|path| {
            let metadata = std::fs::metadata(&path).ok()?;
            let modified = metadata
                .modified()
                .unwrap_or(SystemTime::UNIX_EPOCH)
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            Some((path, metadata.len(), modified))
        })
        .collect()
}

fn photo_db_paths() -> Vec<PathBuf> {
    let mut dbs = Vec::new();
    if let Some(dir) = data_dir() {
        dbs.extend(find_dbs(&dir, "contacts.db"));
    }
    if let Some(dir) = cache_dir() {
        dbs.extend(find_dbs(&dir, "cache.db"));
    }
    dbs.sort();
    dbs
}

fn load_contact_photo_index() -> HashMap<String, Vec<PhotoLocation>> {
    let mut photos = HashMap::new();
    // A book switched off, in EDS or for Hylki, lends no faces.
    let active = registry_books();
    if let Some(dir) = data_dir() {
        for db in find_dbs(&dir, "contacts.db").into_iter().filter(|db| db_book_wanted(db, &active, false)) {
            read_photo_locations(
                &db,
                "SELECT e.value, f.uid FROM folder_id f \
                 JOIN folder_id_email_list e ON f.uid = e.uid \
                 WHERE f.vcard LIKE '%PHOTO%'",
                false,
                &mut photos,
            );
        }
    }
    if let Some(dir) = cache_dir() {
        for db in find_dbs(&dir, "cache.db").into_iter().filter(|db| db_book_wanted(db, &active, false)) {
            read_photo_locations(
                &db,
                "SELECT e.value, o.ECacheUID FROM ECacheObjects o \
                 JOIN attrlist_email_list e ON o.ECacheUID = e.uid \
                 WHERE o.ECacheOBJ LIKE '%PHOTO%'",
                true,
                &mut photos,
            );
        }
    }
    add_local_photo_locations(&mut photos);
    tracing::debug!(count = photos.len(), "indexed EDS contact photos");
    photos
}

/// Index the local Hylki book's contacts that carry a photo.
fn add_local_photo_locations(photos: &mut HashMap<String, Vec<PhotoLocation>>) {
    if !crate::local_contacts::exists() || !crate::contact_sources::hylki_used() {
        return;
    }
    let entries = match crate::local_contacts::photo_entries() {
        Ok(entries) => entries,
        Err(e) => {
            tracing::warn!("contacts: cannot index local photos: {e}");
            return;
        }
    };
    for (email, uid) in entries {
        let location = PhotoLocation { db: PathBuf::new(), uid, cached_book: false, local: true };
        photos.entry(email).or_default().push(location);
    }
}

/// A local write happened: re-index the photos now (rather than at the next
/// 30 second check) so avatars and the Contacts list follow at once.
pub(crate) fn local_changed() {
    start_photo_load();
    start_photo_watcher();
    let index = CONTACT_PHOTOS.get_or_init(new_photo_index).clone();
    let _ = std::thread::Builder::new().name("local-photo-reload".into()).spawn(move || {
        let (photos, fingerprint) = load_stable_photo_index();
        let mut current = index.lock().unwrap_or_else(|p| p.into_inner());
        current.photos = photos;
        current.fingerprint = fingerprint;
        current.generation = current.generation.wrapping_add(1).max(1);
        drop(current);
        notify_photo_change();
    });
}

fn read_photo_locations(
    path: &std::path::Path,
    query: &str,
    cached_book: bool,
    photos: &mut HashMap<String, Vec<PhotoLocation>>,
) {
    let conn = match open_book_db(path) {
        Ok(conn) => conn,
        Err(e) => {
            tracing::warn!("contacts: cannot open {} for photos: {e}", path.display());
            return;
        }
    };
    let mut stmt = match conn.prepare(query) {
        Ok(stmt) => stmt,
        Err(e) => {
            tracing::warn!("contacts: photo query failed on {}: {e}", path.display());
            return;
        }
    };
    let rows = match stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    }) {
        Ok(rows) => rows,
        Err(e) => {
            tracing::warn!("contacts: photo rows failed on {}: {e}", path.display());
            return;
        }
    };
    for (email, uid) in rows.flatten() {
        let key = email.trim().to_lowercase();
        if !key.is_empty() {
            let location = PhotoLocation {
                db: path.to_path_buf(),
                uid,
                cached_book,
                local: false,
            };
            photos.entry(key).or_default().push(location);
        }
    }
}

fn read_photo_at(location: &PhotoLocation) -> Option<Vec<u8>> {
    if location.local {
        let vcard = crate::local_contacts::vcard_by_uid(&location.uid).ok()??;
        return vcard_photo_with(&vcard, false);
    }
    let conn = open_book_db(&location.db).ok()?;
    let query = if location.cached_book {
        "SELECT ECacheOBJ FROM ECacheObjects WHERE ECacheUID = ?1"
    } else {
        "SELECT vcard FROM folder_id WHERE uid = ?1"
    };
    let vcard: Option<String> = conn
        .query_row(query, [&location.uid], |row| row.get(0))
        .ok()?;
    vcard.as_deref().and_then(vcard_photo)
}

fn find_dbs(root: &std::path::Path, file: &str) -> Vec<PathBuf> {
    let mut dbs = Vec::new();
    if let Ok(entries) = std::fs::read_dir(root) {
        for entry in entries.flatten() {
            let db = entry.path().join(file);
            if db.is_file() {
                dbs.push(db);
            }
        }
    }
    dbs.sort();
    dbs
}

/// Extract the display name (`FN`) from a vCard, preserving its capitalisation.
/// Handles line folding (a CRLF/LF followed by a space or tab continues the
/// previous line) and the standard text escapes.
pub(crate) fn unfold_vcard(vcard: &str) -> String {
    vcard
        .replace("\r\n ", "")
        .replace("\r\n\t", "")
        .replace("\n ", "")
        .replace("\n\t", "")
}

/// Split a (unfolded) vCard content line into property (name + params) and
/// value at the first colon *outside* double quotes. A naive colon split
/// breaks on quoted parameter values — iCloud's photo lines carry
/// `X-EVOLUTION-WEBDAV-IMG-URL="https://…"` before the real value, and the
/// `https:` inside the quotes swallowed everything after it.
pub(crate) fn split_vcard_line(line: &str) -> Option<(&str, &str)> {
    let mut in_quotes = false;
    for (i, ch) in line.char_indices() {
        match ch {
            '"' => in_quotes = !in_quotes,
            ':' if !in_quotes => return Some((&line[..i], &line[i + 1..])),
            _ => {}
        }
    }
    None
}

fn vcard_display_name(vcard: &str) -> Option<String> {
    for line in unfold_vcard(vcard).lines() {
        let Some((prop, value)) = line.split_once(':') else {
            continue;
        };
        // The property name is everything before the first ';' (which begins any
        // parameters), e.g. `FN` or `FN;PID=1.1`.
        let name = prop.split(';').next().unwrap_or("").trim();
        if name.eq_ignore_ascii_case("FN") {
            let value = unescape_vcard_text(value);
            let value = value.trim();
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }
    None
}

/// Decode an embedded vCard PHOTO. Handles both Nextcloud's escaped data URI
/// form and the conventional `PHOTO;ENCODING=b` form. Remote PHOTO URLs are
/// deliberately not fetched, both for privacy and to avoid tracking.
fn vcard_photo(vcard: &str) -> Option<Vec<u8>> {
    vcard_photo_with(vcard, true)
}

/// [`vcard_photo`], optionally refusing `file://` photos. The local Hylki book
/// holds vCards from arbitrary `.vcf` files, which must not be able to point
/// at files EDS keeps.
pub(crate) fn vcard_photo_with(vcard: &str, allow_files: bool) -> Option<Vec<u8>> {
    use base64::Engine as _;

    const MAX_PHOTO_BYTES: usize = 2_000_000;
    for line in unfold_vcard(vcard).lines() {
        let Some((property, raw_value)) = split_vcard_line(line) else {
            continue;
        };
        let name = property
            .split(';')
            .next()
            .unwrap_or("")
            .rsplit('.')
            .next()
            .unwrap_or("")
            .trim();
        if !name.eq_ignore_ascii_case("PHOTO") {
            continue;
        }

        let value = raw_value.replace("\\;", ";").replace("\\,", ",");
        let property_upper = property.to_ascii_uppercase();
        let payload = if value.to_ascii_lowercase().starts_with("data:image/") {
            let Some((metadata, payload)) = value.split_once(',') else {
                continue;
            };
            if !metadata.to_ascii_lowercase().ends_with(";base64") {
                continue;
            }
            payload
        } else if property_upper.contains("ENCODING=B")
            || property_upper.contains("ENCODING=BASE64")
        {
            value.as_str()
        } else if allow_files && value.to_ascii_lowercase().starts_with("file://") {
            if let Some(bytes) = read_eds_photo_file(&value, MAX_PHOTO_BYTES) {
                return Some(bytes);
            }
            continue;
        } else {
            // Never fetch http(s) or other remote PHOTO URIs implicitly.
            continue;
        };

        let encoded: String = payload.chars().filter(|c| !c.is_ascii_whitespace()).collect();
        // Base64 expands data by roughly 4/3. Reject oversized input before
        // allocating the decoded image buffer.
        if encoded.len() > (MAX_PHOTO_BYTES * 4 / 3 + 4) {
            continue;
        }
        if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(encoded) {
            if !bytes.is_empty() && bytes.len() <= MAX_PHOTO_BYTES {
                return Some(bytes);
            }
        }
    }
    None
}

/// Read an EDS-materialized `file://` photo, but only from an address-book data
/// or cache directory. A CardDAV vCard must not be able to make Hylki read an
/// arbitrary local file.
fn read_eds_photo_file(uri: &str, max_bytes: usize) -> Option<Vec<u8>> {
    use std::io::Read;

    let (path, host) = gtk::glib::filename_from_uri(uri).ok()?;
    if host.is_some() {
        return None;
    }
    let roots: Vec<PathBuf> = [data_dir(), cache_dir()].into_iter().flatten().collect();
    let path = confine_to_roots(&path, &roots)?;

    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .ok()?
        .take(max_bytes as u64 + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    (!bytes.is_empty() && bytes.len() <= max_bytes).then_some(bytes)
}

/// Canonicalize `path` and admit it only when it lands inside one of `roots`
/// (also canonicalized) — `..` segments and symlinks are resolved before the
/// containment check, so neither can escape a root. No resolvable roots means
/// nothing is admitted.
fn confine_to_roots(path: &std::path::Path, roots: &[PathBuf]) -> Option<PathBuf> {
    let path = path.canonicalize().ok()?;
    roots
        .iter()
        .filter_map(|root| root.canonicalize().ok())
        .any(|root| path.starts_with(&root))
        .then_some(path)
}

/// Unescape a vCard TEXT value: `\n`/`\N` → newline, `\,` → `,`, `\;` → `;`,
/// `\\` → `\`.
pub(crate) fn unescape_vcard_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') | Some('N') => out.push('\n'),
                Some(other) => out.push(other),
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn open_book_db(path: &std::path::Path) -> rusqlite::Result<rusqlite::Connection> {
    use rusqlite::OpenFlags;
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI;
    // Under Flatpak the book DB lives on a read-only host mount; a WAL database
    // can't be opened read-only there (it needs to touch the -shm/-wal files), so
    // open it `immutable=1` to read the committed snapshot without any locking.
    if crate::platform::is_flatpak() {
        rusqlite::Connection::open_with_flags(
            format!("file:{}?immutable=1", path.to_string_lossy()),
            flags,
        )
    } else {
        rusqlite::Connection::open_with_flags(path, flags)
    }
}

fn read_book_db(path: &std::path::Path, query: &str, out: &mut Vec<Contact>, seen: &mut HashSet<String>) -> bool {
    let conn = match open_book_db(path) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("contacts: cannot open {}: {e}", path.display());
            return false;
        }
    };
    let mut stmt = match conn.prepare(query) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("contacts: query failed on {}: {e}", path.display());
            return false;
        }
    };
    let rows = stmt.query_map([], |row| {
        let vcard: Option<String> = row.get(0).ok();
        let email: String = row.get(1)?;
        // EDS indexes a list's members as its addresses, lower-cased and
        // whole ("ann lee <ann@x.org>"): the list is offered as itself.
        let group = vcard.as_deref().is_some_and(is_group_vcard);
        let name = vcard.as_deref().and_then(vcard_display_name).unwrap_or_default();
        Ok((name, email, group))
    });
    let Ok(rows) = rows else { return false };
    for (name, email, _) in rows.flatten().filter(|(_, _, group)| !group) {
        let email = email.trim().to_string();
        if email.is_empty() || !email.contains('@') {
            continue;
        }
        let key = email.to_lowercase();
        if seen.insert(key) {
            let name = if name.trim().is_empty() { email.clone() } else { name };
            out.push(Contact { name, email });
        }
    }
    true
}

/// Address books the user can add contacts to (local + cached/CardDAV).
pub fn writable_books() -> Vec<Book> {
    let mut books = Vec::new();
    let active = registry_books();
    let formats = book_group_formats();
    if let Some(dir) = data_dir() {
        for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            if !entry.path().join("contacts.db").is_file() {
                continue;
            }
            let folder = entry.file_name().to_string_lossy().to_string();
            // The built-in local book's source UID is "system-address-book".
            let uid = if folder == "system" {
                "system-address-book".to_string()
            } else {
                folder
            };
            if !book_uid_active(&uid, &active) {
                continue;
            }
            let name = book_uid_name(&uid, &active, "On This Computer");
            let groups = formats.get(&uid).copied().unwrap_or(Some(GroupFormat::Evolution));
            books.push(Book { uid, name, groups });
        }
    }
    if let Some(dir) = cache_dir() {
        for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            if !entry.path().join("cache.db").is_file() {
                continue;
            }
            let uid = entry.file_name().to_string_lossy().to_string();
            if !book_uid_active(&uid, &active) {
                continue;
            }
            let name = book_uid_name(&uid, &active, "CardDAV Address Book");
            let groups = formats.get(&uid).copied().unwrap_or(Some(GroupFormat::Apple));
            books.push(Book { uid, name, groups });
        }
    }
    if let (Some(dest), Ok(conn)) = (factory_dest(), zbus::blocking::Connection::session()) {
        books.retain(|b| !book_read_only(&conn, &dest, &b.uid));
    }
    // Last: unless chosen in Settings, EDS books keep priority as the
    // default destination.
    if crate::contact_sources::hylki_used() {
        books.push(local_book());
    }
    prefer_book(&mut books, &crate::config::load_contact_book());
    books
}

/// Move the book chosen for new contacts to the front, where every caller
/// takes its default from. An empty or vanished choice changes nothing.
fn prefer_book(books: &mut [Book], uid: &str) {
    if let Some(i) = books.iter().position(|b| !uid.is_empty() && b.uid == uid) {
        books[..=i].rotate_right(1);
    }
}

/// Whether EDS says the book is read-only (Nextcloud's "Recently contacted"
/// and system address books are). `Open` fills in `Writable` — from the
/// server's privileges on first connect, cached after that. Any error counts
/// as writable, so a save still reports EDS's real failure.
fn book_read_only(conn: &zbus::blocking::Connection, dest: &str, uid: &str) -> bool {
    let Ok((bus, path)) = open_book(conn, dest, uid) else { return false };
    conn.call_method(
        Some(bus.as_str()),
        path.as_str(),
        Some("org.freedesktop.DBus.Properties"),
        "Get",
        &(BOOK_IFACE, "Writable"),
    )
    .ok()
    .and_then(|r| r.body().deserialize::<(zbus::zvariant::OwnedValue,)>().ok())
    .and_then(|(v,)| v.downcast_ref::<bool>().ok())
        == Some(false)
}

/// Best-effort display name from the EDS source file for a book UID.
fn source_display_name(uid: &str) -> Option<String> {
    let path = config_dir()?.join(format!("sources/{uid}.source"));
    crate::platform::keyfile_value(&std::fs::read_to_string(path).ok()?, "Data Source", "DisplayName")
}

// ---------------------------------------------------------------------------
// Writing (EDS D-Bus)
// ---------------------------------------------------------------------------

/// Find a versioned EDS bus name (`<prefix><digits>`, e.g. `…AddressBook10`)
/// by asking the session bus itself.
///
/// Inside Flatpak, `/usr/share/dbus-1/services` is the *runtime's* directory
/// and holds none of the host's EDS service files, so the file scan below
/// finds nothing there. The bus, however, lists the host's EDS names (the
/// `--talk-name=org.gnome.evolution.dataserver.*` grant makes them visible),
/// so query it first and keep the file scan as a fallback.
fn bus_dest(prefix: &str) -> Option<String> {
    let conn = zbus::blocking::Connection::session().ok()?;
    let proxy = zbus::blocking::fdo::DBusProxy::new(&conn).ok()?;
    let mut names: Vec<String> =
        proxy.list_names().unwrap_or_default().iter().map(|n| n.as_str().to_string()).collect();
    names.extend(
        proxy.list_activatable_names().unwrap_or_default().iter().map(|n| n.as_str().to_string()),
    );
    // Highest interface version wins if several are present.
    names
        .into_iter()
        .filter_map(|n| {
            let ver: u32 = n.strip_prefix(prefix)?.parse().ok()?;
            Some((ver, n))
        })
        .max_by_key(|(ver, _)| *ver)
        .map(|(_, n)| n)
}

/// The EDS name for `prefix`, from the bus or else the service files, looked
/// up once per run: callers ask on every contact write and directory search,
/// some of them on the UI thread, and the name does not change under a
/// running session. Only a name found is kept, so EDS starting late is
/// still picked up.
fn remembered(prefix: &'static str, from_files: fn() -> Option<String>) -> Option<String> {
    static FOUND: std::sync::Mutex<Vec<(&'static str, String)>> = std::sync::Mutex::new(Vec::new());
    if let Some((_, name)) = FOUND.lock().ok()?.iter().find(|(p, _)| *p == prefix) {
        return Some(name.clone());
    }
    let name = bus_dest(prefix).or_else(from_files)?;
    tracing::debug!("EDS: {prefix} is {name}");
    if let Ok(mut found) = FOUND.lock() {
        found.push((prefix, name.clone()));
    }
    Some(name)
}

/// Discover the versioned AddressBook factory bus name (e.g. `…AddressBook10`).
pub(crate) fn factory_dest() -> Option<String> {
    remembered("org.gnome.evolution.dataserver.AddressBook", factory_dest_from_files)
}

/// Fallback: read the name from the host's D-Bus service files (works outside
/// Flatpak only).
fn factory_dest_from_files() -> Option<String> {
    let dir = std::path::Path::new("/usr/share/dbus-1/services");
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.contains("AddressBook") && name.ends_with(".service") {
            if let Ok(text) = std::fs::read_to_string(entry.path()) {
                if let Some(n) = crate::platform::keyfile_value(&text, "D-BUS Service", "Name") {
                    if n.contains("AddressBook") {
                        return Some(n);
                    }
                }
            }
        }
    }
    None
}

const FACTORY_PATH: &str = "/org/gnome/evolution/dataserver/AddressBookFactory";
const FACTORY_IFACE: &str = "org.gnome.evolution.dataserver.AddressBookFactory";
pub(crate) const BOOK_IFACE: &str = "org.gnome.evolution.dataserver.AddressBook";

/// Open a book by source UID; returns (bus_name, object_path) for the book.
pub(crate) fn open_book(
    conn: &zbus::blocking::Connection,
    dest: &str,
    uid: &str,
) -> Result<(String, String), String> {
    let reply = conn
        .call_method(Some(dest), FACTORY_PATH, Some(FACTORY_IFACE), "OpenAddressBook", &(uid,))
        .map_err(|e| e.to_string())?;
    // Returns (object_path, bus_name).
    let (object_path, bus_name): (String, String) =
        reply.body().deserialize().map_err(|e| e.to_string())?;
    conn.call_method(Some(bus_name.as_str()), object_path.as_str(), Some(BOOK_IFACE), "Open", &())
        .map_err(|e| e.to_string())?;
    Ok((bus_name, object_path))
}

/// What happened when adding an email to Contacts.
#[derive(Debug)]
pub enum AddOutcome {
    /// A new contact was created.
    Created,
    /// The email was merged into an existing contact (carries its name).
    Merged(String),
    /// The email was already in the address book (carries the contact's name).
    AlreadyPresent(String),
}

/// Add `email` (with `name`) to Contacts, detecting duplicates and merging:
/// - if the email already exists anywhere, do nothing;
/// - else if a contact with the same name exists in the book, append the email;
/// - else create a new contact. Blocking — call from a background thread.
pub fn add_or_merge(book_uid: &str, name: &str, email: &str) -> Result<AddOutcome, String> {
    // 1. Already present (in any book)? Don't create a duplicate.
    let email_l = email.trim().to_lowercase();
    if let Some(existing) = read_contacts()
        .into_iter()
        .find(|c| c.email.to_lowercase() == email_l)
    {
        return Ok(AddOutcome::AlreadyPresent(existing.name));
    }

    if is_local_book(book_uid) {
        return add_or_merge_local(name, email);
    }

    let dest = factory_dest().ok_or("Evolution Data Server is not available")?;
    let conn = zbus::blocking::Connection::session().map_err(|e| e.to_string())?;
    let (bus, path) = open_book(&conn, &dest, book_uid)?;

    // 2. A contact with the same name in this book → merge the email into it.
    if !name.trim().is_empty() {
        let query = format!("(is \"full_name\" \"{}\")", sexpr_escape(name.trim()));
        let uids = book_get_uids(&conn, &bus, &path, &query)?;
        // A list of the same name is not that person.
        for uid in &uids {
            let vcard = book_get_contact(&conn, &bus, &path, uid)?;
            if is_group_vcard(&vcard) {
                continue;
            }
            let merged = add_email_to_vcard(&vcard, email.trim());
            book_call(&conn, &bus, &path, "ModifyContacts", &(vec![merged], 0u32))?;
            return Ok(AddOutcome::Merged(name.trim().to_string()));
        }
    }

    // 3. Create a new contact.
    book_call(
        &conn,
        &bus,
        &path,
        "CreateContacts",
        &(vec![vcard_for(name, email)], 0u32),
    )?;
    Ok(AddOutcome::Created)
}

/// [`add_or_merge`] for the local book: merge into the contact with the same
/// name, else create one.
fn add_or_merge_local(name: &str, email: &str) -> Result<AddOutcome, String> {
    let vcards = if crate::local_contacts::exists() {
        crate::local_contacts::list_vcards()?
    } else {
        Vec::new()
    };
    let result = match merge_target(&vcards, name) {
        Some(vcard) => {
            crate::local_contacts::modify(&add_email_to_vcard(&vcard, email.trim()))?;
            AddOutcome::Merged(name.trim().to_string())
        }
        None => {
            crate::local_contacts::create(&vcard_for(name, email))?;
            AddOutcome::Created
        }
    };
    local_changed();
    Ok(result)
}

/// The stored vCard whose display name equals `name` (case-insensitive).
fn merge_target(vcards: &[String], name: &str) -> Option<String> {
    let name = name.trim();
    if name.is_empty() {
        return None;
    }
    vcards
        .iter()
        .find(|v| {
            parse_vcard_fields(v).is_some_and(|d| !d.is_group() && d.name.trim().eq_ignore_ascii_case(name))
        })
        .cloned()
}

fn book_call<B>(
    conn: &zbus::blocking::Connection,
    bus: &str,
    path: &str,
    method: &str,
    body: &B,
) -> Result<zbus::Message, String>
where
    B: serde::Serialize + zbus::zvariant::DynamicType,
{
    conn.call_method(Some(bus), path, Some(BOOK_IFACE), method, body)
        .map_err(|e| e.to_string())
}

fn book_get_uids(
    conn: &zbus::blocking::Connection,
    bus: &str,
    path: &str,
    query: &str,
) -> Result<Vec<String>, String> {
    let reply = book_call(conn, bus, path, "GetContactListUids", &(query,))?;
    let (uids,): (Vec<String>,) = reply.body().deserialize().map_err(|e| e.to_string())?;
    Ok(uids)
}

fn book_get_contact(
    conn: &zbus::blocking::Connection,
    bus: &str,
    path: &str,
    uid: &str,
) -> Result<String, String> {
    let reply = book_call(conn, bus, path, "GetContact", &(uid,))?;
    let (vcard,): (String,) = reply.body().deserialize().map_err(|e| e.to_string())?;
    Ok(vcard)
}

/// Insert an EMAIL line into an existing vCard, just before END:VCARD.
fn add_email_to_vcard(vcard: &str, email: &str) -> String {
    let line = format!("EMAIL;TYPE=INTERNET:{email}\r\n");
    match vcard.rfind("END:VCARD") {
        Some(pos) => {
            let mut s = String::with_capacity(vcard.len() + line.len());
            s.push_str(&vcard[..pos]);
            if !s.ends_with('\n') {
                s.push_str("\r\n");
            }
            s.push_str(&line);
            s.push_str(&vcard[pos..]);
            s
        }
        None => vcard.to_string(),
    }
}

/// Escape a value for an EDS S-expression string literal.
fn sexpr_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

fn vcard_for(name: &str, email: &str) -> String {
    let name = if name.trim().is_empty() { email } else { name.trim() };
    // Minimal vCard 3.0; EDS assigns the UID.
    format!(
        "BEGIN:VCARD\r\nVERSION:3.0\r\nFN:{name}\r\nN:;{name};;;\r\n\
         EMAIL;TYPE=INTERNET:{email}\r\nEND:VCARD\r\n"
    )
}

// ---------------------------------------------------------------------------
// Full contact details (the in-app Contacts view)
// ---------------------------------------------------------------------------

/// A postal address in the parts of a vCard ADR.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Postal {
    pub label: String,
    pub pobox: String,
    pub extended: String,
    pub street: String,
    pub locality: String,
    pub region: String,
    pub code: String,
    pub country: String,
}

impl Postal {
    fn from_adr(label: String, raw: &str) -> Postal {
        let mut parts = split_unescaped(raw, ';').into_iter().map(|p| unescape_vcard_text(p.trim()));
        let mut next = || parts.next().unwrap_or_default();
        Postal {
            label,
            pobox: next(),
            extended: next(),
            street: next(),
            locality: next(),
            region: next(),
            code: next(),
            country: next(),
        }
    }

    pub fn is_empty(&self) -> bool {
        [&self.pobox, &self.extended, &self.street, &self.locality, &self.region, &self.code, &self.country]
            .iter()
            .all(|p| p.trim().is_empty())
    }

    /// The parts on one line, as the card shows the address.
    pub fn one_line(&self) -> String {
        [&self.pobox, &self.extended, &self.street, &self.locality, &self.region, &self.code, &self.country]
            .iter()
            .map(|p| p.trim())
            .filter(|p| !p.is_empty())
            .collect::<Vec<_>>()
            .join(", ")
    }

    fn adr_value(&self) -> String {
        [&self.pobox, &self.extended, &self.street, &self.locality, &self.region, &self.code, &self.country]
            .iter()
            .map(|p| escape_vcard_value(p.trim()))
            .collect::<Vec<_>>()
            .join(";")
    }
}

/// `s` split at each `sep` not escaped with a backslash; the parts keep
/// their escapes.
fn split_unescaped(s: &str, sep: char) -> Vec<&str> {
    let mut parts = Vec::new();
    let (mut start, mut escaped) = (0, false);
    for (i, ch) in s.char_indices() {
        if escaped {
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else if ch == sep {
            parts.push(&s[start..i]);
            start = i + ch.len_utf8();
        }
    }
    parts.push(&s[start..]);
    parts
}

/// A birthday as (year, month, day) from a stored BDAY, the year `None`
/// when it has none ("--04-12").
pub fn birthday_parts(raw: &str) -> Option<(Option<i32>, u32, u32)> {
    let head = raw.split('T').next().unwrap_or(raw).trim();
    let digits: String = head.chars().filter(|c| c.is_ascii_digit()).collect();
    let (year, month, day) = if head.starts_with("--") && digits.len() == 4 {
        (None, &digits[0..2], &digits[2..4])
    } else if digits.len() == 8 {
        (digits[0..4].parse().ok(), &digits[4..6], &digits[6..8])
    } else {
        return None;
    };
    let (month, day) = (month.parse().ok()?, day.parse().ok()?);
    ((1..=12).contains(&month) && (1..=31).contains(&day)).then_some((year, month, day))
}

/// A BDAY value from its parts, with no year as "--MM-DD".
pub fn birthday_value(year: Option<i32>, month: u32, day: u32) -> String {
    match year {
        Some(y) => format!("{y:04}-{month:02}-{day:02}"),
        None => format!("--{month:02}-{day:02}"),
    }
}

/// One labelled value on a contact (a "Home" email, a "Mobile" phone…). The
/// label may be empty when the vCard carries no usable TYPE.
#[derive(Debug, Clone)]
pub struct Labeled {
    pub label: String,
    pub value: String,
}

/// A fully parsed address-book entry, everything the in-app Contacts view
/// shows. Read straight from the EDS vCards, so it matches GNOME Contacts.
#[derive(Debug, Clone, Default)]
pub struct ContactDetails {
    pub name: String,
    pub nickname: String,
    pub org: String,
    pub title: String,
    pub birthday: String,
    pub note: String,
    pub emails: Vec<Labeled>,
    pub phones: Vec<Labeled>,
    pub addresses: Vec<Labeled>,
    /// The same addresses in their parts, for the editor.
    pub postal: Vec<Postal>,
    /// BDAY as stored ("1985-04-12", "--04-12"…), for the editor;
    /// `birthday` is it in words.
    pub birthday_raw: String,
    pub urls: Vec<String>,
    pub photo: Option<Vec<u8>>,
    /// [`photo_key`] of `photo`, set by [`read_contact_details`]; 0 when
    /// there is none or it was not worked out.
    pub photo_key: u64,
    /// EDS source UID of the book this contact came from — where edits go.
    pub book_uid: String,
    /// That book's display name ("On This Computer", the CardDAV account…).
    pub book_name: String,
    /// The contact's own EDS UID (for deletion and deep links).
    pub eds_uid: String,
    /// The stored vCard, verbatim; edits patch this so unknown properties
    /// (PHOTO, UID, X-…) survive the round trip.
    pub raw_vcard: String,
    /// Set for a contact group, with how its vCard keeps the members. A
    /// group has no `emails` of its own: its addresses are the members'.
    pub group: Option<GroupFormat>,
    pub members: Vec<Member>,
    /// Members named by the UID of a contact in the same book
    /// (`urn:uuid:`), with the line naming them, until [`resolve_members`]
    /// looks them up.
    pub(crate) member_refs: Vec<(String, String)>,
    /// The lines of members [`resolve_members`] found no address for (a
    /// person with only a phone, a card not synced yet). They are not
    /// shown, and an edit keeps them.
    pub(crate) hidden_members: Vec<String>,
    /// Mail to the group puts its members in Bcc, so they do not see each
    /// other's addresses: Evolution's "Hide addresses when sending mail to
    /// this list".
    pub hide_addresses: bool,
}

impl ContactDetails {
    /// Whether the card has a photo, held here or let go by the Contacts
    /// list once it made the list's thumbnail.
    pub fn has_photo(&self) -> bool {
        self.photo.is_some() || self.photo_key != 0
    }

    /// The photo's bytes: those held, or read again from the card. The
    /// Contacts list keeps only thumbnails, since iCloud's megapixel photos
    /// came to some 40 MB held for the whole session (#504).
    pub fn photo_bytes(&self) -> Option<Vec<u8>> {
        if self.photo.is_some() || self.photo_key == 0 {
            return self.photo.clone();
        }
        vcard_photo_with(&self.raw_vcard, self.book_uid != crate::local_contacts::LOCAL_BOOK_UID)
    }

    /// The address used for avatars and as the compose default.
    pub fn primary_email(&self) -> Option<&str> {
        self.emails.first().map(|e| e.value.as_str())
    }

    pub fn is_group(&self) -> bool {
        self.group.is_some()
    }

    /// Whether the group can be changed here: Evolution's lists and
    /// Apple's groups, the kinds Hylki writes. A vCard 4 group is only read.
    pub fn group_editable(&self) -> bool {
        matches!(self.group, Some(GroupFormat::Evolution | GroupFormat::Apple))
    }

    /// How a group member found by card is known in a set of addresses.
    pub fn grouped_key(&self) -> String {
        format!("card:{}:{}", self.book_uid, self.eds_uid)
    }

    /// The members with an address to send to: an Apple group may hold
    /// people known only by a phone number.
    pub fn reachable(&self) -> impl Iterator<Item = &Member> {
        self.members.iter().filter(|m| !m.email.is_empty())
    }

    /// The members' addresses as one recipient field.
    pub fn members_field(&self) -> String {
        self.reachable()
            .map(|m| crate::worker::format_recipient(&m.name, &m.email))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// How a group's vCard lists its members.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupFormat {
    /// `X-EVOLUTION-LIST:TRUE`, one `EMAIL` per member: what Evolution
    /// makes, and what Hylki writes, so an exported group imports there.
    Evolution,
    /// `X-ADDRESSBOOKSERVER-KIND:group`, one `X-ADDRESSBOOKSERVER-MEMBER`
    /// per member naming a contact of the same book by UID: Apple's, what
    /// iCloud and most CardDAV clients keep.
    Apple,
    /// `KIND:group` (vCard 4), with `MEMBER` lines.
    Kind,
}

/// One person in a contact group.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Member {
    pub name: String,
    pub email: String,
    /// The vCard line the member was read from, unfolded. An edit writes it
    /// back as it was, so what Evolution keeps about the member (the
    /// contact it was picked from, HTML mail) survives. Empty for a member
    /// added here.
    pub(crate) line: String,
    /// The UID of the member's own card in the group's book, for an Apple
    /// group, which names its members by it. Empty when not known yet.
    pub(crate) uid: String,
    /// The card of a contact of another book, copied into an Apple group's
    /// book when they join it ([`moved_vcard`]); empty for someone added by
    /// address, who gets a card with their name and address.
    pub(crate) card: String,
    /// The contact, by book and UID, a member was picked as in the group's
    /// form; the save finds their card by it.
    pub(crate) source: Option<(String, String)>,
}

impl Member {
    /// Whether `other` is the same person: the same card, or the same
    /// address.
    pub fn same(&self, other: &Member) -> bool {
        (!self.uid.is_empty() && self.uid == other.uid)
            || (!self.email.is_empty() && self.email.eq_ignore_ascii_case(&other.email))
            || (self.source.is_some() && self.source == other.source)
    }

    /// The key [`ContactDetails::grouped_key`] gives the member's card.
    pub fn card_key(&self, book_uid: &str) -> Option<String> {
        (!self.uid.is_empty()).then(|| format!("card:{book_uid}:{}", self.uid))
    }
}

/// Every *live, enabled* address-book source, straight from the EDS source
/// registry over D-Bus: uid → a friendly name for the account it belongs to
/// ("Google — a@gmail.com", "CardDAV — j@mac.com", "On This Computer").
///
/// The cache/data directories of removed accounts linger on disk (EDS never
/// deletes them), so without this a book removed in GOA or GNOME Contacts
/// keeps showing its contacts forever. The registry — not the sources
/// directory — is the authority: GOA-derived sources are registry-only and
/// never written to disk. `None` when the registry can't be reached (then
/// keep everything rather than hide it all).
fn registry_books() -> Option<HashMap<String, String>> {
    let sources: HashMap<String, String> =
        registry_sources()?.into_iter().map(|(_, uid, data)| (uid, data)).collect();
    let mut books = HashMap::new();
    let off = crate::contact_sources::off();
    let names = crate::contact_sources::names();
    for (uid, data) in &sources {
        // An address book, and not switched off (GOA sets the book source's
        // [Data Source] Enabled=false when Contacts is toggled off for the
        // account). Only that section's Enabled counts — other sections
        // (Refresh etc.) have Enabled keys of their own. Nor switched off
        // for Hylki in Settings → Contacts, it or its account.
        if data.contains("[Address Book]")
            && !data_source_disabled(data)
            && crate::contact_sources::book_used(uid, &sources, &off)
        {
            let name = book_display_name(data, &sources);
            let account = names.get(&crate::contact_sources::group_of(uid, &sources));
            books.insert(uid.clone(), with_given_name(&with_account_name(&name, account), names.get(uid)));
        }
    }
    Some(books)
}

/// Every source the EDS registry holds: (object path, UID, key file).
/// `None` when the registry can't be reached.
pub(crate) fn registry_sources() -> Option<Vec<(String, String, String)>> {
    use gtk::gio;

    let dest = sources_dest()?;
    let conn = gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE).ok()?;
    // zbus 4's blocking call has no deadline. A deadlocked source registry
    // used to strand every composer lookup forever. GIO bounds the actual
    // request, so the worker can fall back to cached books and mail history
    // instead of leaving another blocked thread behind on every open.
    let reply = match conn.call_sync(
        Some(dest.as_str()),
        "/org/gnome/evolution/dataserver/SourceManager",
        "org.freedesktop.DBus.ObjectManager",
        "GetManagedObjects",
        None,
        None,
        gio::DBusCallFlags::NONE,
        3000,
        gio::Cancellable::NONE,
    ) {
        Ok(reply) => reply,
        Err(e) => {
            tracing::warn!("EDS source registry unavailable: {e}");
            return None;
        }
    };
    sources_from_reply(&reply)
}

fn sources_from_reply(reply: &gtk::glib::Variant) -> Option<Vec<(String, String, String)>> {
    type Props = HashMap<String, gtk::glib::Variant>;
    type Objects = HashMap<gtk::glib::variant::ObjectPath, HashMap<String, Props>>;
    let (objects,): (Objects,) = reply.get()?;
    let mut sources = Vec::new();
    for (path, ifaces) in &objects {
        let Some(props) = ifaces.get("org.gnome.evolution.dataserver.Source") else {
            continue;
        };
        let uid =
            props.get("UID").and_then(|v| v.str()).map(str::to_string);
        let data =
            props.get("Data").and_then(|v| v.str()).map(str::to_string);
        if let (Some(uid), Some(data)) = (uid, data) {
            sources.push((path.to_string(), uid, data));
        }
    }
    Some(sources)
}

/// A book's friendly name: which account it belongs to and over what — walked
/// up the source's parent chain to the account collection, whose backend
/// (google / microsoft365 / webdav) and identity say it best.
/// A book's name with the one given to its account in Hylki, if any, in
/// place of the account's own ("Work · Family" for "CardDAV — a@b.org ·
/// Family"); a book that is its account alone keeps its address as its own
/// name ("Work · a@b.org").
fn with_account_name(name: &str, account: Option<&String>) -> String {
    match (account, name.split_once(" · "), name.split_once(" — ")) {
        (None, _, _) => name.to_string(),
        (Some(account), Some((_, own)), _) => format!("{account} · {own}"),
        (Some(account), None, Some((_, own))) => format!("{account} · {own}"),
        (Some(account), None, None) => account.clone(),
    }
}

/// A book's name with the one given to it in Hylki, if any, in place of the
/// book's own: after its account's ("CardDAV — a@b.org · Family"), or alone
/// for a book that is no account's.
fn with_given_name(name: &str, given: Option<&String>) -> String {
    match (given, name.split_once(" · ")) {
        (None, _) => name.to_string(),
        (Some(given), Some((account, _))) => format!("{account} · {given}"),
        (Some(given), None) if name.contains(" — ") => format!("{name} · {given}"),
        (Some(given), None) => given.clone(),
    }
}

fn book_display_name(book_data: &str, sources: &HashMap<String, String>) -> String {
    let own = crate::platform::keyfile_value(book_data, "Data Source", "DisplayName");
    let (backend, identity, top_display) = book_account(book_data, sources);
    let account = identity.or(top_display.or(own.clone()));
    let label = match (backend.as_deref(), account.as_deref()) {
        (Some("local"), _) | (None, None) => i18n("On This Computer"),
        (Some("google"), Some(a)) => format!("Google — {a}"),
        (Some("google"), None) => "Google".to_string(),
        (Some("microsoft365"), Some(a)) => format!("Microsoft 365 — {a}"),
        (Some("microsoft365"), None) => "Microsoft 365".to_string(),
        (Some("ldap"), Some(a)) => format!("LDAP — {a}"),
        (Some("ldap"), None) => "LDAP".to_string(),
        (_, Some(a)) => format!("CardDAV — {a}"),
        (_, None) => i18n("CardDAV Address Book"),
    };
    // One account can hold several books (Nextcloud: Contacts, Recently
    // contacted, System address book), so name the book too — unless it is
    // the account itself.
    match own {
        Some(own) if !own.is_empty() && Some(&own) != account.as_ref() => format!("{label} · {own}"),
        _ => label,
    }
}

/// The backend that serves a book, walked up its parent chain to the
/// account collection (google / microsoft365 / webdav / local…), with the
/// account's identity and display name when there is one.
fn book_account(book_data: &str, sources: &HashMap<String, String>) -> (Option<String>, Option<String>, Option<String>) {
    let mut backend = crate::platform::keyfile_value(book_data, "Address Book", "BackendName");
    let mut identity: Option<String> = None;
    let mut top_display = None;
    let mut current = book_data.to_string();
    for _ in 0..4 {
        let Some(parent) = crate::platform::keyfile_value(&current, "Data Source", "Parent") else { break };
        let Some(parent_data) = sources.get(&parent) else { break };
        if let Some(b) = crate::platform::keyfile_value(parent_data, "Collection", "BackendName") {
            backend = Some(b);
        }
        if identity.is_none() {
            identity = crate::platform::keyfile_value(parent_data, "Collection", "Identity");
        }
        if let Some(d) = crate::platform::keyfile_value(parent_data, "Data Source", "DisplayName") {
            top_display = Some(d);
        }
        current = parent_data.clone();
    }
    (backend, identity, top_display)
}

/// How each address book in the registry keeps a new group: Evolution's
/// lists on this computer, Apple's groups on a CardDAV server (what iCloud,
/// Fastmail and the Apple and DAVx⁵ clients read), none on Google, which
/// keeps a group as a person with odd addresses, nor on Microsoft 365 or a
/// directory, which take none.
fn book_group_formats() -> HashMap<String, Option<GroupFormat>> {
    let Some(sources) = registry_sources() else { return HashMap::new() };
    let sources: HashMap<String, String> = sources.into_iter().map(|(_, uid, data)| (uid, data)).collect();
    sources
        .iter()
        .filter(|(_, data)| data.contains("[Address Book]"))
        .map(|(uid, data)| (uid.clone(), group_format_for(book_account(data, &sources).0.as_deref())))
        .collect()
}

fn group_format_for(backend: Option<&str>) -> Option<GroupFormat> {
    match backend {
        None | Some("local") => Some(GroupFormat::Evolution),
        Some("google" | "microsoft365" | "ldap") => None,
        Some(_) => Some(GroupFormat::Apple),
    }
}

/// Whether a source keyfile's `[Data Source]` group says `Enabled=false`.
fn data_source_disabled(data: &str) -> bool {
    crate::platform::keyfile(data).and_then(|kf| kf.boolean("Data Source", "Enabled").ok()) == Some(false)
}

/// Discover the versioned Sources registry bus name (e.g. `…Sources5`).
pub(crate) fn sources_dest() -> Option<String> {
    remembered("org.gnome.evolution.dataserver.Sources", sources_dest_from_files)
}

/// Fallback: read the name from the host's D-Bus service files (works outside
/// Flatpak only).
fn sources_dest_from_files() -> Option<String> {
    let dir = std::path::Path::new("/usr/share/dbus-1/services");
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.contains("evolution.dataserver.Sources") && name.ends_with(".service") {
            if let Ok(text) = std::fs::read_to_string(entry.path()) {
                if let Some(n) = crate::platform::keyfile_value(&text, "D-BUS Service", "Name") {
                    return Some(n);
                }
            }
        }
    }
    None
}

/// Whether the registry lists `uid` as live (fail-open when unreachable,
/// but for a book switched off for Hylki by its own UID).
fn book_uid_active(uid: &str, books: &Option<HashMap<String, String>>) -> bool {
    books.as_ref().map_or_else(|| !crate::contact_sources::off().contains(uid), |map| map.contains_key(uid))
}

/// The registry's friendly name for a book, with a fallback for when the
/// registry is unreachable.
fn book_uid_name(
    uid: &str,
    books: &Option<HashMap<String, String>>,
    fallback: &str,
) -> String {
    books
        .as_ref()
        .and_then(|map| map.get(uid).cloned())
        .unwrap_or_else(|| source_display_name(uid).unwrap_or_else(|| fallback.to_string()))
}

/// Every contact from the local + cached (CardDAV) EDS books, fully parsed
/// and sorted by display name, contact groups among them.
pub fn read_contact_details() -> Vec<ContactDetails> {
    let mut out: Vec<ContactDetails> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let active = registry_books();
    // Walk the book directories directly (like `writable_books`) so each
    // contact remembers which book it belongs to.
    if let Some(dir) = data_dir() {
        for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let db = entry.path().join("contacts.db");
            if !db.is_file() {
                continue;
            }
            let folder = entry.file_name().to_string_lossy().to_string();
            let uid = if folder == "system" { "system-address-book".to_string() } else { folder };
            if !book_uid_active(&uid, &active) {
                continue;
            }
            let name = book_uid_name(&uid, &active, "On This Computer");
            read_detail_db(&db, "SELECT vcard FROM folder_id", &uid, &name, &mut out, &mut seen);
        }
    }
    if let Some(dir) = cache_dir() {
        for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let db = entry.path().join("cache.db");
            if !db.is_file() {
                continue;
            }
            let uid = entry.file_name().to_string_lossy().to_string();
            if !book_uid_active(&uid, &active) {
                continue;
            }
            let name = book_uid_name(&uid, &active, "CardDAV Address Book");
            read_detail_db(
                &db,
                "SELECT ECacheOBJ FROM ECacheObjects",
                &uid,
                &name,
                &mut out,
                &mut seen,
            );
        }
    }
    out.extend(local_details());
    out.sort_by_key(|c| c.name.to_lowercase());
    // Hashed here, off the main thread: the page finds the photos it has
    // decoded already by these (#477).
    for c in &mut out {
        c.photo_key = c.photo.as_deref().map_or(0, photo_key);
    }
    out
}

/// A photo's identity, for the decoded avatars kept between reads. Never 0,
/// which stands for no photo.
pub fn photo_key(bytes: &[u8]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut hasher);
    hasher.finish().max(1)
}

fn read_detail_db(
    path: &std::path::Path,
    query: &str,
    book_uid: &str,
    book_name: &str,
    out: &mut Vec<ContactDetails>,
    seen: &mut HashSet<String>,
) {
    let conn = match open_book_db(path) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("contacts: cannot open {}: {e}", path.display());
            return;
        }
    };
    let mut stmt = match conn.prepare(query) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("contacts: query failed on {}: {e}", path.display());
            return;
        }
    };
    let rows = stmt.query_map([], |row| row.get::<_, String>(0));
    let Ok(rows) = rows else { return };
    let mut book: Vec<ContactDetails> = Vec::new();
    for vcard in rows.flatten() {
        let Some(mut c) = parse_vcard_details(&vcard) else { continue };
        c.book_uid = book_uid.to_string();
        c.book_name = book_name.to_string();
        c.raw_vcard = vcard;
        book.push(c);
    }
    resolve_book_members(&mut book);
    for c in book {
        if c.is_group() {
            out.push(c);
            continue;
        }
        // Every card is listed, the same person in two books twice, each
        // with its book, as an address book app lists them. Only a card
        // read twice (a book in two places on disk) is listed once.
        if c.eds_uid.is_empty() || seen.insert(format!("{}\u{0}{}", c.book_uid, c.eds_uid)) {
            out.push(c);
        }
    }
}

/// Resolve the groups' members named by UID against the people of the same
/// book, all of which are in `book`.
fn resolve_book_members(book: &mut [ContactDetails]) {
    if !book.iter().any(|c| !c.member_refs.is_empty()) {
        return;
    }
    let people: HashMap<String, (String, String)> = book
        .iter()
        .filter(|c| !c.is_group() && !c.eds_uid.is_empty())
        .map(|c| (c.eds_uid.clone(), (c.name.clone(), c.primary_email().unwrap_or_default().to_string())))
        .collect();
    for c in book.iter_mut().filter(|c| !c.member_refs.is_empty()) {
        resolve_members(c, |uid| people.get(uid).cloned());
    }
}

/// The human label for a property's TYPE parameters ("" when none applies).
fn vcard_type_label(params: &[&str]) -> String {
    let joined = params.join(";").to_uppercase();
    for (needle, label) in [
        ("CELL", "Mobile"),
        ("HOME", "Home"),
        ("WORK", "Work"),
        ("FAX", "Fax"),
    ] {
        if joined.contains(needle) {
            return label.to_string();
        }
    }
    String::new()
}

/// "1985-04-12" / "19850412" → "Apr 12, 1985"; "--04-12" → "Apr 12", in the
/// locale's words and the user's date order, as mail dates are written;
/// anything else passes through unchanged.
fn pretty_birthday(raw: &str) -> String {
    let head = raw.split('T').next().unwrap_or(raw);
    let digits: String = head.chars().filter(|c| c.is_ascii_digit()).collect();
    let (year, month, day) = if head.starts_with("--") && digits.len() == 4 {
        (None, &digits[0..2], &digits[2..4])
    } else if digits.len() == 8 {
        (Some(&digits[0..4]), &digits[4..6], &digits[6..8])
    } else {
        return raw.to_string();
    };
    let (Ok(m), Ok(d)) = (month.parse::<i32>(), day.parse::<i32>()) else {
        return raw.to_string();
    };
    // A birthday with no year is placed in a leap year, so 29 February is a
    // date too. Noon keeps the day the same in every time zone.
    let y = year.and_then(|y| y.parse::<i32>().ok()).unwrap_or(2000);
    let Ok(date) = gtk::glib::DateTime::from_local(y, m, d, 12, 0, 0.0) else {
        return raw.to_string();
    };
    match year {
        Some(_) => crate::datefmt::day_month_year(date.to_unix()),
        None => crate::datefmt::day_month(date.to_unix()),
    }
}

/// Parse the fields the Contacts view shows out of one vCard, its photo
/// included. Returns `None` for contact lists and entries with neither a name
/// nor an address.
pub(crate) fn parse_vcard_details(vcard: &str) -> Option<ContactDetails> {
    let mut c = parse_vcard_fields(vcard)?;
    c.photo = vcard_photo(vcard);
    Some(c)
}

/// [`parse_vcard_details`] without the photo, which costs a base64 decode or
/// a file read: for imports and lookups that never show it.
pub(crate) fn parse_vcard_fields(vcard: &str) -> Option<ContactDetails> {
    let mut c = ContactDetails::default();
    // A list's EMAIL lines are its members, and the line saying so may come
    // after them, so they are kept aside until the card is read.
    let mut email_lines: Vec<(String, Vec<String>, String)> = Vec::new();
    let mut show_addresses = None;
    for line in unfold_vcard(vcard).lines() {
        let Some((prop, raw_value)) = split_vcard_line(line) else { continue };
        let mut parts = prop.split(';');
        // Group prefixes (`item1.EMAIL`) hide the property name; strip them.
        let name = parts
            .next()
            .unwrap_or("")
            .rsplit('.')
            .next()
            .unwrap_or("")
            .to_ascii_uppercase();
        let params: Vec<&str> = parts.collect();
        let value = unescape_vcard_text(raw_value.trim());
        if value.is_empty() {
            continue;
        }
        match name.as_str() {
            "X-EVOLUTION-LIST" if value.eq_ignore_ascii_case("true") => c.group = Some(GroupFormat::Evolution),
            "X-EVOLUTION-LIST-SHOW-ADDRESSES" => show_addresses = Some(value.eq_ignore_ascii_case("true")),
            "X-ADDRESSBOOKSERVER-KIND" if value.eq_ignore_ascii_case("group") => {
                c.group.get_or_insert(GroupFormat::Apple);
            }
            "KIND" if value.eq_ignore_ascii_case("group") => {
                c.group.get_or_insert(GroupFormat::Kind);
            }
            "MEMBER" | "X-ADDRESSBOOKSERVER-MEMBER" => {
                let lower = value.to_ascii_lowercase();
                if lower.starts_with("mailto:") {
                    let email = value[7..].trim().to_string();
                    if email.contains('@') && !c.members.iter().any(|m| m.email.eq_ignore_ascii_case(&email)) {
                        c.members.push(Member { email, line: line.to_string(), ..Member::default() });
                    }
                } else {
                    let uid = if lower.starts_with("urn:uuid:") { &value[9..] } else { value.as_str() };
                    c.member_refs.push((uid.trim().to_string(), line.to_string()));
                }
            }
            "UID" if c.eds_uid.is_empty() => c.eds_uid = value,
            "FN" => c.name = value,
            "NICKNAME" if c.nickname.is_empty() => c.nickname = value,
            "ORG" => {
                // ORG is org;department;… — the org alone reads best.
                c.org = value.split(';').next().unwrap_or("").trim().to_string();
            }
            "TITLE" if c.title.is_empty() => c.title = value,
            "BDAY" => {
                c.birthday = pretty_birthday(&value);
                c.birthday_raw = value;
            }
            "NOTE" if c.note.is_empty() => c.note = value,
            "EMAIL" => {
                email_lines.push((line.to_string(), params.iter().map(|p| p.to_string()).collect(), value.clone()));
                if value.contains('@') && !c.emails.iter().any(|e| e.value == value) {
                    c.emails.push(Labeled { label: vcard_type_label(&params), value });
                }
            }
            "TEL" => {
                if !c.phones.iter().any(|p| p.value == value) {
                    c.phones.push(Labeled { label: vcard_type_label(&params), value });
                }
            }
            "URL" => {
                if !c.urls.contains(&value) {
                    c.urls.push(value);
                }
            }
            "ADR" => {
                // pobox;extended;street;locality;region;postal-code;country.
                let formatted = raw_value
                    .split(';')
                    .map(|p| unescape_vcard_text(p.trim()))
                    .filter(|p| !p.is_empty())
                    .collect::<Vec<_>>()
                    .join(", ");
                if !formatted.is_empty() && !c.addresses.iter().any(|a| a.value == formatted) {
                    c.addresses.push(Labeled { label: vcard_type_label(&params), value: formatted });
                    c.postal.push(Postal::from_adr(vcard_type_label(&params), raw_value.trim()));
                }
            }
            _ => {}
        }
    }
    if c.group.is_some() {
        // A group is known by its name: it has no address to fall back on.
        if c.name.trim().is_empty() {
            return None;
        }
        if c.group == Some(GroupFormat::Evolution) {
            // Evolution's own default: a list with no say hides its addresses.
            c.hide_addresses = show_addresses != Some(true);
            for (line, params, value) in email_lines {
                let Some((name, email)) = evolution_member(&params, &value) else { continue };
                if !c.members.iter().any(|m| m.email.eq_ignore_ascii_case(&email)) {
                    c.members.push(Member { name, email, line, ..Member::default() });
                }
            }
        } else {
            // Other groups have no say of their own; Hylki writes
            // Evolution's line into an Apple group to keep Send as Bcc.
            c.hide_addresses = show_addresses == Some(false);
        }
        c.emails.clear();
        return Some(c);
    }
    // A card with no name goes by what else it has, as an address book
    // lists it; one with nothing to go by is skipped.
    if c.name.trim().is_empty() {
        c.name = fallback_name(&c)?;
    }
    Some(c)
}

/// What a card with no name is listed as: its address, nickname, company
/// or phone number.
fn fallback_name(c: &ContactDetails) -> Option<String> {
    c.primary_email()
        .map(str::to_string)
        .or_else(|| (!c.nickname.is_empty()).then(|| c.nickname.clone()))
        .or_else(|| (!c.org.is_empty()).then(|| c.org.clone()))
        .or_else(|| c.phones.first().map(|p| p.value.clone()))
}

/// The (name, address) of one member of an Evolution list. The value is a
/// recipient ("Ann Lee <ann@x.org>"); Evolution also keeps the parts as
/// parameters, which are the fallback when the value does not parse.
fn evolution_member(params: &[String], value: &str) -> Option<(String, String)> {
    if let Some((name, email)) = crate::worker::parse_recipients(value).into_iter().next() {
        if email.contains('@') {
            return Some((name, email));
        }
    }
    let param = |key: &str| {
        params.iter().find_map(|p| {
            let (k, v) = p.split_once('=')?;
            k.trim().eq_ignore_ascii_case(key).then(|| unescape_vcard_text(v.trim().trim_matches('"')))
        })
    };
    let email = param("X-EVOLUTION-DEST-EMAIL").filter(|e| e.contains('@'))?;
    Some((param("X-EVOLUTION-DEST-NAME").unwrap_or_default(), email))
}

/// Fill in the group members named by contact UID, from `lookup` (UID →
/// that contact's name and first address, empty when they have none, in the
/// group's own book). A member whose card is not there is not shown, but is
/// kept.
pub(crate) fn resolve_members(c: &mut ContactDetails, lookup: impl Fn(&str) -> Option<(String, String)>) {
    for (uid, line) in std::mem::take(&mut c.member_refs) {
        let found = lookup(&uid).filter(|(_, email)| {
            email.is_empty() || !c.members.iter().any(|m| m.email.eq_ignore_ascii_case(email))
        });
        match found {
            Some((name, email)) if !c.members.iter().any(|m| m.uid == uid) => {
                c.members.push(Member { name, email, line, uid, ..Member::default() });
            }
            _ => c.hidden_members.push(line),
        }
    }
}

/// The (name, first address or "") of a person's vCard, for
/// [`resolve_members`].
fn person_address(vcard: &str) -> Option<(String, String)> {
    let c = parse_vcard_fields(vcard).filter(|c| !c.is_group())?;
    let email = c.primary_email().unwrap_or_default().to_string();
    Some((c.name, email))
}

/// Whether a stored vCard is a contact group, for the reads that only want
/// people and never parse the whole card.
fn is_group_vcard(vcard: &str) -> bool {
    unfold_vcard(vcard).lines().any(|line| {
        let Some((prop, value)) = line.split_once(':') else { return false };
        let name = prop.split(';').next().unwrap_or("").rsplit('.').next().unwrap_or("").trim();
        let value = value.trim();
        (name.eq_ignore_ascii_case("X-EVOLUTION-LIST") && value.eq_ignore_ascii_case("true"))
            || ((name.eq_ignore_ascii_case("KIND") || name.eq_ignore_ascii_case("X-ADDRESSBOOKSERVER-KIND"))
                && value.eq_ignore_ascii_case("group"))
    })
}

/// The books demo mode offers new contacts, the Hylki book among them.
pub fn demo_books() -> Vec<Book> {
    vec![
        Book { uid: "On This Computer".into(), name: "On This Computer".into(), groups: Some(GroupFormat::Evolution) },
        Book {
            uid: "CardDAV — jason@hylki.hyprlab.co".into(),
            name: "CardDAV — jason@hylki.hyprlab.co".into(),
            groups: Some(GroupFormat::Apple),
        },
        local_book(),
    ]
}

/// Sample contacts for demo mode (HYLKI_DEMO): the people from the demo
/// mailbox, fleshed out so the contacts view has something to show off.
/// No EDS identity: demo entries are display-only. The book's name stands
/// in for its UID, so the book filter has something to tell apart.
pub fn demo_contacts() -> Vec<ContactDetails> {
    let l = |label: &str, value: &str| Labeled { label: label.into(), value: value.into() };
    let contact = |name: &str, book: &str| ContactDetails {
        name: name.into(),
        book_name: book.into(),
        book_uid: if book == "Hylki Address Book" { crate::local_contacts::LOCAL_BOOK_UID.into() } else { book.into() },
        ..ContactDetails::default()
    };
    vec![
        ContactDetails {
            nickname: "Soph".into(),
            org: "Studio.dev".into(),
            title: "Design Lead".into(),
            birthday: "March 4, 1991".into(),
            note: "Prefers Figma links over attachments. Out on Fridays.".into(),
            emails: vec![l("Work", "sophie@studio.dev"), l("Home", "sophie.t@example.com")],
            phones: vec![l("Mobile", "+1 (415) 555-0114")],
            addresses: vec![l("Work", "2261 Market St, San Francisco, CA 94114")],
            urls: vec!["sophie.design".into()],
            ..contact("Sophie Turner", "CardDAV — jason@hylki.hyprlab.co")
        },
        ContactDetails {
            org: "Studio.dev".into(),
            title: "Engineering Manager".into(),
            emails: vec![l("Work", "marcus@studio.dev")],
            phones: vec![l("Mobile", "+1 (415) 555-0187"), l("Work", "+1 (415) 555-0100")],
            ..contact("Marcus Chen", "CardDAV — jason@hylki.hyprlab.co")
        },
        ContactDetails {
            org: "Studio.dev".into(),
            title: "Product Designer".into(),
            birthday: "November 19".into(),
            emails: vec![l("Work", "priya@studio.dev")],
            phones: vec![l("Mobile", "+1 (628) 555-0163")],
            ..contact("Priya Sharma", "Google — jason.m@gmail.com")
        },
        ContactDetails {
            title: "Illustrator".into(),
            note: "Freelance — invoices go to the studio address.".into(),
            emails: vec![l("", "emma@example.com")],
            urls: vec!["emmawright.art".into()],
            ..contact("Emma Wright", "Google — jason.m@gmail.com")
        },
        ContactDetails {
            org: "GNOME Foundation".into(),
            emails: vec![l("Work", "lena@gnome.org")],
            addresses: vec![l("Work", "21 Orinda Way, Orinda, CA 94563")],
            ..contact("Lena Fischer", "On This Computer")
        },
        ContactDetails {
            org: "Ferrous Type".into(),
            title: "Founder".into(),
            emails: vec![l("Work", "diego@ferroustype.com"), l("Home", "d.alvarez@example.com")],
            phones: vec![l("Mobile", "+34 612 555 021")],
            urls: vec!["ferroustype.com".into()],
            ..contact("Diego Álvarez", "CardDAV — jason@hylki.hyprlab.co")
        },
        ContactDetails {
            emails: vec![l("Home", "tom.okafor@example.com")],
            phones: vec![l("Mobile", "+44 7700 900 214")],
            birthday: "July 30, 1988".into(),
            ..contact("Tom Okafor", "Hylki Address Book")
        },
        ContactDetails {
            org: "Kim & Partners".into(),
            title: "Attorney".into(),
            emails: vec![l("Work", "grace@kimpartners.example")],
            phones: vec![l("Work", "+1 (212) 555-0170")],
            addresses: vec![l("Work", "425 Lexington Ave, New York, NY 10017")],
            ..contact("Grace Kim", "CardDAV — jason@hylki.hyprlab.co")
        },
        demo_group("Studio.dev Team", false, &[
            ("Sophie Turner", "sophie@studio.dev"),
            ("Marcus Chen", "marcus@studio.dev"),
            ("Priya Sharma", "priya@studio.dev"),
        ]),
        demo_group("Book Club", true, &[
            ("Emma Wright", "emma@example.com"),
            ("Tom Okafor", "tom.okafor@example.com"),
            ("Grace Kim", "grace@kimpartners.example"),
            ("Lena Fischer", "lena@gnome.org"),
        ]),
    ]
}

/// A demo group in the Hylki book, with a card to edit.
fn demo_group(name: &str, hide_addresses: bool, people: &[(&str, &str)]) -> ContactDetails {
    let members: Vec<Member> = people
        .iter()
        .map(|(n, e)| Member { name: n.to_string(), email: e.to_string(), ..Member::default() })
        .collect();
    let edit = GroupEdit { name: name.into(), members: members.clone(), hide_addresses, kept: Vec::new() };
    ContactDetails {
        name: name.into(),
        book_name: i18n("Hylki Address Book"),
        book_uid: crate::local_contacts::LOCAL_BOOK_UID.into(),
        group: Some(GroupFormat::Evolution),
        members,
        hide_addresses,
        raw_vcard: new_group_vcard(&edit),
        // A group is told apart by its UID, as a real one is.
        eds_uid: format!("demo-{name}"),
        ..ContactDetails::default()
    }
}

// ---------------------------------------------------------------------------
// In-app editing (vCard patching + EDS writes)
// ---------------------------------------------------------------------------

/// The fields the in-app contact editor can change.
#[derive(Debug, Clone, Default)]
pub struct ContactEdit {
    pub name: String,
    pub nickname: String,
    pub org: String,
    pub title: String,
    pub note: String,
    pub emails: Vec<Labeled>,
    pub phones: Vec<Labeled>,
    pub urls: Vec<String>,
    pub addresses: Vec<Postal>,
    /// A BDAY value ([`birthday_value`]), empty for none.
    pub birthday: String,
    pub photo: PhotoEdit,
}

/// What the editor does to a contact's photo.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum PhotoEdit {
    #[default]
    Keep,
    Remove,
    /// A new photo: the image file's bytes, made small by the editor.
    Set(Vec<u8>),
}

/// Escape a value for a vCard text property.
fn escape_vcard_value(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            ',' => out.push_str("\\,"),
            ';' => out.push_str("\\;"),
            '\n' => out.push_str("\\n"),
            '\r' => {}
            other => out.push(other),
        }
    }
    out
}

/// The TYPE parameter matching an editor label ("" for none).
fn type_param_for(label: &str) -> &'static str {
    match label {
        "Home" => ";TYPE=HOME",
        "Work" => ";TYPE=WORK",
        "Mobile" => ";TYPE=CELL",
        "Fax" => ";TYPE=FAX",
        _ => "",
    }
}

/// Rewrite a stored vCard with the editor's values. Only the editable
/// properties are replaced — everything else (UID, PHOTO, ADR, REV, X-…)
/// passes through untouched, so nothing GNOME Contacts manages is lost.
pub fn patched_vcard(original: &str, e: &ContactEdit) -> String {
    const REPLACED: [&str; 11] =
        ["FN", "N", "NICKNAME", "ORG", "TITLE", "NOTE", "EMAIL", "TEL", "URL", "ADR", "BDAY"];
    let photo = e.photo != PhotoEdit::Keep;
    let mut out = kept_lines_by(original, |prop| {
        REPLACED.contains(&prop) || (photo && matches!(prop, "PHOTO" | "X-IMAGETYPE" | "X-IMAGEHASH"))
    });

    // FN is required: with no name, the card goes by what else it has.
    let fallback = || {
        [&e.org, &e.nickname]
            .into_iter()
            .map(|s| s.trim().to_string())
            .chain(e.emails.iter().chain(&e.phones).map(|l| l.value.trim().to_string()))
            .find(|s| !s.is_empty())
            .unwrap_or_default()
    };
    let named = !e.name.trim().is_empty();
    let fn_value = if named { e.name.trim().to_string() } else { fallback() };
    out.push(format!("FN:{}", escape_vcard_value(&fn_value)));
    let name = if named { e.name.trim() } else { "" };
    // Structured name: last word as the family name — the same heuristic an
    // address book's "sort by surname" uses.
    let mut words: Vec<&str> = name.split_whitespace().collect();
    let family = if words.len() > 1 { words.pop().unwrap_or("") } else { "" };
    out.push(format!(
        "N:{};{};;;",
        escape_vcard_value(family),
        escape_vcard_value(&words.join(" "))
    ));
    for a in e.addresses.iter().filter(|a| !a.is_empty()) {
        out.push(format!("ADR{}:{}", type_param_for(&a.label), a.adr_value()));
    }
    if !e.birthday.trim().is_empty() {
        out.push(format!("BDAY:{}", e.birthday.trim()));
    }
    if let PhotoEdit::Set(bytes) = &e.photo {
        use base64::Engine as _;
        let kind = match crate::models::image_ext(bytes) {
            "png" => "PNG",
            "gif" => "GIF",
            _ => "JPEG",
        };
        out.push(format!("PHOTO;ENCODING=b;TYPE={kind}:{}", base64::engine::general_purpose::STANDARD.encode(bytes)));
    }
    if !e.nickname.trim().is_empty() {
        out.push(format!("NICKNAME:{}", escape_vcard_value(e.nickname.trim())));
    }
    if !e.org.trim().is_empty() {
        out.push(format!("ORG:{};", escape_vcard_value(e.org.trim())));
    }
    if !e.title.trim().is_empty() {
        out.push(format!("TITLE:{}", escape_vcard_value(e.title.trim())));
    }
    if !e.note.trim().is_empty() {
        out.push(format!("NOTE:{}", escape_vcard_value(e.note.trim())));
    }
    for email in &e.emails {
        out.push(format!(
            "EMAIL{}:{}",
            type_param_for(&email.label),
            escape_vcard_value(&email.value)
        ));
    }
    for phone in &e.phones {
        out.push(format!(
            "TEL{}:{}",
            type_param_for(&phone.label),
            escape_vcard_value(&phone.value)
        ));
    }
    for url in &e.urls {
        out.push(format!("URL:{}", escape_vcard_value(url)));
    }
    out.push("END:VCARD".to_string());
    out.join("\r\n") + "\r\n"
}

/// A brand-new contact's vCard from the editor's values.
pub fn new_vcard(e: &ContactEdit) -> String {
    patched_vcard("BEGIN:VCARD\r\nVERSION:3.0\r\nEND:VCARD\r\n", e)
}

/// The lines of a stored vCard up to its END, less the properties in
/// `replaced` (folded continuations go with their property).
fn kept_lines(original: &str, replaced: &[&str]) -> Vec<String> {
    kept_lines_by(original, |prop| replaced.contains(&prop))
}

/// The lines of `original` before END:VCARD, less the properties (with
/// their folded continuations) `drop` names; `drop` is given the upper-case
/// property name.
fn kept_lines_by(original: &str, drop: impl Fn(&str) -> bool) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut skipping = false;
    for raw in original.lines() {
        let line = raw.trim_end_matches('\r');
        // Folded continuation lines belong to the previous property.
        if line.starts_with(' ') || line.starts_with('\t') {
            if !skipping {
                out.push(line.to_string());
            }
            continue;
        }
        if line.eq_ignore_ascii_case("END:VCARD") {
            break;
        }
        let prop = line
            .split([':', ';'])
            .next()
            .unwrap_or("")
            .rsplit('.')
            .next()
            .unwrap_or("")
            .to_ascii_uppercase();
        skipping = drop(&prop);
        if !skipping {
            out.push(line.to_string());
        }
    }
    out
}

/// The fields the group editor can change.
#[derive(Debug, Clone, Default)]
pub struct GroupEdit {
    pub name: String,
    pub members: Vec<Member>,
    pub hide_addresses: bool,
    /// Member lines the editor did not show ([`ContactDetails::hidden_members`]),
    /// written back as they were.
    pub(crate) kept: Vec<String>,
}

/// Rewrite a stored Evolution list with the editor's values. A member who
/// was already there keeps the line it was read from; one added here gets
/// a line of the same shape as Evolution's.
pub fn patched_group_vcard(original: &str, e: &GroupEdit) -> String {
    const REPLACED: [&str; 7] = [
        "FN",
        "N",
        "X-EVOLUTION-FILE-AS",
        "X-EVOLUTION-LIST",
        "X-EVOLUTION-LIST-SHOW-ADDRESSES",
        "EMAIL",
        "KIND",
    ];
    let mut out = kept_lines(original, &REPLACED);
    let name = escape_vcard_value(e.name.trim());
    out.push(format!("FN:{name}"));
    out.push("N:;;;;".to_string());
    out.push(format!("X-EVOLUTION-FILE-AS:{name}"));
    out.push("X-EVOLUTION-LIST:TRUE".to_string());
    out.push(format!("X-EVOLUTION-LIST-SHOW-ADDRESSES:{}", if e.hide_addresses { "FALSE" } else { "TRUE" }));
    for m in &e.members {
        if !m.line.is_empty() {
            out.push(m.line.clone());
            continue;
        }
        // A parameter value is quoted and cannot hold a quote.
        let param = |v: &str| v.chars().filter(|c| !matches!(c, '"' | '\r' | '\n')).collect::<String>();
        let mut line = String::from("EMAIL");
        if !m.name.trim().is_empty() {
            line.push_str(&format!(";X-EVOLUTION-DEST-NAME=\"{}\"", param(m.name.trim())));
        }
        line.push_str(&format!(";X-EVOLUTION-DEST-EMAIL=\"{}\"", param(&m.email)));
        line.push_str(&format!(
            ":{}",
            escape_vcard_value(&crate::worker::format_recipient(&m.name, &m.email))
        ));
        out.push(line);
    }
    out.push("END:VCARD".to_string());
    out.join("\r\n") + "\r\n"
}

/// A brand-new group's vCard from the editor's values.
pub fn new_group_vcard(e: &GroupEdit) -> String {
    patched_group_vcard(EMPTY_VCARD, e)
}

const EMPTY_VCARD: &str = "BEGIN:VCARD\r\nVERSION:3.0\r\nEND:VCARD\r\n";

/// Rewrite an Apple group (or write a new one, from [`EMPTY_VCARD`]) with
/// the editor's values. Every member must carry the UID of their card in
/// the group's book, which [`save_group`] sees to.
fn patched_apple_group(original: &str, e: &GroupEdit) -> String {
    const REPLACED: [&str; 5] =
        ["FN", "N", "X-ADDRESSBOOKSERVER-KIND", "X-ADDRESSBOOKSERVER-MEMBER", "X-EVOLUTION-LIST-SHOW-ADDRESSES"];
    let mut out = kept_lines(original, &REPLACED);
    let name = escape_vcard_value(e.name.trim());
    out.push(format!("FN:{name}"));
    out.push(format!("N:{name};;;;"));
    out.push("X-ADDRESSBOOKSERVER-KIND:group".to_string());
    // Apple's groups have no Bcc of their own; Evolution's line, which
    // other apps pass over, keeps the choice.
    if e.hide_addresses {
        out.push("X-EVOLUTION-LIST-SHOW-ADDRESSES:FALSE".to_string());
    }
    for m in &e.members {
        if !m.line.is_empty() {
            out.push(m.line.clone());
        } else if !m.uid.is_empty() {
            out.push(format!("X-ADDRESSBOOKSERVER-MEMBER:urn:uuid:{}", m.uid));
        }
    }
    out.extend(e.kept.iter().cloned());
    out.push("END:VCARD".to_string());
    out.join("\r\n") + "\r\n"
}

/// Write a group to `book_uid`: changed when `original` is its stored card,
/// else new. An Apple group holds cards of its own book, so a member with
/// no card there gets one first, with their name and address (blocking:
/// call from a background thread).
pub fn save_group(book_uid: &str, format: GroupFormat, original: Option<&str>, mut edit: GroupEdit) -> Result<(), String> {
    let create = original.is_none();
    let original = original.unwrap_or(EMPTY_VCARD);
    let vcard = match format {
        GroupFormat::Evolution => patched_group_vcard(original, &edit),
        GroupFormat::Apple => {
            let cardless =
                |m: &Member| m.uid.is_empty() && m.line.is_empty() && (!m.email.is_empty() || !m.card.is_empty());
            if edit.members.iter().any(cardless) {
                let mut cards = BookCards::open(book_uid)?;
                for m in edit.members.iter_mut().filter(|m| cardless(m)) {
                    m.uid = cards.card_for(&m.name, &m.email, &m.card)?;
                }
            }
            patched_apple_group(original, &edit)
        }
        GroupFormat::Kind => return Err(i18n("Hylki cannot change this kind of group")),
    };
    if create {
        create_contact(book_uid, &vcard)
    } else {
        modify_contact(book_uid, &vcard)
    }
}

/// Move a group to another book: made in `to_book` as `format` from `edit`,
/// then deleted from `from_book`, and only once made.
pub fn move_group(from_book: &str, uid: &str, to_book: &str, format: GroupFormat, edit: GroupEdit) -> Result<(), String> {
    save_group(to_book, format, None, edit)?;
    delete_contact(from_book, uid)
}

/// The people's cards of one book, for giving each group member one.
enum BookCards {
    Local,
    Eds { conn: zbus::blocking::Connection, bus: String, path: String },
}

impl BookCards {
    fn open(book_uid: &str) -> Result<Self, String> {
        if is_local_book(book_uid) {
            return Ok(Self::Local);
        }
        let dest = factory_dest().ok_or("Evolution Data Server is not available")?;
        let conn = zbus::blocking::Connection::session().map_err(|e| e.to_string())?;
        let (bus, path) = open_book(&conn, &dest, book_uid)?;
        Ok(Self::Eds { conn, bus, path })
    }

    /// The UID of the person in the book with `email` (or, with none, of
    /// that name), after making them a card when there is none: `copy`, the
    /// card of another book, else one with their name and address.
    fn card_for(&mut self, name: &str, email: &str, copy: &str) -> Result<String, String> {
        let card = if copy.is_empty() {
            new_vcard(&ContactEdit {
                name: if name.trim().is_empty() { email.to_string() } else { name.trim().to_string() },
                emails: vec![Labeled { label: String::new(), value: email.to_string() }],
                ..ContactEdit::default()
            })
        } else {
            copy.to_string()
        };
        // Someone with no address is known by name alone.
        let (field, value) = if email.trim().is_empty() { ("full_name", name.trim()) } else { ("email", email.trim()) };
        match self {
            Self::Local => {
                let found = crate::local_contacts::list_vcards()?.into_iter().find_map(|v| {
                    let c = parse_vcard_fields(&v).filter(|c| !c.is_group())?;
                    let same = if field == "email" {
                        c.emails.iter().any(|e| e.value.eq_ignore_ascii_case(value))
                    } else {
                        c.emails.is_empty() && c.name.trim().eq_ignore_ascii_case(value)
                    };
                    same.then_some(c.eds_uid)
                });
                if let Some(uid) = found.filter(|u| !u.is_empty()) {
                    return Ok(uid);
                }
                let uid = format!("hylki-{}", crate::rng::token(24).map_err(|e| e.to_string())?);
                let card = insert_after_version(&card, &format!("UID:{uid}"));
                write_local(crate::local_contacts::create(&card))?;
                Ok(uid)
            }
            Self::Eds { conn, bus, path } => {
                let query = format!("(is \"{field}\" \"{}\")", sexpr_escape(value));
                for uid in book_get_uids(conn, bus, path, &query)? {
                    let found = book_get_contact(conn, bus, path, &uid)?;
                    // By name, only someone else with no address either.
                    let fits = parse_vcard_fields(&found)
                        .is_some_and(|c| !c.is_group() && (field == "email" || c.emails.is_empty()));
                    if fits {
                        return Ok(uid);
                    }
                }
                let reply = book_call(conn, bus, path, "CreateContacts", &(vec![card], 0u32))?;
                let (uids,): (Vec<String>,) = reply.body().deserialize().map_err(|e| e.to_string())?;
                uids.into_iter().next().ok_or_else(|| "The address book made no contact".to_string())
            }
        }
    }
}

/// `line` put after a card's VERSION line (first, when it has none).
fn insert_after_version(card: &str, line: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut put = false;
    for l in card.split("\r\n").flat_map(|l| l.split('\n')) {
        out.push(l.to_string());
        if !put && l.to_ascii_uppercase().starts_with("VERSION:") {
            out.push(line.to_string());
            put = true;
        }
    }
    if !put {
        out.insert(1.min(out.len()), line.to_string());
    }
    out.join("\r\n")
}

/// Tell the photo index and the UI about a finished local write.
fn write_local(result: Result<(), String>) -> Result<(), String> {
    if result.is_ok() {
        local_changed();
    }
    result
}

/// Import `.vcf` files into the local Hylki book (blocking: call from a
/// background thread).
pub fn import_vcf_files(paths: &[PathBuf]) -> Result<crate::local_contacts::ImportOutcome, String> {
    let outcome = crate::local_contacts::import_files(paths)?;
    if outcome.added + outcome.updated > 0 {
        local_changed();
    }
    Ok(outcome)
}

/// Write a modified vCard back to its book through EDS.
pub fn modify_contact(book_uid: &str, vcard: &str) -> Result<(), String> {
    if is_local_book(book_uid) {
        return write_local(crate::local_contacts::modify(vcard));
    }
    let dest = factory_dest().ok_or("Evolution Data Server is not available")?;
    let conn = zbus::blocking::Connection::session().map_err(|e| e.to_string())?;
    let (bus, path) = open_book(&conn, &dest, book_uid)?;
    book_call(&conn, &bus, &path, "ModifyContacts", &(vec![vcard.to_string()], 0u32))?;
    Ok(())
}

/// Create a contact from a full vCard through EDS.
pub fn create_contact(book_uid: &str, vcard: &str) -> Result<(), String> {
    if is_local_book(book_uid) {
        return write_local(crate::local_contacts::create(vcard));
    }
    let dest = factory_dest().ok_or("Evolution Data Server is not available")?;
    let conn = zbus::blocking::Connection::session().map_err(|e| e.to_string())?;
    let (bus, path) = open_book(&conn, &dest, book_uid)?;
    book_call(&conn, &bus, &path, "CreateContacts", &(vec![vcard.to_string()], 0u32))?;
    Ok(())
}

/// Delete contacts (by their EDS UIDs) from one book, in one call.
pub fn delete_contacts(book_uid: &str, uids: &[String]) -> Result<(), String> {
    if is_local_book(book_uid) {
        for uid in uids {
            crate::local_contacts::delete(uid)?;
        }
        return write_local(Ok(()));
    }
    let dest = factory_dest().ok_or("Evolution Data Server is not available")?;
    let conn = zbus::blocking::Connection::session().map_err(|e| e.to_string())?;
    let (bus, path) = open_book(&conn, &dest, book_uid)?;
    book_call(&conn, &bus, &path, "RemoveContacts", &(uids.to_vec(), 0u32))?;
    Ok(())
}

/// A contact's card as a new one for another book: without the UID, the
/// revision and what the old book's server kept in it, and with its photo
/// in the card itself. A cached photo is a file of the old book, which goes
/// with the contact.
pub fn moved_vcard(c: &ContactDetails) -> String {
    use base64::Engine as _;
    let mut out = kept_lines_by(&c.raw_vcard, |prop| {
        matches!(prop, "UID" | "REV" | "PHOTO" | "X-IMAGETYPE" | "X-IMAGEHASH")
            || prop.starts_with("X-EVOLUTION-WEBDAV")
            || prop.starts_with("X-EVOLUTION-GOOGLE")
            || prop.starts_with("X-EVOLUTION-M365")
    });
    if let Some(photo) = &c.photo_bytes() {
        let kind = crate::models::image_ext(photo).to_ascii_uppercase();
        let kind = if kind == "JPG" { "JPEG".to_string() } else { kind };
        out.push(format!(
            "PHOTO;ENCODING=b;TYPE={kind}:{}",
            base64::engine::general_purpose::STANDARD.encode(photo)
        ));
    }
    out.push("END:VCARD".to_string());
    out.join("\r\n") + "\r\n"
}

/// Move a contact: made in `to_book` from `vcard` ([`moved_vcard`]), then
/// deleted from `from_book`, and only once the new one is made.
pub fn move_contact(from_book: &str, uid: &str, vcard: &str, to_book: &str) -> Result<(), String> {
    create_contact(to_book, vcard)?;
    delete_contact(from_book, uid)
}

/// Delete a contact (by its EDS UID) from its book.
pub fn delete_contact(book_uid: &str, uid: &str) -> Result<(), String> {
    if is_local_book(book_uid) {
        return write_local(crate::local_contacts::delete(uid));
    }
    let dest = factory_dest().ok_or("Evolution Data Server is not available")?;
    let conn = zbus::blocking::Connection::session().map_err(|e| e.to_string())?;
    let (bus, path) = open_book(&conn, &dest, book_uid)?;
    book_call(&conn, &bus, &path, "RemoveContacts", &(vec![uid.to_string()], 0u32))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn registry_reply_keeps_sources_and_skips_unrelated_objects() {
        use gtk::glib::{variant::ObjectPath, variant::ToVariant};
        use std::collections::HashMap;
        let path = ObjectPath::try_from("/org/gnome/evolution/dataserver/Source/test").unwrap();
        let data = "[Data Source]\nDisplayName=Test\n[Address Book]\nBackendName=local\n";
        let objects = HashMap::from([
            (path.clone(), HashMap::from([("org.gnome.evolution.dataserver.Source", HashMap::from([
                ("UID", "test".to_variant()), ("Data", data.to_variant()),
            ]))])),
            (ObjectPath::try_from("/unrelated").unwrap(), HashMap::new()),
        ]);
        assert_eq!(super::sources_from_reply(&(objects,).to_variant()), Some(vec![
            (path.to_string(), "test".into(), data.into()),
        ]));
        assert!(super::sources_from_reply(&"wrong reply type".to_variant()).is_none());
    }

    use super::{
        details_from_local_vcards, is_local_book, prefer_book, Book, merge_target, vcard_display_name, vcard_photo,
        vcard_photo_with,
    };

    const ONE_PIXEL_PNG: &str =
        "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=";

    // iCloud's CardDAV photos: a quoted https URL parameter precedes the real
    // file value — the split must not stop at the colon inside the quotes.
    #[test]
    fn quoted_params_do_not_swallow_the_value() {
        let line = "PHOTO;X-EVOLUTION-WEBDAV-IMG-URL=\"https://gateway.icloud.com/x/y\";\
                    VALUE=uri:file:///tmp/photo.jpg";
        let (prop, value) = super::split_vcard_line(line).unwrap();
        assert!(prop.ends_with("VALUE=uri"));
        assert_eq!(value, "file:///tmp/photo.jpg");
    }

    #[test]
    fn fn_preserves_case() {
        let v = "BEGIN:VCARD\r\nVERSION:3.0\r\nN:Arnwine;Aaron;;;\r\nFN:Aaron Arnwine\r\nEND:VCARD\r\n";
        assert_eq!(vcard_display_name(v).as_deref(), Some("Aaron Arnwine"));
    }

    #[test]
    fn fn_with_parameters() {
        let v = "FN;PID=1.1:Jane O'Brien\r\n";
        assert_eq!(vcard_display_name(v).as_deref(), Some("Jane O'Brien"));
    }

    #[test]
    fn fn_line_folded() {
        // A folded FN value continues on the next line after a leading space.
        let v = "FN:Reallylongfirst\r\n Lastname\r\n";
        assert_eq!(vcard_display_name(v).as_deref(), Some("ReallylongfirstLastname"));
    }

    #[test]
    fn fn_escaped_comma() {
        let v = "FN:Smith\\, John\r\n";
        assert_eq!(vcard_display_name(v).as_deref(), Some("Smith, John"));
    }

    #[test]
    fn empty_or_missing_fn() {
        assert_eq!(vcard_display_name("FN:\r\nN:x;;;;\r\n"), None);
        assert_eq!(vcard_display_name("N:Doe;John;;;\r\n"), None);
    }

    #[test]
    fn photo_decodes_nextcloud_data_uri() {
        let vcard = format!("item1.PHOTO:data:image/image/png\\;base64\\,{ONE_PIXEL_PNG}\r\n");
        let photo = vcard_photo(&vcard).unwrap();
        assert_eq!(&photo[..8], b"\x89PNG\r\n\x1a\n");
    }

    #[test]
    fn photo_decodes_folded_encoding_b() {
        let split = ONE_PIXEL_PNG.len() / 2;
        let vcard = format!(
            "PHOTO;ENCODING=b;TYPE=PNG:{}\r\n {}\r\n",
            &ONE_PIXEL_PNG[..split],
            &ONE_PIXEL_PNG[split..]
        );
        assert!(vcard_photo(&vcard).is_some());
    }

    #[test]
    fn photo_does_not_fetch_remote_uri() {
        assert!(vcard_photo("PHOTO;VALUE=uri:https://example.com/tracker.png\r\n").is_none());
    }

    /// A scratch directory tree for exercising [`super::confine_to_roots`]:
    /// `<tmp>/vireo-confine-<pid>-<n>/{root,outside}` with one file in each,
    /// removed on drop.
    struct ConfineFixture {
        base: std::path::PathBuf,
    }

    impl ConfineFixture {
        fn new(n: u32) -> Self {
            let base = std::env::temp_dir()
                .join(format!("hylki-confine-{}-{n}", std::process::id()));
            std::fs::create_dir_all(base.join("root")).unwrap();
            std::fs::create_dir_all(base.join("outside")).unwrap();
            std::fs::write(base.join("root/photo.png"), b"in").unwrap();
            std::fs::write(base.join("outside/secret"), b"out").unwrap();
            Self { base }
        }

        fn root(&self) -> std::path::PathBuf {
            self.base.join("root")
        }
    }

    impl Drop for ConfineFixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.base);
        }
    }

    #[test]
    fn photo_files_inside_a_root_are_admitted() {
        let fx = ConfineFixture::new(1);
        let roots = vec![fx.root()];
        let admitted = super::confine_to_roots(&fx.root().join("photo.png"), &roots);
        assert!(admitted.is_some());
    }

    #[test]
    fn dotdot_traversal_cannot_leave_a_root() {
        let fx = ConfineFixture::new(2);
        let roots = vec![fx.root()];
        let escape = fx.root().join("../outside/secret");
        assert_eq!(super::confine_to_roots(&escape, &roots), None);
    }

    #[test]
    fn a_symlink_planted_inside_a_root_cannot_reach_outside() {
        let fx = ConfineFixture::new(3);
        let roots = vec![fx.root()];
        let link = fx.root().join("link.png");
        std::os::unix::fs::symlink(fx.base.join("outside/secret"), &link).unwrap();
        assert_eq!(super::confine_to_roots(&link, &roots), None);
    }

    #[test]
    fn a_lookalike_sibling_directory_is_not_a_root() {
        let fx = ConfineFixture::new(4);
        let sibling = fx.base.join("root-evil");
        std::fs::create_dir_all(&sibling).unwrap();
        std::fs::write(sibling.join("photo.png"), b"no").unwrap();
        let roots = vec![fx.root()];
        assert_eq!(super::confine_to_roots(&sibling.join("photo.png"), &roots), None);
    }

    #[test]
    fn no_roots_admits_nothing() {
        let fx = ConfineFixture::new(5);
        assert_eq!(super::confine_to_roots(&fx.root().join("photo.png"), &[]), None);
        // A root that doesn't resolve is the same as no root at all.
        let ghost = vec![fx.base.join("missing")];
        assert_eq!(super::confine_to_roots(&fx.root().join("photo.png"), &ghost), None);
    }

    #[test]
    fn a_host_bearing_file_uri_is_refused() {
        assert_eq!(
            super::read_eds_photo_file("file://evil-host/etc/passwd", 1024),
            None
        );
    }

    /// A birthday is written as mail dates are, so the words and the order
    /// are the locale's; a vCard value that is no date passes through.
    #[test]
    fn birthdays_are_written_as_dates() {
        let full = super::pretty_birthday("1985-04-12");
        assert!(full.contains("1985") && full.contains("12"), "{full}");
        assert_eq!(super::pretty_birthday("19850412"), full);
        let leap = super::pretty_birthday("--02-29");
        assert!(leap.contains("29") && !leap.contains("2000"), "{leap}");
        assert_eq!(super::pretty_birthday("1985-02-30"), "1985-02-30");
        assert_eq!(super::pretty_birthday("sometime"), "sometime");
    }

    /// EDS `.source` files are key files: a book's name comes from its
    /// account collection, and only `[Data Source]`'s Enabled switches it off.
    #[test]
    fn eds_sources_are_read_as_key_files() {
        let book = "[Data Source]\nDisplayName=Contacts\nEnabled=true\nParent=acct\n\n\
                    [Address Book]\nBackendName=carddav\n\n[Refresh]\nEnabled=false\n";
        let acct = "[Data Source]\nDisplayName=Google\n\n[Collection]\nBackendName=google\n\
                    Identity=someone@gmail.com\n";
        let sources: std::collections::HashMap<String, String> =
            [("acct".to_string(), acct.to_string())].into();
        assert_eq!(super::book_display_name(book, &sources), "Google — someone@gmail.com · Contacts");
        assert!(!super::data_source_disabled(book), "[Refresh] Enabled=false is not the switch");
        // Nextcloud: several books under one account get distinct names…
        let nc = "[Data Source]\nDisplayName=alice@cloud.example\n\n[Collection]\nBackendName=webdav\n\
                  Identity=alice\n";
        let sources: std::collections::HashMap<String, String> = [("acct".to_string(), nc.to_string())].into();
        let recent = book.replace("DisplayName=Contacts", "DisplayName=Recently contacted");
        assert_eq!(super::book_display_name(book, &sources), "CardDAV — alice · Contacts");
        assert_eq!(super::book_display_name(&recent, &sources), "CardDAV — alice · Recently contacted");
        // …while a standalone book (no parent) isn't named twice.
        let lone = "[Data Source]\nDisplayName=Work\n\n[Address Book]\nBackendName=carddav\n";
        assert_eq!(super::book_display_name(lone, &sources), "CardDAV — Work");
        assert!(super::data_source_disabled(&book.replace("Enabled=true", "Enabled=false")));
        assert_eq!(crate::platform::keyfile_value(book, "Address Book", "Missing"), None);
    }

    fn card(name: &str, email: &str) -> String {
        format!("BEGIN:VCARD\r\nVERSION:3.0\r\nUID:{name}\r\nFN:{name}\r\nEMAIL:{email}\r\nEND:VCARD\r\n")
    }

    #[test]
    fn books_that_fill_themselves_from_mail_are_spotted() {
        assert!(super::is_auto_collected("contact-collected", ""));
        assert!(super::is_auto_collected("x1", "Collected Addresses"));
        assert!(super::is_auto_collected("x2", "CardDAV \u{2014} alice · Recently contacted"));
        assert!(super::is_auto_collected("x3", "Google \u{2014} me@gmail.com · Other contacts"));
        assert!(!super::is_auto_collected("system-address-book", "On This Computer"));
        assert!(!super::is_auto_collected("x4", "CardDAV \u{2014} alice · Contacts"));
    }

    #[test]
    fn the_chosen_book_comes_first() {
        let book = |uid: &str| Book { uid: uid.into(), name: uid.into(), groups: None };
        let mut books = vec![book("a"), book("b"), book("hylki-local")];
        prefer_book(&mut books, "hylki-local");
        assert_eq!(books.iter().map(|b| b.uid.as_str()).collect::<Vec<_>>(), ["hylki-local", "a", "b"]);
        prefer_book(&mut books, "gone");
        prefer_book(&mut books, "");
        assert_eq!(books[0].uid, "hylki-local");
    }

    #[test]
    fn only_the_reserved_uid_is_local() {
        assert!(is_local_book("hylki-local"));
        assert!(!is_local_book("system-address-book"));
    }

    #[test]
    fn local_details_carry_the_book_and_the_raw_vcard() {
        let v = card("Ann", "ann@x.org");
        let out = details_from_local_vcards(vec![v.clone()], "Hylki");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].book_uid, "hylki-local");
        assert_eq!(out[0].book_name, "Hylki");
        assert_eq!(out[0].eds_uid, "Ann");
        assert_eq!(out[0].raw_vcard, v);
    }

    #[test]
    fn same_email_in_two_books_is_kept_per_book() {
        // The EDS reader de-duplicates among EDS books by first email; the
        // local list is built on its own, so a shared address is not dropped.
        let a = details_from_local_vcards(vec![card("Ann", "same@x.org")], "Hylki");
        let b = details_from_local_vcards(vec![card("Ann", "same@x.org")], "Hylki");
        assert_eq!(a[0].primary_email(), b[0].primary_email());
        assert_eq!(a.len() + b.len(), 2);
    }

    #[test]
    fn local_vcards_never_read_file_photos() {
        let v = "BEGIN:VCARD\r\nFN:Ann\r\nPHOTO;VALUE=uri:file:///etc/hostname\r\nEND:VCARD\r\n";
        assert!(vcard_photo_with(v, false).is_none());
    }

    #[test]
    fn merge_target_matches_the_name_ignoring_case() {
        let vcards = vec![card("Ann Lee", "ann@x.org"), card("Bob", "bob@x.org")];
        assert!(merge_target(&vcards, " ann lee ").unwrap().contains("ann@x.org"));
        assert!(merge_target(&vcards, "Carl").is_none());
        assert!(merge_target(&vcards, "  ").is_none());
    }

    /// An Evolution list as EDS 3.60 stores it: folded inside a parameter,
    /// commas escaped in quoted values too.
    const EDS_LIST: &str = "BEGIN:VCARD\r\nVERSION:3.0\r\nUID:l1\r\nFN:Book Club\r\nN:;;;;\r\n\
        X-EVOLUTION-FILE-AS:Book Club\r\nX-EVOLUTION-LIST:TRUE\r\nX-EVOLUTION-LIST-SHOW-ADDRESSES:FALSE\r\n\
        EMAIL;X-EVOLUTION-DEST-HTML-MAIL=FALSE;X-EVOLUTION-DEST-EMAIL=\"alice@exampl\r\n e.org\";\
        X-EVOLUTION-DEST-NAME=\"Alice Able\";X-EVOLUTION-DEST-EMAIL-NUM=0;X-E\r\n VOLUTION-DEST-CONTACT-UID=p1:\
        Alice Able <alice@example.org>\r\n\
        EMAIL;X-EVOLUTION-DEST-EMAIL=\"bob@example.org\";X-EVOLUTION-DEST-NAME=\"Bob\\,\r\n  Jr.\":\
        \"Bob\\, Jr.\" <bob@example.org>\r\nREV:2026-10-17T12:15:22Z\r\nEND:VCARD\r\n";

    fn people(c: &super::ContactDetails) -> Vec<(&str, &str)> {
        c.members.iter().map(|m| (m.name.as_str(), m.email.as_str())).collect()
    }

    #[test]
    fn an_evolution_list_is_a_group_of_its_members() {
        let c = super::parse_vcard_fields(EDS_LIST).unwrap();
        assert_eq!(c.group, Some(super::GroupFormat::Evolution));
        assert_eq!(c.name, "Book Club");
        assert_eq!(people(&c), vec![("Alice Able", "alice@example.org"), ("Bob, Jr.", "bob@example.org")]);
        // Its members are not its own addresses.
        assert!(c.emails.is_empty());
        assert!(c.hide_addresses);
        assert!(c.group_editable());
        assert!(super::is_group_vcard(EDS_LIST));
        let shown = EDS_LIST.replace("SHOW-ADDRESSES:FALSE", "SHOW-ADDRESSES:TRUE");
        assert!(!super::parse_vcard_fields(&shown).unwrap().hide_addresses);
    }

    #[test]
    fn a_member_whose_value_does_not_parse_comes_from_its_parameters() {
        let v = "BEGIN:VCARD\r\nFN:L\r\nX-EVOLUTION-LIST:TRUE\r\n\
                 EMAIL;X-EVOLUTION-DEST-NAME=\"Ann\";X-EVOLUTION-DEST-EMAIL=\"ann@x.org\":Ann\r\nEND:VCARD\r\n";
        assert_eq!(people(&super::parse_vcard_fields(v).unwrap()), vec![("Ann", "ann@x.org")]);
    }

    #[test]
    fn a_group_needs_a_name() {
        let v = "BEGIN:VCARD\r\nX-EVOLUTION-LIST:TRUE\r\nEMAIL:ann@x.org\r\nEND:VCARD\r\n";
        assert!(super::parse_vcard_fields(v).is_none());
    }

    #[test]
    fn kind_and_apple_groups_find_their_members_in_their_book() {
        let person = "BEGIN:VCARD\r\nVERSION:3.0\r\nUID:p1\r\nFN:Pat Doe\r\nEMAIL:pat@x.org\r\nEND:VCARD\r\n";
        let kind = "BEGIN:VCARD\r\nVERSION:4.0\r\nUID:g1\r\nKIND:group\r\nFN:Team\r\n\
                    MEMBER:mailto:ann@x.org\r\nMEMBER:urn:uuid:p1\r\nMEMBER:urn:uuid:gone\r\nEND:VCARD\r\n";
        let apple = "BEGIN:VCARD\r\nVERSION:3.0\r\nUID:g2\r\nX-ADDRESSBOOKSERVER-KIND:group\r\nN:Family\r\n\
                     FN:Family\r\nX-ADDRESSBOOKSERVER-MEMBER:urn:uuid:p1\r\nEND:VCARD\r\n";
        let mut book: Vec<super::ContactDetails> =
            [person, kind, apple].iter().map(|v| super::parse_vcard_fields(v).unwrap()).collect();
        super::resolve_book_members(&mut book);
        assert_eq!(people(&book[1]), vec![("", "ann@x.org"), ("Pat Doe", "pat@x.org")]);
        assert_eq!(people(&book[2]), vec![("Pat Doe", "pat@x.org")]);
        // Neither has Evolution's Bcc switch; Hylki edits Apple's only.
        assert!(!book[1].hide_addresses && !book[1].group_editable());
        assert!(!book[2].hide_addresses && book[2].group_editable());
        assert_eq!(book[2].group, Some(super::GroupFormat::Apple));
        // The member with no card in the book is kept, out of sight.
        assert_eq!(book[1].hidden_members, vec!["MEMBER:urn:uuid:gone".to_string()]);
        assert!(super::is_group_vcard(apple));
        assert!(!super::is_group_vcard(person));
    }

    #[test]
    fn a_moved_card_leaves_its_old_book_behind() {
        let c = super::ContactDetails {
            raw_vcard: "BEGIN:VCARD\r\nVERSION:3.0\r\nUID:abc\r\nFN:Pat Doe\r\nEMAIL:pat@x.org\r\n\
                        PHOTO;VALUE=uri:file:///home/u/.cache/evolution/addressbook/b/photos/abc.jpg\r\n\
                        X-EVOLUTION-WEBDAV-ETAG:k4lwse3r\r\nREV:2026-10-18T09:00:00Z\r\nNOTE:Long note that\r\n \
                        goes on\r\nEND:VCARD\r\n"
                .into(),
            photo: Some(vec![0x89, b'P', b'N', b'G', 1, 2]),
            ..Default::default()
        };
        let out = super::moved_vcard(&c);
        assert!(!out.contains("UID:") && !out.contains("REV:") && !out.contains("WEBDAV"), "{out}");
        assert!(!out.contains("file://"), "{out}");
        assert!(out.contains("PHOTO;ENCODING=b;TYPE=PNG:iVBORwEC\r\n"), "{out}");
        assert!(out.contains("NOTE:Long note that\r\n goes on\r\n"), "{out}");
        assert!(out.ends_with("END:VCARD\r\n"));
        let back = super::parse_vcard_fields(&out).unwrap();
        assert_eq!((back.name.as_str(), back.primary_email()), ("Pat Doe", Some("pat@x.org")));
    }

    #[test]
    fn an_apple_group_shows_people_with_no_address() {
        let person = "BEGIN:VCARD\r\nVERSION:3.0\r\nUID:p1\r\nFN:Pat Doe\r\nTEL:+1 555 0100\r\nEND:VCARD\r\n";
        let mailed = "BEGIN:VCARD\r\nVERSION:3.0\r\nUID:p2\r\nFN:Kim\r\nEMAIL:kim@x.org\r\nEND:VCARD\r\n";
        let apple = "BEGIN:VCARD\r\nVERSION:3.0\r\nUID:g\r\nX-ADDRESSBOOKSERVER-KIND:group\r\nFN:Mission\r\n\
                     X-ADDRESSBOOKSERVER-MEMBER:urn:uuid:p1\r\nX-ADDRESSBOOKSERVER-MEMBER:urn:uuid:p2\r\n\
                     X-ADDRESSBOOKSERVER-MEMBER:urn:uuid:p1\r\nEND:VCARD\r\n";
        let mut book: Vec<super::ContactDetails> =
            [person, mailed, apple].iter().map(|v| super::parse_vcard_fields(v).unwrap()).collect();
        super::resolve_book_members(&mut book);
        let g = &book[2];
        assert_eq!(people(g), vec![("Pat Doe", ""), ("Kim", "kim@x.org")]);
        assert_eq!(g.hidden_members.len(), 1, "the repeated card is kept, once shown");
        // Mail goes to those with an address only.
        assert_eq!(g.members_field(), "Kim <kim@x.org>");
        assert_eq!(super::group_suggestions(vec![g.clone()])[0].group.as_ref().unwrap().members.len(), 1);
    }

    #[test]
    fn an_edited_apple_group_names_its_members_by_card() {
        let apple = "BEGIN:VCARD\r\nVERSION:3.0\r\nPRODID:-//Apple Inc.//iOS 18//EN\r\nUID:g2\r\n\
                     X-ADDRESSBOOKSERVER-KIND:group\r\nN:Family\r\nFN:Family\r\n\
                     X-ADDRESSBOOKSERVER-MEMBER:urn:uuid:p1\r\nX-ADDRESSBOOKSERVER-MEMBER:urn:uuid:p2\r\n\
                     X-ADDRESSBOOKSERVER-MEMBER:urn:uuid:nomail\r\nREV:2026-10-18T09:00:00Z\r\nEND:VCARD\r\n";
        let mut g = super::parse_vcard_fields(apple).unwrap();
        super::resolve_members(&mut g, |uid| match uid {
            "p1" => Some(("Pat".into(), "pat@x.org".into())),
            "p2" => Some(("Kim".into(), "kim@x.org".into())),
            _ => None,
        });
        assert_eq!(g.members[0].uid, "p1");
        // Kim leaves, Lee (whose card save_group made) joins, Bcc goes on.
        let mut members = vec![g.members[0].clone()];
        members.push(super::Member { name: "Lee".into(), email: "lee@x.org".into(), uid: "p3".into(), ..Default::default() });
        let edit = super::GroupEdit { name: "Family; Close".into(), members, hide_addresses: true, kept: g.hidden_members.clone() };
        let out = super::patched_apple_group(apple, &edit);
        assert!(out.contains("\r\nUID:g2\r\n") && out.contains("PRODID:-//Apple Inc.//iOS 18//EN"), "{out}");
        assert!(out.contains("X-ADDRESSBOOKSERVER-MEMBER:urn:uuid:p1\r\n"), "{out}");
        assert!(out.contains("X-ADDRESSBOOKSERVER-MEMBER:urn:uuid:p3\r\n"), "{out}");
        assert!(out.contains("X-ADDRESSBOOKSERVER-MEMBER:urn:uuid:nomail\r\n"), "{out}");
        assert!(!out.contains("urn:uuid:p2"), "{out}");
        assert_eq!(out.matches("X-ADDRESSBOOKSERVER-KIND:group").count(), 1);
        assert!(!out.contains("X-EVOLUTION-LIST:"), "{out}");
        let back = super::parse_vcard_fields(&out).unwrap();
        assert_eq!(back.name, "Family; Close");
        assert_eq!(back.group, Some(super::GroupFormat::Apple));
        assert!(back.hide_addresses);
        let refs: Vec<&str> = back.member_refs.iter().map(|(uid, _)| uid.as_str()).collect();
        assert_eq!(refs, vec!["p1", "p3", "nomail"]);

        // A new one, with Bcc off, says nothing of it.
        let edit = super::GroupEdit { name: "Team".into(), hide_addresses: false, ..edit };
        let new = super::patched_apple_group(super::EMPTY_VCARD, &edit);
        assert!(new.starts_with("BEGIN:VCARD\r\nVERSION:3.0\r\nFN:Team\r\nN:Team;;;;\r\n"), "{new}");
        assert!(!new.contains("SHOW-ADDRESSES") && !new.contains("UID:"), "{new}");
    }

    #[test]
    fn books_keep_groups_as_their_servers_do() {
        use super::{group_format_for, GroupFormat};
        assert_eq!(group_format_for(Some("local")), Some(GroupFormat::Evolution));
        assert_eq!(group_format_for(None), Some(GroupFormat::Evolution));
        assert_eq!(group_format_for(Some("webdav")), Some(GroupFormat::Apple));
        assert_eq!(group_format_for(Some("carddav")), Some(GroupFormat::Apple));
        assert_eq!(group_format_for(Some("google")), None);
        assert_eq!(group_format_for(Some("microsoft365")), None);
    }

    #[test]
    fn an_edited_list_keeps_the_lines_of_the_members_it_keeps() {
        let c = super::parse_vcard_fields(EDS_LIST).unwrap();
        let mut members = vec![c.members[0].clone()];
        members.push(super::Member { name: "Carol \"CJ\", Ng".into(), email: "carol@x.org".into(), ..Default::default() });
        let edit = super::GroupEdit { name: "Readers, All".into(), members, hide_addresses: false, kept: Vec::new() };
        let out = super::patched_group_vcard(EDS_LIST, &edit);
        assert!(out.contains("X-EVOLUTION-DEST-CONTACT-UID=p1"), "{out}");
        assert!(out.contains("\r\nUID:l1\r\n") && out.contains("\r\nREV:"), "{out}");
        assert!(!out.contains("bob@example.org"), "{out}");
        assert_eq!(out.matches("X-EVOLUTION-LIST:TRUE").count(), 1);
        let back = super::parse_vcard_fields(&out).unwrap();
        assert_eq!(back.name, "Readers, All");
        assert!(!back.hide_addresses);
        assert_eq!(people(&back), vec![("Alice Able", "alice@example.org"), ("Carol \"CJ\", Ng", "carol@x.org")]);
    }

    #[test]
    fn a_new_group_reads_back_as_written() {
        let edit = super::GroupEdit {
            name: "Book Club".into(),
            members: vec![super::Member { name: "Ann".into(), email: "ann@x.org".into(), ..Default::default() }],
            hide_addresses: true,
            kept: Vec::new(),
        };
        let back = super::parse_vcard_fields(&super::new_group_vcard(&edit)).unwrap();
        assert_eq!(back.group, Some(super::GroupFormat::Evolution));
        assert!(back.hide_addresses);
        assert_eq!(people(&back), vec![("Ann", "ann@x.org")]);
        assert_eq!(back.members_field(), "Ann <ann@x.org>");
    }

    #[test]
    fn a_group_is_suggested_once_and_only_with_someone_in_it() {
        let g = |name: &str, emails: &[&str]| super::ContactDetails {
            name: name.into(),
            group: Some(super::GroupFormat::Evolution),
            members: emails.iter().map(|e| super::Member { email: e.to_string(), ..Default::default() }).collect(),
            ..Default::default()
        };
        let out = super::group_suggestions(vec![
            g("Team", &["a@x.org", "b@x.org"]),
            g("team", &["B@x.org", "a@x.org"]),
            g("Empty", &[]),
            g("Team", &["c@x.org"]),
        ]);
        assert_eq!(out.len(), 2);
        assert!(out.iter().all(|s| s.email.is_empty() && s.group.is_some()));
        // Only the name finds a group, not its members' addresses.
        assert!(out[0].matches("tea") && !out[0].matches("a@x"));
    }

    /// GROUP_LIVE=<book uid>: make an Apple group there through EDS with
    /// one new member, change it, and remove what it made (GROUP_KEEP=1
    /// leaves it). For a sealed rig or a book the tester owns.
    #[test]
    #[ignore]
    fn live_group() {
        let Ok(book) = std::env::var("GROUP_LIVE") else { return };
        // "apple": the first book that keeps Apple's groups.
        let book = if book == "apple" {
            let books = super::writable_books();
            println!("books: {books:?}");
            books.into_iter().find(|b| b.groups == Some(super::GroupFormat::Apple)).expect("a CardDAV book").uid
        } else {
            book
        };
        let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
        let name = format!("Hylki group test {stamp} (delete me)");
        let member = |n: &str, e: &str| super::Member { name: n.into(), email: e.into(), ..Default::default() };
        let edit = super::GroupEdit {
            name: name.clone(),
            members: vec![member("Hylki Test Member (delete me)", &format!("hylki-test-{stamp}@example.com"))],
            hide_addresses: true,
            kept: Vec::new(),
        };
        super::save_group(&book, super::GroupFormat::Apple, None, edit).unwrap();
        let dest = super::factory_dest().unwrap();
        let conn = zbus::blocking::Connection::session().unwrap();
        let (bus, path) = super::open_book(&conn, &dest, &book).unwrap();
        let find = |q: String| super::book_get_uids(&conn, &bus, &path, &q).unwrap();
        let group_uid = find(format!("(is \"full_name\" \"{name}\")")).pop().expect("the group");
        let card = super::book_get_contact(&conn, &bus, &path, &group_uid).unwrap();
        println!("=== made:\n{card}");
        let stub = find(format!("(is \"email\" \"hylki-test-{stamp}@example.com\")")).pop().expect("the member's card");
        println!("=== member's card:\n{}", super::book_get_contact(&conn, &bus, &path, &stub).unwrap());
        let mut g = super::parse_vcard_fields(&card).unwrap();
        assert_eq!(g.group, Some(super::GroupFormat::Apple));
        assert!(g.hide_addresses);
        assert_eq!(g.member_refs.iter().map(|(u, _)| u.as_str()).collect::<Vec<_>>(), vec![stub.as_str()]);
        super::resolve_members(&mut g, |uid| {
            super::person_address(&super::book_get_contact(&conn, &bus, &path, uid).ok()?)
        });
        // Bcc off and a second member, who already has a card by then.
        let mut members = g.members.clone();
        members.push(member("Again", &format!("hylki-test-{stamp}@example.com")));
        members.dedup_by(|a, b| a.email == b.email);
        let edit = super::GroupEdit { name: name.clone(), members, hide_addresses: false, kept: g.hidden_members.clone() };
        super::save_group(&book, super::GroupFormat::Apple, Some(&card), edit).unwrap();
        let changed = super::book_get_contact(&conn, &bus, &path, &group_uid).unwrap();
        println!("=== changed:\n{changed}");
        let back = super::parse_vcard_fields(&changed).unwrap();
        assert!(!back.hide_addresses);
        assert_eq!(back.member_refs.len(), 1);
        if std::env::var_os("GROUP_KEEP").is_some() {
            return;
        }
        super::delete_contact(&book, &group_uid).unwrap();
        super::delete_contact(&book, &stub).unwrap();
        println!("removed {group_uid} and {stub}");
    }

    /// MOVE_LIVE=1: move a contact with a photo between the first two books
    /// that keep Apple's groups (a sealed rig's CardDAV books), and remove it.
    #[test]
    #[ignore]
    fn live_move() {
        if std::env::var_os("MOVE_LIVE").is_none() {
            return;
        }
        let books: Vec<String> = super::writable_books()
            .into_iter()
            .filter(|b| b.groups == Some(super::GroupFormat::Apple))
            .map(|b| b.uid)
            .collect();
        let (from, to) = (&books[0], &books[1]);
        let card = format!(
            "BEGIN:VCARD\r\nVERSION:3.0\r\nFN:Hylki Move Test\r\nN:Test;Hylki Move;;;\r\nEMAIL:move-test@example.com\r\n\
             PHOTO;ENCODING=b;TYPE=PNG:{ONE_PIXEL_PNG}\r\nEND:VCARD\r\n"
        );
        super::create_contact(from, &card).unwrap();
        let dest = super::factory_dest().unwrap();
        let conn = zbus::blocking::Connection::session().unwrap();
        let find = |book: &str| {
            let (bus, path) = super::open_book(&conn, &dest, book).unwrap();
            let uids = super::book_get_uids(&conn, &bus, &path, "(is \"email\" \"move-test@example.com\")").unwrap();
            uids.into_iter().map(|u| (u.clone(), super::book_get_contact(&conn, &bus, &path, &u).unwrap())).collect::<Vec<_>>()
        };
        let (uid, stored) = find(from).pop().expect("made in the first book");
        let mut c = super::parse_vcard_details(&stored).unwrap();
        c.raw_vcard = stored;
        println!("photo bytes: {:?}", c.photo.as_ref().map(Vec::len));
        let moved = super::moved_vcard(&c);
        super::move_contact(from, &uid, &moved, to).unwrap();
        assert!(find(from).is_empty(), "gone from the first book");
        let (new_uid, there) = find(to).pop().expect("in the second book");
        println!("=== in the second book:\n{there}");
        assert!(super::parse_vcard_details(&there).unwrap().photo.is_some());
        super::delete_contact(to, &new_uid).unwrap();
    }

    /// MOVE_LIVE=1: a contact with only a phone number, in one book, joins
    /// a new Apple group in another: copied there whole, then named in it.
    #[test]
    #[ignore]
    fn live_copy_into_group() {
        if std::env::var_os("MOVE_LIVE").is_none() {
            return;
        }
        let books: Vec<String> = super::writable_books()
            .into_iter()
            .filter(|b| b.groups == Some(super::GroupFormat::Apple))
            .map(|b| b.uid)
            .collect();
        let (from, to) = (&books[0], &books[1]);
        let card = "BEGIN:VCARD\r\nVERSION:3.0\r\nFN:Hylki Phone Only\r\nN:Only;Hylki Phone;;;\r\n\
                    TEL;TYPE=CELL:+1 555 0199\r\nNOTE:copied whole\r\nEND:VCARD\r\n";
        let dest = super::factory_dest().unwrap();
        let conn = zbus::blocking::Connection::session().unwrap();
        let find = |book: &str, q: &str| {
            let (bus, path) = super::open_book(&conn, &dest, book).unwrap();
            super::book_get_uids(&conn, &bus, &path, q)
                .unwrap()
                .into_iter()
                .map(|u| (u.clone(), super::book_get_contact(&conn, &bus, &path, &u).unwrap()))
                .collect::<Vec<_>>()
        };
        let by_name = "(is \"full_name\" \"Hylki Phone Only\")";
        // What an earlier run left.
        for book in [from, to] {
            for q in [by_name, "(beginswith \"full_name\" \"Hylki copy test\")"] {
                for (uid, _) in find(book, q) {
                    super::delete_contact(book, &uid).unwrap();
                }
            }
        }
        super::create_contact(from, card).unwrap();
        let (src_uid, stored) = find(from, by_name).pop().expect("made");
        let mut c = super::parse_vcard_details(&stored).unwrap();
        c.raw_vcard = stored;
        let member = super::Member { name: c.name.clone(), card: super::moved_vcard(&c), ..Default::default() };
        let edit = super::GroupEdit { name: "Hylki copy test".into(), members: vec![member], ..Default::default() };
        super::save_group(to, super::GroupFormat::Apple, None, edit.clone()).unwrap();
        let (copy_uid, copy) = find(to, by_name).pop().expect("copied to the group's book");
        println!("=== the copy:\n{copy}");
        assert!(copy.contains("+1 555 0199") && copy.contains("copied whole"));
        let (group_uid, group) = find(to, "(is \"full_name\" \"Hylki copy test\")").pop().expect("the group");
        let refs = super::parse_vcard_fields(&group).unwrap().member_refs;
        assert_eq!(refs.iter().map(|(u, _)| u.as_str()).collect::<Vec<_>>(), vec![copy_uid.as_str()], "{group}");
        // Joining another group finds the copy, by name, and makes none.
        super::save_group(to, super::GroupFormat::Apple, None, super::GroupEdit { name: "Hylki copy test 2".into(), ..edit }).unwrap();
        assert_eq!(find(to, by_name).len(), 1);
        let (group2, _) = find(to, "(is \"full_name\" \"Hylki copy test 2\")").pop().unwrap();
        for (book, uid) in [(to, &group_uid), (to, &group2), (to, &copy_uid), (from, &src_uid)] {
            super::delete_contact(book, uid).unwrap();
        }
    }

    /// MOVE_LIVE=1: an Apple group, with a member who has only a phone
    /// number, moves to another book: made there with the member copied,
    /// gone from the first.
    #[test]
    #[ignore]
    fn live_move_group() {
        if std::env::var_os("MOVE_LIVE").is_none() {
            return;
        }
        let books: Vec<String> = super::writable_books()
            .into_iter()
            .filter(|b| b.groups == Some(super::GroupFormat::Apple))
            .map(|b| b.uid)
            .collect();
        let (from, to) = (&books[0], &books[1]);
        let dest = super::factory_dest().unwrap();
        let conn = zbus::blocking::Connection::session().unwrap();
        let find = |book: &str, q: &str| {
            let (bus, path) = super::open_book(&conn, &dest, book).unwrap();
            super::book_get_uids(&conn, &bus, &path, q)
                .unwrap()
                .into_iter()
                .map(|u| (u.clone(), super::book_get_contact(&conn, &bus, &path, &u).unwrap()))
                .collect::<Vec<_>>()
        };
        let person_q = "(is \"full_name\" \"Hylki Mover\")";
        let group_q = "(is \"full_name\" \"Hylki moving group\")";
        for book in [from, to] {
            for q in [person_q, group_q] {
                for (uid, _) in find(book, q) {
                    super::delete_contact(book, &uid).unwrap();
                }
            }
        }
        super::create_contact(from, "BEGIN:VCARD\r\nVERSION:3.0\r\nFN:Hylki Mover\r\nTEL:+1 555 0142\r\nEND:VCARD\r\n").unwrap();
        let (person_uid, stored) = find(from, person_q).pop().unwrap();
        let mut person = super::parse_vcard_details(&stored).unwrap();
        person.raw_vcard = stored;
        let member = super::Member { name: person.name.clone(), uid: person_uid.clone(), ..Default::default() };
        let edit = super::GroupEdit { name: "Hylki moving group".into(), members: vec![member], ..Default::default() };
        super::save_group(from, super::GroupFormat::Apple, None, edit).unwrap();
        let (group_uid, _) = find(from, group_q).pop().unwrap();
        // As the form sends it: the member's card copied from the old book.
        let moved = super::Member { name: person.name.clone(), card: super::moved_vcard(&person), ..Default::default() };
        let edit = super::GroupEdit { name: "Hylki moving group".into(), members: vec![moved], ..Default::default() };
        super::move_group(from, &group_uid, to, super::GroupFormat::Apple, edit).unwrap();
        assert!(find(from, group_q).is_empty(), "gone from the first book");
        let (new_group, group) = find(to, group_q).pop().expect("in the second book");
        let (copy_uid, copy) = find(to, person_q).pop().expect("the member copied");
        assert!(copy.contains("555 0142"));
        let refs = super::parse_vcard_fields(&group).unwrap().member_refs;
        assert_eq!(refs.iter().map(|(u, _)| u.as_str()).collect::<Vec<_>>(), vec![copy_uid.as_str()]);
        for (book, uid) in [(to, &new_group), (to, &copy_uid), (from, &person_uid)] {
            super::delete_contact(book, uid).unwrap();
        }
    }

    #[test]
    fn an_edit_writes_addresses_birthdays_and_photos() {
        let original = "BEGIN:VCARD\r\nVERSION:3.0\r\nUID:u1\r\nFN:Pat Doe\r\n\
                        ADR;TYPE=HOME:PO 7;Flat 2;1 Main St\\, Rear;Springfield;IL;62701;USA\r\n\
                        BDAY:1985-04-12\r\nPHOTO;ENCODING=b;TYPE=PNG:iVBORw0KGgo=\r\nEND:VCARD\r\n";
        let c = super::parse_vcard_fields(original).unwrap();
        assert_eq!(c.postal[0].street, "1 Main St, Rear");
        assert_eq!((c.postal[0].pobox.as_str(), c.postal[0].label.as_str()), ("PO 7", "Home"));
        assert_eq!(super::birthday_parts(&c.birthday_raw), Some((Some(1985), 4, 12)));
        let mut postal = c.postal.clone();
        postal[0].locality = "Shelbyville".into();
        postal.push(super::Postal { label: "Work".into(), street: "2 Side Rd".into(), ..Default::default() });
        let edit = super::ContactEdit {
            name: "Pat Doe".into(),
            addresses: postal,
            birthday: super::birthday_value(None, 4, 12),
            ..Default::default()
        };
        let out = super::patched_vcard(original, &edit);
        assert!(out.contains("ADR;TYPE=HOME:PO 7;Flat 2;1 Main St\\, Rear;Shelbyville;IL;62701;USA\r\n"), "{out}");
        assert!(out.contains("ADR;TYPE=WORK:;;2 Side Rd;;;;\r\n"), "{out}");
        assert!(out.contains("BDAY:--04-12\r\n") && !out.contains("1985"), "{out}");
        assert!(out.contains("PHOTO;ENCODING=b;TYPE=PNG:iVBORw0KGgo="), "a kept photo stays: {out}");
        let gone = super::patched_vcard(original, &super::ContactEdit { photo: super::PhotoEdit::Remove, ..edit.clone() });
        assert!(!gone.contains("PHOTO"), "{gone}");
        let set = super::patched_vcard(
            original,
            &super::ContactEdit { photo: super::PhotoEdit::Set(vec![0xFF, 0xD8, 0xFF, 1]), ..edit },
        );
        assert!(set.contains("PHOTO;ENCODING=b;TYPE=JPEG:/9j/AQ==\r\n") && !set.contains("iVBOR"), "{set}");
    }

    #[test]
    fn a_card_with_no_name_or_address_is_still_a_contact() {
        let phone = "BEGIN:VCARD\r\nVERSION:3.0\r\nTEL:+1 555 0100\r\nEND:VCARD\r\n";
        assert_eq!(super::parse_vcard_fields(phone).unwrap().name, "+1 555 0100");
        let org = "BEGIN:VCARD\r\nVERSION:3.0\r\nORG:Acme;\r\nTEL:+1 555 0100\r\nEND:VCARD\r\n";
        assert_eq!(super::parse_vcard_fields(org).unwrap().name, "Acme");
        assert!(super::parse_vcard_fields("BEGIN:VCARD\r\nVERSION:3.0\r\nEND:VCARD\r\n").is_none());
        // Saved with no name, it goes by its company.
        let edit = super::ContactEdit {
            org: "Acme".into(),
            phones: vec![super::Labeled { label: "Work".into(), value: "+1 555 0100".into() }],
            ..Default::default()
        };
        let out = super::new_vcard(&edit);
        assert!(out.contains("\r\nFN:Acme\r\n") && out.contains("TEL;TYPE=WORK:+1 555 0100"), "{out}");
    }

    #[test]
    fn an_account_named_in_hylki_names_its_books_account() {
        let work = "Work".to_string();
        assert_eq!(super::with_account_name("CardDAV — a@b.org · card", Some(&work)), "Work · card");
        assert_eq!(super::with_account_name("CardDAV — a@b.org", Some(&work)), "Work · a@b.org");
        assert_eq!(super::with_account_name("On This Computer · Personal", Some(&work)), "Work · Personal");
        assert_eq!(super::with_account_name("CardDAV — a@b.org · card", None), "CardDAV — a@b.org · card");
        // And a book named too.
        let family = "Family".to_string();
        assert_eq!(
            super::with_given_name(&super::with_account_name("CardDAV — a@b.org · card", Some(&work)), Some(&family)),
            "Work · Family"
        );
    }

    #[test]
    fn a_book_named_in_hylki_keeps_its_account() {
        let given = "Family".to_string();
        assert_eq!(super::with_given_name("CardDAV — a@b.org · card", Some(&given)), "CardDAV — a@b.org · Family");
        assert_eq!(super::with_given_name("CardDAV — Work", Some(&given)), "CardDAV — Work · Family");
        assert_eq!(super::with_given_name("Hylki Address Book", Some(&given)), "Family");
        assert_eq!(super::with_given_name("CardDAV — a@b.org · card", None), "CardDAV — a@b.org · card");
    }

    #[test]
    fn a_line_goes_after_the_version() {
        assert_eq!(
            super::insert_after_version("BEGIN:VCARD\r\nVERSION:3.0\r\nFN:A\r\nEND:VCARD\r\n", "UID:x"),
            "BEGIN:VCARD\r\nVERSION:3.0\r\nUID:x\r\nFN:A\r\nEND:VCARD\r\n"
        );
    }
}
