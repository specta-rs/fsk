#!/usr/bin/env python3
"""One-shot HTTPS fixture for deterministic curl tests (loopback only)."""
from http.server import BaseHTTPRequestHandler, HTTPServer
from pathlib import Path
import ssl
import sys

mode, portfile, payload = sys.argv[1:4]
fixture_dir = Path(__file__).resolve().parent / "fixtures"

class Handler(BaseHTTPRequestHandler):
    def do_GET(self):
        if mode == "oversized":
            self.send_response(200)
            self.send_header("Content-Length", str(256 * 1024 * 1024 + 1))
            self.send_header("Connection", "close")
            self.end_headers()
            self.wfile.flush()
            self.close_connection = True
            return
        body = Path(payload).read_bytes()
        self.send_response(200)
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Connection", "close")
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *_):
        pass

server = HTTPServer(("127.0.0.1", 0), Handler)
context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
context.load_cert_chain(fixture_dir / "cert.pem", fixture_dir / "key.pem")
server.socket = context.wrap_socket(server.socket, server_side=True)
Path(portfile).write_text(str(server.server_port))
server.serve_forever()
