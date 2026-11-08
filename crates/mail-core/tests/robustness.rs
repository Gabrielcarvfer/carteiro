//! Mutation fuzzing (no extra crates): every parser that sees data from
//! servers or senders is fed thousands of corrupted versions of valid input
//! and must never panic. Deterministic, so failures reproduce.

use mail_core::{
    b64, backup, doctext, invites, recur, carddav, contacts, dedup, graph, http, ical, imap, imap_proto, message, presets, rules, safety, search, settings_io,
    store, threading, tracking, tz, update, xml,
};

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

/// Longer runs, exploring new inputs each time (the seed is printed on failure):
/// FUZZ_SCALE=50 FUZZ_SEED=$RANDOM cargo test --release --test robustness
fn fuzz_seed() -> u64 {
    std::env::var("FUZZ_SEED").ok().and_then(|v| v.parse().ok()).unwrap_or(0)
}

fn fuzz(name: &str, seeds: &[&[u8]], iterations: usize, f: impl Fn(&[u8])) {
    let scale: usize = std::env::var("FUZZ_SCALE").ok().and_then(|v| v.parse().ok()).unwrap_or(1);
    let iterations = iterations * scale;
    let extra = fuzz_seed();
    eprintln!("fuzz {name}: FUZZ_SCALE={scale} FUZZ_SEED={extra}");
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15 ^ name.len() as u64 ^ (scale as u64).rotate_left(17) ^ extra.wrapping_mul(0xA24B_AED4_963E_E407));
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
fn contact_files() {
    fuzz(
        "contacts",
        &[
            b"BEGIN:VCARD\r\nVERSION:2.1\r\nN;ENCODING=QUOTED-PRINTABLE:Pere=\r\nira;Jo=C3=A3o\r\nitem1.EMAIL:a@b.c\r\nORG:A\\;B;C\r\nEND:VCARD\r\n",
            b"Name,E-mail 1 - Value,Phone 1 - Value\n\"A, \"\"B\"\"\",a@x ::: b@y,+1\n",
            b"dn:: Y249Sm/Do28=\ncn:: Sm/Do28=\nmail: j@x\n description\n\n",
        ],
        20_000,
        |b| {
            for c in contacts::parse_any(&String::from_utf8_lossy(b)) {
                let _ = contacts::parse_vcards(&contacts::to_vcard(&c));
            }
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
    let mut rng = Rng(42 ^ fuzz_seed().wrapping_mul(0xA24B_AED4_963E_E407));
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

/// Stored mail: whatever arrives is saved, threaded, searchable, and comes
/// back byte for byte (attachments are stored apart and spliced back in).
#[test]
fn store_keeps_hostile_mail_intact() {
    let st = store::Store::open_in_memory().unwrap();
    let ep = mail_core::Endpoint { host: "h".into(), port: 993, security: mail_core::Security::Tls, allow_insecure: false };
    let mut a = mail_core::Account {
        id: 0, name: "Me".into(), email: "me@x.org".into(), username: "me".into(),
        incoming_protocol: mail_core::Protocol::Imap, incoming: ep.clone(), smtp: ep, smtp_username: None,
        signature: String::new(), auth: mail_core::AuthMethod::Password,
    };
    st.add_account(&mut a).unwrap();
    let big = [MAIL, b"\r\n", &[b'A'; 6000][..]].concat();
    let big_att = String::from_utf8_lossy(MAIL).replace("QUJD", &"QUJD".repeat(2000)).into_bytes();
    let uid = std::cell::Cell::new(0u32);
    fuzz("store", &[MAIL, &big, &big_att], 1_500, |raw| {
        uid.set(uid.get() + 1);
        let meta = message::parse_meta(raw);
        let id = st
            .insert_message(&store::NewMessage {
                account_id: a.id, folder: "INBOX".into(), uid: Some(uid.get()), pop_uidl: None, meta,
                seen: false, flagged: false, size: raw.len() as u64, raw: Some(raw.to_vec()),
            })
            .unwrap();
        assert_eq!(st.raw(id).unwrap().as_deref(), Some(raw), "message {id} changed in storage");
        let _ = st.message(id).unwrap();
        let (stripped, parts) = dedup::split_attachments(raw);
        assert!(stripped.len() + parts.iter().map(|p| p.end - p.start).sum::<usize>() == raw.len());
    });
    // Search terms from the search box are data, never SQL.
    fuzz("store-search", &[b"from:ana subject:\"q3 plan\" has:attachment is:unread", b"after:2026-11-01 before:2026-12-01 to:me", b"%_'\";--"], 3_000, |q| {
        let q = String::from_utf8_lossy(q);
        st.list_threads(a.id, "INBOX", Some(&q), 50, 0).unwrap();
        st.search_events(&q, 20).unwrap();
        st.list_contacts(&q, 20).unwrap();
        st.suggest_addresses(&q, 8).unwrap();
        let _ = search::parse(&q);
    });
}

#[test]
fn newer_parsers() {
    fuzz("trackers", &[b"||trk.example.com^\n! comment\nclick.shop.example\n@@||ok.example^$third-party\n"], 10_000, |b| {
        let t = String::from_utf8_lossy(b);
        let _ = tracking::parse_host_list(&t);
        let _ = tracking::is_tracker_host(&t);
        let _ = tracking::remote_images(&t);
    });
    fuzz("images", &[b"<img src=\"https://x.org/a.png\" srcset=\"a 1x, //b 2x\" width=1 height=1><img src='http://trk.example/p.gif'>"], 10_000, |b| {
        let h = String::from_utf8_lossy(b);
        let _ = tracking::remote_images(&h);
        let _ = safety::neutralize_html(&h);
        let _ = safety::link_warnings(&h);
    });
    let base = http::Url::parse("https://dav.example/addressbooks/me/").unwrap();
    let report: &[u8] = b"<?xml version=\"1.0\"?><d:multistatus xmlns:d=\"DAV:\" xmlns:c=\"urn:ietf:params:xml:ns:carddav\"><d:response><d:href>/ab/1.vcf</d:href><d:propstat><d:prop><d:getetag>\"1\"</d:getetag><c:address-data>BEGIN:VCARD\r\nVERSION:3.0\r\nFN:Ana\r\nEMAIL:ana@x.org\r\nEND:VCARD\r\n</c:address-data></d:prop></d:propstat></d:response></d:multistatus>";
    fuzz("carddav", &[report], 5_000, |b| {
        if let Ok(doc) = xml::parse(&String::from_utf8_lossy(b)) {
            let _ = carddav::parse_report(&doc, &base);
        }
    });
    let ev: &[u8] = br#"{"id":"1","subject":"Hi","isAllDay":false,"start":{"dateTime":"2026-11-08T10:00:00.0000000","timeZone":"W. Europe Standard Time"},"end":{"dateTime":"2026-11-08T11:00:00","timeZone":"UTC"},"location":{"displayName":"Room"},"bodyPreview":"x","recurrence":null,"type":"singleInstance"}"#;
    let ct: &[u8] = br#"{"id":"c1","displayName":"Ana","emailAddresses":[{"address":"ana@x.org","name":"Ana"}],"mobilePhone":"+1","businessPhones":["2"],"companyName":"Acme","personalNotes":"n"}"#;
    fuzz("graph", &[ev, ct], 10_000, |b| {
        if let Ok(v) = serde_json::from_slice::<serde_json::Value>(b) {
            let _ = graph::parse_event(&v);
            let _ = graph::parse_contact(&v);
        }
    });
    fuzz("update", &[br#"{"version":"0.2.0","notes":"x","platforms":{"linux-x86_64":{"url":"https://github.com/a/b/releases/download/v0.2.0/C.AppImage","sha256":"00"}}}"#], 5_000, |b| {
        let _ = update::verify_manifest(b, "AAAA", &[0u8; 32]);
        let t = String::from_utf8_lossy(b);
        let _ = update::parse_version(&t);
        let _ = update::is_newer(&t, "0.1.0");
    });
    fuzz("names", &[b"W. Europe Standard Time", b"Re: Fwd: AW: [list] Subject", b"someone@outlook.com"], 10_000, |b| {
        let t = String::from_utf8_lossy(b);
        let _ = tz::to_iana(&t);
        let _ = threading::normalize_subject(&t);
        let _ = threading::is_reply_subject(&t);
        let _ = presets::guess(&t);
        let _ = safety::attachment_risk(&t);
    });
}

/// A backup file is opened before anything is trusted: a damaged or hostile
/// one must fail cleanly, never panic or restore garbage.
#[test]
fn backup_files() {
    let mut sealed = Vec::new();
    backup::encrypt_with(&mut &MAIL[..], &mut sealed, "fuzz passphrase", 1).unwrap();
    fuzz("backup", &[&sealed], 3_000, |b| {
        let mut out = Vec::new();
        if backup::decrypt(&mut &b[..], &mut out, "fuzz passphrase").is_ok() {
            assert_eq!(out, MAIL, "a changed backup decrypted to different data");
        }
    });
}

/// Invitations come from strangers: whatever they send must not crash the
/// calendar or blow up into endless events.
#[test]
fn invitations_and_repeat_rules() {
    let ics: &[u8] = b"BEGIN:VCALENDAR\r\nMETHOD:REQUEST\r\nBEGIN:VEVENT\r\nUID:u1\r\nSEQUENCE:1\r\nSUMMARY:Sync\r\nDTSTART;TZID=Europe/Lisbon:20261109T100000\r\nDTEND;TZID=Europe/Lisbon:20261109T103000\r\nRRULE:FREQ=MONTHLY;BYDAY=-1FR,2TU;INTERVAL=2;COUNT=40\r\nEXDATE:20261208T100000\r\nEND:VEVENT\r\nBEGIN:VEVENT\r\nUID:u1\r\nRECURRENCE-ID:20261110T100000\r\nDTSTART:20261110T150000Z\r\nSTATUS:CANCELLED\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
    fuzz("recur", &[ics, b"RRULE:FREQ=YEARLY;BYMONTH=2;BYMONTHDAY=29,-1;UNTIL=20400101T000000Z"], 4_000, |b| {
        let t = String::from_utf8_lossy(b);
        let _ = mail_core::ical::calendar_method(&t);
        let evs = mail_core::ical::parse_events(&t);
        let out = recur::expand_all(evs, 1_700_000_000, 1_900_000_000, 500);
        assert!(out.len() <= 500 * 4, "expansion is bounded");
        let mut e = mail_core::ical::Event { start: 1_794_225_600, end: 1_794_229_200, ..Default::default() };
        e.rrule = t.lines().find_map(|l| l.strip_prefix("RRULE:")).unwrap_or(&t).to_string();
        assert!(recur::expand(&e, 0, i64::MAX / 4, 300).len() <= 300);
    });
    let mail = [b"From: a@b.org\r\nContent-Type: text/calendar; method=REQUEST\r\n\r\n".as_slice(), ics].concat();
    fuzz("invite-mail", &[&mail], 1_500, |raw| {
        for part in invites::calendar_parts(raw) {
            let _ = mail_core::ical::parse_events(&part);
        }
    });
}

/// Attachments are strangers' files: damaged PDFs and office documents must
/// give up quietly, within memory bounds.
#[test]
fn attachment_documents() {
    let content = b"BT /F1 12 Tf (Invoice \\(paid\\)) Tj [(Tot) -300 (al)] TJ ET";
    let packed = miniz_oxide::deflate::compress_to_vec_zlib(content, 6);
    let mut pdf = b"%PDF-1.4\n1 0 obj << /Filter /FlateDecode >>\nstream\n".to_vec();
    pdf.extend(&packed);
    pdf.extend(b"\nendstream endobj 2 0 obj << >>\nstream\nbegincmap 1 beginbfchar <01> <0041> endbfchar 1 beginbfrange <10> <20> <0061> endbfrange endcmap\nendstream %%EOF");
    fuzz("pdf", &[&pdf], 3_000, |b| {
        if let Some(t) = doctext::extract("f.pdf", "", b) {
            assert!(t.chars().count() <= 200_000);
        }
    });
    let docx = std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/sample.docx")).unwrap_or_default();
    if !docx.is_empty() {
        fuzz("docx", &[&docx], 3_000, |b| {
            let _ = doctext::extract("f.docx", "", b);
        });
    }
}
