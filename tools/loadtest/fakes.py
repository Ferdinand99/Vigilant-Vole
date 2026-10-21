"""Fake targets for the load test, run in their own process.

Nothing here talks to the internet:

* an HTTP target with per-request latency, keyword/JSON bodies, failing and
  flapping endpoints, plus a webhook sink that counts notification deliveries,
* a TCP listener (accepts and closes),
* a tiny DNS responder (A records only) for `dns` monitors.

The parent process reads counters and the sink log from ``GET /_stats``.
Flapping is a pure function of wall-clock time (:func:`flap_down`), so the
parent can work out when a target changed state without asking.
"""

from __future__ import annotations

import asyncio
import json
import random
import struct
import time
from collections import Counter
from typing import Any

from aiohttp import web


def flap_down(n: int, now: float, period: float, down: float) -> bool:
    """Whether flapping target ``n`` answers 503 at ``now`` (epoch seconds)."""
    return (now + n * 41) % period < down


def flap_last_change(n: int, now: float, period: float, down: float) -> tuple[float, bool]:
    """``(time, is_down)`` of the most recent state change of target ``n``."""
    x = (now + n * 41) % period
    if x < down:
        return now - x, True
    return now - (x - down), False


class Fakes:
    def __init__(self, cfg: dict[str, Any]) -> None:
        self.cfg = cfg
        self.hits: Counter[str] = Counter()
        self.tcp_conns = 0
        self.dns_queries = 0
        self.deliveries: list[dict[str, Any]] = []
        self.rng = random.Random(cfg.get("seed", 1))

    # -- HTTP target ---------------------------------------------------------

    async def _delay(self, request: web.Request, default_ms: int) -> None:
        try:
            ms = int(request.query.get("ms", default_ms))
        except ValueError:
            ms = default_ms
        if ms > 0:
            await asyncio.sleep(ms * self.rng.uniform(0.8, 1.2) / 1000)

    async def ok(self, request: web.Request) -> web.Response:
        self.hits["ok"] += 1
        await self._delay(request, 30)
        n = request.match_info["n"]
        return web.Response(text=f"<html><body>vole-ok service {n}</body></html>", content_type="text/html")

    async def json_doc(self, request: web.Request) -> web.Response:
        self.hits["json"] += 1
        await self._delay(request, 30)
        n = int(request.match_info["n"])
        return web.json_response({"status": "ok", "n": n, "queue": n % 7, "version": "1.2.3"})

    async def slow(self, request: web.Request) -> web.Response:
        self.hits["slow"] += 1
        await self._delay(request, 2000)
        return web.Response(text="vole-ok slow")

    async def fail(self, request: web.Request) -> web.Response:
        self.hits["fail"] += 1
        await self._delay(request, 20)
        return web.Response(status=500, text="internal error")

    async def flap(self, request: web.Request) -> web.Response:
        self.hits["flap"] += 1
        n = int(request.match_info["n"])
        if flap_down(n, time.time(), self.cfg["flap_period"], self.cfg["flap_down"]):
            return web.Response(status=503, text="flapping: down")
        return web.Response(text="vole-ok flapping: up")

    # -- webhook sink ----------------------------------------------------------

    async def hook(self, request: web.Request) -> web.Response:
        arrived = time.time()
        raw = await request.read()
        try:
            body = json.loads(raw)
        except ValueError:
            body = {}
        self.deliveries.append(
            {
                "t": arrived,
                "channel": request.match_info["ch"],
                "monitor_id": body.get("monitor_id"),
                "event": body.get("event"),
                "occurred_at": body.get("occurred_at"),
            }
        )
        return web.Response(text="ok")

    async def stats(self, request: web.Request) -> web.Response:
        since = int(request.query.get("since", "0"))
        return web.json_response(
            {
                "hits": dict(self.hits),
                "tcp_conns": self.tcp_conns,
                "dns_queries": self.dns_queries,
                "deliveries_total": len(self.deliveries),
                "deliveries": self.deliveries[since:],
            }
        )

    def app(self) -> web.Application:
        app = web.Application()
        app.router.add_get("/ok/{n}", self.ok)
        app.router.add_get("/json/{n}", self.json_doc)
        app.router.add_get("/slow/{n}", self.slow)
        app.router.add_get("/fail/{n}", self.fail)
        app.router.add_get("/flap/{n}", self.flap)
        app.router.add_post("/hook/{ch}", self.hook)
        app.router.add_get("/_stats", self.stats)
        return app

    # -- TCP -------------------------------------------------------------------

    async def tcp_conn(self, reader: asyncio.StreamReader, writer: asyncio.StreamWriter) -> None:
        self.tcp_conns += 1
        writer.close()


class DnsResponder(asyncio.DatagramProtocol):
    """Answers every A query with 10.20.30.40; anything else gets no answers."""

    def __init__(self, fakes: Fakes) -> None:
        self.fakes = fakes
        self.transport: asyncio.DatagramTransport | None = None

    def connection_made(self, transport: asyncio.BaseTransport) -> None:
        self.transport = transport  # type: ignore[assignment]

    def datagram_received(self, data: bytes, addr: tuple[str, int]) -> None:
        self.fakes.dns_queries += 1
        reply = dns_reply(data)
        if reply is not None and self.transport is not None:
            self.transport.sendto(reply, addr)


def dns_reply(query: bytes) -> bytes | None:
    if len(query) < 12:
        return None
    qid, _flags, qdcount = struct.unpack("!HHH", query[:6])
    if qdcount != 1:
        return None
    i = 12
    while i < len(query) and query[i] != 0:
        i += 1 + query[i]
    end = i + 5  # zero label + QTYPE + QCLASS
    if end > len(query):
        return None
    qtype = struct.unpack("!H", query[i + 1 : i + 3])[0]
    question = query[12:end]
    answers = b""
    if qtype == 1:  # A
        answers = struct.pack("!HHHIH", 0xC00C, 1, 1, 60, 4) + bytes([10, 20, 30, 40])
    header = struct.pack("!HHHHHH", qid, 0x8180, 1, 1 if answers else 0, 0, 0)
    return header + question + answers


async def serve(cfg: dict[str, Any], ready: Any) -> None:
    fakes = Fakes(cfg)
    runner = web.AppRunner(fakes.app(), access_log=None)
    await runner.setup()
    await web.TCPSite(runner, cfg["bind"], cfg["http_port"], backlog=1024).start()
    tcp = await asyncio.start_server(fakes.tcp_conn, cfg["bind"], cfg["tcp_port"], backlog=1024)
    loop = asyncio.get_running_loop()
    await loop.create_datagram_endpoint(lambda: DnsResponder(fakes), local_addr=(cfg["bind"], cfg["dns_port"]))
    ready.set()
    async with tcp:
        await asyncio.Event().wait()


def run(cfg: dict[str, Any], ready: Any) -> None:
    """Process entry point (``multiprocessing.Process(target=run)``)."""
    try:
        asyncio.run(serve(cfg, ready))
    except KeyboardInterrupt:
        pass
