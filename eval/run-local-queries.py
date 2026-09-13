#!/usr/bin/env python3
"""Run the local-search query suite and report pass/fail per expectation.

This is a behavioural suite, not a relevance metric — it asks "did the engine
do the right *kind* of thing?" rather than "was the ranking optimal". Both
matter; this one catches the failures that make a demo look broken.
"""
import json
import sys
import time
import urllib.parse
import urllib.request

API = sys.argv[1] if len(sys.argv) > 1 else "http://localhost:8080"
# Bengaluru: dense OSM coverage, so failures indicate engine problems rather
# than gaps in the underlying data.
LAT, LON = 12.9716, 77.5946
RADIUS = 3000


def query(q):
    params = urllib.parse.urlencode(
        {"q": q, "lat": LAT, "lon": LON, "radius": RADIUS, "limit": 5}
    )
    with urllib.request.urlopen(f"{API}/nearby?{params}", timeout=30) as r:
        return json.load(r)


def words(s):
    return [w for w in "".join(c if c.isalnum() else " " for c in s.lower()).split() if len(w) > 2]


def check(expect, q, d):
    """Return (ok, note)."""
    results = d.get("results", [])
    place = d.get("resolved_place")

    if expect == "decline":
        if place and place.get("scope_too_broad"):
            return True, "declined (too broad)"
        if not results:
            return True, "no match"
        return False, f"returned {results[0]['name'][:34]}"

    if expect == "place":
        if not place:
            return False, "place not resolved"
        if not results:
            return False, f"resolved {place['name']} but 0 results"
        return True, f"{place['name']} ({place['kind']}), {len(results)} results"

    if expect == "named":
        if not results:
            return False, "no results"
        # At least one distinctive word of the query should appear in a name.
        stop = {"near", "nearest", "closest", "the"}
        qw = [w for w in words(q) if w not in stop]
        for r in results:
            hay = f"{r['name']} {r.get('brand','')}".lower()
            if any(w in hay for w in qw):
                return True, f"{r['name'][:34]}"
        return False, f"no name match; got {results[0]['name'][:30]}"

    # expect == "local"
    if not results:
        return False, "no results"
    return True, f"{len(results)} results, nearest {results[0]['distance_m']:.0f}m"


def main():
    cases = []
    with open("eval/local-queries.txt") as f:
        for line in f:
            line = line.strip()
            if not line or line.startswith("#"):
                continue
            expect, q = line.split("\t", 1)
            cases.append((expect.strip(), q.strip()))

    by_group = {}
    failures = []
    t0 = time.time()

    for expect, q in cases:
        try:
            d = query(q)
            ok, note = check(expect, q, d)
        except Exception as e:
            ok, note = False, f"ERROR {e}"
        by_group.setdefault(expect, [0, 0])
        by_group[expect][1] += 1
        if ok:
            by_group[expect][0] += 1
        else:
            failures.append((expect, q, note))

    total = len(cases)
    passed = sum(g[0] for g in by_group.values())
    elapsed = time.time() - t0

    print(f"{'EXPECTATION':<12} {'PASS':>6} {'TOTAL':>6}  RATE")
    print("-" * 40)
    for k in ("local", "place", "named", "decline"):
        if k in by_group:
            p, t = by_group[k]
            print(f"{k:<12} {p:>6} {t:>6}  {100*p/t:5.1f}%")
    print("-" * 40)
    print(f"{'TOTAL':<12} {passed:>6} {total:>6}  {100*passed/total:5.1f}%")
    print(f"\n{total} queries in {elapsed:.1f}s ({1000*elapsed/total:.0f} ms/query)")

    if failures:
        print(f"\nFAILURES ({len(failures)}):")
        for expect, q, note in failures:
            print(f"  [{expect}] {q:<38} {note}")
    return 0 if not failures else 1


if __name__ == "__main__":
    sys.exit(main())
