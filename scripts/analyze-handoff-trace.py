#!/usr/bin/env python3
"""Summarize lan-mouse handoff-trace.jsonl sessions.

Usage:
    scripts/analyze-handoff-trace.py [path]

Each receiver session is keyed by (peer, serial) from receiver.enter_received;
controller sessions are keyed by serial from capture.*. The script prints:

  role, peer, serial, arm->first input ms, input count, p95/max gap ms,
  first-motion send latency ms
"""

from __future__ import annotations

import json
import statistics
import sys
from collections import defaultdict
from pathlib import Path


def default_path() -> Path:
    xdg = Path.home() / ".cache"
    return xdg / "lan-mouse" / "handoff-trace.jsonl"


def pct(values: list[float], fraction: float) -> float:
    if not values:
        return 0.0
    values = sorted(values)
    return values[min(len(values) - 1, int(len(values) * fraction))]


def summarize(path: Path) -> list[str]:
    sessions: dict[tuple[str, str, object], dict] = defaultdict(
        lambda: {"first_input": None, "gaps": [], "first_motion": None}
    )
    for line in path.read_text(errors="replace").splitlines():
        try:
            rec = json.loads(line)
        except json.JSONDecodeError:
            continue
        if not isinstance(rec, dict):
            continue
        role = rec.get("role")
        stage = rec.get("stage", "")
        peer = rec.get("peer", "")
        serial = rec.get("serial")
        key = (str(role), str(peer), serial)
        session = sessions[key]
        if stage == "receiver.first_input" and session["first_input"] is None:
            session["first_input"] = rec.get("elapsed_us")
        elif stage == "receiver.input_gap_exceeded":
            try:
                session["gaps"].append(float(rec["gap_us"]) / 1000.0)
            except (KeyError, TypeError, ValueError):
                pass
        elif stage == "capture.first_motion_sent" and session["first_motion"] is None:
            session["first_motion"] = rec.get("elapsed_us")

    rows = []
    for (role, peer, serial), session in sorted(sessions.items(), key=lambda item: str(item[0])):
        if role == "receiver":
            first = session["first_input"]
            first_ms = f"{float(first) / 1000:.1f}" if first is not None else "-"
            gaps = session["gaps"]
            p95 = f"{pct(gaps, 0.95):.1f}" if gaps else "-"
            worst = f"{max(gaps):.1f}" if gaps else "-"
            rows.append(
                f"receiver peer={peer} serial={serial} first_input={first_ms}ms "
                f"gaps={len(gaps)} p95={p95}ms max={worst}ms"
            )
        elif role == "controller":
            first = session["first_motion"]
            first_ms = f"{float(first) / 1000:.1f}" if first is not None else "-"
            rows.append(
                f"controller peer={peer} serial={serial} first_motion={first_ms}ms"
            )
    return rows


def main() -> int:
    path = Path(sys.argv[1]) if len(sys.argv) > 1 else default_path()
    if not path.exists():
        print(f"trace file not found: {path}", file=sys.stderr)
        print("enable LAN_MOUSE_HANDOFF_TRACE=1 and reproduce a crossing", file=sys.stderr)
        return 1
    rows = summarize(path)
    if not rows:
        print(f"no handoff sessions in {path}")
        return 1
    print("\n".join(rows))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
