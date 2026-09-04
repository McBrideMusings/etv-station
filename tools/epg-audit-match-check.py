# /// script
# requires-python = ">=3.11"
# dependencies = ["textual>=0.60", "httpx>=0.27"]
# ///
"""Assert find_matching_audit_item matches on whole-second resolution.

XMLTV's datetime format (XMLTV_DATETIME in epg-browser.py) can only express
whole seconds. The audit report's `start` is RFC3339 formatted from the
chunk file's OffsetDateTime and keeps microseconds
(crates/etv-station/src/audit_report.rs's `rfc3339`). Comparing the two for
exact equality can never succeed for any real item on any channel
(etv-station-d4ss.7) — this fails against the pre-fix exact-equality
comparison and passes once both sides are truncated to the whole second.

Run with: uv run tools/epg-audit-match-check.py
"""

from __future__ import annotations

import importlib.util
import sys
from datetime import datetime, timezone
from pathlib import Path

SRC = Path(__file__).resolve().parent / "epg-browser.py"
spec = importlib.util.spec_from_file_location("epg_browser", SRC)
epgb = importlib.util.module_from_spec(spec)
sys.modules["epg_browser"] = epgb
spec.loader.exec_module(epgb)


def check_matches_despite_subsecond_drift() -> bool:
    """An audit item with sub-second precision must match a programme start
    naming the same whole second."""
    report = {
        "items": [
            {"start": "2026-09-04T01:26:04.678777Z"},
        ]
    }
    target = datetime.fromisoformat("2026-09-04T01:26:04+00:00")
    match = epgb.find_matching_audit_item(report, target)
    ok = match is not None
    print(f"  {'subsecond match':20} {match!r}  {'OK' if ok else 'FAIL (no match found)'}")
    return ok


def check_genuine_miss_still_reports_none() -> bool:
    """An item a full second away (or more) from the programme start is a
    real miss — the schedule genuinely moved — and must still return None."""
    report = {
        "items": [
            {"start": "2026-09-04T01:26:05.000000Z"},
        ]
    }
    target = datetime.fromisoformat("2026-09-04T01:26:04+00:00")
    match = epgb.find_matching_audit_item(report, target)
    ok = match is None
    print(f"  {'genuine miss':20} {match!r}  {'OK' if ok else 'FAIL (matched a different second)'}")
    return ok


def main() -> None:
    results = [
        check_matches_despite_subsecond_drift(),
        check_genuine_miss_still_reports_none(),
    ]
    ok = all(results)
    print("PASS" if ok else "FAIL")
    sys.exit(0 if ok else 1)


if __name__ == "__main__":
    main()
