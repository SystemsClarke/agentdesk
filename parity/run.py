#!/usr/bin/env python3
"""Runs the parity suite against the agentdesk binary and prints what is and is not at parity with the original.

  python3 parity/run.py                 report; exit 1 only if a check that used to pass (baseline.json) now fails
  python3 parity/run.py --strict        exit 1 unless every feature passes (the 100% bar)
  python3 parity/run.py -k hook         only features whose id contains "hook"
  python3 parity/run.py --update-baseline   record today's passes as the floor that must not regress
  python3 parity/run.py --json out.json     machine-readable results
"""
import argparse
import importlib
import json
import os
import pkgutil
import sys
import time
import traceback

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import lib  # noqa: E402


def load_checks():
    import checks

    for m in pkgutil.iter_modules(checks.__path__):
        importlib.import_module("checks." + m.name)


def run_one(f):
    b = lib.Board()
    t0 = time.time()
    try:
        f["fn"](b)
        return "pass", ""
    except lib.Missing as e:
        return "missing", str(e)
    except lib.Waived as e:
        return "waived", str(e)
    except AssertionError as e:
        return "fail", "%s\n%s" % (e, "".join(traceback.format_exc(limit=-2).splitlines(True)[-4:]))
    except Exception as e:  # a crashing check is a failing feature, with the traceback to fix it
        return "fail", "%s: %s\n%s" % (type(e).__name__, e, traceback.format_exc(limit=4))
    finally:
        b.close()
        f["seconds"] = round(time.time() - t0, 2)


def main():
    for stream in (sys.stdout, sys.stderr):
        if hasattr(stream, "reconfigure"):
            stream.reconfigure(encoding="utf-8", errors="replace")  # the window draws block characters; a cp1252 console cannot
    ap = argparse.ArgumentParser()
    ap.add_argument("-k", help="only features whose id contains this")
    ap.add_argument("--strict", action="store_true")
    ap.add_argument("--no-ratchet", action="store_true", help="report only: do not fail on regressions from baseline.json")
    ap.add_argument("--update-baseline", action="store_true")
    ap.add_argument("--json")
    ap.add_argument("-j", "--jobs", type=int, default=8, help="checks to run at once (each has its own board)")
    ap.add_argument("-v", "--verbose", action="store_true", help="show the reason for every non-pass")
    args = ap.parse_args()
    load_checks()
    feats = [f for f in lib.FEATURES.values() if not args.k or args.k in f["id"]]
    results = {}
    from concurrent.futures import ThreadPoolExecutor

    with ThreadPoolExecutor(max_workers=max(1, args.jobs)) as pool:
        outcomes = list(pool.map(run_one, feats))
    for f, (status, why) in zip(feats, outcomes):
        results[f["id"]] = {"status": status, "why": why, "area": f["area"], "title": f["title"], "source": f["source"], "seconds": f["seconds"]}
        mark = {"pass": "ok  ", "fail": "FAIL", "missing": "miss", "waived": "waiv"}[status]
        print("%s %-44s %s" % (mark, f["id"], ("- " + why.splitlines()[0][:90]) if (why and (status != "pass") and (args.verbose or status == "fail")) else ""), flush=True)

    counted = [r for r in results.values() if r["status"] != "waived"]
    passed = sum(1 for r in counted if r["status"] == "pass")
    print("\n%-14s %5s %5s %5s %5s" % ("area", "pass", "fail", "miss", "total"))
    for area in sorted({r["area"] for r in results.values()}):
        rs = [r for r in results.values() if r["area"] == area and r["status"] != "waived"]
        c = lambda s: sum(1 for r in rs if r["status"] == s)  # noqa: E731
        print("%-14s %5d %5d %5d %5d" % (area, c("pass"), c("fail"), c("missing"), len(rs)))
    waived = sum(1 for r in results.values() if r["status"] == "waived")
    pct = 100.0 * passed / len(counted) if counted else 100.0
    print("\nPARITY: %d of %d features (%.1f%%)%s" % (passed, len(counted), pct, ("; %d waived by design" % waived) if waived else ""))

    if args.json:
        json.dump({"passed": passed, "total": len(counted), "percent": round(pct, 1), "results": results}, open(args.json, "w"), indent=1)
    baseline_path = os.path.join(HERE, "baseline.json")
    if args.update_baseline and not args.k:
        json.dump(sorted(i for i, r in results.items() if r["status"] == "pass"), open(baseline_path, "w"), indent=1)
        print("baseline updated: %d features" % passed)
        return 0
    rc = 0
    if args.strict and passed != len(counted):
        rc = 1
    if os.path.exists(baseline_path) and not args.k and not args.no_ratchet:
        regressed = [i for i in json.load(open(baseline_path)) if i in results and results[i]["status"] != "pass"]
        if regressed:
            print("\nREGRESSED (passed before, not now): " + ", ".join(regressed))
            rc = 1
    return rc


if __name__ == "__main__":
    sys.exit(main())
