//! Hylki's own address book, kept in an application SQLite file.
//!
//! It sits next to the Evolution Data Server (EDS) books: people who do not
//! run EDS can still keep contacts, and everyone can import `.vcf` files.
//! vCards are stored verbatim, so properties Hylki does not edit (PHOTO, ADR,
//! X-...) survive a round trip, exactly as with an EDS book. The
//! `contact_email` table is derived data, rebuilt on every write.
//!
//! This module is the only code that touches `local_contacts.db`;
//! `contacts.rs` routes to it by [`LOCAL_BOOK_UID`].

use std::path::{Path, PathBuf};

use rusqlite::{params, Connection, OptionalExtension, Transaction};

/// The reserved `book_uid` of the local book.
pub const LOCAL_BOOK_UID: &str = "hylki-local";

/// Largest `.vcf` file read in one go.
const MAX_FILE_BYTES: u64 = 50 * 1024 * 1024;
/// Largest single vCard accepted (a contact with an embedded photo can be big).
const MAX_VCARD_BYTES: usize = 5 * 1024 * 1024;
/// Schema version stored in `PRAGMA user_version`.
const SCHEMA_VERSION: i64 = 1;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS contact (
    uid     TEXT PRIMARY KEY,
    vcard   TEXT NOT NULL,
    name    TEXT NOT NULL,
    updated INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS contact_email (
    uid         TEXT NOT NULL REFERENCES contact(uid) ON DELETE CASCADE,
    email_lower TEXT NOT NULL,
    email       TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS contact_email_idx ON contact_email(email_lower);
";

/// What an import did. Counts are per contact, except `failed_files`, which
/// names the files that could not be read at all (with the reason).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImportOutcome {
    pub added: usize,
    pub updated: usize,
    /// Blocks that are not people: no name and no email, or a distribution list.
    pub skipped: usize,
    /// Truncated or oversized vCards.
    pub errors: usize,
    pub failed_files: Vec<String>,
}

impl ImportOutcome {
    /// Add another outcome's counts to this one.
    fn merge(&mut self, other: ImportOutcome) {
        self.added += other.added;
        self.updated += other.updated;
        self.skipped += other.skipped;
        self.errors += other.errors;
        self.failed_files.extend(other.failed_files);
    }
}

/// How one vCard fared on its way into the store.
#[derive(Debug, PartialEq, Eq)]
enum Upsert {
    Added,
    Updated,
    Invalid,
    TooBig,
}

/// An open `local_contacts.db`.
pub struct Store {
    conn: Connection,
}

impl Store {
    /// Open (creating if needed) the database in the application data dir.
    fn open() -> Result<Store, String> {
        let dir = crate::config::data_base()
            .ok_or("no XDG data directory for the local address book")?
            .join("hylki");
        std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        Store::open_at(&dir.join("local_contacts.db"))
    }

    /// Open the database at `path`, restricting it to the owner.
    fn open_at(path: &Path) -> Result<Store, String> {
        let conn = Connection::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
        restrict(path);
        Store::init(conn).map_err(|e| format!("{}: {e}", path.display()))
    }

    /// Enable foreign keys, create the schema and check its version.
    fn init(conn: Connection) -> Result<Store, String> {
        conn.execute_batch("PRAGMA foreign_keys = ON;").map_err(|e| e.to_string())?;
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .map_err(|e| e.to_string())?;
        if version > SCHEMA_VERSION {
            return Err(format!("the local address book is from a newer Hylki (schema {version})"));
        }
        conn.execute_batch(SCHEMA).map_err(|e| e.to_string())?;
        conn.execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION};"))
            .map_err(|e| e.to_string())?;
        Ok(Store { conn })
    }

    /// An in-memory store, for tests.
    #[cfg(test)]
    fn in_memory() -> Store {
        Store::init(Connection::open_in_memory().unwrap()).unwrap()
    }

    /// Insert or update every vCard in one transaction and count the results.
    fn upsert_all(&mut self, vcards: &[String]) -> Result<ImportOutcome, String> {
        let tx = self.conn.transaction().map_err(|e| e.to_string())?;
        let mut out = ImportOutcome::default();
        for vcard in vcards {
            match upsert_tx(&tx, vcard, true)? {
                Upsert::Added => out.added += 1,
                Upsert::Updated => out.updated += 1,
                Upsert::Invalid => out.skipped += 1,
                Upsert::TooBig => out.errors += 1,
            }
        }
        tx.commit().map_err(|e| e.to_string())?;
        Ok(out)
    }

    /// Insert or update one vCard, failing when it is not a usable contact.
    fn upsert_one(&mut self, vcard: &str) -> Result<(), String> {
        let tx = self.conn.transaction().map_err(|e| e.to_string())?;
        match upsert_tx(&tx, vcard, false)? {
            Upsert::Added | Upsert::Updated => {}
            Upsert::Invalid => return Err("a contact needs a name or an email address".into()),
            Upsert::TooBig => return Err("the contact is too large".into()),
        }
        tx.commit().map_err(|e| e.to_string())
    }

    /// Remove a contact (its email rows go with it).
    fn delete(&self, uid: &str) -> Result<(), String> {
        self.conn
            .execute("DELETE FROM contact WHERE uid = ?1", [uid])
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    /// Every stored vCard, sorted by display name.
    fn vcards(&self) -> Result<Vec<String>, String> {
        let mut stmt = self
            .conn
            .prepare("SELECT vcard FROM contact ORDER BY name COLLATE NOCASE, uid")
            .map_err(|e| e.to_string())?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0)).map_err(|e| e.to_string())?;
        rows.collect::<Result<_, _>>().map_err(|e| e.to_string())
    }

    /// One stored vCard by its UID.
    fn vcard(&self, uid: &str) -> Result<Option<String>, String> {
        self.conn
            .query_row("SELECT vcard FROM contact WHERE uid = ?1", [uid], |r| r.get(0))
            .optional()
            .map_err(|e| e.to_string())
    }

    /// (display name, email) for every address in the book.
    fn emails(&self) -> Result<Vec<(String, String)>, String> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT c.name, e.email FROM contact_email e \
                 JOIN contact c ON c.uid = e.uid ORDER BY c.name COLLATE NOCASE, e.email",
            )
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
            .map_err(|e| e.to_string())?;
        rows.collect::<Result<_, _>>().map_err(|e| e.to_string())
    }

    /// (lower-cased email, uid) of the contacts whose vCard carries a PHOTO.
    fn photo_entries(&self) -> Result<Vec<(String, String)>, String> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT e.email_lower, c.uid FROM contact_email e \
                 JOIN contact c ON c.uid = e.uid WHERE c.vcard LIKE '%PHOTO%'",
            )
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
            .map_err(|e| e.to_string())?;
        rows.collect::<Result<_, _>>().map_err(|e| e.to_string())
    }
}

/// Restrict the database file to its owner (the book holds personal data).
fn restrict(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

/// Write one vCard into the open transaction, keeping `contact_email` in sync.
/// A vCard without a UID gets one: derived from its content when `stable_uid`
/// (imports, so importing the same file twice updates instead of duplicating),
/// random otherwise (a contact made in the editor).
fn upsert_tx(tx: &Transaction, vcard: &str, stable_uid: bool) -> Result<Upsert, String> {
    if vcard.len() > MAX_VCARD_BYTES {
        return Ok(Upsert::TooBig);
    }
    let Some(details) = crate::contacts::parse_vcard_details(vcard) else {
        return Ok(Upsert::Invalid);
    };
    let (uid, vcard) = if details.eds_uid.is_empty() {
        let uid = if stable_uid { content_uid(vcard) } else { random_uid()? };
        let with_uid = insert_uid(vcard, &uid);
        (uid, with_uid)
    } else {
        (details.eds_uid.clone(), vcard.to_string())
    };
    let existed = tx
        .query_row("SELECT 1 FROM contact WHERE uid = ?1", [&uid], |_| Ok(()))
        .optional()
        .map_err(|e| e.to_string())?
        .is_some();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    tx.execute(
        "INSERT INTO contact (uid, vcard, name, updated) VALUES (?1, ?2, ?3, ?4) \
         ON CONFLICT(uid) DO UPDATE SET vcard = ?2, name = ?3, updated = ?4",
        params![uid, vcard, details.name, now],
    )
    .map_err(|e| e.to_string())?;
    tx.execute("DELETE FROM contact_email WHERE uid = ?1", [&uid]).map_err(|e| e.to_string())?;
    for email in &details.emails {
        tx.execute(
            "INSERT INTO contact_email (uid, email_lower, email) VALUES (?1, ?2, ?3)",
            params![uid, email.value.to_lowercase(), email.value],
        )
        .map_err(|e| e.to_string())?;
    }
    Ok(if existed { Upsert::Updated } else { Upsert::Added })
}

/// A random UID for a contact created in the editor.
fn random_uid() -> Result<String, String> {
    Ok(format!("hylki-{}", crate::rng::token(24).map_err(|e| e.to_string())?))
}

/// A UID derived from the vCard text, so the same card always maps to the same contact.
fn content_uid(vcard: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(vcard.replace("\r\n", "\n").trim().as_bytes());
    let hex: String = digest.iter().take(12).map(|b| format!("{b:02x}")).collect();
    format!("hylki-{hex}")
}

/// Insert a `UID:` line just before `END:VCARD` (returns the text unchanged
/// when there is no end marker).
fn insert_uid(vcard: &str, uid: &str) -> String {
    let Some(pos) = vcard.to_ascii_uppercase().rfind("END:VCARD") else {
        return vcard.to_string();
    };
    let mut out = String::with_capacity(vcard.len() + uid.len() + 8);
    out.push_str(&vcard[..pos]);
    if !out.is_empty() && !out.ends_with('\n') {
        out.push_str("\r\n");
    }
    out.push_str(&format!("UID:{uid}\r\n"));
    out.push_str(&vcard[pos..]);
    out
}

/// Split the text of a `.vcf` file into its `BEGIN:VCARD` ... `END:VCARD`
/// blocks, each kept verbatim (with CRLF line ends). Returns the blocks and
/// how many were cut short (a `BEGIN` with no `END`). Text between blocks is
/// ignored. Continuation lines (folding) stay inside their block.
pub fn split_vcards(text: &str) -> (Vec<String>, usize) {
    let mut blocks = Vec::new();
    let mut truncated = 0;
    let mut current: Option<String> = None;
    for line in text.split('\n') {
        let line = line.trim_end_matches('\r');
        let marker = line.trim_end();
        if marker.eq_ignore_ascii_case("BEGIN:VCARD") {
            if current.is_some() {
                truncated += 1;
            }
            current = Some(String::new());
        }
        let Some(block) = current.as_mut() else { continue };
        block.push_str(line);
        block.push_str("\r\n");
        if marker.eq_ignore_ascii_case("END:VCARD") {
            blocks.extend(current.take());
        }
    }
    if current.is_some() {
        truncated += 1;
    }
    (blocks, truncated)
}

/// Decode the bytes of a `.vcf` file: UTF-8 (a BOM is dropped), else Latin-1,
/// which older exporters use. UTF-16 and binary data are refused.
pub fn decode_text(bytes: &[u8]) -> Result<String, String> {
    if bytes.starts_with(&[0xFF, 0xFE]) || bytes.starts_with(&[0xFE, 0xFF]) {
        return Err("UTF-16 files are not supported".into());
    }
    if bytes.contains(&0) {
        return Err("the file is not a text file".into());
    }
    let bytes = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
    match std::str::from_utf8(bytes) {
        Ok(s) => Ok(s.to_string()),
        Err(_) => Ok(bytes.iter().map(|&b| b as char).collect()),
    }
}

/// Import the vCards found in `text` into the local book, in one transaction.
pub fn import_text(text: &str) -> Result<ImportOutcome, String> {
    import_text_into(&mut Store::open()?, text)
}

/// [`import_text`] against an already open store.
fn import_text_into(store: &mut Store, text: &str) -> Result<ImportOutcome, String> {
    let (blocks, truncated) = split_vcards(text);
    let mut out = store.upsert_all(&blocks)?;
    out.errors += truncated;
    Ok(out)
}

/// Import several `.vcf` files. A file that cannot be read or stored is
/// listed in `failed_files`; the others are still imported.
pub fn import_files(paths: &[PathBuf]) -> Result<ImportOutcome, String> {
    let mut store = Store::open()?;
    let mut total = ImportOutcome::default();
    for path in paths {
        let label = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        match read_vcf_file(path).and_then(|text| import_text_into(&mut store, &text)) {
            Ok(outcome) => total.merge(outcome),
            Err(e) => total.failed_files.push(format!("{label}: {e}")),
        }
    }
    Ok(total)
}

/// Read and decode one `.vcf` file, refusing anything over [`MAX_FILE_BYTES`].
fn read_vcf_file(path: &Path) -> Result<String, String> {
    let len = std::fs::metadata(path).map_err(|e| e.to_string())?.len();
    if len > MAX_FILE_BYTES {
        return Err(format!("the file is larger than {} MB", MAX_FILE_BYTES / 1024 / 1024));
    }
    decode_text(&std::fs::read(path).map_err(|e| e.to_string())?)
}

/// Every stored vCard, sorted by display name.
pub fn list_vcards() -> Result<Vec<String>, String> {
    Store::open()?.vcards()
}

/// One stored vCard by UID.
pub fn vcard_by_uid(uid: &str) -> Result<Option<String>, String> {
    Store::open()?.vcard(uid)
}

/// (display name, email) of every address in the local book.
pub fn emails() -> Result<Vec<(String, String)>, String> {
    Store::open()?.emails()
}

/// (lower-cased email, uid) of the local contacts that have a photo.
pub fn photo_entries() -> Result<Vec<(String, String)>, String> {
    Store::open()?.photo_entries()
}

/// Add a contact from a full vCard (a missing UID is generated).
pub fn create(vcard: &str) -> Result<(), String> {
    Store::open()?.upsert_one(vcard)
}

/// Replace a stored contact with its edited vCard.
pub fn modify(vcard: &str) -> Result<(), String> {
    Store::open()?.upsert_one(vcard)
}

/// Delete a contact by UID.
pub fn delete(uid: &str) -> Result<(), String> {
    Store::open()?.delete(uid)
}

/// Whether the database file exists yet (nothing to read before the first write).
pub fn exists() -> bool {
    crate::config::data_base()
        .map(|d| d.join("hylki").join("local_contacts.db").is_file())
        .unwrap_or(false)
}

/// The file the fingerprint watcher should look at, with its journal files.
pub fn db_paths() -> Vec<PathBuf> {
    let Some(db) = crate::config::data_base().map(|d| d.join("hylki").join("local_contacts.db"))
    else {
        return Vec::new();
    };
    ["", "-journal", "-wal"]
        .iter()
        .map(|suffix| PathBuf::from(format!("{}{suffix}", db.to_string_lossy())))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn card(name: &str, email: &str) -> String {
        format!("BEGIN:VCARD\r\nVERSION:3.0\r\nFN:{name}\r\nEMAIL:{email}\r\nEND:VCARD\r\n")
    }

    #[test]
    fn split_finds_several_cards() {
        let text = format!("{}{}", card("Ann", "a@x.org"), card("Bob", "b@x.org"));
        let (blocks, truncated) = split_vcards(&text);
        assert_eq!(blocks.len(), 2);
        assert_eq!(truncated, 0);
        assert!(blocks[1].contains("FN:Bob"));
    }

    #[test]
    fn split_accepts_lf_and_lowercase_markers() {
        let (blocks, _) = split_vcards("begin:vcard\nFN:Ann\nEMAIL:a@x.org\nend:vcard\n");
        assert_eq!(blocks.len(), 1);
        assert!(blocks[0].contains("\r\n"));
    }

    #[test]
    fn split_keeps_folded_lines_inside_the_card() {
        let text = "BEGIN:VCARD\r\nFN:Ann\r\nNOTE:one\r\n two\r\nEND:VCARD\r\n";
        let (blocks, _) = split_vcards(text);
        assert_eq!(blocks.len(), 1);
        assert!(blocks[0].contains("NOTE:one\r\n two"));
    }

    #[test]
    fn split_ignores_text_between_cards() {
        let text = format!("junk\r\n{}more junk\r\n{}", card("Ann", "a@x.org"), card("Bob", "b@x.org"));
        assert_eq!(split_vcards(&text).0.len(), 2);
    }

    #[test]
    fn split_counts_a_truncated_card_and_keeps_the_next() {
        let text = format!("BEGIN:VCARD\r\nFN:Cut\r\n{}", card("Bob", "b@x.org"));
        let (blocks, truncated) = split_vcards(&text);
        assert_eq!(truncated, 1);
        assert_eq!(blocks.len(), 1);
        assert!(blocks[0].contains("FN:Bob"));
    }

    #[test]
    fn split_counts_a_truncated_card_at_the_end() {
        let text = format!("{}BEGIN:VCARD\r\nFN:Cut\r\n", card("Ann", "a@x.org"));
        let (blocks, truncated) = split_vcards(&text);
        assert_eq!((blocks.len(), truncated), (1, 1));
    }

    #[test]
    fn decode_drops_a_bom_and_reads_utf8() {
        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        bytes.extend_from_slice("Zoë".as_bytes());
        assert_eq!(decode_text(&bytes).unwrap(), "Zoë");
    }

    #[test]
    fn decode_falls_back_to_latin1() {
        assert_eq!(decode_text(&[b'Z', b'o', 0xEB]).unwrap(), "Zoë");
    }

    #[test]
    fn decode_refuses_utf16_and_binary() {
        assert!(decode_text(&[0xFF, 0xFE, b'a', 0]).is_err());
        assert!(decode_text(&[b'a', 0, b'b']).is_err());
    }

    #[test]
    fn insert_uid_goes_before_the_end_marker() {
        let out = insert_uid(&card("Ann", "a@x.org"), "u1");
        assert!(out.contains("UID:u1\r\nEND:VCARD"));
    }

    #[test]
    fn import_adds_then_updates_and_is_idempotent() {
        let mut store = Store::in_memory();
        let text = "BEGIN:VCARD\r\nVERSION:3.0\r\nUID:u1\r\nFN:Ann\r\nEMAIL:a@x.org\r\nEND:VCARD\r\n\
                    BEGIN:VCARD\r\nVERSION:3.0\r\nFN:Bob\r\nEMAIL:b@x.org\r\nEND:VCARD\r\n";
        let first = import_text_into(&mut store, text).unwrap();
        assert_eq!((first.added, first.updated), (2, 0));
        let second = import_text_into(&mut store, text).unwrap();
        assert_eq!((second.added, second.updated), (0, 2));
        assert_eq!(store.vcards().unwrap().len(), 2);
    }

    #[test]
    fn import_skips_cards_without_name_or_email_and_lists() {
        let mut store = Store::in_memory();
        let text = "BEGIN:VCARD\r\nVERSION:3.0\r\nNOTE:nobody\r\nEND:VCARD\r\n\
                    BEGIN:VCARD\r\nVERSION:3.0\r\nFN:Team\r\nX-EVOLUTION-LIST:TRUE\r\nEMAIL:t@x.org\r\nEND:VCARD\r\n";
        let out = import_text_into(&mut store, text).unwrap();
        assert_eq!((out.added, out.skipped), (0, 2));
    }

    #[test]
    fn import_counts_oversized_and_truncated_cards_as_errors() {
        let mut store = Store::in_memory();
        let big = format!(
            "BEGIN:VCARD\r\nFN:Big\r\nNOTE:{}\r\nEND:VCARD\r\n",
            "x".repeat(MAX_VCARD_BYTES + 1)
        );
        let text = format!("{big}BEGIN:VCARD\r\nFN:Cut\r\n");
        let out = import_text_into(&mut store, &text).unwrap();
        assert_eq!((out.added, out.errors), (0, 2));
    }

    #[test]
    fn create_modify_delete_keep_the_email_index_in_sync() {
        let mut store = Store::in_memory();
        store.upsert_one("BEGIN:VCARD\r\nVERSION:3.0\r\nUID:u1\r\nFN:Ann\r\nEMAIL:A@X.org\r\nEND:VCARD\r\n").unwrap();
        assert_eq!(store.emails().unwrap(), vec![("Ann".to_string(), "A@X.org".to_string())]);

        store.upsert_one("BEGIN:VCARD\r\nVERSION:3.0\r\nUID:u1\r\nFN:Ann B\r\nEMAIL:new@x.org\r\nEND:VCARD\r\n").unwrap();
        assert_eq!(store.emails().unwrap(), vec![("Ann B".to_string(), "new@x.org".to_string())]);

        store.delete("u1").unwrap();
        assert!(store.emails().unwrap().is_empty());
        assert!(store.vcards().unwrap().is_empty());
    }

    #[test]
    fn create_rejects_a_card_with_nothing_in_it() {
        let mut store = Store::in_memory();
        assert!(store.upsert_one("BEGIN:VCARD\r\nVERSION:3.0\r\nEND:VCARD\r\n").is_err());
    }

    #[test]
    fn created_cards_get_a_stored_uid() {
        let mut store = Store::in_memory();
        store.upsert_one(&card("Ann", "a@x.org")).unwrap();
        let stored = store.vcards().unwrap().remove(0);
        assert!(stored.contains("\r\nUID:hylki-"));
    }

    #[test]
    fn photo_entries_list_only_contacts_with_a_photo() {
        let mut store = Store::in_memory();
        store.upsert_one("BEGIN:VCARD\r\nUID:p1\r\nFN:Pic\r\nEMAIL:Pic@x.org\r\nPHOTO;ENCODING=b:AAAA\r\nEND:VCARD\r\n").unwrap();
        store.upsert_one(&card("Plain", "plain@x.org")).unwrap();
        assert_eq!(store.photo_entries().unwrap(), vec![("pic@x.org".to_string(), "p1".to_string())]);
    }

    #[test]
    fn a_newer_schema_is_refused() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA user_version = 99;").unwrap();
        assert!(Store::init(conn).is_err());
    }
}
