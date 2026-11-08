//! CardDAV (RFC 6352): finding address books and reading every contact.
//! Uses the same authenticated WebDAV requests as CalDAV.

use crate::caldav::CalDav;
use crate::contacts::{self, Contact};
use crate::error::{Error, Result};
use crate::http::Url;
use crate::xml;

#[derive(Debug, Clone, PartialEq)]
pub struct AddressBook {
    pub url: String,
    pub name: String,
}

const PROPFIND_KIND: &str = r#"<?xml version="1.0" encoding="utf-8"?><d:propfind xmlns:d="DAV:"><d:prop><d:resourcetype/><d:displayname/><d:current-user-principal/></d:prop></d:propfind>"#;
const PROPFIND_HOME: &str = r#"<?xml version="1.0" encoding="utf-8"?><d:propfind xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:carddav"><d:prop><c:addressbook-home-set/></d:prop></d:propfind>"#;
const PROPFIND_BOOKS: &str = r#"<?xml version="1.0" encoding="utf-8"?><d:propfind xmlns:d="DAV:"><d:prop><d:resourcetype/><d:displayname/></d:prop></d:propfind>"#;
const REPORT_ALL: &str = r#"<?xml version="1.0" encoding="utf-8"?><c:addressbook-query xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:carddav"><d:prop><d:getetag/><c:address-data/></d:prop></c:addressbook-query>"#;

fn is_book(r: &xml::Node) -> bool {
    r.find("resourcetype").is_some_and(|t| t.child("addressbook").is_some())
}

/// The user's address books, starting from a server URL (the server root, a
/// principal, an address book home, or an address book itself).
pub async fn discover(dav: &CalDav, start: &str) -> Result<Vec<AddressBook>> {
    let start_url = Url::parse(start)?;
    let name_of = |r: &xml::Node, href: &str| r.text_of("displayname").filter(|n| !n.is_empty()).unwrap_or_else(|| href.trim_end_matches('/').rsplit('/').next().unwrap_or("Contacts").to_string());
    // The URL may already be an address book (Google's is).
    let mut principal = None;
    for candidate in [start.to_string(), start_url.join("/.well-known/carddav")?.to_string_full()] {
        let Ok((resp, doc)) = dav.dav("PROPFIND", &candidate, "0", PROPFIND_KIND).await else { continue };
        if let Some(r) = doc.all("response").into_iter().find(|r| is_book(r)) {
            return Ok(vec![AddressBook { url: resp.url.clone(), name: name_of(r, &resp.url) }]);
        }
        if let Some(href) = doc.find("current-user-principal").and_then(|n| n.text_of("href")) {
            principal = Some(Url::parse(&resp.url)?.join(&href)?);
            break;
        }
    }
    let principal = principal.unwrap_or(start_url);
    let (resp, doc) = dav.dav("PROPFIND", &principal.to_string_full(), "0", PROPFIND_HOME).await?;
    let home = match doc.find("addressbook-home-set").and_then(|n| n.text_of("href")) {
        Some(h) => Url::parse(&resp.url)?.join(&h)?,
        None => principal,
    };
    let (resp, doc) = dav.dav("PROPFIND", &home.to_string_full(), "1", PROPFIND_BOOKS).await?;
    let base = Url::parse(&resp.url)?;
    let mut out = Vec::new();
    for r in doc.all("response").into_iter().filter(|r| is_book(r)) {
        let Some(href) = r.text_of("href") else { continue };
        out.push(AddressBook { url: base.join(&href)?.to_string_full(), name: name_of(r, &href) });
    }
    if out.is_empty() {
        return Err(Error::Other("no address books found on this server".into()));
    }
    Ok(out)
}

/// Every contact of an address book: (resource URL, etag, contact).
pub async fn contacts(dav: &CalDav, book_url: &str) -> Result<Vec<(String, Option<String>, Contact)>> {
    let (resp, doc) = dav.dav("REPORT", book_url, "1", REPORT_ALL).await?;
    Ok(parse_report(&doc, &Url::parse(&resp.url)?))
}

pub fn parse_report(doc: &xml::Node, base: &Url) -> Vec<(String, Option<String>, Contact)> {
    let mut out = Vec::new();
    for r in doc.all("response") {
        let (Some(href), Some(data)) = (r.text_of("href"), r.text_of("address-data")) else { continue };
        let url = base.join(&href).map(|u| u.to_string_full()).unwrap_or(href);
        let etag = r.text_of("getetag");
        for c in contacts::parse_vcards(&data).into_iter().filter_map(Contact::normalized) {
            out.push((url.clone(), etag.clone(), c));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_an_addressbook_report() {
        let xml = r#"<d:multistatus xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:carddav">
            <d:response><d:href>/books/me/ana.vcf</d:href><d:propstat><d:prop><d:getetag>"1"</d:getetag>
            <c:address-data>BEGIN:VCARD&#13;
VERSION:3.0&#13;
FN:Ana Souza&#13;
EMAIL:ana@example.org&#13;
END:VCARD&#13;
</c:address-data></d:prop></d:propstat></d:response>
            <d:response><d:href>/books/me/empty.vcf</d:href><d:propstat><d:prop><d:getetag>"2"</d:getetag></d:prop></d:propstat></d:response>
        </d:multistatus>"#;
        let doc = xml::parse(xml).unwrap();
        let got = parse_report(&doc, &Url::parse("https://dav.example/books/me/").unwrap());
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0, "https://dav.example/books/me/ana.vcf");
        assert_eq!(got[0].2.emails, ["ana@example.org"]);
    }
}
