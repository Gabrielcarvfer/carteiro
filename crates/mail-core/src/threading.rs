//! Conversation helpers. Threads are primarily linked through Message-ID,
//! In-Reply-To and References (see `Store::insert_message`); the normalized
//! subject is only a fallback for clients that drop those headers.

const PREFIXES: &[&str] = &["re", "fw", "fwd", "aw", "wg", "sv", "vs", "tr", "rif", "res", "enc", "antw"];

/// Strips reply/forward prefixes ("Re:", "FW:", "Re[2]:", "RES:") and
/// normalizes case and whitespace.
pub fn normalize_subject(subject: &str) -> String {
    let mut s = subject.trim();
    loop {
        let before = s;
        if let Some(colon) = s.find(':') {
            let head = s[..colon].trim();
            let base = head.split('[').next().unwrap_or(head).trim().to_ascii_lowercase();
            if PREFIXES.contains(&base.as_str()) {
                s = s[colon + 1..].trim_start();
            }
        }
        if s == before {
            break;
        }
    }
    s.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}

pub fn is_reply_subject(subject: &str) -> bool {
    normalize_subject(subject) != subject.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes() {
        assert_eq!(normalize_subject("Re: RE: Fwd:  Budget  Q3"), "budget q3");
        assert_eq!(normalize_subject("Re[2]: hi"), "hi");
        assert_eq!(normalize_subject("RES: Proposta"), "proposta");
        assert_eq!(normalize_subject("Meeting: tomorrow"), "meeting: tomorrow");
        assert!(is_reply_subject("Re: x"));
        assert!(!is_reply_subject("Meeting: x"));
    }
}
