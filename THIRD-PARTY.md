# Third-party software bundled with Carteiro

## ClamAV

Carteiro's desktop and Android builds include **ClamAV** (https://www.clamav.net),
the open-source antivirus engine by Cisco Talos, to scan mail and calendars.

- License: GNU General Public License, version 2 (GPL-2.0). The full license
  text (`COPYING*`) is installed next to the ClamAV programs.
- ClamAV runs as separate programs (`clamd`, `freshclam`) that Carteiro starts
  and talks to over a local socket; Carteiro itself is not linked against it.
- Source code: https://github.com/Cisco-Talos/clamav (the exact version is
  pinned in `scripts/fetch-clamav.sh`).
- Virus signatures are downloaded by `freshclam` from ClamAV's official
  mirrors and verified with ClamAV's own signing certificate.
