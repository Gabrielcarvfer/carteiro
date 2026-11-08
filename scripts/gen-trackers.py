#!/usr/bin/env python3
"""Writes crates/mail-core/data/trackers.txt: the tracker domains from AdGuard's
Tracking Protection filter (GPL-3.0, https://github.com/AdguardTeam/AdGuardFilters).

Host-anchored rules that would block an image in mail are used ("||host^",
with "$third-party" etc.), minus host exceptions. Path rules can't be matched by
host, and the CNAME-disguised mail tracker sub-list is too large to build in:
Carteiro also drops 1x1 tracking pixels and tracking-style host names.
Carteiro uses them to keep images from tracker hosts blocked even after you
choose "Load images". Rerun to refresh the snapshot.

Usage: python3 -I scripts/gen-trackers.py [path to a downloaded filter]
"""
import pathlib
import re
import sys
import urllib.request

URL = "https://filters.adtidy.org/extension/chromium/filters/3.txt"
ROOT = pathlib.Path(__file__).resolve().parent.parent
OUT = ROOT / "crates" / "mail-core" / "data" / "trackers.txt"

text = (pathlib.Path(sys.argv[1]).read_text() if len(sys.argv) > 1
        else urllib.request.urlopen(URL, timeout=60).read().decode())
meta = {}
for k, v in re.findall(r"^! (Title|Version|TimeUpdated): (.*)$", text, re.M):
    meta.setdefault(k, v)  # the first header is the filter's own (sub-lists follow)

# Host rules that block an image loaded by mail: "||host^" or "host$image",
# with no modifiers that limit them to other sites, other resource types, or
# that rewrite rather than block.
rule = re.compile(r"^(@@)?(\|\|)?([a-z0-9][a-z0-9.-]*\.[a-z]{2,})\^?(?:\$(.+))?$")
NOT_BLOCKING = {"domain", "removeparam", "cookie", "csp", "redirect", "redirect-rule", "badfilter", "app", "denyallow",
                "header", "permissions", "jsonprune", "replace", "hls", "removeheader", "stealth", "urltransform", "to", "method"}
TYPES = {"image", "script", "stylesheet", "xmlhttprequest", "font", "media", "subdocument", "ping", "websocket", "other", "document", "object"}


def blocks_mail_images(anchored, mods):
    keys = [m.split("=")[0].lstrip("~") for m in mods]
    if any(k in NOT_BLOCKING for k in keys) or "~image" in mods:
        return False
    types = [m for m in mods if m in TYPES]
    if not anchored:
        # "host$image" rules are the CNAME-disguised mail trackers sub-list:
        # ~300k sender subdomains (11 MB), too large to build in. Carteiro
        # covers them by dropping 1x1 pixels and tracking-style host names.
        return False
    return not types or "image" in types


block, allow = set(), set()
for line in text.splitlines():
    m = rule.match(line.strip())
    if not m:
        continue
    mods = m.group(4).split(",") if m.group(4) else []
    if blocks_mail_images(bool(m.group(2)), mods):
        (allow if m.group(1) else block).add(m.group(3))
domains = sorted(block - allow)
header = [
    f"# {meta.get('Title', 'AdGuard Tracking Protection filter')}, version {meta.get('Version', '?')} ({meta.get('TimeUpdated', '?')})",
    f"# {URL}",
    "# License: GPL-3.0 (AdGuard Filters). Host rules that block images; regenerate with scripts/gen-trackers.py.",
]
OUT.write_text("\n".join(header + domains) + "\n")
print(f"{OUT.relative_to(ROOT)}: {len(domains)} domains ({len(allow)} exceptions applied)")
