#!/usr/bin/env python3
"""Measure the network path to a peer without going through Lan Mouse.

Round-trip time is taken from the TCP handshake to any open port of the peer
(answered by its kernel, so the application is not involved) and compared with
ICMP pings to the default gateway sent at the same time. A spike that shows up
only on the peer path is the peer's radio or its access point link; a spike on
both is this machine's link.

Usage:
    scripts/net-probe.py PEER_IP [--port 7680] [--seconds 120] [--gateway IP]

Windows answers on 7680 (Delivery Optimization) by default; any open TCP port
works. ICMP to Windows is usually blocked by its firewall, which is why the
TCP handshake is used.
"""

import argparse
import re
import socket
import subprocess
import sys
import time


def percentile(values, p):
    ordered = sorted(values)
    return ordered[min(len(ordered) - 1, int(len(ordered) * p))]


def default_gateway():
    out = subprocess.run(["ip", "route"], capture_output=True, text=True).stdout
    match = re.search(r"^default via (\S+)", out, re.M)
    return match.group(1) if match else None


def summary(label, samples):
    if not samples:
        return f"{label}: no samples"
    return (
        f"{label}: n={len(samples)} p50={percentile(samples, 0.5):.1f} "
        f"p90={percentile(samples, 0.9):.1f} p99={percentile(samples, 0.99):.1f} "
        f"max={max(samples):.1f} ms"
    )


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("peer")
    parser.add_argument("--port", type=int, default=7680)
    parser.add_argument("--seconds", type=float, default=120.0)
    parser.add_argument("--interval", type=float, default=0.2)
    parser.add_argument("--gateway", default=None)
    parser.add_argument("--spike-ms", type=float, default=15.0)
    args = parser.parse_args()

    gateway = args.gateway or default_gateway()
    ping = None
    if gateway:
        ping = subprocess.Popen(
            ["ping", "-i", str(args.interval), "-D", "-n", gateway],
            stdout=subprocess.PIPE,
            text=True,
        )

    peer = []
    started = time.time()
    while time.time() - started < args.seconds:
        sock = socket.socket()
        sock.settimeout(1.0)
        begin = time.perf_counter()
        try:
            sock.connect((args.peer, args.port))
            rtt = (time.perf_counter() - begin) * 1000
        except ConnectionRefusedError:
            # A refusal is still an answer from the peer's kernel.
            rtt = (time.perf_counter() - begin) * 1000
        except OSError:
            rtt = None
        finally:
            sock.close()
        peer.append((time.time(), rtt))
        time.sleep(args.interval)

    gw = []
    if ping:
        ping.terminate()
        for match in re.finditer(r"\[(\d+\.\d+)\].*time=([\d.]+) ms", ping.stdout.read()):
            gw.append((float(match.group(1)), float(match.group(2))))

    answered = [rtt for _, rtt in peer if rtt is not None]
    if not answered:
        sys.exit(f"no answer from {args.peer}:{args.port}; try another --port")
    print(summary(f"peer {args.peer}:{args.port}", answered), f"lost={len(peer) - len(answered)}")
    if gw:
        print(summary(f"gateway {gateway}", [rtt for _, rtt in gw]))

    spikes = [(t, rtt) for t, rtt in peer if rtt is None or rtt >= args.spike_ms]
    print(f"spikes >= {args.spike_ms:.0f} ms on the peer path: {len(spikes)} of {len(peer)}")
    for t, rtt in spikes:
        near = min(gw, key=lambda sample: abs(sample[0] - t))[1] if gw else float("nan")
        shown = "LOST" if rtt is None else f"{rtt:.1f}"
        clock = time.strftime("%H:%M:%S", time.localtime(t))
        print(f"  {clock} peer={shown} gateway={near:.1f}")
    if len(spikes) > 2:
        gaps = [round(b[0] - a[0], 1) for a, b in zip(spikes, spikes[1:])]
        print(f"seconds between spikes: {gaps}")


if __name__ == "__main__":
    main()
