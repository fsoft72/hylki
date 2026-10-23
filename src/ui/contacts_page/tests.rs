//! Tests of the contact list.

use crate::contacts::ContactDetails;

use super::rows::same_contacts;

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
    let card = |rev: &str, tel: &str| {
        format!("BEGIN:VCARD\r\nFN:Ann\r\nREV:{rev}\r\nTEL:{tel}\r\nEND:VCARD")
    };
    let shown = vec![contact(&card("2026-10-18T12:23:02Z", "1"))];
    assert!(same_contacts(
        &shown,
        &[contact(&card("2026-10-18T12:25:15Z", "1"))]
    ));
    assert!(!same_contacts(
        &shown,
        &[contact(&card("2026-10-18T12:25:15Z", "2"))]
    ));
    let mut photo = contact(&card("2026-10-18T12:23:02Z", "1"));
    photo.photo_key = 7;
    assert!(!same_contacts(&shown, &[photo]));
    assert!(!same_contacts(&shown, &[]));
}
