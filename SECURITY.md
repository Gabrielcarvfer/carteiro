# Security

How Carteiro protects your mail and what was done to check it. To report a vulnerability, use GitHub's private reporting (*Security → Report a vulnerability* on the repository) rather than a public issue.

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

**Scanning once.** Results are remembered by the SHA-256 of what was scanned: the same message or attachment isn't scanned again while ClamAV's signatures are unchanged. After a signature update, clean content is checked once more (new signatures may catch what old ones missed); anything found infected stays blocked.

**Attachments** are saved only after passing every check: the message scanned clean, the file type isn't dangerous, and the attachment scans clean on its own. Names are sanitised, existing files are never overwritten, and saved files are readable only by you.

**Backups** are encrypted with a key derived from your passphrase (PBKDF2-HMAC-SHA256, 600,000 rounds) using AES-256-GCM, before they leave the app's private folder: the plain copy made while backing up is owner-only and deleted at once. Each 1 MiB chunk is authenticated along with its position and the file header, so a reordered, cut-short or altered backup is refused instead of restored.

**Local files.** On Linux, the mail cache, calendars and exports are readable only by your user (folder `0700`, files `0600`).

**Penetration tests** (local test server and fake accounts only):
- **Man in the middle** (`tests/pentest_tls.rs`, against real `openssl s_server` instances): self-signed, wrong-host and expired certificates and TLS 1.0/1.1-only servers are all refused; a correct server connects.
- **Hostile email, opened in the Android app** with a listener recording any request that got out:
  - HTML in the subject, sender name, attachment name and folder name;
  - scripts, event handlers, SVG, frames, objects, forms, `meta refresh`, `<base>`, CSS imports, fonts and backgrounds, `srcset`, media, and `javascript:`/`data:` links;
  - breaking out of the mail frame, and UTF-7 script.

  Zero requests reached the listener, even after *Load images*, and nothing was injected into the app's page. Attachment names like `../../../../.bashrc` are saved inside the Downloads folder.
- **Header injection:** line breaks hidden in an encoded subject can't add headers (such as `Bcc:`) to a reply.
- **Local attackers:** the data folder is owner-only, there are no listening network ports (only a one-time loopback during sign-in), and the ClamAV socket is owner-only.
- **Third parties:** ClamAV's updater sends a per-install UUID with every update; Carteiro replaces it with a fresh random one before each update, so updates can't be linked to each other.

**Robustness.** `tests/robustness.rs` is a dependency-free mutation fuzzer. It feeds corrupted input to every parser, and to the IMAP/SMTP/POP3/HTTP clients through hostile fake servers. It found and fixed 4 crash bugs, including unbounded recursion on nested IMAP parentheses. Run it longer with `FUZZ_SCALE=50`.

**Supply chain**
- Only crates.io sources are allowed, and CI fails on known advisories or yanked crates (`cargo-deny`). Builds use `--locked`.
- The mail engine has 46 crates in total. Functionality was written in-house where that was simple and safe: base64, HTTP, XML, iCalendar, message writing, OAuth, notifications and secret storage on Linux.
- Third-party downloads (ClamAV, Android dependencies, the GreenMail test image) are pinned by SHA-256 or image digest.

**End-to-end tests.** The end-to-end tests use made-up accounts (`alice@carteiro.test`, `bob@carteiro.test`) on a local server, never real accounts. They cover sending, replies and threading, rules moving mail on the server, flags, POP3, the Outbox, offline actions replayed on reconnect, STARTTLS refusal, offline download and `.mbox` export. Only debug builds trust the test CA, and only when `CARTEIRO_TEST_CA` is set.

## Running the security tests

```sh
cargo test -p mail-core --test no_plaintext_credentials   # bytes on the wire in every failure case
cargo test -p mail-core --test pentest_tls                 # man in the middle (needs openssl)
FUZZ_SCALE=50 cargo test -p mail-core --test robustness   # longer fuzzing run
scripts/wire-check.sh                                      # packet capture of a fake login (needs dumpcap/tshark)
```
