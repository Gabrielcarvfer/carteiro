#!/usr/bin/env python3
"""Writes ui/licenses.json, the component list shown in Settings > About.

Rust crates come from `cargo metadata` (everything the app can link on any
platform); bundled programs are listed here, next to the scripts that pin
them. CI regenerates this file and fails if it changed, so it can't go stale.

Usage: python3 -I scripts/gen-licenses.py
"""
import json
import pathlib
import subprocess

ROOT = pathlib.Path(__file__).resolve().parent.parent

# Programs and C libraries shipped inside the packages (not Rust crates).
BUNDLED = [
    {"name": "ClamAV", "version": "1.5.4", "license": "GPL-2.0-only", "url": "https://www.clamav.net", "note": "virus scanner (clamd, freshclam); all platforms"},
    {"name": "zlib", "version": "1.3.1", "license": "Zlib", "url": "https://zlib.net", "note": "used by ClamAV on Android"},
    {"name": "bzip2", "version": "1.0.8", "license": "bzip2-1.0.6", "url": "https://sourceware.org/bzip2/", "note": "used by ClamAV on Android"},
    {"name": "OpenSSL", "version": "3.5.4", "license": "Apache-2.0", "url": "https://www.openssl.org", "note": "used by ClamAV on Android"},
    {"name": "PCRE2", "version": "10.45", "license": "BSD-3-Clause WITH PCRE2-exception", "url": "https://www.pcre.org", "note": "used by ClamAV on Android"},
    {"name": "libxml2", "version": "2.14.6", "license": "MIT", "url": "https://gitlab.gnome.org/GNOME/libxml2", "note": "used by ClamAV on Android"},
    {"name": "json-c", "version": "0.18", "license": "MIT", "url": "https://github.com/json-c/json-c", "note": "used by ClamAV on Android"},
    {"name": "curl", "version": "8.16.0", "license": "curl", "url": "https://curl.se", "note": "used by ClamAV on Android"},
    {"name": "ncurses", "version": "6.5", "license": "X11", "url": "https://invisible-island.net/ncurses/", "note": "used by ClamAV on Android"},
    {"name": "AdGuard Tracking Protection filter", "version": "snapshot", "license": "GPL-3.0", "url": "https://github.com/AdguardTeam/AdGuardFilters", "note": "tracker hosts whose images are never loaded"},
    {"name": "Unicode CLDR Windows time zone mapping", "version": "49", "license": "Unicode-3.0", "url": "https://github.com/unicode-org/cldr", "note": "Outlook / Microsoft time zone names"},
    {"name": "AdGuard CNAME-disguised mail trackers list", "version": "downloaded daily", "license": "MIT", "url": "https://github.com/AdguardTeam/cname-trackers", "note": "mail tracker hosts whose images are never loaded"},
]


# The platforms Carteiro ships for (Linux AppImage, Windows MSI, Android APK).
TARGETS = ["x86_64-unknown-linux-gnu", "x86_64-pc-windows-msvc", "aarch64-linux-android", "x86_64-linux-android"]


def crates():
    """Crates the shipped builds use, including build-time tools (build
    scripts, code generators): `cargo metadata` alone would also list
    dependencies for platforms Carteiro doesn't ship (iOS, macOS...)."""
    used = set()
    for t in TARGETS:
        out = subprocess.check_output(
            ["cargo", "tree", "-p", "carteiro", "--locked", "--target", t, "-e", "normal,build", "--prefix", "none", "--format", "{p}"],
            cwd=ROOT, text=True)
        for line in out.splitlines():
            name, version = line.split()[:2]
            used.add((name, version.lstrip("v")))
    meta = json.loads(subprocess.check_output(["cargo", "metadata", "--format-version", "1", "--locked"], cwd=ROOT))
    out = []
    for p in meta["packages"]:
        if (p["name"], p["version"]) in used and p["source"] is not None:
            out.append({"name": p["name"], "version": p["version"], "license": p.get("license") or "see its repository",
                        "url": p.get("repository") or f"https://crates.io/crates/{p['name']}"})
    return sorted(out, key=lambda c: (c["name"].lower(), c["version"]))


data = {
    "app": {"name": "Carteiro", "license": "GPL-3.0-or-later", "url": "https://github.com/Gabrielcarvfer/carteiro"},
    "bundled": BUNDLED,
    "crates": crates(),
}
(ROOT / "ui" / "licenses.json").write_text(json.dumps(data, indent=1, ensure_ascii=False) + "\n")
print(f"ui/licenses.json: {len(data['bundled'])} bundled components, {len(data['crates'])} crates")
