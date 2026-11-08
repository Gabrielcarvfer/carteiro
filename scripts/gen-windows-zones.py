#!/usr/bin/env python3
"""Writes crates/mail-core/data/windows_zones.txt: Windows time zone names
(used by Outlook and Microsoft Graph) mapped to IANA names, from Unicode
CLDR's windowsZones.xml (Unicode-3.0 license), the "001" (default) entries.

Usage: python3 -I scripts/gen-windows-zones.py
"""
import pathlib
import re
import urllib.request

TAG = "release-49"
URL = f"https://raw.githubusercontent.com/unicode-org/cldr/{TAG}/common/supplemental/windowsZones.xml"
OUT = pathlib.Path(__file__).resolve().parent.parent / "crates" / "mail-core" / "data" / "windows_zones.txt"

xml = urllib.request.urlopen(URL, timeout=60).read().decode()
pairs = sorted(set(re.findall(r'<mapZone other="([^"]+)" territory="001" type="([^" ]+)"', xml)))
assert len(pairs) > 100, f"only {len(pairs)} zones: format changed?"
OUT.write_text(f"# Windows -> IANA time zone names, Unicode CLDR {TAG} windowsZones.xml (Unicode-3.0).\n# Regenerate with scripts/gen-windows-zones.py\n"
               + "".join(f"{w}\t{i}\n" for w, i in pairs))
print(f"{OUT.name}: {len(pairs)} zones")
