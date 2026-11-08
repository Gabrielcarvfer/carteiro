//! Signature-free safety checks that run on every platform (including
//! Android, where a full antivirus engine doesn't fit): dangerous attachment
//! types and deceptive links. They complement ClamAV, never replace it.

/// File types that can run code when opened.
const DANGEROUS_EXT: &[&str] = &[
    // Windows executables and installers
    "exe", "com", "scr", "pif", "cpl", "msi", "msp", "msix", "msixbundle", "appx", "appxbundle", "dll", "sys", "drv", "ocx", "lnk",
    // scripts
    "bat", "cmd", "vbs", "vbe", "js", "jse", "wsf", "wsh", "wsc", "hta", "ps1", "psm1", "psd1", "sct", "reg", "inf", "scf", "url",
    // other platforms
    "jar", "apk", "app", "sh", "command", "run", "bin",
    // disk images often used to smuggle executables past filters
    "iso", "img", "vhd", "vhdx",
    // documents with macros, and OneNote (abused to embed scripts)
    "docm", "dotm", "xlsm", "xltm", "xlam", "pptm", "potm", "ppam", "sldm", "one", "onepkg",
    // help / settings files that execute
    "chm", "settingcontent-ms", "library-ms", "search-ms",
];

/// Extensions people expect to be harmless; used to spot "invoice.pdf.exe".
const DECOY_EXT: &[&str] = &["pdf", "doc", "docx", "xls", "xlsx", "ppt", "pptx", "txt", "jpg", "jpeg", "png", "gif", "zip", "rtf", "csv", "html"];

/// Returns why an attachment is dangerous, or None.
pub fn attachment_risk(name: &str) -> Option<String> {
    // Right-to-left override and friends make "exe.pdf" display as "fdp.exe".
    if name.chars().any(|c| matches!(c, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' | '\u{200E}' | '\u{200F}')) {
        return Some("file name contains hidden text-direction characters (a disguise trick)".into());
    }
    let lower = name.trim().trim_end_matches(['.', ' ']).to_lowercase();
    let parts: Vec<&str> = lower.split('.').collect();
    if parts.len() < 2 {
        return None;
    }
    let ext = parts[parts.len() - 1];
    if DANGEROUS_EXT.contains(&ext) {
        if parts.len() >= 3 && DECOY_EXT.contains(&parts[parts.len() - 2]) {
            return Some(format!("disguised program: looks like .{} but is .{}", parts[parts.len() - 2], ext));
        }
        return Some(format!(".{ext} files can run code on your device"));
    }
    None
}

fn host_of(url: &str) -> Option<String> {
    let rest = url.trim().split_once("://")?.1;
    let host = rest.split(['/', '?', '#']).next()?;
    let host = host.rsplit('@').next()?; // drop userinfo ("paypal.com@evil.net")
    let host = host.split(':').next()?.trim_end_matches('.').to_lowercase();
    (!host.is_empty()).then_some(host)
}

/// "www.paypal.com" -> "paypal.com" (good enough for comparing link text).
fn base_domain(host: &str) -> String {
    let labels: Vec<&str> = host.split('.').filter(|l| !l.is_empty()).collect();
    // Keep three labels for common two-part suffixes (bbc.co.uk).
    let two_part = labels.len() >= 3 && matches!(labels[labels.len() - 2], "co" | "com" | "org" | "net" | "gov" | "ac" | "edu");
    let keep = if two_part { 3 } else { 2 };
    labels[labels.len().saturating_sub(keep)..].join(".")
}

/// Visible link text that itself looks like a web address.
fn text_as_host(text: &str) -> Option<String> {
    let t = text.trim().trim_end_matches('/');
    if t.contains(' ') || !t.contains('.') {
        return None;
    }
    let candidate = if t.contains("://") { t.to_string() } else { format!("https://{t}") };
    let h = host_of(&candidate)?;
    // Must look like a domain: letters in the last label.
    let tld = h.rsplit('.').next()?;
    (tld.len() >= 2 && tld.chars().all(|c| c.is_ascii_alphabetic())).then_some(h)
}

/// Visible text of an HTML fragment (tags dropped, entities decoded).
pub fn strip_tags(s: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    crate::xml::decode_entities(&out)
}

/// Warnings for links whose visible text names a different site than the
/// link actually opens, and for links to raw IP addresses.
pub fn link_warnings(html: &str) -> Vec<String> {
    let mut out = Vec::new();
    let lower = html.to_ascii_lowercase(); // same byte positions as `html`
    let mut pos = 0;
    while let Some(i) = lower[pos..].find("<a") {
        let start = pos + i;
        let Some(tag_end) = lower[start..].find('>').map(|e| start + e) else { break };
        let Some(close) = lower[tag_end..].find("</a").map(|e| tag_end + e) else { break };
        pos = close + 3;
        let tag = &html[start..tag_end];
        let Some(href) = attr(tag, "href") else { continue };
        let Some(target) = host_of(&href) else { continue };
        let text = strip_tags(&html[tag_end + 1..close]);
        if target.parse::<std::net::IpAddr>().is_ok() || target.starts_with('[') {
            push_unique(&mut out, format!("A link goes to a raw IP address ({target}) instead of a named website."));
        }
        if let Some(shown) = text_as_host(&text) {
            if base_domain(&shown) != base_domain(&target) {
                push_unique(&mut out, format!("A link shows \"{}\" but actually opens {target}.", text.trim()));
            }
        }
        if out.len() >= 5 {
            break;
        }
    }
    out
}

/// Removes tags that could act on the viewer regardless of the display
/// sandbox: `<meta http-equiv=...>` (refresh/redirect, cookies) and `<base>`.
/// Scripts, frames, forms and remote loads are already blocked by the
/// sandbox and its CSP; this is defense in depth across web engines.
pub fn neutralize_html(html: &str) -> String {
    let lower = html.to_ascii_lowercase();
    let mut out = String::with_capacity(html.len());
    let mut pos = 0;
    while let Some(i) = lower[pos..].find('<').map(|i| pos + i) {
        let rest = &lower[i + 1..];
        let is_meta = rest.starts_with("meta") && rest[4..].starts_with(|c: char| c.is_whitespace() || c == '/' || c == '>');
        let is_base = rest.starts_with("base") && rest[4..].starts_with(|c: char| c.is_whitespace() || c == '/' || c == '>');
        let end = lower[i..].find('>').map(|e| i + e + 1).unwrap_or(lower.len());
        if (is_meta && lower[i..end].contains("http-equiv")) || is_base {
            out.push_str(&html[pos..i]);
            pos = end;
        } else {
            out.push_str(&html[pos..i + 1]);
            pos = i + 1;
        }
    }
    out.push_str(&html[pos..]);
    out
}

fn push_unique(v: &mut Vec<String>, s: String) {
    if !v.contains(&s) {
        v.push(s);
    }
}

/// Value of an attribute in a start tag (quoted or not), entity-decoded.
pub(crate) fn attr(tag: &str, name: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let mut from = 0;
    while let Some(i) = lower[from..].find(name) {
        let at = from + i;
        from = at + name.len();
        // Must be a whole attribute name.
        if at > 0 && !lower.as_bytes()[at - 1].is_ascii_whitespace() {
            continue;
        }
        let rest = tag[at + name.len()..].trim_start();
        let Some(rest) = rest.strip_prefix('=') else { continue };
        let rest = rest.trim_start();
        let value = match rest.chars().next()? {
            q @ ('"' | '\'') => rest[1..].split(q).next()?,
            _ => rest.split(|c: char| c.is_whitespace() || c == '>').next()?,
        };
        return Some(crate::xml::decode_entities(value));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_dangerous_attachments() {
        assert!(attachment_risk("setup.EXE").is_some());
        assert!(attachment_risk("report.docm").is_some());
        assert!(attachment_risk("invoice.pdf.exe").unwrap().contains("disguised"));
        assert!(attachment_risk("photo\u{202E}gpj.exe").unwrap().contains("text-direction"));
        assert!(attachment_risk("archive.iso").is_some());
        assert!(attachment_risk("trailing.exe. ").is_some());
        assert!(attachment_risk("report.pdf").is_none());
        assert!(attachment_risk("notes.docx").is_none());
        assert!(attachment_risk("README").is_none());
    }

    #[test]
    fn unicode_case_changes_dont_shift_positions() {
        // 'İ' lowercases to 3 bytes with full Unicode rules (2 bytes as-is).
        let html = "İİİİ <a href=\"https://evil.example/\">www.paypal.com</a> İ";
        assert_eq!(link_warnings(html).len(), 1);
        assert_eq!(neutralize_html("İ<META http-equiv=refresh>İ"), "İİ");
    }

    #[test]
    fn neutralizes_meta_and_base() {
        let h = r#"<p>a</p><META HTTP-EQUIV="refresh" content="0;url=https://x"><meta charset="utf-8"><base href="https://evil/"><basefont><p>b</p>"#;
        assert_eq!(neutralize_html(h), r#"<p>a</p><meta charset="utf-8"><basefont><p>b</p>"#);
        assert_eq!(neutralize_html("a < b and <unclosed"), "a < b and <unclosed");
    }

    #[test]
    fn flags_deceptive_links() {
        let html = r#"<p>Pay <a href="https://paypa1-secure.example.ru/login">www.paypal.com</a>
            or <A HREF='https://www.paypal.com/x'>paypal.com</A>,
            <a href="http://192.168.4.20/x">click</a>,
            <a href="https://www.paypal.com@evil.example/">https://www.paypal.com</a>,
            <a href="https://news.bbc.co.uk/a">bbc.co.uk</a> and <a href="https://x.org">Read more</a></p>"#;
        let w = link_warnings(html);
        assert_eq!(w.len(), 3, "{w:#?}");
        assert!(w[0].contains("paypa1-secure.example.ru"));
        assert!(w[1].contains("192.168.4.20"));
        assert!(w[2].contains("evil.example"));
    }
}
