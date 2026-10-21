#!/usr/bin/env python3
"""Load and soak test harness for Vigilant Vole.

    python tools/loadtest/loadtest.py --mode smoke --image vv-load
    python tools/loadtest/loadtest.py --mode load  --image vv-load
    python tools/loadtest/loadtest.py --mode soak  --image vv-load --hours 2

Starts the server (Docker image, local binary or an existing URL), runs fake
targets locally, seeds monitors/tags/channels/status pages/maintenance through
the API, applies dashboard, status-page, API, push and agent load, samples
resource use and latencies every few seconds, and writes ``samples.csv``,
``summary.md`` and ``summary.json``. Exits non-zero when a threshold fails.
See docs/performance.md.
"""

from __future__ import annotations

import argparse
import asyncio
import csv
import datetime as dt
import json
import multiprocessing
import os
import platform
import random
import re
import sys
import time
import traceback
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Awaitable, Callable

import aiohttp

sys.path.insert(0, str(Path(__file__).resolve().parent))

import fakes  # noqa: E402
import report  # noqa: E402
from report import histogram_quantile  # noqa: E402
from instance import DockerInstance, ExternalInstance, Instance, LocalInstance, free_port  # noqa: E402

ADMIN_USER = "loadtest"
ADMIN_PASSWORD = "Load-Test-Vole-Passphrase-7"
BULK_TAG = "lt-bulk"
AGENT_VERSION = "0.4.0"

MODES: dict[str, dict[str, Any]] = {
    "smoke": {"monitors": 200, "duration": 300, "retention_days": None, "backfill_days": 1},
    "load": {"monitors": 1000, "duration": 1800, "retention_days": None, "backfill_days": 2},
    "soak": {"monitors": 1000, "duration": 7200, "retention_days": 1, "backfill_days": 3},
}

# (kind, share of all monitors). Groups are monitors too.
MIX = [
    ("http", 0.38),
    ("keyword", 0.12),
    ("json", 0.10),
    ("tcp", 0.10),
    ("dns", 0.05),
    ("push", 0.05),
    ("agent", 0.06),
    ("bulk", 0.05),
    ("slow", 0.03),
    ("fail", 0.02),
    ("flap", 0.02),
    ("group", 0.02),
]
INTERVALS = [(20, 0.05), (30, 0.20), (60, 0.50), (120, 0.15), (300, 0.10)]
TAGS = ["env", "team-a", "team-b", "team-c", "region-eu", "region-us", "critical", BULK_TAG]


# ---------------------------------------------------------------------------
# Measurements
# ---------------------------------------------------------------------------


@dataclass
class ClassStats:
    window: list[float] = field(default_factory=list)
    total: list[float] = field(default_factory=list)
    errors_window: int = 0
    errors_total: int = 0
    error_samples: list[str] = field(default_factory=list)


class Recorder:
    def __init__(self) -> None:
        self.classes: dict[str, ClassStats] = {}

    def get(self, name: str) -> ClassStats:
        return self.classes.setdefault(name, ClassStats())

    def ok(self, name: str, ms: float) -> None:
        c = self.get(name)
        c.window.append(ms)
        c.total.append(ms)

    def error(self, name: str, what: str, ms: float | None = None) -> None:
        c = self.get(name)
        c.errors_window += 1
        c.errors_total += 1
        if ms is not None:
            c.window.append(ms)
            c.total.append(ms)
        if len(c.error_samples) < 10:
            c.error_samples.append(what[:300])

    def roll(self) -> dict[str, tuple[list[float], int]]:
        out = {}
        for name, c in self.classes.items():
            out[name] = (c.window, c.errors_window)
            c.window, c.errors_window = [], 0
        return out


def pct(values: list[float], p: float) -> float | None:
    if not values:
        return None
    v = sorted(values)
    return v[min(len(v) - 1, int(p * len(v)))]


def parse_rfc3339(s: str | None) -> float | None:
    if not s:
        return None
    m = re.match(r"(\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d)(?:\.(\d+))?(Z|[+-]\d\d:\d\d)$", s)
    if not m:
        return None
    base = dt.datetime.fromisoformat(m.group(1) + (m.group(3) if m.group(3) != "Z" else "+00:00"))
    frac = float("0." + m.group(2)) if m.group(2) else 0.0
    return base.timestamp() + frac


def parse_prom(text: str) -> dict[str, float]:
    """Unlabelled and selected labelled samples as ``name{labels}`` keys."""
    out: dict[str, float] = {}
    for line in text.splitlines():
        if not line or line.startswith("#"):
            continue
        name, _, value = line.rpartition(" ")
        if name.startswith(("vigilant_check_delay", "vigilant_checks", "vigilant_running", "vigilant_notifications")):
            try:
                out[name] = out.get(name, 0.0) + float(value)
            except ValueError:
                pass
    return out


def delay_buckets(prom: dict[str, float]) -> list[tuple[float, float]]:
    out = []
    for key, value in prom.items():
        m = re.match(r'vigilant_check_delay_seconds_bucket\{le="([^"]+)"\}', key)
        if m:
            le = float("inf") if m.group(1) == "+Inf" else float(m.group(1))
            out.append((le, value))
    return sorted(out)


# ---------------------------------------------------------------------------
# The run
# ---------------------------------------------------------------------------


class Harness:
    def __init__(self, args: argparse.Namespace, inst: Instance, out_dir: Path) -> None:
        self.args = args
        self.inst = inst
        self.out_dir = out_dir
        self.base = inst.base_url
        self.rng = random.Random(args.seed)
        self.rec = Recorder()
        self.running = True
        self.token = ""
        self.agent_token = ""
        self.monitors: list[dict[str, Any]] = []
        self.pages: list[str] = []
        self.flappers: dict[int, int] = {}  # monitor id -> flap index
        self.sse = {"connected": 0, "events": 0, "bytes": 0, "reconnects": 0, "refused": 0}
        self.sse_window = {"events": 0, "bytes": 0}
        self.inflight = 0
        self.dropped = 0
        self.deliveries_seen = 0
        self.notif_window: list[float] = []
        self.detect_window: list[float] = []
        self.notif_total: list[float] = []
        self.detect_total: list[float] = []
        self.deliveries_by_event: dict[str, int] = {}
        self.rows: list[dict[str, Any]] = []
        self.delay_acc: dict[float, float] = {}  # run-wide check delay histogram
        self.events: list[str] = []
        self.started_at = 0.0
        self.server_started = 0  # epoch seconds of the last (re)start
        self.fake_base = f"http://127.0.0.1:{args.fake_http_port}"

    def log(self, msg: str) -> None:
        stamp = time.strftime("%H:%M:%S")
        print(f"[{stamp}] {msg}", flush=True)
        self.events.append(f"{stamp} {msg}")

    # -- sessions --------------------------------------------------------------

    def session(self, **kw: Any) -> aiohttp.ClientSession:
        kw.setdefault("timeout", aiohttp.ClientTimeout(total=60))
        return aiohttp.ClientSession(base_url=self.base, connector=aiohttp.TCPConnector(limit=0), **kw)

    async def setup_accounts(self) -> None:
        jar = aiohttp.CookieJar(unsafe=True)
        self.ui = self.session(cookie_jar=jar, headers={"Origin": self.base})
        async with self.ui.post(
            "/setup",
            data={
                "username": ADMIN_USER,
                "password": ADMIN_PASSWORD,
                "confirm": ADMIN_PASSWORD,
                "site_name": "Load test",
                "timezone": "UTC",
            },
            allow_redirects=False,
        ) as r:
            if r.status not in (302, 303):
                raise RuntimeError(f"setup failed: {r.status} {(await r.text())[:300]}")
        await self.login()
        async with self.ui.post(
            "/settings/tokens", data={"name": "loadtest", "scope": "write", "expires": "never"}
        ) as r:
            m = re.search(r"vv_[A-Za-z0-9_\-]{16,}", await r.text())
            if not m:
                raise RuntimeError(f"no API token in response ({r.status})")
            self.token = m.group(0)
        async with self.ui.post("/settings/agents", data={"name": "lt-agent", "location": "lab"}) as r:
            m = re.search(r"vva_[A-Za-z0-9_\-]{16,}", await r.text())
            if not m:
                raise RuntimeError(f"no agent token in response ({r.status})")
            self.agent_token = m.group(0)
        self.api = self.session(headers={"Authorization": f"Bearer {self.token}"})
        self.public = self.session()
        self.sse_http = self.session(cookie_jar=jar, timeout=aiohttp.ClientTimeout(total=None, sock_read=90))
        settings: dict[str, Any] = {"metrics_enabled": True}
        if self.args.retention_days:
            settings["retention_days"] = self.args.retention_days
        await self.api_json("PATCH", "/api/v1/settings", settings)

    async def login(self) -> None:
        async with self.ui.post(
            "/login", data={"username": ADMIN_USER, "password": ADMIN_PASSWORD, "next": "/"}, allow_redirects=False
        ) as r:
            if r.status not in (302, 303):
                raise RuntimeError(f"login failed: {r.status}")

    async def api_json(self, method: str, path: str, body: Any = None) -> Any:
        async with self.api.request(method, path, json=body) as r:
            text = await r.text()
            if r.status >= 300:
                raise RuntimeError(f"{method} {path} -> {r.status}: {text[:500]}")
            return json.loads(text) if text else None

    # -- seeding ---------------------------------------------------------------

    def plan(self) -> list[dict[str, Any]]:
        n = self.args.monitors
        kinds: list[str] = []
        for kind, share in MIX:
            kinds += [kind] * max(1, round(n * share))
        kinds = kinds[:n]
        while len(kinds) < n:
            kinds.append("http")
        self.rng.shuffle(kinds)
        ivs, weights = zip(*INTERVALS)
        out = []
        for i, kind in enumerate(kinds):
            interval = self.rng.choices(ivs, weights)[0]
            if kind in ("flap",):
                interval = 30
            elif kind in ("push", "group", "bulk"):
                interval = 60
            out.append({"i": i, "kind": kind, "interval": interval})
        return out

    def monitor_body(self, spec: dict[str, Any], channels: list[int]) -> dict[str, Any]:
        i, kind, interval = spec["i"], spec["kind"], spec["interval"]
        host = f"{self.inst.target_host}:{self.args.fake_http_port}"
        latency = self.rng.choice([5, 20, 40, 80, 150, 300])
        body: dict[str, Any] = {
            "name": f"lt-{kind}-{i:04d}",
            "monitor_type": "http",
            "interval_seconds": interval,
            "timeout_seconds": min(10, interval),
            "retries": 1,
        }
        if kind in ("http", "agent", "bulk"):
            body["target"] = f"http://{host}/ok/{i}?ms={latency}"
        elif kind == "keyword":
            body.update(monitor_type="keyword", target=f"http://{host}/ok/{i}?ms={latency}", keyword="vole-ok")
        elif kind == "json":
            body.update(
                monitor_type="json_query",
                target=f"http://{host}/json/{i}?ms={latency}",
                json_path="$.status",
                json_operator="==",
                json_expected="ok",
            )
        elif kind == "tcp":
            body.update(monitor_type="tcp", target=f"{self.inst.target_host}:{self.args.fake_tcp_port}")
        elif kind == "dns":
            body.update(
                monitor_type="dns",
                target=f"svc{i}.loadtest.internal",
                dns_resolver=f"{self.inst.dns_ip}:{self.args.fake_dns_port}",
                dns_record_type="A",
            )
        elif kind == "push":
            body = {"name": body["name"], "monitor_type": "push", "interval_seconds": interval}
        elif kind == "slow":
            body["target"] = f"http://{host}/slow/{i}?ms={self.rng.choice([1500, 2500, 4000])}"
        elif kind == "fail":
            body["target"] = f"http://{host}/fail/{i}"
        elif kind == "flap":
            body.update(target=f"http://{host}/flap/{i}", retries=0, flap_detection=False)
        elif kind == "group":
            body = {"name": body["name"], "monitor_type": "group"}
        if kind == "agent":
            body["run_on_agents"] = True
        tags: list[Any] = []
        if kind == "bulk":
            tags.append(BULK_TAG)
        for t in self.rng.sample(TAGS[:-1], self.rng.randint(1, 3)):
            tags.append({"name": t, "value": self.rng.choice(["prod", "staging"])} if t == "env" else t)
        body["tags"] = tags
        if kind == "flap":
            body["notifications"] = channels
        elif kind == "fail":
            body["notifications"] = channels[:1]
        elif kind != "group" and self.rng.random() < 0.5:
            body["notifications"] = [self.rng.choice(channels)]
        return body

    async def seed(self) -> None:
        started = time.monotonic()
        host = f"{self.inst.target_host}:{self.args.fake_http_port}"
        channels = []
        for c in range(1, 6):
            ch = await self.api_json(
                "POST",
                "/api/v1/notification-channels",
                {"name": f"lt-hook-{c}", "kind": "webhook", "config": {"url": f"http://{host}/hook/{c}"}},
            )
            channels.append(ch["id"])
        for t in TAGS:
            await self.api_json("POST", "/api/v1/tags", {"name": t})

        plan = self.plan()
        groups = [s for s in plan if s["kind"] == "group"]
        others = [s for s in plan if s["kind"] != "group"]
        sem = asyncio.Semaphore(8)
        create_ms: list[float] = []

        async def create(spec: dict[str, Any], parent: int | None) -> None:
            body = self.monitor_body(spec, channels)
            if parent is not None:
                body["parent_id"] = parent
            async with sem:
                t0 = time.perf_counter()
                m = await self.api_json("POST", "/api/v1/monitors", body)
                create_ms.append((time.perf_counter() - t0) * 1000)
            self.monitors.append({**spec, "id": m["id"], "push_token": m.get("push_token")})
            if spec["kind"] == "flap":
                self.flappers[m["id"]] = spec["i"]

        await asyncio.gather(*(create(s, None) for s in groups))
        group_ids = [m["id"] for m in self.monitors if m["kind"] == "group"]
        children = [s for s in others if s["kind"] in ("http", "keyword", "tcp")]
        parent_of = {s["i"]: group_ids[k % len(group_ids)] for k, s in enumerate(children[: len(group_ids) * 8])}
        await asyncio.gather(*(create(s, parent_of.get(s["i"])) for s in others))
        self.monitors.sort(key=lambda m: m["id"])

        ids = [m["id"] for m in self.monitors if m["kind"] != "group"]
        pages = [("lt-public", 5, 10), ("lt-big", 10, 30), ("lt-internal", 4, 25)]
        for slug, nsec, per in pages:
            pick = self.rng.sample(ids, min(len(ids), nsec * per))
            sections = [
                {"name": f"Section {k + 1}", "monitor_ids": pick[k * per : (k + 1) * per]} for k in range(nsec)
            ]
            await self.api_json(
                "POST",
                "/api/v1/status-pages",
                {"title": slug.replace("-", " ").title(), "slug": slug, "published": True, "sections": sections},
            )
            self.pages.append(slug)
        await self.api_json(
            "POST",
            "/api/v1/maintenance",
            {
                "title": "lt-rolling",
                "strategy": "cron",
                "cron": "*/10 * * * *",
                "duration_minutes": 2,
                "timezone": "UTC",
                "monitor_ids": self.rng.sample(ids, min(20, len(ids))),
            },
        )
        self.log(
            f"seeded {len(self.monitors)} monitors, {len(channels)} channels, {len(self.pages)} status pages "
            f"in {time.monotonic() - started:.1f}s (create p95 {pct(create_ms, 0.95):.0f} ms)"
        )
        self.seed_create_p95 = pct(create_ms, 0.95)

    # -- load generators ---------------------------------------------------------

    async def timed(self, cls: str, coro_fn: Callable[[], Awaitable[aiohttp.ClientResponse]]) -> None:
        if self.inflight >= self.args.max_inflight:
            self.dropped += 1
            return
        self.inflight += 1
        t0 = time.perf_counter()
        try:
            async with await coro_fn() as r:
                await r.read()
                ms = (time.perf_counter() - t0) * 1000
                if r.status >= 400:
                    self.rec.error(cls, f"{r.status} {r.url}", ms)
                else:
                    self.rec.ok(cls, ms)
        except Exception as exc:  # noqa: BLE001 - every failure counts as an error
            self.rec.error(cls, f"{type(exc).__name__}: {exc}")
        finally:
            self.inflight -= 1

    async def open_loop(self, rps: float, pick: Callable[[], tuple[str, Callable[[], Any]]]) -> None:
        """Fires at a fixed rate whatever the response times (no coordinated omission)."""
        if rps <= 0:
            return
        gap = 1.0 / rps
        nxt = time.monotonic() + self.rng.random() * gap
        tasks: set[asyncio.Task] = set()
        while self.running:
            await asyncio.sleep(max(0.0, nxt - time.monotonic()))
            nxt += gap
            cls, fn = pick()
            task = asyncio.create_task(self.timed(cls, fn))
            tasks.add(task)
            task.add_done_callback(tasks.discard)

    def pick_status(self) -> tuple[str, Callable[[], Any]]:
        slug = self.rng.choices(self.pages, [5, 3, 2])[0]
        r = self.rng.random()
        if r < 0.5:
            return "status_page", lambda: self.public.get(f"/status/{slug}")
        if r < 0.9:
            return "status_fragment", lambda: self.public.get(f"/status/{slug}/fragment")
        return "status_json", lambda: self.public.get(f"/api/status-page/{slug}")

    def pick_api(self) -> tuple[str, Callable[[], Any]]:
        m = self.rng.choice(self.monitors)["id"]
        r = self.rng.random()
        # Mostly per-monitor reads (scripts, Grafana panels); one in ten lists.
        if r < 0.07:
            return "api_list", lambda: self.api.get("/api/v1/monitors?limit=100")
        if r < 0.10:
            return "api_list", lambda: self.api.get("/api/v1/monitors?status=down")
        if r < 0.45:
            return "api_monitor", lambda: self.api.get(f"/api/v1/monitors/{m}")
        if r < 0.75:
            return "api_heartbeats", lambda: self.api.get(f"/api/v1/monitors/{m}/heartbeats?limit=50")
        return "api_uptime", lambda: self.api.get(f"/api/v1/monitors/{m}/uptime?days=30")

    def pick_ui(self) -> tuple[str, Callable[[], Any]]:
        if self.rng.random() < 0.5:
            return "ui_dashboard", lambda: self.ui.get("/")
        m = self.rng.choice(self.monitors)["id"]
        return "ui_monitor", lambda: self.ui.get(f"/monitors/{m}")

    async def sse_client(self, k: int) -> None:
        query = ["", "", "", "?status=down", "?q=lt-http", "?tag=critical"][k % 6]
        await asyncio.sleep(self.rng.random() * 5)
        first = True
        while self.running:
            try:
                async with self.sse_http.get("/events" + query, headers={"Accept": "text/event-stream"}) as r:
                    if r.status != 200:
                        self.sse["refused"] += 1
                        self.rec.error("sse_connect", f"{r.status}")
                        await asyncio.sleep(5)
                        continue
                    self.sse["connected"] += 1
                    if not first:
                        self.sse["reconnects"] += 1
                    first = False
                    tail = b""
                    try:
                        async for chunk in r.content.iter_any():
                            if not self.running:
                                break
                            data = tail + chunk
                            n = data.count(b"event: monitors-updated")
                            self.sse["events"] += n
                            self.sse_window["events"] += n
                            self.sse["bytes"] += len(chunk)
                            self.sse_window["bytes"] += len(chunk)
                            tail = data[-32:]
                    finally:
                        self.sse["connected"] -= 1
            except Exception as exc:  # noqa: BLE001
                if self.running:
                    self.rec.error("sse_connect", f"{type(exc).__name__}: {exc}")
                    await asyncio.sleep(2)

    async def push_client(self, m: dict[str, Any]) -> None:
        token = m["push_token"]
        period = m["interval"] / 2
        await asyncio.sleep(self.rng.random() * period)
        while self.running:
            ping = self.rng.randint(5, 80)
            await self.timed("push", lambda: self.public.get(f"/api/push/{token}?status=up&msg=ok&ping={ping}"))
            await asyncio.sleep(period)

    async def agent(self) -> None:
        headers = {
            "Authorization": f"Bearer {self.agent_token}",
            "x-agent-version": AGENT_VERSION,
            "x-agent-name": "lt-agent",
        }
        assigned: dict[int, int] = {}
        due: dict[int, float] = {}
        last_poll = 0.0
        while self.running:
            now = time.monotonic()
            if now - last_poll >= 30:
                last_poll = now
                t0 = time.perf_counter()
                try:
                    async with self.public.get("/api/agent/v1/monitors", headers=headers) as r:
                        body = await r.json()
                        self.rec.ok("agent_poll", (time.perf_counter() - t0) * 1000)
                    assigned = {m["id"]: m["interval_seconds"] for m in body["monitors"]}
                except Exception as exc:  # noqa: BLE001
                    self.rec.error("agent_poll", f"{type(exc).__name__}: {exc}")
            batch = []
            stamp = dt.datetime.now(dt.timezone.utc).strftime("%Y-%m-%d %H:%M:%S")
            for mid, interval in assigned.items():
                if due.get(mid, 0) <= now:
                    due[mid] = now + interval + self.rng.random()
                    batch.append(
                        {"monitor_id": mid, "status": "up", "response_time_ms": self.rng.randint(20, 120),
                         "message": "200 OK", "checked_at": stamp}  # fmt: skip
                    )
            if batch:
                await self.timed(
                    "agent_results", lambda: self.public.post("/api/agent/v1/results", json=batch, headers=headers)
                )
            await asyncio.sleep(5)

    async def bulk_editor(self) -> None:
        k = 0
        while self.running:
            await asyncio.sleep(self.args.bulk_every)
            if not self.running:
                break
            k += 1
            body = {"filter": {"tag": BULK_TAG}, "set": {"interval_seconds": 90 if k % 2 else 60}}
            await self.timed("bulk_edit", lambda: self.api.post("/api/v1/monitors/bulk", json=body))

    # -- sampling ----------------------------------------------------------------

    async def scrape_metrics(self) -> dict[str, float]:
        t0 = time.perf_counter()
        try:
            # Bounded so a saturated server can't stall the sample cadence.
            limit = aiohttp.ClientTimeout(total=max(5.0, self.args.sample_every))
            async with self.api.get("/metrics", timeout=limit) as r:
                text = await r.text()
                ms = (time.perf_counter() - t0) * 1000
                if r.status != 200:
                    self.rec.error("metrics", f"{r.status}", ms)
                    return {}
                self.rec.ok("metrics", ms)
                return parse_prom(text)
        except Exception as exc:  # noqa: BLE001
            self.rec.error("metrics", f"{type(exc).__name__}: {exc}")
            return {}

    async def fake_stats(self, s: aiohttp.ClientSession) -> dict[str, Any]:
        try:
            async with s.get(f"{self.fake_base}/_stats?since={self.deliveries_seen}") as r:
                return await r.json()
        except Exception:  # noqa: BLE001
            return {}

    def account_deliveries(self, stats: dict[str, Any]) -> None:
        for d in stats.get("deliveries", []):
            self.deliveries_seen += 1
            ev = d.get("event") or "?"
            self.deliveries_by_event[ev] = self.deliveries_by_event.get(ev, 0) + 1
            occurred = parse_rfc3339(d.get("occurred_at"))
            if occurred is not None:
                lat = d["t"] - occurred
                self.notif_window.append(lat)
                self.notif_total.append(lat)
            n = self.flappers.get(d.get("monitor_id") or -1)
            if n is not None and ev in ("down", "up"):
                changed, is_down = fakes.flap_last_change(n, d["t"], self.args.flap_period, self.args.flap_down)
                if is_down == (ev == "down"):
                    self.detect_window.append(d["t"] - changed)
                    self.detect_total.append(d["t"] - changed)

    async def sampler(self) -> None:
        fs = aiohttp.ClientSession(timeout=aiohttp.ClientTimeout(total=20))
        prev: dict[str, Any] = {}
        lookback = 2 * max(iv for iv, _ in INTERVALS) + 60
        try:
            while True:
                tick = time.monotonic()
                wall = time.time()
                proc, prom, fstats = await asyncio.gather(
                    asyncio.to_thread(self.inst.proc_stats), self.scrape_metrics(), self.fake_stats(fs)
                )
                since = int(prev.get("wall", wall - self.args.sample_every))
                try:
                    dbs = await asyncio.to_thread(
                        self.inst.db, "stats", str(since), str(lookback), BULK_TAG, str(self.server_started)
                    )
                except Exception as exc:  # noqa: BLE001
                    dbs = {}
                    self.log(f"db probe failed: {exc}")
                self.account_deliveries(fstats)
                self.rows.append(self.make_row(wall, proc, prom, dbs, fstats, prev))
                # Counter deltas need the last scrape that worked.
                prev = {
                    "wall": wall,
                    "proc": proc,
                    "prom": prom or prev.get("prom", {}),
                    "prom_wall": wall if prom else prev.get("prom_wall"),
                    "dbs": dbs,
                }
                self.print_row(self.rows[-1])
                if not self.running:
                    break
                await asyncio.sleep(max(0.0, self.args.sample_every - (time.monotonic() - tick)))
        finally:
            await fs.close()

    def make_row(
        self, wall: float, proc: dict, prom: dict, dbs: dict, fstats: dict, prev: dict[str, Any]
    ) -> dict[str, Any]:
        dtw = wall - prev["wall"] if prev else None
        row: dict[str, Any] = {
            "elapsed_s": round(wall - self.started_at, 1),
            "time": dt.datetime.fromtimestamp(wall, dt.timezone.utc).strftime("%H:%M:%S"),
        }
        row["rss_mb"] = proc.get("rss_mb")
        row["rss_peak_mb"] = proc.get("rss_peak_mb")
        row["cgroup_mb"] = proc.get("cgroup_mb")
        row["cgroup_file_mb"] = proc.get("cgroup_file_mb")
        row["threads"] = proc.get("threads")
        row["fds"] = proc.get("fds")
        if dtw and "cpu_seconds" in proc and "cpu_seconds" in prev.get("proc", {}):
            row["cpu_pct"] = round(100 * (proc["cpu_seconds"] - prev["proc"]["cpu_seconds"]) / dtw, 1)
        row["db_mb"] = round(dbs["db_bytes"] / 1048576, 2) if "db_bytes" in dbs else None
        row["wal_mb"] = round(dbs["wal_bytes"] / 1048576, 2) if "wal_bytes" in dbs else None
        row["free_pages"] = dbs.get("freelist_count")
        row["heartbeats"] = dbs.get("heartbeats")
        row["heartbeat_daily"] = dbs.get("heartbeat_daily")
        row["watermark"] = dbs.get("watermark")
        late = dbs.get("lateness") or {}
        row["gap_late_n"] = late.get("n")
        row["gap_late_p50_s"] = late.get("p50")
        row["gap_late_p95_s"] = late.get("p95")
        row["gap_late_max_s"] = late.get("max")
        row["gap_late_over5"] = late.get("over5")
        row["db_probe_ms"] = (dbs.get("count_ms") or 0) + (dbs.get("lateness_ms") or 0) if dbs else None
        pp = prev.get("prom", {}) if prev else {}
        dtp = wall - prev["prom_wall"] if prev and prev.get("prom_wall") else None
        if dtp and prom and pp:
            checks = prom.get("vigilant_checks_total", 0) - pp.get("vigilant_checks_total", 0)
            row["checks_per_s"] = round(checks / dtp, 2)
            cur, old = delay_buckets(prom), dict(delay_buckets(pp))
            window = [(le, c - old.get(le, 0)) for le, c in cur]
            for le, c in window:
                self.delay_acc[le] = self.delay_acc.get(le, 0) + c
            for q in (0.5, 0.95, 0.99):
                v = histogram_quantile(q, window)
                row[f"delay_p{int(q * 100)}_s"] = round(v, 3) if v is not None else None
            total = window[-1][1] if window else 0
            under1 = next((c for le, c in window if le == 1.0), None)
            row["delay_over1s"] = int(total - under1) if under1 is not None else None
        row["running_tasks"] = prom.get("vigilant_running_tasks")
        row["notif_sent"] = sum(v for k, v in prom.items() if k.startswith("vigilant_notifications_sent_total"))
        row["notif_failed"] = sum(v for k, v in prom.items() if k.startswith("vigilant_notifications_failed_total"))
        row["deliveries"] = fstats.get("deliveries_total")
        row["notif_lat_p95_s"] = round(pct(self.notif_window, 0.95), 3) if self.notif_window else None
        row["detect_p95_s"] = round(pct(self.detect_window, 0.95), 1) if self.detect_window else None
        self.notif_window, self.detect_window = [], []
        hits = fstats.get("hits", {})
        row["target_hits"] = sum(hits.values()) if hits else None
        row["sse_connected"] = self.sse["connected"]
        row["sse_events"] = self.sse_window["events"]
        row["sse_mb"] = round(self.sse_window["bytes"] / 1048576, 2)
        self.sse_window = {"events": 0, "bytes": 0}
        row["inflight"] = self.inflight
        row["dropped"] = self.dropped
        for cls, (lat, errs) in sorted(self.rec.roll().items()):
            row[f"{cls}_n"] = len(lat)
            row[f"{cls}_p50"] = round(pct(lat, 0.5), 1) if lat else None
            row[f"{cls}_p95"] = round(pct(lat, 0.95), 1) if lat else None
            row[f"{cls}_p99"] = round(pct(lat, 0.99), 1) if lat else None
            row[f"{cls}_err"] = errs
        return row

    def print_row(self, row: dict[str, Any]) -> None:
        def f(key: str, fmt: str = "{}") -> str:
            v = row.get(key)
            return "-" if v is None else fmt.format(v)

        self.log(
            f"t={row['elapsed_s']:>6.0f}s rss={f('rss_mb', '{:.0f}')}MB cpu={f('cpu_pct')}% fds={f('fds')} "
            f"db={f('db_mb')}MB wal={f('wal_mb')}MB hb={f('heartbeats')} chk/s={f('checks_per_s')} "
            f"delay95={f('delay_p95_s')}s gap95={f('gap_late_p95_s')}s page95={f('status_page_p95')}ms "
            f"frag95={f('status_fragment_p95')}ms api95={f('api_monitor_p95')}ms sse={row['sse_connected']}/"
            f"{row['sse_events']}ev deliv={f('deliveries')}"
        )

    # -- orchestration -------------------------------------------------------------

    async def run_load(self, duration: float) -> None:
        self.started_at = time.time()
        tasks = [
            asyncio.create_task(self.open_loop(self.args.status_rps, self.pick_status)),
            asyncio.create_task(self.open_loop(self.args.api_rps, self.pick_api)),
            asyncio.create_task(self.open_loop(self.args.ui_rps, self.pick_ui)),
            asyncio.create_task(self.agent()),
            asyncio.create_task(self.bulk_editor()),
        ]
        tasks += [asyncio.create_task(self.sse_client(k)) for k in range(self.args.sse_clients)]
        tasks += [asyncio.create_task(self.push_client(m)) for m in self.monitors if m["kind"] == "push"]
        sampler = asyncio.create_task(self.sampler())
        self.log(f"load running for {duration:.0f}s")
        await asyncio.sleep(duration)
        self.running = False
        await sampler
        for t in tasks:
            t.cancel()
        await asyncio.gather(*tasks, return_exceptions=True)

    async def close(self) -> None:
        for name in ("ui", "api", "public", "sse_http"):
            s = getattr(self, name, None)
            if s is not None:
                await s.close()


async def main_async(args: argparse.Namespace, inst: Instance, out_dir: Path) -> int:
    h = Harness(args, inst, out_dir)
    meta: dict[str, Any] = {"args": {k: v for k, v in vars(args).items() if k != "func"}}
    try:
        h.log(f"starting {inst.kind} instance")
        t0 = time.monotonic()
        h.server_started = int(time.time()) - 1
        await asyncio.to_thread(inst.start)
        meta["startup_s"] = round(time.monotonic() - t0, 1)
        await h.setup_accounts()
        await h.seed()
        meta["seed_create_p95_ms"] = h.seed_create_p95
        if args.backfill_days:
            h.log(f"stopping the server to backfill {args.backfill_days} days of heartbeats")
            await asyncio.to_thread(inst.stop)
            fill = await asyncio.to_thread(inst.db, "backfill", str(args.backfill_days))
            h.log(f"backfill: {fill}")
            meta["backfill"] = fill
            t0 = time.monotonic()
            h.server_started = int(time.time()) - 1
            await asyncio.to_thread(inst.restart)
            meta["restart_s"] = round(time.monotonic() - t0, 1)
            h.log(f"restarted in {meta['restart_s']}s")
            await h.login()
        if args.warmup:
            h.log(f"warm-up {args.warmup}s (load runs, not reported in the run-wide numbers)")
            await h.run_load(args.warmup)
            h.rows.clear()
            h.rec = Recorder()
            h.notif_total, h.detect_total = [], []
            h.delay_acc = {}
            h.deliveries_by_event = {}
            h.running = True
        await h.run_load(args.duration)
        try:
            meta["query_plans"] = await asyncio.to_thread(inst.db, "explain")
        except Exception as exc:  # noqa: BLE001
            meta["query_plans"] = {"error": str(exc)}
    except Exception:
        traceback.print_exc()
        meta["crash"] = traceback.format_exc()
    finally:
        await h.close()
        logs = await asyncio.to_thread(inst.logs)
        (out_dir / "server.log").write_text(logs, encoding="utf-8")
    meta["describe"] = await asyncio.to_thread(inst.describe)
    meta["host"] = host_info()
    return report.write(out_dir, h, meta, logs)


def host_info() -> dict[str, Any]:
    cpu = platform.processor()
    try:
        for line in Path("/proc/cpuinfo").read_text().splitlines():
            if line.lower().startswith(("model name", "hardware")):
                cpu = line.split(":", 1)[1].strip()
                break
    except OSError:
        pass
    if sys.platform == "win32":
        try:
            import winreg  # noqa: PLC0415 - Windows only

            key = winreg.OpenKey(winreg.HKEY_LOCAL_MACHINE, r"HARDWARE\DESCRIPTION\System\CentralProcessor\0")
            cpu = str(winreg.QueryValueEx(key, "ProcessorNameString")[0]).strip()
        except OSError:
            pass
    return {
        "os": platform.platform(),
        "machine": platform.machine(),
        "cpu": cpu,
        "cores": os.cpu_count(),
        "python": platform.python_version(),
    }


def parse_args(argv: list[str]) -> argparse.Namespace:
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--mode", choices=MODES, default="smoke")
    p.add_argument("--monitors", type=int, help="number of monitors (default by mode: 200/1000/1000)")
    p.add_argument("--duration", type=float, help="seconds of measured load (default by mode: 300/1800/7200)")
    p.add_argument("--hours", type=float, help="soak length in hours (overrides --duration)")
    p.add_argument("--warmup", type=float, default=60, help="seconds of load before measuring (default 60)")
    p.add_argument("--sse-clients", type=int, default=50)
    p.add_argument("--status-rps", type=float, default=20)
    p.add_argument("--api-rps", type=float, default=10)
    p.add_argument("--ui-rps", type=float, default=0.5)
    p.add_argument("--bulk-every", type=float, default=60, help="seconds between bulk edits")
    p.add_argument("--sample-every", type=float, default=10)
    p.add_argument("--retention-days", type=int, help="retention setting (soak default 1)")
    p.add_argument(
        "--backfill-days",
        type=int,
        help="days of synthetic history written before the load (default by mode: 1/2/3; 0 = empty database)",
    )
    p.add_argument("--flap-period", type=float, default=300)
    p.add_argument("--flap-down", type=float, default=90)
    p.add_argument("--max-inflight", type=int, default=500)
    p.add_argument("--seed", type=int, default=7)
    inst = p.add_argument_group("instance")
    inst.add_argument("--image", help="Docker image to run (Docker mode)")
    inst.add_argument("--cpus", default="1")
    inst.add_argument("--memory", default="512m")
    inst.add_argument("--sidecar-image", default="python:3.12-alpine")
    inst.add_argument("--binary", help="local vigilant-vole binary (local mode)")
    inst.add_argument("--url", help="an already running, freshly installed server (external mode)")
    inst.add_argument("--target-host", default="127.0.0.1", help="how an external server reaches this machine")
    inst.add_argument("--bind", help="fake target bind address (default 0.0.0.0 on Linux, else 127.0.0.1)")
    inst.add_argument("--max-concurrent-checks", help="VV_MAX_CONCURRENT_CHECKS for the server")
    inst.add_argument("--keep", action="store_true", help="leave the container/volume/data dir behind")
    out = p.add_argument_group("output and thresholds")
    out.add_argument("--out", help="output directory (default tools/loadtest/results/<mode>-<time>)")
    out.add_argument("--label", default="", help="free text shown in the summary (e.g. git revision)")
    report.add_threshold_args(out)
    args = p.parse_args(argv)
    mode = MODES[args.mode]
    args.monitors = args.monitors or mode["monitors"]
    args.duration = args.hours * 3600 if args.hours else (args.duration or mode["duration"])
    if args.retention_days is None:
        args.retention_days = mode["retention_days"]
    if args.backfill_days is None:
        args.backfill_days = 0 if args.url else mode["backfill_days"]
    if sum(x is not None for x in (args.image, args.binary, args.url)) != 1:
        p.error("give exactly one of --image, --binary or --url")
    if args.url and args.backfill_days:
        p.error("--backfill-days needs --image or --binary (the server must be stopped)")
    args.bind = args.bind or ("0.0.0.0" if sys.platform.startswith("linux") else "127.0.0.1")  # noqa: S104
    report.apply_threshold_defaults(args)
    return args


def main(argv: list[str]) -> int:
    for stream in (sys.stdout, sys.stderr):
        if hasattr(stream, "reconfigure"):
            stream.reconfigure(encoding="utf-8", errors="replace")
    args = parse_args(argv)
    stamp = time.strftime("%Y%m%d-%H%M%S")
    out_dir = Path(args.out or Path(__file__).resolve().parent / "results" / f"{args.mode}-{stamp}")
    out_dir.mkdir(parents=True, exist_ok=True)
    args.fake_http_port, args.fake_tcp_port, args.fake_dns_port = free_port(), free_port(), free_port()
    cfg = {
        "bind": args.bind,
        "http_port": args.fake_http_port,
        "tcp_port": args.fake_tcp_port,
        "dns_port": args.fake_dns_port,
        "flap_period": args.flap_period,
        "flap_down": args.flap_down,
        "seed": args.seed,
    }
    ready = multiprocessing.Event()
    proc = multiprocessing.Process(target=fakes.run, args=(cfg, ready), daemon=True)
    proc.start()
    if not ready.wait(30):
        raise SystemExit("fake targets did not start")

    env = {"RUST_LOG": "info"}
    if args.max_concurrent_checks:
        env["VV_MAX_CONCURRENT_CHECKS"] = args.max_concurrent_checks
    run_id = f"{args.mode}-{stamp}-{os.getpid()}"
    inst: Instance
    if args.image:
        inst = DockerInstance(args.image, args.cpus, args.memory, run_id, env, args.sidecar_image)
    elif args.binary:
        inst = LocalInstance(args.binary, env)
    else:
        inst = ExternalInstance(args.url, args.target_host, None)
    try:
        return asyncio.run(main_async(args, inst, out_dir))
    finally:
        if not args.keep:
            inst.cleanup()
        else:
            inst.stop()
        proc.terminate()
        proc.join(10)
        print(f"results in {out_dir}")


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
