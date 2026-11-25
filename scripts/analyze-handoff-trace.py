#!/usr/bin/env python3
"""Summarize lan-mouse handoff-trace.jsonl sessions.

Usage:
    scripts/analyze-handoff-trace.py [path ...]
    scripts/analyze-handoff-trace.py --report [--since YYYY-MM-DD] [path ...]

`--report` prints one fluidity summary (percentiles over every session) with
the targets of docs/testing/handoff-fluidity.md instead of one row per
session; with no path it reads the trace and its rotated `.1` backup.

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
        lambda: {
            "first_input": None,
            "gaps": [],
            "first_motion": None,
            "inputs": [],
            "inject_us": [],
            "controller_gaps": [],
        }
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
        elif stage == "receiver.input_applied":
            session["inputs"].append(rec.get("ts_unix_us"))
            try:
                session["inject_us"].append(float(rec["inject_us"]))
            except (KeyError, TypeError, ValueError):
                pass
        elif stage == "controller.input_gap_exceeded":
            try:
                session["controller_gaps"].append(float(rec["gap_us"]) / 1000.0)
            except (KeyError, TypeError, ValueError):
                pass
        elif stage == "capture.first_motion_sent" and session["first_motion"] is None:
            session["first_motion"] = rec.get("elapsed_us")

    rows = []
    for (role, peer, serial), session in sorted(sessions.items(), key=lambda item: str(item[0])):
        if role == "receiver":
            first = session["first_input"]
            first_ms = f"{float(first) / 1000:.1f}" if first is not None else "-"
            applied = [float(ts) for ts in session["inputs"] if ts is not None]
            intervals = [
                (applied[i] - applied[i - 1]) / 1000.0 for i in range(1, len(applied))
            ]
            p95 = f"{pct(intervals, 0.95):.1f}" if intervals else "-"
            worst = f"{max(intervals):.1f}" if intervals else "-"
            med = f"{statistics.median(intervals):.1f}" if intervals else "-"
            inject = session["inject_us"]
            inject_med = f"{statistics.median(inject):.0f}" if inject else "-"
            rows.append(
                f"receiver peer={peer} serial={serial} first_input={first_ms}ms "
                f"inputs={len(intervals) + 1} interval_med={med}ms p95={p95}ms "
                f"max={worst}ms inject_med={inject_med}us"
            )
        elif role == "controller":
            first = session["first_motion"]
            first_ms = f"{float(first) / 1000:.1f}" if first is not None else "-"
            gaps = session["controller_gaps"]
            local_gap = f"{max(gaps):.1f}" if gaps else "-"
            rows.append(
                f"controller peer={peer} serial={serial} first_motion={first_ms}ms "
                f"local_gap_max={local_gap}ms count={len(gaps)}"
            )
    return rows


# A gap longer than this is the hand pausing, not a stall.
PAUSE_MS = 100.0
# Gaps between this and PAUSE_MS while the mouse moves are felt as a stall.
STALL_MS = 25.0


def records(paths: list[Path], since_us: int | None):
    for path in paths:
        for line in path.read_text(errors="replace").splitlines():
            try:
                rec = json.loads(line)
            except json.JSONDecodeError:
                continue
            if not isinstance(rec, dict):
                continue
            ts = rec.get("ts_unix_us")
            if since_us is not None and (not isinstance(ts, int) or ts < since_us):
                continue
            yield rec


def fluidity(recs) -> dict:
    """Fluidity metrics over every hand-off in `recs` (see --report)."""
    receiver: dict[tuple, dict] = defaultdict(lambda: {"first": None, "applied": []})
    # controller serials restart with the service: every capture.begin opens
    # a new hand-off, the stages after it belong to it
    controller: list[dict] = []
    for rec in recs:
        stage = rec.get("stage", "")
        serial = rec.get("serial")
        if rec.get("role") == "receiver":
            session = receiver[(rec.get("peer", ""), serial)]
            if stage == "receiver.first_input" and session["first"] is None:
                session["first"] = _ms(rec.get("elapsed_us"))
            elif stage == "receiver.input_applied" and isinstance(rec.get("ts_unix_us"), int):
                session["applied"].append(rec["ts_unix_us"])
        elif rec.get("role") == "controller":
            if stage == "capture.begin":
                controller.append({"enter": None, "first": None, "dropped": 0})
            if not controller:
                continue
            session = controller[-1]
            if stage == "capture.enter_sent" and session["enter"] is None:
                session["enter"] = _ms(rec.get("elapsed_us"))
            elif stage == "capture.first_motion_sent" and session["first"] is None:
                session["first"] = _ms(rec.get("elapsed_us"))
            elif stage == "capture.reverse_motion_dropped":
                session["dropped"] += 1

    first_inputs = [s["first"] for s in receiver.values() if s["first"] is not None]
    moving_gaps = []
    stalls = 0
    for session in receiver.values():
        applied = sorted(session["applied"])
        for a, b in zip(applied, applied[1:]):
            gap = (b - a) / 1000.0
            if gap <= PAUSE_MS:
                moving_gaps.append(gap)
                if gap > STALL_MS:
                    stalls += 1
    enters = [s["enter"] for s in controller if s["enter"] is not None]
    firsts = [s["first"] for s in controller if s["first"] is not None]
    dropped = [s["dropped"] for s in controller]
    return {
        "receiver_sessions": len(receiver),
        "first_input_ms": _spread(first_inputs),
        "moving_gap_ms": _spread(moving_gaps),
        "stalls_per_1000": 1000.0 * stalls / len(moving_gaps) if moving_gaps else 0.0,
        "controller_sessions": len(controller),
        "enter_sent_ms": _spread(enters),
        "first_motion_sent_ms": _spread(firsts),
        "sessions_with_dropped_return": (
            100.0 * sum(1 for d in dropped if d) / len(dropped) if dropped else 0.0
        ),
    }


def _ms(value) -> float | None:
    try:
        return float(value) / 1000.0
    except (TypeError, ValueError):
        return None


def _spread(values: list[float]) -> dict:
    if not values:
        return {}
    return {
        "p50": pct(values, 0.50),
        "p90": pct(values, 0.90),
        "p99": pct(values, 0.99),
        "max": max(values),
        "n": len(values),
    }


def _fmt(spread: dict) -> str:
    if not spread:
        return "-"
    return (
        f"p50 {spread['p50']:.1f}  p90 {spread['p90']:.1f}  p99 {spread['p99']:.1f}  "
        f"max {spread['max']:.1f}  (n={spread['n']})"
    )


def report(metrics: dict) -> list[str]:
    return [
        f"receiver (a peer drives this PC): {metrics['receiver_sessions']} hand-offs",
        f"  enter -> first input applied, ms   {_fmt(metrics['first_input_ms'])}   target p90 <= 16",
        f"  gap between inputs while moving, ms {_fmt(metrics['moving_gap_ms'])}",
        f"  stalls ({STALL_MS:.0f}-{PAUSE_MS:.0f} ms gaps) per 1000 inputs  "
        f"{metrics['stalls_per_1000']:.1f}   target 0",
        f"controller (this PC's mouse crosses): {metrics['controller_sessions']} hand-offs",
        f"  begin -> Enter sent, ms             {_fmt(metrics['enter_sent_ms'])}",
        f"  begin -> first motion sent, ms      {_fmt(metrics['first_motion_sent_ms'])}",
        f"  hand-offs with return motion dropped  "
        f"{metrics['sessions_with_dropped_return']:.0f}%  (activation backswing included)",
    ]


def main() -> int:
    args = sys.argv[1:]
    if args and args[0] == "--report":
        args = args[1:]
        since_us = None
        if len(args) >= 2 and args[0] == "--since":
            import datetime

            day = datetime.datetime.strptime(args[1], "%Y-%m-%d")
            since_us = int(day.timestamp() * 1_000_000)
            args = args[2:]
        paths = [Path(a) for a in args] or [
            p for p in (default_path().with_suffix(".jsonl.1"), default_path()) if p.exists()
        ]
        if not paths:
            print("no trace file; enable LAN_MOUSE_HANDOFF_TRACE=1", file=sys.stderr)
            return 1
        print("\n".join(report(fluidity(records(paths, since_us)))))
        return 0
    path = Path(args[0]) if args else default_path()
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
