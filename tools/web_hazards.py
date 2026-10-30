#!/usr/bin/env python3
"""The web build's hazard check (web/README.md, "Checks").

std's file system, clocks, environment and threads fail or panic in a browser, and the page's
thread may not block: the web build calls horizon_platform's counterparts instead. Every merge of
upstream brings new std calls, so this lints the wasm32 build with clippy's disallowed methods
(web/clippy-web/clippy.toml) and compares what it finds, by file and method, with the calls known
to run only natively (web/hazards.json). A call that isn't there fails the check, with its place.

    python3 tools/web_hazards.py            # check
    python3 tools/web_hazards.py --update   # accept the current calls as the baseline
    python3 tools/web_hazards.py --json F   # use clippy's JSON output in F instead of running it
"""
import argparse
import collections
import json
import os
import pathlib
import subprocess
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
BASELINE = ROOT / "web" / "hazards.json"
CODES = {"clippy::disallowed_methods", "clippy::disallowed_types"}


def clippy_messages():
    env = dict(os.environ, CLIPPY_CONF_DIR=str(ROOT / "web" / "clippy-web"))
    cmd = [
        "cargo", "clippy", "--config", "web/simd.toml", "-p", "horizon-client",
        "--bin", "horizon-client", "--no-default-features",
        "--target", "wasm32-unknown-unknown", "--message-format", "json",
    ]
    run = subprocess.run(cmd, cwd=ROOT, env=env, capture_output=True, text=True)
    if run.returncode != 0 and '"level":"error"' in run.stdout:
        sys.stderr.write(run.stderr[-4000:])
        sys.exit("web_hazards: the wasm32 build does not compile")
    return run.stdout.splitlines()


def hazards(lines):
    """{(file, method): [lines]} of the disallowed calls clippy reported."""
    found = collections.defaultdict(list)
    for line in lines:
        try:
            m = json.loads(line)
        except ValueError:
            continue
        if m.get("reason") != "compiler-message":
            continue
        msg = m["message"]
        if (msg.get("code") or {}).get("code") not in CODES:
            continue
        span = next(s for s in msg["spans"] if s["is_primary"])
        method = msg["message"].split("`")[1]
        found[(span["file_name"], method)].append(span["line_start"])
    return found


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("--update", action="store_true", help="write the current calls as the baseline")
    ap.add_argument("--json", help="clippy's JSON output, instead of running clippy")
    args = ap.parse_args()
    lines = open(args.json).read().splitlines() if args.json else clippy_messages()
    found = hazards(lines)
    if args.update:
        out = collections.defaultdict(dict)
        for (path, method), at in sorted(found.items()):
            out[path][method] = len(at)
        BASELINE.write_text(json.dumps(out, indent=1, sort_keys=True) + "\n")
        print(f"web_hazards: {sum(len(v) for v in found.values())} calls kept in {BASELINE.relative_to(ROOT)}")
        return 0
    known = json.loads(BASELINE.read_text()) if BASELINE.exists() else {}
    new = []
    for (path, method), at in sorted(found.items()):
        allowed = known.get(path, {}).get(method, 0)
        if len(at) > allowed:
            new.append((path, method, at, allowed))
    if not new:
        print(f"web_hazards: no new calls ({sum(len(v) for v in found.values())} known, native only)")
        return 0
    print("web_hazards: calls the web build can't make (use horizon_platform's, or cfg them off the web):")
    for path, method, at, allowed in new:
        places = ", ".join(f"{path}:{n}" for n in at)
        print(f"  {method}: {len(at)} where {allowed} are known: {places}")
    print("If they run only natively, accept them with: python3 tools/web_hazards.py --update")
    return 1


if __name__ == "__main__":
    sys.exit(main())
