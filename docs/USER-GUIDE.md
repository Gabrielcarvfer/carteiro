# Using Carteiro

The left rail (the bottom bar on phones) switches between **Mail**, **Calendar** and **Contacts**. On a wide screen each section shows a list beside the open item; on a phone it shows one at a time, and Android's back gesture steps back.

## Accounts

- **Add an account:** at first start, or from Settings → *Manage accounts…*. Type your address. Gmail and Outlook / Microsoft 365 addresses offer **Sign in with Google / Microsoft**, which opens your browser. Any other provider takes a password; Carteiro finds the right server settings and user-name form, and you can set the IMAP/POP3 and SMTP servers by hand.
- **More than one account:** the account menu at the top switches between them; *All inboxes* shows them together.

## Mail

- **Read:** messages are grouped into conversations, newest first. Older messages in a long thread stay folded until you open them, and quoted text is folded too.
- **Write:** the compose button (the pencil on phones). The To, Cc and Bcc fields suggest addresses from your contacts as you type. Reply, Reply all and Forward are on each message.
- **Swipe** a conversation sideways on a touch screen: left marks it unread and right marks it read, unless you choose other actions (flag, archive, delete or nothing) in Settings → *general*.
- **Several at once:** press and hold a conversation (Ctrl-click or ⌘-click on a computer), then tap others to add or remove them. The bar at the top marks them read or unread, flags, deletes or moves them together; ✕ or Back stops picking.
- **Act on messages:** flag, mark read or unread, archive, delete, and *Move to* any folder. With no connection, sending goes to the Outbox, and every action is replayed when you're back online.
- **Filter:** the filter button limits the list to *Unread*, *Flagged*, *To me* or *Has attachments*. Tap the chip above the list to show everything again.
- **Search:** the search box understands words and these terms, which combine:
  `from:ana`, `to:bob`, `subject:invoice`, `has:attachment`, `is:unread`, `is:flagged`, `after:2026-11-01`, `before:2026-12-01`, and `from:"Ana Souza"` for phrases.
- **Rules:** the Rules button (or *Create rule…* on a message) moves, flags or marks incoming mail by sender, recipient, subject or words, in the order you set. *Run rules on this folder now* applies them to mail already there.
- **Images and safety:** remote images stay blocked until you choose *Load images*, and even then trackers stay blocked. A badge on each message says whether ClamAV has checked it; attachments can be saved only once they pass.
- **Saved files:** attachments and exports go to **Downloads/Carteiro**, named with your local date and time.

## Calendar

- **Views:** day, week and month on a wide screen; an agenda on phones. Swipe sideways to change month or week, and scroll through the weeks.
- **Events:** *New event* creates one (on phones, the + button); tap an event to edit it. Each event can have its own time zone; when it differs from yours, the event also shows your local time.
- **Search:** the search box finds events by title, place or description.
- **Invitations:** when a message brings a calendar invitation (an `.ics` file or an invite from Outlook, Google and the like) and ClamAV has found it clean, the event, or the whole repeating series, goes straight into your **Invitations** calendar. Updates from the same sender replace it, and cancellations remove it. Nobody is told unless you choose *Send answer* (below).
- **Calendar files in mail:** tapping an `.ics` attachment shows what's in it and asks: *Add to calendar* (or *Show in calendar* when it's already there; adding it again brings back dates you removed), or *Save the file to Downloads*. A cancellation file offers to remove the event instead.
- **Removing dates from a series:** open an invitation's event and choose *Delete*: remove only that date, it and all later ones, every one in a period you pick, or the whole series. Removed dates stay removed when the organizer sends updates. (Series from Google, Microsoft or CalDAV calendars are changed in those services.)
- **Will you go?** On any event, mark *Going*, *Maybe* or *Not going*; in a repeating series, each date has its own answer (the card in the message answers for its next date). It's your own note, kept on this device, and events you won't go to fade out. On an invitation, **Send answer** tells the organizer, from the account the invitation came to, as a reply their calendar understands; until you press it, nobody is told.
- **Calendars:** the menu (☰ on phones) lists your calendars with checkboxes to show or hide each one. *Add calendar* offers:
  - Google and Microsoft calendars, brought in by those accounts automatically;
  - any CalDAV server (Fastmail, iCloud, Nextcloud…) with its address, user name and password;
  - a subscribed `.ics` link;
  - public holidays for a country and, where they differ, a region;
  - local calendars kept only on this device.
- **Week start:** Settings → *Calendar*.

## Contacts

- **Browse:** contacts are listed A–Z. Tap a letter heading to see the whole alphabet and jump to any letter that has contacts. The search box finds names and addresses.
- **Your contacts:** *New* creates one; open a contact to edit or delete it. Tapping an email address starts a message to it.
- **People you mail with** are added automatically ("From your mail"): everyone who writes to you and everyone you write to, except no-reply and other robot addresses, mail in Junk or Trash, and people already in your contacts. Edit or delete them like your own; a deleted one doesn't come back.
- **Phone contacts (Android):** in the address-book dialog, *Bring in* reads the contacts saved on the phone (Android asks for permission first) and keeps them up to date. Change them in the phone's own app; *Stop* removes them from Carteiro.
- **Import from Gmail, Outlook or Thunderbird:** export your contacts there, then use *Import* here:
  - **Gmail** (contacts.google.com → Export): *vCard* or *Google CSV*;
  - **Outlook** (People → Manage → Export contacts): CSV;
  - **Thunderbird** (Address Book → Tools → Export): vCard, CSV or LDIF.

  A contact that matches one you already have (same address or phone) is merged into it instead of duplicated.
- **Export:** saves every contact as one `.vcf` file in Downloads/Carteiro.
- **Address books that stay in sync:** Google and Microsoft accounts bring their contacts automatically. The address-book button adds any CardDAV server (Fastmail, iCloud, Nextcloud, mailbox.org…). Synced contacts are read-only here: change them on the service and they update at the next sync. Deleting one removes it from Carteiro only; it stays on the service and doesn't come back here. Accounts signed in before contacts existed need one fresh sign-in to grant access.

## Backups and moving to another device

Settings → *Your data* → **Back up everything…** saves one file, `Carteiro backup <date>.carteiro`, holding all your mail, contacts, calendars, rules, settings and passwords. It's encrypted with a passphrase you choose, and without that passphrase nobody can open it, not even you, so write it down.

- **Where it goes:** on desktop, any folder you type (a USB drive, a synced folder, another disk), or Downloads/Carteiro. On Android, Download/Carteiro, which stays even if the app is uninstalled; copy it off the phone too.
- **Restoring:** on a new install, the welcome screen offers **Restore from a backup** (also under Settings → *Your data*). Pick the file, enter the passphrase, and everything is back, passwords included, so mail syncs right away. It replaces whatever that install had. A wrong passphrase or a damaged file changes nothing.
- **Any device to any device:** a backup made on Android restores on Windows or Linux and the other way round.
- This is also the way back if an app can't be updated in place, for example if an Android build was signed with a different key: back up, uninstall, install, restore.

## Settings

Settings has four sections side by side: swipe sideways (or tap a title, or use the arrow keys) to move between them. Changes are saved as you make them.

- **general:** your accounts; when to check for mail (every 15 minutes during working hours by default, Monday to Friday 08:00–18:00, and hourly outside them, or only when you ask); notifications; keeping Carteiro in the tray; the first day of the week.
- **security & privacy:** turn ClamAV on or off, update its signatures, or use your own `clamd`; refresh the tracker list (it also updates daily).
- **your data:** how much mail to keep offline (from the last day to all of it); backups (below); save all mail as `.mbox`; export or import your settings (passwords are never included).
- **about:** updates (turn off automatic installation, check now, or install now), the version, and the licenses of the bundled software.

The sun/moon button switches between light and dark.

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

Carteiro bundles ClamAV; see [THIRD-PARTY.md](../THIRD-PARTY.md) for its GPL-2.0 notice.
