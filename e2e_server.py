"""Local fixture server for end-to-end testing of pixpull.

Serves real image bytes and simulates the failure modes a scraper hits:
flaky connections (for resume), 5xx (for retry), and garbage bodies (for
integrity validation).
"""
import base64
import http.server
import socketserver
import threading

PORT = 8765

# 1x1 valid PNG
PNG = base64.b64decode(
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg=="
)
# 1x1 valid JPEG
JPEG = base64.b64decode(
    "/9j/4AAQSkZJRgABAQEAYABgAAD/2wBDAAgGBgcGBQgHBwcJCQgKDBQNDAsLDBkSEw8UHRofHh0aHBwgJC4nICIsIxwcKDcpLDAxNDQ0Hyc5PTgyPC4zNDL/wAALCAABAAEBAREA/8QAFAABAAAAAAAAAAAAAAAAAAAACf/EABQQAQAAAAAAAAAAAAAAAAAAAAD/2gAIAQEAAD8AVN//2Q=="
)

# A larger PNG made of repeated valid PNG chunks (still sniffed as PNG).
BIG = PNG * 200

state = {
    "flaky_hits": 0,
    "flaky_lock": threading.Lock(),
    "connections": 0,
    "conn_lock": threading.Lock(),
}


class CountingServer(socketserver.ThreadingMixIn, http.server.HTTPServer):
    daemon_threads = True

    def get_request(self):
        sock, addr = super().get_request()
        with state["conn_lock"]:
            state["connections"] += 1
        return sock, addr


class Handler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def _send(self, status, body, extra=None, close=False):
        self.send_response(status)
        self.send_header("Content-Type", "application/octet-stream")
        self.send_header("Accept-Ranges", "bytes")
        self.send_header("Content-Length", str(len(body)))
        for k, v in (extra or {}).items():
            self.send_header(k, v)
        self.end_headers()
        try:
            self.wfile.write(body)
            self.wfile.flush()
        except (BrokenPipeError, ConnectionResetError):
            pass
        finally:
            if close:
                self.close_connection = True

    def _partial(self, body):
        """Honour a Range header: return 206 with the requested slice."""
        rng = self.headers.get("Range")
        if rng and rng.startswith("bytes="):
            spec = rng[6:]
            if "-" in spec:
                start_s, _, end_s = spec.partition("-")
                start = int(start_s) if start_s else 0
                end = int(end_s) if end_s else len(body) - 1
                end = min(end, len(body) - 1)
                if start > end or start >= len(body):
                    self._send(416, b"", close=True)
                    return None
                chunk = body[start : end + 1]
                self.send_response(206)
                self.send_header("Content-Type", "application/octet-stream")
                self.send_header("Accept-Ranges", "bytes")
                self.send_header("Content-Range", f"bytes {start}-{end}/{len(body)}")
                self.send_header("Content-Length", str(len(chunk)))
                self.end_headers()
                try:
                    self.wfile.write(chunk)
                    self.wfile.flush()
                except (BrokenPipeError, ConnectionResetError):
                    pass
                finally:
                    self.close_connection = True
                return chunk
        self._send(200, body)
        return body

    def do_GET(self):
        path = self.path.split("?", 1)[0]

        if path == "/stats":
            body = str(state["connections"]).encode()
            self._send(200, body)
        elif path == "/ok/photo.png":
            self._send(200, PNG)
        elif path == "/ok/photo.jpg":
            self._send(200, JPEG)
        elif path == "/ok/weird":
            # No extension in URL; should be named .png after detection.
            self._send(200, PNG)
        elif path == "/ok/wrongext.gif":
            # URL says .gif but body is a PNG; extension should be corrected.
            self._send(200, PNG)
        elif path == "/big.png":
            self._partial(BIG)
        elif path == "/flaky.png":
            # First request: send half the body then drop the connection.
            # Subsequent requests: serve the remainder via Range (resume).
            with state["flaky_lock"]:
                state["flaky_hits"] += 1
                hit = state["flaky_hits"]
            if hit == 1:
                half = len(BIG) // 2
                self.send_response(200)
                self.send_header("Content-Type", "application/octet-stream")
                self.send_header("Accept-Ranges", "bytes")
                self.send_header("Content-Length", str(len(BIG)))
                self.end_headers()
                try:
                    self.wfile.write(BIG[:half])
                    self.wfile.flush()
                except (BrokenPipeError, ConnectionResetError):
                    pass
                finally:
                    self.close_connection = True
            else:
                self._partial(BIG)
        elif path == "/flaky500.jpg":
            # First request returns 500, subsequent requests succeed.
            with state["flaky_lock"]:
                state["flaky_hits"] += 1
                hit = state["flaky_hits"]
            if hit == 1:
                self._send(500, b"boom")
            else:
                self._send(200, JPEG)
        elif path == "/retryafter.jpg":
            # First request: 429 with Retry-After, then success.
            with state["flaky_lock"]:
                state["flaky_hits"] += 1
                hit = state["flaky_hits"]
            if hit == 1:
                self._send(429, b"slow down", extra={"Retry-After": "1"})
            else:
                self._send(200, JPEG)
        elif path == "/garbage.jpg":
            # Always returns non-image bytes; should fail validation.
            self._send(200, b"<html>not an image</html>" * 10)
        elif path == "/empty.jpg":
            self._send(200, b"")
        elif path == "/missing.png":
            self._send(404, b"nope")
        else:
            self._send(404, b"not found")


class Server(socketserver.ThreadingMixIn, http.server.HTTPServer):
    daemon_threads = True


if __name__ == "__main__":
    srv = CountingServer(("127.0.0.1", PORT), Handler)
    print(f"fixture server on http://127.0.0.1:{PORT}", flush=True)
    try:
        srv.serve_forever()
    except KeyboardInterrupt:
        print(f"\nconnections seen: {state['connections']}", flush=True)
