# Developing Carteiro

## Why Rust + Tauri 2 + plain JavaScript

| Option | Windows + Android from one codebase | Speed / size | Mail/TLS ecosystem | Verdict |
|---|---|---|---|---|
| **Tauri 2 (Rust core + plain JS UI)** | Yes | Small installers, native core, no JS build step | rustls, SQLite, mail-parser | **Chosen** |
| Flutter | Yes | Good | Dart mail libraries are thinner; TLS goes through Dart | Viable second choice |
| .NET MAUI | Yes | OK | MailKit is excellent | Weak on Linux; heavier |
| Electron | No Android | Heavy | Node | No |

The mail engine (`crates/mail-core`) is plain Rust with no UI dependency, so it can be tested headless. The UI is plain HTML, CSS and JavaScript modules in `ui/`: no npm, no bundler, no build step. Tauri shows it in the system web view (WebView2 on Windows, Android System WebView, WebKitGTK on Linux), which is needed anyway to display HTML email.

## Build and run

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

## Layout

```
crates/mail-core/      protocol, storage and calendar engine (most tests live here)
  net.rs, http.rs      TCP + TLS (rustls), STARTTLS upgrade, minimal HTTPS client
  imap*.rs pop3.rs smtp.rs   mail protocols
  message.rs           parsing (mail-parser) and writing (built in) of messages
  store.rs dedup.rs    SQLite cache, threading, deduplicated attachments
  rules.rs             rules engine
  caldav.rs graph.rs ical.rs xml.rs calendar.rs   calendars
  carddav.rs contacts.rs                          contacts
  oauth.rs             Google / Microsoft sign-in
  scan.rs safety.rs    ClamAV client, attachment and link heuristics
  update.rs            signed self-update from GitHub releases
  sync.rs ops.rs settings_io.rs b64.rs
src-tauri/             app shell: commands, tray, secrets, bundled ClamAV engine
ui/                    the interface
scripts/               ClamAV fetch/build, local test mail server, wire check
```

## Testing

```sh
cargo test -p mail-core                                  # unit, wire and fuzz tests
scripts/test-mail-server.sh start                        # local GreenMail + throwaway CA
CARTEIRO_TEST_CA=$PWD/.testmail/ca.pem \
  cargo test -p mail-core --test live_greenmail -- --ignored --test-threads=1
CARTEIRO_CARDDAV_URL=http://127.0.0.1:5232/ \
  cargo test -p mail-core --test live_carddav -- --ignored  # a Radicale with alice/alice-pw
cargo test -p carteiro -- --ignored engine_detects_eicar # real ClamAV (after fetch-clamav.sh)
```

**Android.** `scripts/build-clamav-android.sh arm64-v8a|x86_64`, then `cargo tauri android init`, `scripts/prepare-android.sh` and `cargo tauri android build`. Debug builds on the emulator reach the test server at `10.0.2.2`, which the test certificate covers. Since Android apps can't be given environment variables, copy the CA to the app's data folder as `test-ca.pem` (`adb run-as dev.carteiro.mail`). Release builds never read it.

The end-to-end tests use made-up accounts on a local server, never real ones. See [SECURITY.md](SECURITY.md) for the security tests.

## Google and Microsoft sign-in

Sign-in uses Carteiro's own app registrations, built into release packages from the repository secrets `CARTEIRO_GOOGLE_CLIENT_ID`, `CARTEIRO_GOOGLE_CLIENT_SECRET` and `CARTEIRO_MICROSOFT_CLIENT_ID` (client IDs of desktop apps aren't secret). Users never enter them. For a local build, set the same variables when building; a build without them doesn't offer Google or Microsoft sign-in.

- **Google:** in Google Cloud Console, create a project and enable the Gmail API, the CalDAV API and the Google Contacts CardDAV API. Configure the OAuth consent screen with the scopes `https://mail.google.com/`, `https://www.googleapis.com/auth/calendar` and `https://www.googleapis.com/auth/carddav`, then create an OAuth client of type *Desktop app*. Until Google verifies the app, only test users you add can sign in, and their sign-ins expire after 7 days.
- **Microsoft:** in Entra ID → App registrations, create an app for "Accounts in any organizational directory and personal Microsoft accounts". Add the platform *Mobile and desktop applications* with redirect URI `http://localhost`, and enable *Allow public client flows*. Add these delegated permissions:
  - `IMAP.AccessAsUser.All` and `SMTP.Send` (Office 365 Exchange Online)
  - `Calendars.ReadWrite` and `Contacts.Read` (Microsoft Graph)
  - `offline_access`, `openid`, `email` and `profile`

  Some organisations require an admin to approve third-party apps.

Sign-in opens the system browser and returns through a one-time loopback address (`127.0.0.1:<random port>`) protected by PKCE and a state check, on desktop and Android alike. On Android, switch back to Carteiro after the browser says you're signed in.

## CI and releases

- `ci.yml`, on every push to `master` and every pull request:
  - engine tests and clippy on Linux and Windows;
  - a dependency audit (`cargo-deny`, built from a pinned crates.io release, auditing the shipped platforms);
  - the Windows MSI, the Linux AppImage and the Android APK, built as for a release. Each package is checked: unpacked, every bundled program finds its libraries, and the bundled `clamd` starts. They're kept 7 days as test artifacts, signed with throwaway keys.
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

## Known gaps and next steps

- IMAP IDLE (push), connection pooling, full-text search.
- Drafts, rich-text compose, opening attachments directly (saving works).
- Editing repeating calendar events (shown, but edited in the web calendar for now).
- Android: notifications while the app is closed, and Keystore-wrapped secrets.
