#!/usr/bin/env python3
"""Test our API against locally-generated labeled payloads.

Runs each payload through the API and compares with the brute-force
ground truth label. Reports TP/TN/FP/FN counts and accuracy.

Usage: accuracy.py <base_url> <payloads.json>
"""
import json
import sys
import time
from concurrent.futures import ThreadPoolExecutor
from http.client import HTTPConnection
from threading import local

if len(sys.argv) < 3:
    print("Usage: accuracy.py <base_url> <payloads.json>")
    sys.exit(1)

BASE = sys.argv[1]
payloads_path = sys.argv[2]
host_port = BASE.replace("http://", "").split(":")
host = host_port[0]
port = int(host_port[1])

with open(payloads_path) as f:
    data = json.load(f)
entries = data["entries"]
print(f"Loaded {len(entries)} payloads from {payloads_path}")
print(f"  fraud: {data['stats'].get('fraud_count', '?')}, legit: {data['stats'].get('legit_count', '?')}")

_tl = local()


def conn():
    c = getattr(_tl, "c", None)
    if c is None:
        c = HTTPConnection(host, port, timeout=10)
        _tl.c = c
    return c


def one(entry):
    request = entry["request"] if "request" in entry else entry
    expected = entry.get("expected_approved", None)
    if expected is None:
        return ("skip", None)
    body = json.dumps(request)
    c = conn()
    try:
        c.request("POST", "/fraud-score", body,
                  {"Content-Type": "application/json"})
        r = c.getresponse()
        raw = r.read()
        if r.status != 200:
            return ("err", None)
        approved = json.loads(raw)["approved"]
    except Exception:
        try:
            _tl.c = None
        except Exception:
            pass
        return ("err", None)
    if expected == approved:
        return ("tn" if approved else "tp", None)
    return ("fn" if approved else "fp", None)


t0 = time.time()
tally = {"tp": 0, "tn": 0, "fp": 0, "fn": 0, "err": 0, "skip": 0}
with ThreadPoolExecutor(max_workers=16) as ex:
    for kind, _ in ex.map(one, entries):
        tally[kind] += 1
dt = time.time() - t0

n = sum(tally.values()) - tally["skip"]
tp, tn, fp, fn, errs = tally["tp"], tally["tn"], tally["fp"], tally["fn"], tally["err"]
E = fp * 1 + fn * 3 + errs * 5
failures = fp + fn + errs
epsilon = E / n if n else 0
acc = (tp + tn) / n if n else 0

print(f"\n  entries        : {len(entries)} (processed {n})")
print(f"  elapsed        : {dt:.1f}s ({n/dt:.0f} req/s)")
print(f"  TP (deny fraud): {tp}")
print(f"  TN (ok legit)  : {tn}")
print(f"  FP (deny legit): {fp}   weight 1")
print(f"  FN (miss fraud): {fn}   weight 3")
print(f"  errors (non200): {errs}   weight 5")
print(f"  failures       : {failures}  ({100*failures/n:.3f}%)")
print(f"  weighted E     : {E}")
print(f"  epsilon (E/N)  : {epsilon:.5f}")
print(f"  accuracy       : {100*acc:.3f}%")

# Bot score estimate (using same formula as oficial)
import math
p99_ms = 2.5  # placeholder; not measured here
p99_score = 1000 * math.log10(1000 / max(p99_ms, 1))
p99_score = max(0, min(3000, p99_score))
det_score = 0
if n > 0:
    if failures / n > 0.15:
        det_score = -3000
    else:
        rate = max(epsilon, 0.001)
        det_score = 1000 * math.log10(1 / rate) - 300 * math.log10(1 + E)
        det_score = max(-3000, min(3000, det_score))
print(f"  p99_score (est): {p99_score:.2f} (assuming p99=2.5ms)")
print(f"  det_score (est): {det_score:.2f}")
print(f"  final_score est: {p99_score + det_score:.2f}")
