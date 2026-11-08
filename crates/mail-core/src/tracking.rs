//! Remote content in mail. Nothing remote loads unless the user asks for a
//! message's images; even then, images from tracker hosts stay blocked. Tracker
//! hosts come from AdGuard's Tracking Protection filter (data/trackers.txt,
//! GPL-3.0, see scripts/gen-trackers.py), AdGuard's list of mail trackers
//! disguised as the sender's own subdomains (downloaded at runtime, too large
//! to build in), plus the subdomain names email services use for tracking.

use std::collections::HashSet;
use std::hash::{Hash, Hasher};
use std::sync::{OnceLock, RwLock};

use crate::error::{Error, Result};
use crate::http::{self, Request};

use serde::Serialize;

static TRACKERS: &str = include_str!("../data/trackers.txt");

fn trackers() -> &'static HashSet<&'static str> {
    static SET: OnceLock<HashSet<&'static str>> = OnceLock::new();
    SET.get_or_init(|| TRACKERS.lines().filter(|l| !l.is_empty() && !l.starts_with('#')).collect())
}

/// First labels email services use for tracking hosts (click.brand.com...).
const TRACKING_LABELS: &[&str] = &["click", "clicks", "trk", "track", "tracking", "tracker", "open", "opens", "pixel", "beacon", "ct", "links"];

/// AdGuard's disguised mail trackers (one host per line), downloaded at runtime.
pub const MAIL_TRACKERS_URL: &str = "https://raw.githubusercontent.com/AdguardTeam/cname-trackers/master/data/combined_disguised_mail_trackers_justdomains.txt";

/// The downloaded list as sorted 64-bit fingerprints: ~2.5 MB instead of
/// ~25 MB of strings for 300k hosts. A collision could at worst block one
/// harmless image.
static DOWNLOADED: RwLock<Vec<u64>> = RwLock::new(Vec::new());

fn fingerprint(host: &str) -> u64 {
    // DefaultHasher::new() uses fixed keys, so fingerprints are stable.
    let mut h = std::collections::hash_map::DefaultHasher::new();
    host.hash(&mut h);
    h.finish()
}

/// Parses a downloaded host list strictly: plain host names only. Rejects the
/// whole list if it looks truncated or isn't a host list (then the previous
/// copy stays in use).
pub fn parse_host_list(text: &str) -> Result<Vec<u64>> {
    let (mut ok, mut bad) = (Vec::new(), 0usize);
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')) {
        let valid = line.len() <= 253
            && line.contains('.')
            && line.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b))
            && !line.starts_with(['.', '-']);
        if valid {
            ok.push(fingerprint(line));
        } else {
            bad += 1;
        }
    }
    if ok.len() < 10_000 || bad * 100 > ok.len() {
        return Err(Error::Other(format!("tracker list rejected: {} hosts, {bad} invalid lines", ok.len())));
    }
    ok.sort_unstable();
    ok.dedup();
    Ok(ok)
}

/// Uses a parsed list for every later check; returns how many hosts it has.
pub fn set_downloaded(list: Vec<u64>) -> usize {
    let n = list.len();
    if let Ok(mut d) = DOWNLOADED.write() {
        *d = list;
    }
    n
}

pub fn downloaded_count() -> usize {
    DOWNLOADED.read().map(|d| d.len()).unwrap_or(0)
}

pub fn builtin_count() -> usize {
    trackers().len()
}

/// Downloads the mail tracker list unless it's unchanged since `etag`.
/// Ok(None): unchanged. Ok(Some((text, etag))) otherwise.
pub async fn download_mail_trackers(etag: Option<&str>) -> Result<Option<(String, Option<String>)>> {
    let mut req = Request::new("GET", MAIL_TRACKERS_URL).max_body(64 << 20);
    if let Some(e) = etag.filter(|e| e.bytes().all(|b| b.is_ascii_graphic())) {
        req = req.header("If-None-Match", e);
    }
    let resp = http::send(req).await?;
    if resp.status == 304 {
        return Ok(None);
    }
    let resp = resp.ok()?;
    let etag = resp.header("ETag").map(str::to_string);
    let text = String::from_utf8(resp.body).map_err(|_| Error::Other("tracker list isn't text".into()))?;
    Ok(Some((text, etag)))
}

pub fn is_tracker_host(host: &str) -> bool {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    if host.split('.').next().is_some_and(|first| TRACKING_LABELS.contains(&first)) && host.matches('.').count() >= 2 {
        return true;
    }
    let set = trackers();
    let downloaded = DOWNLOADED.read().ok();
    // The host or any parent domain ("a.b.tracker.com" -> "tracker.com").
    let mut rest = host.as_str();
    loop {
        if set.contains(rest) || downloaded.as_ref().is_some_and(|d| d.binary_search(&fingerprint(rest)).is_ok()) {
            return true;
        }
        match rest.split_once('.') {
            Some((_, parent)) if parent.contains('.') => rest = parent,
            _ => return false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RemoteImage {
    pub url: String,
    pub host: String,
    pub tracker: bool,
}

fn host_of(url: &str) -> Option<String> {
    let rest = url.trim().strip_prefix("https://").or_else(|| url.trim().strip_prefix("http://"))?;
    let host = rest.split(['/', '?', '#']).next()?.rsplit('@').next()?.split(':').next()?.to_ascii_lowercase();
    (!host.is_empty()).then_some(host)
}

/// Remote images an HTML message would load: <img src/srcset> and the
/// legacy background attribute. (Remote CSS and fonts are never loaded.)
pub fn remote_images(html: &str) -> Vec<RemoteImage> {
    let lower = html.to_ascii_lowercase(); // same byte positions as `html`
    fn add(out: &mut Vec<RemoteImage>, url: &str) {
        if let Some(host) = host_of(url) {
            if !out.iter().any(|i| i.url == url) {
                out.push(RemoteImage { url: url.to_string(), tracker: is_tracker_host(&host), host });
            }
        }
    }
    let mut out: Vec<RemoteImage> = Vec::new();
    let mut pos = 0;
    while let Some(i) = lower[pos..].find('<').map(|i| pos + i) {
        let end = lower[i..].find('>').map(|e| i + e).unwrap_or(lower.len());
        let tag = &html[i..end];
        let name = lower[i + 1..end].split(|c: char| c.is_whitespace() || c == '/').next().unwrap_or("");
        if name == "img" {
            if let Some(src) = crate::safety::attr(tag, "src") {
                add(&mut out, &src);
            }
            for candidate in crate::safety::attr(tag, "srcset").unwrap_or_default().split(',') {
                if let Some(url) = candidate.split_whitespace().next() {
                    add(&mut out, url);
                }
            }
        }
        if let Some(bg) = crate::safety::attr(tag, "background") {
            add(&mut out, &bg);
        }
        pos = end.max(i + 1);
        if out.len() > 200 {
            break;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracker_hosts() {
        assert!(is_tracker_host("google-analytics.com"));
        assert!(is_tracker_host("www.google-analytics.com"), "subdomains of a listed domain");
        assert!(is_tracker_host("trk.mailchimp.com"));
        assert!(is_tracker_host("click.news.somebrand.com"), "tracking-style first label");
        assert!(!is_tracker_host("click.com"), "the label rule needs a real subdomain");
        for ok in ["github.com", "avatars.githubusercontent.com", "images.example.org", "cdn.shopify.com"] {
            assert!(!is_tracker_host(ok), "{ok}");
        }
    }

    #[test]
    fn downloaded_list_is_validated_and_used() {
        let good: String = (0..12_000).map(|i| format!("click{i}.brand{i}.example\n")).collect::<String>() + "10dkim1._domainkey.mail.example\n";
        assert!(parse_host_list("# title\na.example\n").is_err(), "too short: looks truncated");
        let mut junk = good.clone();
        for _ in 0..500 {
            junk.push_str("<html>not a host list</html>\n");
        }
        assert!(parse_host_list(&junk).is_err(), "too many invalid lines");
        let list = parse_host_list(&format!("# AdGuard\n\n{good}")).unwrap();
        assert_eq!(list.len(), 12_001);
        assert!(!is_tracker_host("news.brand7.example"));
        set_downloaded(list);
        assert!(is_tracker_host("click7.brand7.example"));
        assert!(is_tracker_host("img.click7.brand7.example"), "subdomains too");
        assert!(!is_tracker_host("news.brand7.example"));
        set_downloaded(Vec::new());
    }

    #[test]
    fn finds_remote_images() {
        let html = r#"<p>Hi</p><IMG SRC="https://cdn.example.org/logo.png" width=120>
            <img src="https://www.google-analytics.com/collect?x=1" width="1" height="1">
            <img srcset="https://img.example.org/a.png 1x, https://img.example.org/b.png 2x">
            <img src="cid:part1"><img src="data:image/png;base64,AAAA">
            <table background="http://bg.example.net/b.jpg"></table>"#;
        let imgs = remote_images(html);
        let hosts: Vec<_> = imgs.iter().map(|i| (i.host.as_str(), i.tracker)).collect();
        assert_eq!(hosts, [("cdn.example.org", false), ("www.google-analytics.com", true), ("img.example.org", false), ("img.example.org", false), ("bg.example.net", false)]);
    }
}
