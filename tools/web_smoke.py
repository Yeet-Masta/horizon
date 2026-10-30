#!/usr/bin/env python3
"""The web build's smoke test (web/README.md, "Checks"): web/dist in headless Chromium.

It serves web/dist as web/serve.py does (cross-origin isolated, so the threaded build has its
threads), opens it with WebGPU on SwiftShader, and checks that the module loads. With prepared
resources (--resources, a zip web/pack-resources.sh made), it then presses Play on a new world and
checks that the player joins single player's server within the time limit and that the page keeps
drawing afterwards: what a blocking call on the page's thread, a panic or a hang would stop.

    python3 tools/web_smoke.py                                   # the module loads
    python3 tools/web_smoke.py --resources .local/horizon-web-resources.zip

Needs Playwright for Python (pip install playwright; python3 -m playwright install chromium).
"""
import argparse
import os
import pathlib
import socket
import subprocess
import sys
import time

ROOT = pathlib.Path(__file__).resolve().parent.parent
CHROMIUM = [
    "--enable-unsafe-webgpu", "--use-webgpu-adapter=swiftshader", "--ignore-gpu-blocklist",
    "--enable-unsafe-swiftshader", "--disable-features=Vulkan",
    "--autoplay-policy=no-user-gesture-required",
]


def free_port():
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def wait_for_server(port, timeout=20):
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            socket.create_connection(("127.0.0.1", port), timeout=1).close()
            return
        except OSError:
            time.sleep(0.2)
    sys.exit("web_smoke: web/serve.py did not start")


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("--resources", type=pathlib.Path, help="prepared resources zip: also play")
    ap.add_argument("--join-timeout", type=float, default=240, help="seconds to join the world")
    ap.add_argument("--play", type=float, default=20, help="seconds the page must keep drawing")
    ap.add_argument("--screenshot", type=pathlib.Path, help="save the last frame here")
    args = ap.parse_args()
    if not (ROOT / "web" / "dist" / "pkg").is_dir():
        sys.exit("web_smoke: no web/dist: build it first (web/build.sh)")
    from playwright.sync_api import sync_playwright

    port = free_port()
    serve = [sys.executable, str(ROOT / "web" / "serve.py"), "--port", str(port)]
    if args.resources:
        serve += ["--resources", str(args.resources.resolve())]
    server = subprocess.Popen(serve, cwd=ROOT, stdout=subprocess.DEVNULL, stderr=subprocess.STDOUT)
    failures = []
    try:
        wait_for_server(port)
        with sync_playwright() as pw:
            # Full Chromium in a window (under xvfb-run in CI): headless, its WebGPU device
            # is lost early on SwiftShader, and the headless shell has no adapter at all.
            browser = pw.chromium.launch(
                channel="chromium", headless=not os.environ.get("DISPLAY"), args=CHROMIUM
            )
            page = browser.new_page(viewport={"width": 640, "height": 360})
            log = []
            started = time.time()

            def console(msg):
                text = f"{time.time() - started:7.1f} [{msg.type}] {msg.text[:300]}"
                log.append(text)
                if msg.type in ("error", "warning") or "Horizon:" in msg.text:
                    print(text, flush=True)

            page.on("console", console)
            page.on("pageerror", lambda e: failures.append(f"page error: {e}"))
            page.on("crash", lambda: failures.append("the page crashed"))
            page.add_init_script(
                "globalThis.__frames = 0;"
                "const tick = () => { globalThis.__frames++; requestAnimationFrame(tick); };"
                "requestAnimationFrame(tick);"
            )
            page.goto(f"http://127.0.0.1:{port}/")
            # The module loaded: its sign-in button turns on once it is instantiated.
            page.wait_for_selector("#sign-in:not([disabled])", timeout=120_000)
            print(f"web_smoke: the module loaded in {time.time() - started:.1f} s", flush=True)
            if args.resources and not failures:
                page.wait_for_selector("#play:not([disabled])", timeout=120_000)
                page.evaluate(
                    "document.getElementById('args').value = '--world Smoke --creative --seed 777'"
                )
                page.click("#play")
                deadline = time.time() + args.join_timeout
                while time.time() < deadline and not failures:
                    if any("logged in with entity id" in line for line in log):
                        break
                    page.wait_for_timeout(1000)
                else:
                    if not failures:
                        failures.append(f"the player did not join within {args.join_timeout:.0f} s")
                if not failures:
                    print(f"web_smoke: joined after {time.time() - started:.1f} s", flush=True)
                    # The page keeps drawing: a page that waits on something forever stops.
                    before = page.evaluate("globalThis.__frames")
                    page.wait_for_timeout(args.play * 1000)
                    frames = page.evaluate("globalThis.__frames") - before
                    print(f"web_smoke: {frames} frames in {args.play:.0f} s of play", flush=True)
                    if frames < args.play:
                        failures.append(f"only {frames} frames in {args.play:.0f} s: the page stalls")
            if args.screenshot:
                page.screenshot(path=str(args.screenshot))
            browser.close()
    finally:
        server.terminate()
    if failures:
        print("web_smoke: FAILED", *failures, sep="\n  ")
        return 1
    print("web_smoke: ok")
    return 0


if __name__ == "__main__":
    sys.exit(main())
