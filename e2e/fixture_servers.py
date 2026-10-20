"""Tiny local servers the suite points monitors and notification channels at.

All of them bind 127.0.0.1 on a free port and run in daemon threads, so they
die with the pytest process.
"""

from __future__ import annotations

import json
import socket
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from typing import Any, Callable


def free_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


class _QuietHandler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *_args: Any) -> None:  # keep pytest output clean
        pass


class HttpTarget:
    """An HTTP endpoint whose status code and body can be flipped at runtime."""

    def __init__(self) -> None:
        self.status = 200
        self.body = "Welcome to the vole burrow. Everything is OK."
        self.hits = 0
        target = self

        class Handler(_QuietHandler):
            def do_GET(self) -> None:  # noqa: N802
                target.hits += 1
                payload = target.body.encode()
                self.send_response(target.status)
                self.send_header("Content-Type", "text/plain; charset=utf-8")
                self.send_header("Content-Length", str(len(payload)))
                self.end_headers()
                self.wfile.write(payload)

            do_HEAD = do_GET  # noqa: N815

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.port = self.server.server_address[1]
        self.url = f"http://127.0.0.1:{self.port}/health"
        threading.Thread(target=self.server.serve_forever, daemon=True).start()

    def up(self) -> None:
        self.status = 200

    def down(self) -> None:
        self.status = 500

    def close(self) -> None:
        self.server.shutdown()
        self.server.server_close()


class TcpListener:
    """Accepts TCP connections and closes them straight away."""

    def __init__(self) -> None:
        self.sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        self.sock.bind(("127.0.0.1", 0))
        self.sock.listen(16)
        self.port = self.sock.getsockname()[1]
        self.address = f"127.0.0.1:{self.port}"
        self._stop = False
        threading.Thread(target=self._serve, daemon=True).start()

    def _serve(self) -> None:
        while not self._stop:
            try:
                conn, _ = self.sock.accept()
            except OSError:
                return
            conn.close()

    def close(self) -> None:
        self._stop = True
        self.sock.close()


class FakeRedis:
    """A tiny Redis: AUTH (password `hunter2`), SELECT, PING, INFO and QUIT."""

    PASSWORD = "hunter2"

    def __init__(self) -> None:
        self.sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        self.sock.bind(("127.0.0.1", 0))
        self.sock.listen(16)
        self.port = self.sock.getsockname()[1]
        self._stop = False
        threading.Thread(target=self._serve, daemon=True).start()

    def _serve(self) -> None:
        while not self._stop:
            try:
                conn, _ = self.sock.accept()
            except OSError:
                return
            threading.Thread(target=self._handle, args=(conn,), daemon=True).start()

    @staticmethod
    def _command(reader: Any) -> list[str] | None:
        head = reader.readline()
        if not head.startswith(b"*"):
            return None
        args = []
        for _ in range(int(head[1:])):
            length = int(reader.readline()[1:])
            args.append(reader.read(length + 2)[:-2].decode())
        return args

    def _handle(self, conn: socket.socket) -> None:
        reader = conn.makefile("rb")
        authed = False
        with conn:
            while True:
                args = self._command(reader)
                if not args:
                    return
                cmd = args[0].upper()
                if cmd == "AUTH":
                    authed = args[-1] == self.PASSWORD
                    reply = b"+OK\r\n" if authed else b"-WRONGPASS invalid username-password pair\r\n"
                elif not authed:
                    reply = b"-NOAUTH Authentication required.\r\n"
                elif cmd in ("SELECT", "PING"):
                    reply = b"+OK\r\n" if cmd == "SELECT" else b"+PONG\r\n"
                elif cmd == "INFO":
                    body = b"# Server\r\nredis_version:7.2.4\r\n"
                    reply = b"$%d\r\n%s\r\n" % (len(body), body)
                elif cmd == "QUIT":
                    conn.sendall(b"+OK\r\n")
                    return
                else:
                    reply = b"-ERR unknown command\r\n"
                conn.sendall(reply)

    def close(self) -> None:
        self._stop = True
        self.sock.close()


class WebhookCapture:
    """Records every request body it receives (JSON decoded when possible)."""

    def __init__(self) -> None:
        self.requests: list[dict[str, Any]] = []
        self._cond = threading.Condition()
        capture = self

        class Handler(_QuietHandler):
            def _record(self) -> None:
                length = int(self.headers.get("Content-Length") or 0)
                raw = self.rfile.read(length) if length else b""
                try:
                    body: Any = json.loads(raw.decode() or "null")
                except ValueError:
                    body = raw.decode(errors="replace")
                with capture._cond:
                    capture.requests.append(
                        {"method": self.command, "path": self.path, "headers": {k.lower(): v for k, v in self.headers.items()}, "json": body}
                    )
                    capture._cond.notify_all()
                self.send_response(204)
                self.send_header("Content-Length", "0")
                self.end_headers()

            do_POST = _record  # noqa: N815
            do_PUT = _record  # noqa: N815
            do_PATCH = _record  # noqa: N815

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.port = self.server.server_address[1]
        threading.Thread(target=self.server.serve_forever, daemon=True).start()

    def url(self, path: str = "/hook") -> str:
        return f"http://127.0.0.1:{self.port}{path}"

    def wait_for(self, predicate: Callable[[dict[str, Any]], bool], timeout: float = 30.0) -> dict[str, Any]:
        """Blocks until a recorded request matches, returning it."""
        deadline = time.monotonic() + timeout
        with self._cond:
            while True:
                for req in self.requests:
                    if predicate(req):
                        return req
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    seen = [r["json"] for r in self.requests]
                    raise AssertionError(f"no matching webhook within {timeout}s; received: {seen!r}")
                self._cond.wait(remaining)

    def close(self) -> None:
        self.server.shutdown()
        self.server.server_close()
