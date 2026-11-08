//! CardDAV against a real server (Radicale), with made-up data:
//!   CARTEIRO_CARDDAV_URL=http://127.0.0.1:5232/ \
//!   cargo test -p mail-core --test live_carddav -- --ignored
//! The server needs user alice / alice-pw and an address book with Ana, Bob
//! and Carol (see the README's Testing section).

use mail_core::account::Credential;
use mail_core::caldav::CalDav;
use mail_core::carddav;

#[tokio::test]
#[ignore]
async fn discovers_and_reads_an_address_book() {
    let url = std::env::var("CARTEIRO_CARDDAV_URL").expect("CARTEIRO_CARDDAV_URL");
    let dav = CalDav { user: "alice".into(), cred: Credential::Password("alice-pw".into()) };
    // From the server root: principal, home set, then the address book.
    let books = carddav::discover(&dav, &url).await.unwrap();
    assert_eq!(books.len(), 1, "{books:?}");
    assert_eq!(books[0].name, "Work contacts");
    let mut names: Vec<String> = carddav::contacts(&dav, &books[0].url).await.unwrap().into_iter().map(|(_, _, c)| c.name).collect();
    names.sort();
    assert_eq!(names, ["Ana Souza", "Bob Silva", "Carol Pereira"]);
    // Given the address book itself, it's used directly.
    assert_eq!(carddav::discover(&dav, &books[0].url).await.unwrap(), books);
    // Wrong password: refused.
    let wrong = CalDav { user: "alice".into(), cred: Credential::Password("nope".into()) };
    assert!(carddav::discover(&wrong, &url).await.is_err());
}
