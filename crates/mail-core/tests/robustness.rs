//! Mutation fuzzing (no extra crates): every parser that sees data from
//! servers or senders is fed thousands of corrupted versions of valid input
//! and must never panic. Deterministic, so failures reproduce.

use mail_core::{b64, ical, imap, imap_proto, message, rules, safety, settings_io, xml};

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        // xorshift64*
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545F4914F6CDD1D)
    }
    fn below(&mut self, n: usize) -> usize {
        if n == 0 { 0 } else { (self.next() % n as u64) as usize }
    }
}

const INTERESTING: &[&[u8]] = &[
    b"{", b"}", b"{999999999}\r\n", b"(", b")", b"\"", b"\\", b"\r\n", b"\0", b"<", b">", b"</", b"<![CDATA[", b"&#x110000;",
    b"BEGIN:VEVENT\r\n", b"END:VEVENT", b"=?UTF-8?B?", b"?=", b"--", b"\xff\xfe", b"\xe2\x80\xae", b"%", b"&", b"*", b":", b";",
];

fn mutate(rng: &mut Rng, seed: &[u8]) -> Vec<u8> {
    let mut v = seed.to_vec();
    for _ in 0..1 + rng.below(8) {
        match rng.below(6) {
            0 if !v.is_empty() => {
                let i = rng.below(v.len());
                v[i] = rng.next() as u8;
            }
            1 if !v.is_empty() => {
                let i = rng.below(v.len());
                v.truncate(i);
            }
            2 => {
                let i = rng.below(v.len() + 1);
                let ins = INTERESTING[rng.below(INTERESTING.len())];
                v.splice(i..i, ins.iter().copied());
            }
            3 if !v.is_empty() => {
                let a = rng.below(v.len());
                let b = (a + rng.below(64)).min(v.len());
                v.drain(a..b);
            }
            4 if !v.is_empty() => {
                // Duplicate a chunk (deep nesting, repeated headers).
                let a = rng.below(v.len());
                let b = (a + rng.below(64)).min(v.len());
                let chunk = v[a..b].to_vec();
                for _ in 0..rng.below(20) {
                    v.splice(a..a, chunk.iter().copied());
                }
            }
            _ => v.push(rng.next() as u8),
        }
    }
    v
}

/// FUZZ_SCALE=50 cargo test --release --test robustness  (longer runs)
fn fuzz(name: &str, seeds: &[&[u8]], iterations: usize, f: impl Fn(&[u8])) {
    let scale: usize = std::env::var("FUZZ_SCALE").ok().and_then(|v| v.parse().ok()).unwrap_or(1);
    let iterations = iterations * scale;
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15 ^ name.len() as u64 ^ (scale as u64).rotate_left(17));
    for s in seeds {
        f(s);
    }
    for i in 0..iterations {
        let seed = seeds[i % seeds.len()];
        let input = mutate(&mut rng, seed);
        f(&input);
    }
}

const MAIL: &[u8] = b"Message-ID: <a@b>\r\nReferences: <r@x> <p@x>\r\nFrom: =?UTF-8?B?Sm9zw6k=?= <j@x.org>\r\nTo: a@b.c, \"B, C\" <b@c.d>\r\nSubject: =?UTF-8?Q?Ol=C3=A1?=\r\nDate: Tue, 1 Oct 2024 10:00:00 +0000\r\nMIME-Version: 1.0\r\nContent-Type: multipart/mixed; boundary=\"b\"\r\n\r\n--b\r\nContent-Type: text/html\r\n\r\n<p>Hi <a href=\"https://x.org\">www.y.org</a></p>\r\n--b\r\nContent-Type: application/pdf; name=\"invoice.pdf.exe\"\r\nContent-Disposition: attachment; filename=\"invoice.pdf.exe\"\r\nContent-Transfer-Encoding: base64\r\n\r\nQUJD\r\n--b--\r\n";

#[test]
fn imap_responses() {
    fuzz(
        "imap",
        &[
            b"1 FETCH (UID 7 FLAGS (\\Seen) RFC822.SIZE 99 BODY[HEADER] {5}\r\nhello)",
            b"LIST (\\HasNoChildren \\Sent) \"/\" \"Sent &AOk-\"",
            b"1 FETCH (UID 5 BODY[] {3}\r\nabc BODY[HEADER.FIELDS (FROM)] NIL)",
        ],
        20_000,
        |b| {
            let _ = imap_proto::parse_tokens(b);
            let _ = imap::parse_fetch(b);
            let _ = imap_proto::utf7_decode(&String::from_utf8_lossy(b));
        },
    );
}

#[test]
fn mail_parsing_and_safety() {
    fuzz("mail", &[MAIL, b"From: a\r\n\r\nbody"], 5_000, |b| {
        let meta = message::parse_meta(b);
        let body = message::parse_body(b);
        if let Some(h) = &body.html {
            let _ = safety::link_warnings(h);
            let _ = safety::neutralize_html(h);
        }
        for a in &body.attachments {
            let _ = safety::attachment_risk(&a.name);
        }
        let _ = message::split_quoted(&String::from_utf8_lossy(b));
        let _ = rules::evaluate(&[rules::Rule::move_from_sender("a", "b")], &meta, body.text.as_deref());
    });
}

#[test]
fn xml_and_calendars() {
    fuzz(
        "xml",
        &[
            b"<d:multistatus xmlns:d=\"DAV:\"><d:response><d:href>/a</d:href><d:propstat><d:prop><c:calendar-data><![CDATA[BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nDTSTART:20261109T090000Z\r\nDURATION:PT1H\r\nSUMMARY:x\\, y\r\nEND:VEVENT\r\nEND:VCALENDAR]]></c:calendar-data></d:prop></d:propstat></d:response></d:multistatus>",
            b"BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:x\r\nDTSTART;VALUE=DATE:20261225\r\nDTEND:20261226T000000Z\r\nRRULE:FREQ=WEEKLY\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n",
        ],
        20_000,
        |b| {
            let s = String::from_utf8_lossy(b);
            if let Ok(doc) = xml::parse(&s) {
                let _ = doc.all("response");
            }
            let _ = ical::parse_events(&s);
            let _ = ical::parse_duration(&s);
            let _ = ical::parse_datetime(&s);
            let _ = ical::parse_iso(&s);
        },
    );
}

#[test]
fn encodings_and_settings_files() {
    fuzz(
        "enc",
        &[b"Zm9vYmFy", b"{\"format\":\"carteiro-settings\",\"version\":1,\"exported_at\":0,\"accounts\":[],\"rules\":[],\"calendars\":[]}"],
        20_000,
        |b| {
            let s = String::from_utf8_lossy(b);
            let _ = b64::decode(&s);
            let _ = b64::decode_with(&s, b64::URL_SAFE);
            let _ = message::parse_address_list(&s);
            let _ = settings_io::parse(&s);
        },
    );
}

#[test]
fn deep_nesting_is_bounded() {
    // Pathological inputs that once crash naive recursive parsers.
    let deep_xml = "<a>".repeat(10_000) + &"</a>".repeat(10_000);
    assert!(xml::parse(&deep_xml).is_err(), "nesting limit enforced");
    let deep_imap = b"(".repeat(100_000);
    assert!(imap_proto::parse_tokens(&deep_imap).is_err(), "IMAP nesting limit enforced");
    let ok = b"(((((a)))))";
    assert!(imap_proto::parse_tokens(ok).is_ok());
}

/// Runs `client` against a fake server that sends `reply` and then hangs up.
fn against_server<F, Fut>(reply: Vec<u8>, client: F)
where
    F: FnOnce(tokio::io::DuplexStream) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    use tokio::io::AsyncWriteExt;
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    rt.block_on(async move {
        let (c, mut s) = tokio::io::duplex(1 << 20);
        let server = tokio::spawn(async move {
            let _ = s.write_all(&reply).await;
            // Drain whatever the client sends, then close.
            let mut sink = vec![0u8; 4096];
            use tokio::io::AsyncReadExt;
            let _ = tokio::time::timeout(std::time::Duration::from_millis(5), async { while s.read(&mut sink).await.map(|n| n > 0).unwrap_or(false) {} }).await;
        });
        // Every client call must finish (with Ok or Err) well before the IO timeout.
        tokio::time::timeout(std::time::Duration::from_secs(10), client(c)).await.expect("client hung on malformed input");
        let _ = server.await;
    });
}

#[test]
fn protocol_clients_survive_hostile_servers() {
    use mail_core::account::Credential;
    use mail_core::http::{send_on, Request, Url};
    use mail_core::imap::ImapClient;
    use mail_core::pop3::Pop3Client;
    use mail_core::smtp::SmtpClient;

    let scale: usize = std::env::var("FUZZ_SCALE").ok().and_then(|v| v.parse().ok()).unwrap_or(1);
    let mut rng = Rng(42);
    let imap_seed: &[u8] = b"* OK [CAPABILITY IMAP4rev1 AUTH=PLAIN SASL-IR] hi\r\nA0001 OK\r\n* CAPABILITY IMAP4rev1 MOVE\r\nA0002 OK\r\n* 1 FETCH (UID 5 FLAGS (\\Seen) RFC822.SIZE 9 BODY[] {5}\r\nhello)\r\nA0003 OK\r\n";
    let smtp_seed: &[u8] = b"220 hi\r\n250-mx\r\n250-AUTH PLAIN LOGIN\r\n250 8BITMIME\r\n235 ok\r\n250 ok\r\n250 ok\r\n354 go\r\n250 queued\r\n";
    let pop_seed: &[u8] = b"+OK hi\r\n+OK\r\n+OK\r\n+OK\r\n1 abc\r\n2 def\r\n.\r\n+OK\r\nSubject: x\r\n\r\n..dot\r\n.\r\n";
    let http_seed: &[u8] = b"HTTP/1.1 207 Multi-Status\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n0\r\n\r\n";
    for _ in 0..300 * scale {
        against_server(mutate(&mut rng, imap_seed), |c| async move {
            let mut im = ImapClient::new(c);
            if im.greeting().await.is_ok() && im.authenticate("u", &Credential::Password("p".into())).await.is_ok() {
                let _ = im.uid_fetch("1:*", "UID FLAGS RFC822.SIZE BODY.PEEK[]").await;
            }
        });
        against_server(mutate(&mut rng, smtp_seed), |c| async move {
            let mut sm = SmtpClient::new(c);
            if sm.greeting().await.is_ok() && sm.ehlo().await.is_ok() && sm.auth("u", "p").await.is_ok() {
                let _ = sm.send("a@b.c", &["d@e.f".into()], b"Subject: x\r\n\r\nhi").await;
            }
        });
        against_server(mutate(&mut rng, pop_seed), |c| async move {
            let mut p = Pop3Client::new(c);
            if p.greeting().await.is_ok() && p.login("u", "p").await.is_ok() {
                if let Ok(list) = p.uidl().await {
                    if let Some((n, _)) = list.first() {
                        let _ = p.retr(*n).await;
                    }
                }
            }
        });
        against_server(mutate(&mut rng, http_seed), |c| async move {
            let url = Url::parse("https://example.com/x").unwrap();
            let _ = send_on(c, &url, &Request::new("REPORT", "https://example.com/x")).await;
        });
    }
}
