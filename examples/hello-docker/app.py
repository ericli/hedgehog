"""Has a Dockerfile, so hedgehog uses it. Keeps a counter in $DATA_DIR,
which survives redeploys."""

import os
from http.server import BaseHTTPRequestHandler, HTTPServer
from pathlib import Path

COUNTER = Path(os.environ.get("DATA_DIR", ".")) / "counter.txt"


class Handler(BaseHTTPRequestHandler):
    def do_GET(self):
        count = int(COUNTER.read_text()) + 1 if COUNTER.exists() else 1
        COUNTER.write_text(str(count))
        body = f"<h1>Hello from Python 🦔</h1><p>Requests served: {count}</p>".encode()
        self.send_response(200)
        self.send_header("content-type", "text/html; charset=utf-8")
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


port = int(os.environ.get("PORT", "8080"))
print(f"listening on {port}")
HTTPServer(("0.0.0.0", port), Handler).serve_forever()
