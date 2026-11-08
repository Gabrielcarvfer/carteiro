//! End-to-end tests against the local GreenMail test server (never a real
//! account). Start it first, then run with its CA trusted:
//!
//!   scripts/test-mail-server.sh start
//!   CARTEIRO_TEST_CA=$PWD/.testmail/ca.pem cargo test -p mail-core --test live_greenmail -- --ignored --test-threads=1

use std::sync::Mutex;

use mail_core::account::{Account, AuthMethod, Credential, Protocol};
use mail_core::net::{Endpoint, Security};
use mail_core::ops::{self, Draft, Flag, SendResult};
use mail_core::rules::Rule;
use mail_core::store::Store;
use mail_core::sync;

fn ep(port: u16, security: Security) -> Endpoint {
    Endpoint { host: "localhost".into(), port, security, allow_insecure: false }
}

fn account(user: &str, protocol: Protocol) -> Account {
    let incoming = match protocol {
        Protocol::Imap => ep(3993, Security::Tls),
        Protocol::Pop3 => ep(3995, Security::Tls),
    };
    Account {
        id: 0,
        name: user.to_string(),
        email: format!("{user}@carteiro.test"),
        // Deliberately the short form: setup must discover the full address.
        username: user.to_string(),
        incoming_protocol: protocol,
        incoming,
        smtp: ep(3465, Security::Tls),
        smtp_username: None,
        signature: String::new(),
        auth: AuthMethod::Password,
    }
}

fn pw(user: &str) -> Credential {
    Credential::Password(format!("{user}-pw"))
}

fn unique(tag: &str) -> String {
    let n = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    format!("{tag} {n}")
}

/// A store with the account added (after verification, like the app does).
async fn setup(user: &str, protocol: Protocol) -> (Mutex<Store>, Account) {
    let verified = sync::verify_account(&account(user, protocol), &pw(user)).await.expect("verify");
    assert_eq!(verified.username, format!("{user}@carteiro.test"), "user name form discovered");
    let store = Store::open_in_memory().unwrap();
    let mut a = verified;
    store.add_account(&mut a).unwrap();
    (Mutex::new(store), a)
}

fn draft(to: &str, subject: &str, body: &str) -> Draft {
    Draft { to: to.into(), cc: String::new(), bcc: String::new(), subject: subject.into(), body: body.into(), reply_to_message: None }
}

fn inbox_subjects(store: &Mutex<Store>, a: &Account) -> Vec<String> {
    store.lock().unwrap().list_threads(a.id, "INBOX", None, 500, 0).unwrap().into_iter().map(|t| t.subject).collect()
}

#[tokio::test]
#[ignore]
async fn send_sync_reply_and_thread() {
    let (astore, alice) = setup("alice", Protocol::Imap).await;
    let (bstore, bob) = setup("bob", Protocol::Imap).await;
    let subject = unique("Lunch?");

    let sent = ops::send(&astore, &alice, &pw("alice"), &draft("Bob <bob@carteiro.test>", &subject, "Olá Bob, almoço às 13h?")).await.unwrap();
    assert_eq!(sent, SendResult::Sent);

    sync::sync_account(&bstore, &bob, &pw("bob")).await.unwrap();
    let threads = bstore.lock().unwrap().list_threads(bob.id, "INBOX", Some(&subject), 10, 0).unwrap();
    assert_eq!(threads.len(), 1, "Bob received it");
    let msgs = bstore.lock().unwrap().thread_messages(bob.id, &threads[0].thread_key).unwrap();
    let views = ops::open_thread(&bstore, &bob, &pw("bob"), &threads[0].thread_key, &Default::default()).await.unwrap();
    assert!(views[0].body_text.contains("almoço às 13h"), "UTF-8 body survives: {:?}", views[0].body_text);

    // Bob replies; Alice sees one conversation with both messages.
    let mut reply = draft("alice@carteiro.test", &format!("RE: {subject}"), "Sim!");
    reply.reply_to_message = Some(msgs[0].id);
    ops::send(&bstore, &bob, &pw("bob"), &reply).await.unwrap();
    sync::sync_account(&astore, &alice, &pw("alice")).await.unwrap();
    let a_threads = astore.lock().unwrap().list_threads(alice.id, "INBOX", Some(&subject), 10, 0).unwrap();
    assert_eq!(a_threads.len(), 1);
    let conv = astore.lock().unwrap().thread_messages(alice.id, &a_threads[0].thread_key).unwrap();
    let subjects: Vec<_> = conv.iter().map(|m| m.subject.as_str()).collect();
    assert!(subjects.iter().any(|s| s.starts_with("RE:")), "reply is threaded with the original: {subjects:?}");
}

#[tokio::test]
#[ignore]
async fn rules_move_on_the_server_and_flags_sync() {
    let (astore, alice) = setup("alice", Protocol::Imap).await;
    let (bstore, bob) = setup("bob", Protocol::Imap).await;
    let folder = unique("From Alice").replace(' ', "-");
    let mut rule = Rule::move_from_sender("alice@carteiro.test", &folder);
    rule.account_id = Some(bob.id);
    bstore.lock().unwrap().save_rule(&mut rule).unwrap();

    let subject = unique("Report");
    ops::send(&astore, &alice, &pw("alice"), &draft("bob@carteiro.test", &subject, "see attached")).await.unwrap();
    let report = sync::sync_account(&bstore, &bob, &pw("bob")).await.unwrap();
    assert!(report.moved_by_rules >= 1, "{report:?}");
    assert!(!inbox_subjects(&bstore, &bob).contains(&subject), "moved out of the Inbox");
    // A second sync picks it up in the new folder (created on the server).
    sync::sync_account(&bstore, &bob, &pw("bob")).await.unwrap();
    let in_folder = bstore.lock().unwrap().messages_in_folder(bob.id, &folder).unwrap();
    let m = in_folder.iter().find(|m| m.subject == subject).expect("in the rule's folder");

    // Flag it, then check from a fresh store that the server kept the flag.
    ops::set_flag(&bstore, &bob, &pw("bob"), &[m.id], Flag::Flagged, true).await.unwrap();
    let fresh = Mutex::new(Store::open_in_memory().unwrap());
    let mut b2 = bob.clone();
    fresh.lock().unwrap().add_account(&mut b2).unwrap();
    sync::sync_account(&fresh, &b2, &pw("bob")).await.unwrap();
    let again = fresh.lock().unwrap().messages_in_folder(b2.id, &folder).unwrap();
    assert!(again.iter().any(|x| x.subject == subject && x.flagged), "flag stored on the server");
}

#[tokio::test]
#[ignore]
async fn pop3_over_tls() {
    let (astore, alice) = setup("alice", Protocol::Imap).await;
    let (pstore, bob_pop) = setup("bob", Protocol::Pop3).await;
    let subject = unique("Via POP3");
    ops::send(&astore, &alice, &pw("alice"), &draft("bob@carteiro.test", &subject, "hello")).await.unwrap();
    sync::sync_account(&pstore, &bob_pop, &pw("bob")).await.unwrap();
    assert!(inbox_subjects(&pstore, &bob_pop).contains(&subject));
}

#[tokio::test]
#[ignore]
async fn refuses_server_without_starttls() {
    // GreenMail's plain ports don't offer STARTTLS: requiring it must fail
    // instead of logging in unencrypted.
    let mut a = account("alice", Protocol::Imap);
    a.incoming = ep(3143, Security::StartTls);
    assert!(sync::verify_account(&a, &pw("alice")).await.is_err());
    let mut s = account("alice", Protocol::Imap);
    s.smtp = ep(3025, Security::StartTls);
    assert!(mail_core::smtp::verify(&s.smtp, "alice@carteiro.test", &pw("alice")).await.is_err());
}

#[tokio::test]
#[ignore]
async fn outbox_holds_mail_until_the_server_is_reachable() {
    let (astore, mut alice) = setup("alice", Protocol::Imap).await;
    let good_smtp = alice.smtp.clone();
    alice.smtp.port = 1; // nothing listens here: "offline"
    let subject = unique("Queued");
    let r = ops::send(&astore, &alice, &pw("alice"), &draft("bob@carteiro.test", &subject, "later")).await.unwrap();
    assert_eq!(r, SendResult::Queued);
    assert_eq!(astore.lock().unwrap().outbox(alice.id).unwrap().len(), 1);

    alice.smtp = good_smtp; // back online
    assert_eq!(ops::flush_outbox(&astore, &alice, &pw("alice")).await.unwrap(), 1);
    assert!(astore.lock().unwrap().outbox(alice.id).unwrap().is_empty());

    let (bstore, bob) = setup("bob", Protocol::Imap).await;
    sync::sync_account(&bstore, &bob, &pw("bob")).await.unwrap();
    assert!(inbox_subjects(&bstore, &bob).contains(&subject), "queued mail was delivered");
}

#[tokio::test]
#[ignore]
async fn wrong_password_is_rejected_cleanly() {
    let err = sync::verify_account(&account("alice", Protocol::Imap), &Credential::Password("nope".into())).await.unwrap_err();
    assert!(err.to_string().contains("rejected the password"), "{err}");
}

#[tokio::test]
#[ignore]
async fn offline_download_prune_and_mbox_export() {
    let (astore, alice) = setup("alice", Protocol::Imap).await;
    let (bstore, bob) = setup("bob", Protocol::Imap).await;
    let tag = unique("Export");
    for i in 0..3 {
        ops::send(&astore, &alice, &pw("alice"), &draft("bob@carteiro.test", &format!("{tag} #{i}"), "From the start of a line\nbody")).await.unwrap();
    }
    sync::sync_account(&bstore, &bob, &pw("bob")).await.unwrap();

    // Free every cached body (as if they were older than the chosen period)...
    let far_future = 4_000_000_000;
    let freed = bstore.lock().unwrap().clear_bodies_before(bob.id, far_future).unwrap();
    assert!(freed >= 3, "freed {freed}");
    // ...then make everything available offline again.
    let r = ops::download_offline(&bstore, &bob, &pw("bob"), None).await.unwrap();
    assert!(r.downloaded >= 3, "{r:?}");
    assert!(bstore.lock().unwrap().messages_missing_body(bob.id, "INBOX", None).unwrap().is_empty());

    let dir = std::env::temp_dir().join(unique("carteiro-export").replace(' ', "-"));
    let rep = ops::export_mbox(&bstore, &bob, &pw("bob"), &dir).await.unwrap();
    assert_eq!(rep.skipped, 0, "{rep:?}");
    let inbox = std::fs::read_to_string(dir.join("bob@carteiro.test").join("INBOX.mbox")).unwrap();
    for i in 0..3 {
        assert!(inbox.contains(&format!("{tag} #{i}")), "message {i} exported");
    }
    assert!(inbox.contains("\n>From the start of a line"), "body 'From ' lines are escaped");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
#[ignore]
async fn offline_actions_replay_on_the_server() {
    let (astore, alice) = setup("alice", Protocol::Imap).await;
    let (bstore, bob) = setup("bob", Protocol::Imap).await;
    let tag = unique("Offline");
    for what in ["flag", "move", "delete"] {
        ops::send(&astore, &alice, &pw("alice"), &draft("bob@carteiro.test", &format!("{tag} {what}"), what)).await.unwrap();
    }
    sync::sync_account(&bstore, &bob, &pw("bob")).await.unwrap();
    let find = |s: &Mutex<Store>, folder: &str, what: &str| {
        s.lock().unwrap().messages_in_folder(bob.id, folder).unwrap().into_iter().find(|m| m.subject == format!("{tag} {what}"))
    };
    let (f, m, d) = (find(&bstore, "INBOX", "flag").unwrap(), find(&bstore, "INBOX", "move").unwrap(), find(&bstore, "INBOX", "delete").unwrap());
    let dest = unique("Kept").replace(' ', "-");

    // Offline: the server is unreachable.
    let mut offline = bob.clone();
    offline.incoming.port = 1;
    ops::set_flag(&bstore, &offline, &pw("bob"), &[f.id], Flag::Flagged, true).await.unwrap();
    ops::move_messages(&bstore, &offline, &pw("bob"), &[m.id], &dest).await.unwrap();
    ops::delete_messages(&bstore, &offline, &pw("bob"), &[d.id]).await.unwrap();
    assert_eq!(bstore.lock().unwrap().pending_ops(bob.id).unwrap().len(), 3, "three actions queued");
    assert!(bstore.lock().unwrap().message(f.id).unwrap().unwrap().flagged, "applied locally right away");
    assert_eq!(bstore.lock().unwrap().message(m.id).unwrap().unwrap().folder, dest);

    // Back online: the next sync replays them.
    sync::sync_account(&bstore, &bob, &pw("bob")).await.unwrap();
    assert!(bstore.lock().unwrap().pending_ops(bob.id).unwrap().is_empty(), "queue drained");

    // A fresh client sees the server state.
    let fresh = Mutex::new(Store::open_in_memory().unwrap());
    let mut b2 = bob.clone();
    fresh.lock().unwrap().add_account(&mut b2).unwrap();
    sync::sync_account(&fresh, &b2, &pw("bob")).await.unwrap();
    let in_folder = |folder: &str| fresh.lock().unwrap().messages_in_folder(b2.id, folder).unwrap();
    assert!(in_folder("INBOX").iter().any(|x| x.subject == format!("{tag} flag") && x.flagged), "flag on the server");
    assert!(in_folder(&dest).iter().any(|x| x.subject == format!("{tag} move")), "moved on the server");
    assert!(!in_folder("INBOX").iter().any(|x| x.subject == format!("{tag} move") || x.subject == format!("{tag} delete")), "gone from the Inbox");
    let trash = fresh.lock().unwrap().folder_by_special(b2.id, "trash").unwrap().map(|t| t.name).unwrap_or_else(|| "Trash".into());
    assert!(in_folder(&trash).iter().any(|x| x.subject == format!("{tag} delete")), "deleted to Trash on the server");
}

/// Fetches the bodies of a conversation (scanning off: the scan is tested
/// elsewhere) and returns their raw bytes.
async fn raw_of(store: &Mutex<Store>, a: &Account, user: &str, subject: &str) -> Vec<Vec<u8>> {
    let threads = store.lock().unwrap().list_threads(a.id, "INBOX", Some(subject), 10, 0).unwrap();
    assert_eq!(threads.len(), 1, "{subject} arrived");
    let off = mail_core::scan::ScannerConfig { enabled: false, ..Default::default() };
    let views = ops::open_thread(store, a, &pw(user), &threads[0].thread_key, &off).await.unwrap();
    views.iter().map(|v| store.lock().unwrap().raw(v.msg.id).unwrap().expect("downloaded")).collect()
}

#[tokio::test]
#[ignore]
async fn invitation_answer_reaches_the_organizer() {
    let (astore, alice) = setup("alice", Protocol::Imap).await;
    let (bstore, bob) = setup("bob", Protocol::Imap).await;
    let tag = unique("Design review");
    let uid = format!("live-{}", tag.replace(' ', "-"));
    // Bob invites Alice to a weekly series, the way calendar apps do.
    let ics = format!("BEGIN:VCALENDAR\r\nVERSION:2.0\r\nMETHOD:REQUEST\r\nBEGIN:VEVENT\r\nUID:{uid}\r\nSEQUENCE:0\r\nORGANIZER:mailto:bob@carteiro.test\r\nATTENDEE;RSVP=TRUE:mailto:alice@carteiro.test\r\nSUMMARY:{tag}\r\nDTSTART;TZID=Europe/Lisbon:20271109T100000\r\nDTEND;TZID=Europe/Lisbon:20271109T110000\r\nRRULE:FREQ=WEEKLY;COUNT=3\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n");
    let invite = format!("From: Bob <bob@carteiro.test>\r\nTo: alice@carteiro.test\r\nSubject: Invitation: {tag}\r\nMessage-ID: <{uid}@carteiro.test>\r\nMIME-Version: 1.0\r\nContent-Type: multipart/alternative; boundary=b\r\n\r\n--b\r\nContent-Type: text/plain\r\n\r\nJoin us\r\n--b\r\nContent-Type: text/calendar; method=REQUEST\r\n\r\n{ics}--b--\r\n");
    mail_core::smtp::send_mail(&bob.smtp, bob.smtp_user(), &pw("bob"), &bob.email, &["alice@carteiro.test".into()], invite.as_bytes()).await.unwrap();

    sync::sync_account(&astore, &alice, &pw("alice")).await.unwrap();
    let raw = raw_of(&astore, &alice, "alice", &tag).await.remove(0);
    let msg = astore.lock().unwrap().list_threads(alice.id, "INBOX", Some(&tag), 1, 0).unwrap()[0].latest_id;
    let now = mail_core::ical::parse_datetime("20271101T000000Z").unwrap().0;
    let touched = mail_core::invites::import(&mut astore.lock().unwrap(), msg, "bob@carteiro.test", &raw, now).unwrap();
    assert_eq!(touched, std::slice::from_ref(&uid));

    // Alice won't make the second week, and tells Bob.
    let cal = astore.lock().unwrap().invitations_calendar().unwrap();
    let weeks: Vec<_> = astore.lock().unwrap().events_between(0, i64::MAX / 2).unwrap().into_iter().filter(|e| e.calendar_id == cal && e.remote_id == uid).collect();
    assert_eq!(weeks.len(), 3);
    astore.lock().unwrap().set_rsvp(weeks[1].id, "no").unwrap();
    assert_eq!(ops::send_rsvp(&astore, &alice, &pw("alice"), weeks[1].id).await.unwrap(), SendResult::Sent);

    sync::sync_account(&bstore, &bob, &pw("bob")).await.unwrap();
    let reply = raw_of(&bstore, &bob, "bob", &format!("Declined: {tag}")).await.remove(0);
    let parts = mail_core::invites::calendar_parts(&reply);
    assert_eq!(parts.len(), 1, "a calendar reply Bob's calendar can read");
    let r = &parts[0];
    assert_eq!(mail_core::ical::calendar_method(r), "REPLY");
    assert!(r.contains(&format!("UID:{uid}")), "{r}");
    assert!(r.contains("ATTENDEE;PARTSTAT=DECLINED;CN=\"alice\":mailto:alice@carteiro.test"), "{r}");
    assert!(r.contains("RECURRENCE-ID;TZID=Europe/Lisbon:20271116T100000"), "only the second week: {r}");
    // Answers are private until sent: the other weeks have none.
    let others: Vec<String> = astore.lock().unwrap().events_between(0, i64::MAX / 2).unwrap().into_iter().filter(|e| e.remote_id == uid).map(|e| e.rsvp).collect();
    assert_eq!(others, ["", "no", ""]);
}

#[tokio::test]
#[ignore]
async fn older_mail_on_request_and_offline_progress() {
    let (bstore, bob) = setup("bob", Protocol::Imap).await;
    let tag = unique("Older");
    for i in 0..5 {
        ops::send(&bstore, &bob, &pw("bob"), &draft("alice@carteiro.test", &format!("{tag} #{i}"), "x")).await.unwrap();
    }
    // A fresh device that has listed nothing yet: older mail comes in pages.
    let (astore, alice) = setup("alice", Protocol::Imap).await;
    astore.lock().unwrap().upsert_folder(alice.id, "INBOX", "INBOX", Some("."), Some("inbox")).unwrap();
    let first = ops::load_older(&astore, &alice, &pw("alice"), "INBOX", 2).await.unwrap();
    assert_eq!(first, 2, "a page of the newest");
    let ours = |s: &Mutex<Store>| s.lock().unwrap().list_threads(alice.id, "INBOX", Some(&tag), 50, 0).unwrap().len();
    assert_eq!(ours(&astore), 2, "the two newest are ours");
    let mut more = 0;
    loop {
        let n = ops::load_older(&astore, &alice, &pw("alice"), "INBOX", 200).await.unwrap();
        if n == 0 { break; }
        more += n;
    }
    assert!(more >= 3 && ours(&astore) == 5, "all of them, then nothing more ({more})");
    // The offline download says how far it is.
    let seen = std::sync::Mutex::new(Vec::new());
    let report = ops::download_offline_with(&astore, &alice, &pw("alice"), None, &|p| seen.lock().unwrap().push((p.phase, p.done, p.total))).await.unwrap();
    let seen = seen.into_inner().unwrap();
    assert!(report.downloaded >= 5, "{report:?}");
    assert!(seen.iter().any(|(phase, _, total)| *phase == "downloading" && *total >= 5), "progress reported: {seen:?}");
    assert!(seen.iter().all(|(_, done, total)| done <= total));
}
