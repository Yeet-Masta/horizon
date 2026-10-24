#!/usr/bin/env python3
"""Serves web/dist for playing the web build on this machine (web/README.md).

  python3 web/serve.py [--port 8080] [--resources PATH.zip]

To this machine only (it listens on 127.0.0.1), it also serves `local-resources.zip`: the zip
web/pack-resources.sh made from your prepared resources (.local/horizon-web-resources.zip),
which the page then takes without asking. Prepared resources come from your own copy of the
game: never put them on a public server.
"""
import argparse
import functools
import http.server
import pathlib

HERE = pathlib.Path(__file__).resolve().parent
ROOT = HERE.parent


class Handler(http.server.SimpleHTTPRequestHandler):
    extensions_map = {
        **http.server.SimpleHTTPRequestHandler.extensions_map,
        ".wasm": "application/wasm",
        ".js": "text/javascript",
    }

    def __init__(self, *args, resources=None, **kwargs):
        self.resources = resources
        super().__init__(*args, **kwargs)

    def end_headers(self):
        self.send_header("Cache-Control", "no-cache")
        # Cross-origin isolation: the threaded build's shared memory needs it.
        self.send_header("Cross-Origin-Opener-Policy", "same-origin")
        self.send_header("Cross-Origin-Embedder-Policy", "require-corp")
        super().end_headers()

    def serve_resources(self, body):
        if self.resources is None or not self.resources.is_file():
            self.send_error(404, "no local resources (web/pack-resources.sh)")
            return
        size = self.resources.stat().st_size
        self.send_response(200)
        self.send_header("Content-Type", "application/zip")
        self.send_header("Content-Length", str(size))
        self.end_headers()
        if body:
            with self.resources.open("rb") as f:
                while chunk := f.read(1 << 20):
                    self.wfile.write(chunk)

    def do_GET(self):
        if self.path.split("?")[0] == "/local-resources.zip":
            self.serve_resources(True)
            return
        super().do_GET()

    def do_HEAD(self):
        if self.path.split("?")[0] == "/local-resources.zip":
            self.serve_resources(False)
            return
        super().do_HEAD()


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--port", type=int, default=8080)
    parser.add_argument("--resources", type=pathlib.Path, default=ROOT / ".local" / "horizon-web-resources.zip")
    args = parser.parse_args()
    dist = HERE / "dist"
    if not (dist / "index.html").is_file():
        raise SystemExit("web/dist is empty: run web/build.sh first")
    handler = functools.partial(Handler, directory=str(dist), resources=args.resources)
    server = http.server.ThreadingHTTPServer(("127.0.0.1", args.port), handler)
    found = "found" if args.resources.is_file() else "none yet: web/pack-resources.sh"
    print(f"Horizon web: http://localhost:{args.port}/  (local resources: {found})")
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass


if __name__ == "__main__":
    main()
