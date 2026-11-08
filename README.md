# Carteiro

An email and calendar client with an Outlook-style layout. On a wide screen it shows a three-pane window like Outlook on the web; on a phone it switches to one pane at a time, like Outlook mobile. Targets Windows and Android first; Linux works too.

## Why Rust + Tauri 2 + plain JavaScript

| Option | Windows + Android from one codebase | Speed / size | Mail/TLS ecosystem | Verdict |
|---|---|---|---|---|
| **Tauri 2 (Rust core + plain JS UI)** | Yes | Small installers, native core, no JS build step | rustls, SQLite, mail-parser | **Chosen** |
| Flutter | Yes | Good | Dart mail libraries are thinner; TLS goes through Dart | Viable second choice |
| .NET MAUI | Yes | OK | MailKit is excellent | Weak on Linux; heavier |
| Electron | No Android | Heavy | Node | No |

The mail engine (`crates/mail-core`) is plain Rust with no UI dependency, so it can be tested headless. The UI is plain HTML, CSS and JavaScript modules in `ui/`: no npm, no bundler, no build step. Tauri shows it in the system web view (WebView2 on Windows, Android System WebView, WebKitGTK on Linux), which is needed anyway to display HTML email.

## Features

- **Mail:** IMAP and POP3 with TLS or STARTTLS, SMTP sending, unified inbox across accounts, conversations, Outlook-style rules, flags, search, works offline (Outbox for sending, and flags, moves and deletes replay on reconnect), desktop notifications, tray icon with background sync.
- **Calendar:** day, week and month views (an agenda list on phones); Microsoft 365 / Outlook (Graph), Google and other CalDAV servers, subscribed `.ics` links, public holidays for 200+ countries with regions, and local calendars.
- **Sign-in:** passwords (the right user-name form is found automatically) or Google / Microsoft sign-in (OAuth 2.0 with PKCE).
- **Your data:** keep the last day / week / month / 2 months / 6 months / year / all mail offline; save all mail as `.mbox`; export and import settings (never including passwords).

## Layout

```
crates/mail-core/      protocol, storage and calendar engine (most tests live here)
  net.rs, http.rs      TCP + TLS (rustls), STARTTLS upgrade, minimal HTTPS client
  imap*.rs pop3.rs smtp.rs   mail protocols
  message.rs           parsing (mail-parser) and writing (built in) of messages
  store.rs dedup.rs    SQLite cache, threading, deduplicated attachments
  rules.rs             rules engine
  caldav.rs graph.rs ical.rs xml.rs calendar.rs   calendars
  oauth.rs             Google / Microsoft sign-in
  scan.rs safety.rs    ClamAV client, attachment and link heuristics
  update.rs            signed self-update from GitHub releases
  sync.rs ops.rs settings_io.rs b64.rs
src-tauri/             app shell: commands, tray, secrets, bundled ClamAV engine
ui/                    the interface
scripts/               ClamAV fetch/build, local test mail server, wire check
```

## Security

**Connections**
- Implicit TLS or STARTTLS; STARTTLS is strict. If the server doesn't offer it, or refuses the upgrade, Carteiro stops and never falls back to plaintext. Data a server sends before the TLS handshake is rejected (the STARTTLS command-injection class of bugs).
- Plaintext connections are refused unless explicitly allowed per server.
- Certificates and host names are always verified. TLS 1.2 and 1.3 only (1.3 in practice).
- Protocol arguments, HTTP headers and message headers are checked for CR/LF injection. HTTP never follows a redirect from HTTPS to HTTP, and never forwards credentials to another site.

**Verified on the wire.** `tests/no_plaintext_credentials.rs` records every byte the client sends to fake servers in each failure scenario: refused STARTTLS, missing STARTTLS, TLS against a plaintext server, and HTTP redirects. A packet capture of the end-to-end tests (`tshark`) showed passwords, subjects and message text 0 times, and every TLS session was TLS 1.3. A control login sent deliberately in plaintext was visible, which proves the capture would have caught a leak. The same was done for the Android app, with `tcpdump` inside an emulator, covering a whole session from first launch: adding an account, syncing, reading, sending, and downloading and using ClamAV signatures. All 16 connections were TLS 1.3 from the first byte, to the mail server and `database.clamav.net`. The password, addresses, subjects and message text appeared 0 times, and a plaintext control login was again detected.

**Secrets**
- Windows: Credential Manager.
- Linux: the Secret Service (GNOME Keyring / KWallet) via `secret-tool`, with the secret passed on stdin.
- Android: the app-private database. Wrapping it with an Android Keystore key is still to do.
- Exports never contain secrets.

**Displaying mail**
- HTML mail renders in a sandboxed frame: no scripts (ever), no forms, no pop-ups. A Content Security Policy blocks all remote content by default: images, CSS and fonts. `<meta http-equiv>` and `<base>` are stripped.
- *Load images* loads a message's images over HTTPS from hosts that aren't trackers only. Tracker hosts (about 430,000):
  - AdGuard's Tracking Protection filter, built in (`crates/mail-core/data/trackers.txt`, refreshed with `scripts/gen-trackers.py`);
  - AdGuard's list of mail trackers disguised as the sender's own subdomains, downloaded daily (only when it changed) into the app's private folder. A download that looks truncated or isn't a host list is rejected and the previous copy kept;
  - the subdomain names email services use for tracking (`click.`, `trk.`…). 1×1 open-tracking pixels are dropped from any host.

  Tested on a phone: before loading, nothing was requested; after, only the real image was, not the built-in or the downloaded-list tracker.
- Links lose their tracking parameters (`utm_*`, `fbclid`, `gclid`, …) before they open.
- Every open message shows whether ClamAV has scanned it, and each attachment whether it may be saved.
- Links open in the system browser, and only `http`, `https` and `mailto` links, never local files or other URL schemes. Invisible text-direction characters are rejected.
- A navigation guard keeps the app window on Carteiro's own pages.
- Tested in a browser with a hostile message that tries scripts, event handlers, `javascript:` links, remote images/CSS/fonts, iframes, objects, forms and `meta refresh`, plus HTML in the subject and sender name. Nothing ran, and no request reached the test listener.

**Malware**
- A bundled **ClamAV** engine scans every message and every downloaded calendar before formatted content or attachments are shown. It's on by default, and unscanned messages show plain text only.
  - Carteiro starts its own `clamd`. On Linux and Android it listens on a Unix socket only you can open, so no other program can pose as the scanner. Windows' `clamd` only supports TCP, so there it listens on localhost.
  - Signatures come from the official `freshclam`, and ClamAV verifies them with its signing certificate.
  - Windows and Linux use the official ClamAV build pinned by SHA-256 (`scripts/fetch-clamav.sh`). Android builds ClamAV from pinned, checksum-verified sources (`scripts/build-clamav-android.sh`, Android 9+).
- Heuristics on every platform:
  - Dangerous attachment types: executables, scripts, macro documents and disk images.
  - Disguised names: double extensions such as `invoice.pdf.exe`, and hidden text-direction characters.
  - Links whose text names a different site than they open, and links to raw IP addresses.
  - Messages with such links open as plain text.

**Attachments** are saved only after passing every check: the message scanned clean, the file type isn't dangerous, and the attachment scans clean on its own. Names are sanitised, existing files are never overwritten, and saved files are readable only by you.

**Local files.** On Linux, the mail cache, calendars and exports are readable only by your user (folder `0700`, files `0600`).

**Robustness.** `tests/robustness.rs` is a dependency-free mutation fuzzer. It feeds corrupted input to every parser, and to the IMAP/SMTP/POP3/HTTP clients through hostile fake servers. It found and fixed 4 crash bugs, including unbounded recursion on nested IMAP parentheses. Run it longer with `FUZZ_SCALE=50`.

**Supply chain**
- Only crates.io sources are allowed, and CI fails on known advisories or yanked crates (`cargo-deny`). Builds use `--locked`.
- The mail engine has 46 crates in total. Functionality was written in-house where that was simple and safe: base64, HTTP, XML, iCalendar, message writing, OAuth, notifications and secret storage on Linux.
- Third-party downloads (ClamAV, Android dependencies, the GreenMail test image) are pinned by SHA-256 or image digest.

## Testing

```sh
cargo test -p mail-core                                  # unit, wire and fuzz tests
scripts/test-mail-server.sh start                        # local GreenMail + throwaway CA
CARTEIRO_TEST_CA=$PWD/.testmail/ca.pem \
  cargo test -p mail-core --test live_greenmail -- --ignored --test-threads=1
cargo test -p carteiro -- --ignored engine_detects_eicar # real ClamAV (after fetch-clamav.sh)
```

**Android.** `scripts/build-clamav-android.sh arm64-v8a|x86_64`, then `cargo tauri android init`, `scripts/prepare-android.sh` and `cargo tauri android build`. Debug builds on the emulator reach the test server at `10.0.2.2`, which the test certificate covers. Since Android apps can't be given environment variables, copy the CA to the app's data folder as `test-ca.pem` (`adb run-as dev.carteiro.mail`). Release builds never read it.

The end-to-end tests use made-up accounts (`alice@carteiro.test`, `bob@carteiro.test`) on a local server, never real accounts. They cover sending, replies and threading, rules moving mail on the server, flags, POP3, the Outbox, offline actions replayed on reconnect, STARTTLS refusal, offline download and `.mbox` export. Only debug builds trust the test CA, and only when `CARTEIRO_TEST_CA` is set.

## Develop

Prerequisites:

```sh
brew install rustup && rustup default stable     # or your distribution's Rust
cargo install tauri-cli --version "=2.12.4" --locked
# Linux desktop (Fedora; use a toolbox on Silverblue/Bluefin):
sudo dnf install webkit2gtk4.1-devel openssl-devel libayatana-appindicator-gtk3-devel librsvg2-devel libsecret
# Android: Android Studio with the SDK and NDK, and the Rust Android targets
```

```sh
python3 -m http.server -d ui      # UI only, in a browser, against a mock backend
scripts/fetch-clamav.sh linux-x64 # once: the bundled ClamAV engine
cargo tauri icon app-icon.svg     # once: generates src-tauri/icons
scripts/install-dev-desktop.sh    # once: dock icon for dev builds (Linux)
cargo tauri dev                   # desktop app
```

## Google and Microsoft sign-in

Sign-in needs your own app registrations; client IDs of desktop apps aren't secret. Put the IDs in Settings → *Sign-in apps*, or bake them into CI builds with the repository secrets `CARTEIRO_GOOGLE_CLIENT_ID`, `CARTEIRO_GOOGLE_CLIENT_SECRET` and `CARTEIRO_MICROSOFT_CLIENT_ID`.

- **Google:** in Google Cloud Console, create a project and enable the Gmail API and the CalDAV API. Configure the OAuth consent screen with the scopes `https://mail.google.com/` and `https://www.googleapis.com/auth/calendar`, then create an OAuth client of type *Desktop app*. Until Google verifies the app, only test users you add can sign in, and their sign-ins expire after 7 days.
- **Microsoft:** in Entra ID → App registrations, create an app for "Accounts in any organizational directory and personal Microsoft accounts". Add the platform *Mobile and desktop applications* with redirect URI `http://localhost`, and enable *Allow public client flows*. Add these delegated permissions:
  - `IMAP.AccessAsUser.All` and `SMTP.Send` (Office 365 Exchange Online)
  - `Calendars.ReadWrite` (Microsoft Graph)
  - `offline_access`, `openid`, `email` and `profile`

  Some organisations require an admin to approve third-party apps.

Sign-in opens the system browser and returns through a one-time loopback address (`127.0.0.1:<random port>`) protected by PKCE and a state check, on desktop and Android alike. On Android, switch back to Carteiro after the browser says you're signed in.

## CI and releases

- `ci.yml`: engine tests and clippy on Linux and Windows, a dependency audit (`cargo-deny`), and a compile check of the Windows app with ClamAV bundled.
- `release.yml` builds a **Windows MSI**, a **Linux AppImage** and an **Android APK**, each with ClamAV. It runs on a `v*` tag, or manually without publishing.
  1. All three packages are uploaded to **VirusTotal**. Any engine flagging one as malicious or suspicious stops the release; links to the reports are in the run summary.
  2. On a tag, after the scan passes, the release is published with `SHA256SUMS` and a signed update manifest (`latest.json` + `latest.json.sig`).
  3. The tag must match the version in `Cargo.toml`.
- Every GitHub Action is pinned to a commit SHA; Dependabot (`.github/dependabot.yml`) opens weekly pull requests to update them.

Repository secrets:

| Secret | Needed for |
|---|---|
| `VIRUSTOTAL_API_KEY` | Any release (nothing ships unscanned). A free VirusTotal account works. |
| `UPDATE_SIGNING_KEY` | Any release: the Ed25519 key that signs updates. |
| `ANDROID_KEYSTORE_BASE64`, `ANDROID_KEYSTORE_PASSWORD`, `ANDROID_KEY_ALIAS` | Releasing the APK (manual runs use a throwaway test key). |
| `CARTEIRO_GOOGLE_CLIENT_ID`, `CARTEIRO_GOOGLE_CLIENT_SECRET`, `CARTEIRO_MICROSOFT_CLIENT_ID` | Google / Microsoft sign-in. |

Create the update key once and keep a backup: if it's lost, installed copies can't receive updates any more.

```sh
openssl genpkey -algorithm ed25519 -out carteiro-update.pem
gh secret set UPDATE_SIGNING_KEY < carteiro-update.pem
```

## Updates

Release builds check GitHub twice a day. An update is used only if:

- its manifest carries a valid signature from the release key built into the app;
- it's newer than the running version (no downgrades);
- the download matches the manifest's SHA-256;
- and the bundled ClamAV doesn't flag it.

How it's applied:

- **Linux (AppImage):** the new AppImage replaces the old one, and the next start uses it. Copies installed some other way only get a notification.
- **Windows (MSI):** it installs when Carteiro quits; Windows asks for permission.
- **Android:** apps can't replace themselves, so Carteiro notifies you and opens the release page. Android installs the APK only if it's signed with the same key.

Settings → *Updates* can turn off automatic installation, check now or install now. Local builds have no update channel.

See `THIRD-PARTY.md` for ClamAV's GPL-2.0 notice.

## Known gaps / next steps

- IMAP IDLE (push), connection pooling, full-text search.
- Drafts, rich-text compose, opening attachments directly (saving works).
- Editing repeating calendar events (shown, but edited in the web calendar for now).
- Android: notifications while the app is closed, and Keystore-wrapped secrets.
