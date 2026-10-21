"""CSV / Markdown / JSON output and pass/fail thresholds for the load test."""

from __future__ import annotations

import argparse
import csv
import json
import re
import time
from collections import Counter
from pathlib import Path
from typing import Any

# Per mode: smoke runs on shared CI runners, so its limits leave headroom.
THRESHOLDS: dict[str, dict[str, float]] = {
    "smoke": {"status_p95_ms": 500, "api_p95_ms": 500, "ui_p95_ms": 2000, "delay_p95_s": 2, "gap_p95_s": 3,
              "rss_mb": 200, "error_rate": 0.0},  # fmt: skip
    "load": {"status_p95_ms": 1000, "api_p95_ms": 1000, "ui_p95_ms": 3000, "delay_p95_s": 2, "gap_p95_s": 3,
             "rss_mb": 384, "error_rate": 0.0},  # fmt: skip
    "soak": {"status_p95_ms": 1000, "api_p95_ms": 1000, "ui_p95_ms": 3000, "delay_p95_s": 2, "gap_p95_s": 3,
             "rss_mb": 384, "error_rate": 0.0},  # fmt: skip
}

STATUS = ("status_page", "status_fragment", "status_json")
API = ("api_list", "api_monitor", "api_heartbeats", "api_uptime")
UI = ("ui_dashboard", "ui_monitor")


def add_threshold_args(g: argparse._ArgumentGroup) -> None:
    g.add_argument("--max-status-p95-ms", type=float, help="status page/fragment/JSON p95")
    g.add_argument("--max-api-p95-ms", type=float, help="REST API read p95")
    g.add_argument("--max-ui-p95-ms", type=float, help="dashboard and monitor page p95")
    g.add_argument("--max-delay-p95-s", type=float, help="scheduled check start delay p95 (from /metrics)")
    g.add_argument("--max-gap-p95-s", type=float, help="heartbeat gap minus interval: p95 over the samples of each sample's p95")
    g.add_argument("--max-rss-mb", type=float, help="peak resident memory")
    g.add_argument("--max-error-rate", type=float, help="failed requests / all requests")
    g.add_argument("--no-thresholds", action="store_true", help="report only, always exit 0")


def apply_threshold_defaults(args: argparse.Namespace) -> None:
    d = THRESHOLDS[args.mode]
    for arg, key in (
        ("max_status_p95_ms", "status_p95_ms"),
        ("max_api_p95_ms", "api_p95_ms"),
        ("max_ui_p95_ms", "ui_p95_ms"),
        ("max_delay_p95_s", "delay_p95_s"),
        ("max_gap_p95_s", "gap_p95_s"),
        ("max_rss_mb", "rss_mb"),
        ("max_error_rate", "error_rate"),
    ):
        if getattr(args, arg) is None:
            setattr(args, arg, d[key])


def pct(values: list[float], p: float) -> float | None:
    if not values:
        return None
    v = sorted(values)
    return v[min(len(v) - 1, int(p * len(v)))]


def histogram_quantile(q: float, buckets: list[tuple[float, float]]) -> float | None:
    """Prometheus-style quantile from cumulative ``(le, count)`` deltas."""
    if not buckets or buckets[-1][1] <= 0:
        return None
    rank = q * buckets[-1][1]
    prev_le, prev_count = 0.0, 0.0
    for le, count in buckets:
        if count >= rank:
            if le == float("inf"):
                return prev_le
            if count == prev_count:
                return le
            return prev_le + (le - prev_le) * (rank - prev_count) / (count - prev_count)
        prev_le, prev_count = le, count
    return prev_le


def fmt(v: Any, nd: int = 1) -> str:
    if v is None:
        return "–"
    if isinstance(v, float):
        return f"{v:.{nd}f}"
    return str(v)


def col(rows: list[dict[str, Any]], key: str) -> list[float]:
    return [r[key] for r in rows if isinstance(r.get(key), (int, float))]


LOG_LINE = re.compile(r"\b(WARN|ERROR)\b.*?(vigilant_vole[\w:]*|tower_http[\w:]*): (.*)$")


def log_findings(logs: str) -> dict[str, Any]:
    levels: Counter[str] = Counter()
    for line in logs.splitlines():
        m = LOG_LINE.search(line)
        if m:
            msg = re.split(r" \w+=", m.group(3), maxsplit=1)[0].strip()
            levels[f"{m.group(1)} {m.group(2)}: {msg}"] += 1
    retention = [ln.strip() for ln in logs.splitlines() if "retention run finished" in ln]
    return {"warnings": levels.most_common(20), "retention_runs": retention}


def write(out_dir: Path, h: Any, meta: dict[str, Any], logs: str) -> int:
    rows = h.rows
    args = h.args
    keys: list[str] = []
    for r in rows:
        keys += [k for k in r if k not in keys]
    with open(out_dir / "samples.csv", "w", newline="", encoding="utf-8") as f:
        w = csv.DictWriter(f, fieldnames=keys)
        w.writeheader()
        w.writerows(rows)

    hours = (rows[-1]["elapsed_s"] - rows[0]["elapsed_s"]) / 3600 if len(rows) > 1 else 0
    res: dict[str, Any] = {"label": args.label, "mode": args.mode, "monitors": args.monitors,
                           "duration_s": args.duration, "hours": round(hours, 3)}  # fmt: skip
    rss = col(rows, "rss_mb")
    res["rss_start_mb"] = rss[0] if rss else None
    res["rss_end_mb"] = rss[-1] if rss else None
    res["rss_max_mb"] = max(col(rows, "rss_peak_mb") or rss or [0]) or None
    cg = col(rows, "cgroup_mb")
    res["cgroup_max_mb"] = max(cg) if cg else None
    cpu = col(rows, "cpu_pct")
    res["cpu_avg_pct"] = round(sum(cpu) / len(cpu), 1) if cpu else None
    res["cpu_p95_pct"] = pct(cpu, 0.95)
    res["fds_max"] = max(col(rows, "fds") or [0]) or None
    res["threads_max"] = max(col(rows, "threads") or [0]) or None
    db = col(rows, "db_mb")
    res["db_start_mb"], res["db_end_mb"] = (db[0], db[-1]) if db else (None, None)
    res["db_growth_mb_per_h"] = round((db[-1] - db[0]) / hours, 2) if db and hours else None
    res["wal_max_mb"] = max(col(rows, "wal_mb") or [0])
    hb = col(rows, "heartbeats")
    res["heartbeats_start"], res["heartbeats_end"] = (hb[0], hb[-1]) if hb else (None, None)
    res["heartbeat_rows_per_h"] = round((hb[-1] - hb[0]) / hours) if hb and hours else None
    rates = col(rows, "checks_per_s")
    res["checks_per_s"] = round(sum(rates) / len(rates), 1) if rates else None

    acc: dict[float, float] = h.delay_acc
    buckets = sorted(acc.items())
    for q in (0.5, 0.95, 0.99):
        v = histogram_quantile(q, buckets)
        res[f"delay_p{int(q * 100)}_s"] = round(v, 3) if v is not None else None
    over1 = None
    if buckets:
        under1 = acc.get(1.0)
        over1 = buckets[-1][1] - under1 if under1 is not None else None
    res["delay_checks"] = buckets[-1][1] if buckets else None
    res["delay_over_1s"] = over1
    gap95 = col(rows, "gap_late_p95_s")
    res["gap_p95_worst_s"] = max(gap95) if gap95 else None
    # Typical sample: a one-off (the first checks after a restart move onto
    # their phase slot, up to half an interval later) doesn't fail the run.
    res["gap_p95_typical_s"] = pct(gap95, 0.95)
    res["gap_p50_median_s"] = pct(col(rows, "gap_late_p50_s"), 0.5)
    res["gap_max_s"] = max(col(rows, "gap_late_max_s") or [0]) if gap95 else None
    res["gap_over5"] = int(sum(col(rows, "gap_late_over5")))

    classes = {}
    requests = errors = 0
    for name, c in sorted(h.rec.classes.items()):
        n = len(c.total)
        classes[name] = {"n": n, "p50": pct(c.total, 0.5), "p95": pct(c.total, 0.95), "p99": pct(c.total, 0.99),
                         "max": max(c.total) if c.total else None, "errors": c.errors_total,
                         "error_samples": c.error_samples}  # fmt: skip
        requests += max(n, c.errors_total)
        errors += c.errors_total
    res["classes"] = classes
    res["requests"] = requests
    res["errors"] = errors
    res["error_rate"] = errors / requests if requests else 0.0
    res["dropped"] = h.dropped
    res["sse"] = dict(h.sse)
    res["sse_events_per_s"] = round(sum(col(rows, "sse_events")) / max(1, hours * 3600), 2)
    res["sse_mb_per_s"] = round(sum(col(rows, "sse_mb")) / max(1, hours * 3600), 3)
    res["deliveries"] = dict(h.deliveries_by_event)
    res["notif_latency_p50_s"] = pct(h.notif_total, 0.5)
    res["notif_latency_p95_s"] = pct(h.notif_total, 0.95)
    res["detect_p50_s"] = pct(h.detect_total, 0.5)
    res["detect_p95_s"] = pct(h.detect_total, 0.95)
    res["detect_max_s"] = max(h.detect_total) if h.detect_total else None
    res["notif_failed"] = rows[-1].get("notif_failed") if rows else None
    res["notif_sent"] = rows[-1].get("notif_sent") if rows else None
    lf = log_findings(logs)
    res["log_warnings"] = lf["warnings"]
    res["retention_runs"] = lf["retention_runs"]

    def worst(names: tuple[str, ...]) -> float | None:
        vals = [classes[n]["p95"] for n in names if n in classes and classes[n]["p95"] is not None]
        return max(vals) if vals else None

    checks = [
        ("status p95 (ms)", worst(STATUS), args.max_status_p95_ms),
        ("api p95 (ms)", worst(API), args.max_api_p95_ms),
        ("ui p95 (ms)", worst(UI), args.max_ui_p95_ms),
        ("check delay p95 (s)", res["delay_p95_s"], args.max_delay_p95_s),
        ("heartbeat gap p95, 95th-percentile sample (s)", res["gap_p95_typical_s"], args.max_gap_p95_s),
        ("peak RSS (MB)", res["rss_max_mb"], args.max_rss_mb),
        ("error rate", res["error_rate"], args.max_error_rate),
        ("notification failures", res["notif_failed"], 0),
    ]
    verdicts = []
    for name, value, limit in checks:
        ok = value is None or value <= limit
        verdicts.append({"check": name, "value": value, "limit": limit, "ok": ok})
    if "crash" in meta:
        verdicts.append({"check": "harness ran to completion", "value": "crashed", "limit": "-", "ok": False})
    if sum(h.deliveries_by_event.values()) == 0 and any(m["kind"] == "flap" for m in h.monitors):
        verdicts.append({"check": "notifications delivered", "value": 0, "limit": ">= 1", "ok": False})
    if args.sse_clients and h.sse["events"] == 0:
        verdicts.append({"check": "SSE events received", "value": 0, "limit": ">= 1", "ok": False})
    res["thresholds"] = verdicts
    passed = all(v["ok"] for v in verdicts)
    res["passed"] = passed
    meta["results"] = res
    (out_dir / "summary.json").write_text(json.dumps(meta, indent=2, default=str), encoding="utf-8")
    (out_dir / "summary.md").write_text(markdown(meta, res), encoding="utf-8")
    print((out_dir / "summary.md").read_text(encoding="utf-8"))
    return 0 if passed or args.no_thresholds else 1


def markdown(meta: dict[str, Any], r: dict[str, Any]) -> str:
    a = meta["args"]
    d = meta.get("describe", {})
    host = meta.get("host", {})
    out = [f"# Vigilant Vole {a['mode']} run {('— ' + a['label']) if a.get('label') else ''}".rstrip(), ""]
    out.append(f"*{time.strftime('%Y-%m-%d %H:%M UTC', time.gmtime())}* · **{'PASS' if r['passed'] else 'FAIL'}**")
    out += ["", "## Setup", ""]
    out.append(f"- {a['monitors']} monitors, {r['hours'] * 60:.0f} min measured after {a['warmup']:.0f} s warm-up; "
               f"{a['sse_clients']} SSE dashboards, {a['status_rps']:g} status-page req/s, {a['api_rps']:g} API req/s, "
               f"{a['ui_rps']:g} UI req/s, bulk edit every {a['bulk_every']:g} s")  # fmt: skip
    if d:
        out.append("- Instance: " + ", ".join(f"{k}: {v}" for k, v in d.items()))
    out.append(f"- Load generator host: {host.get('os')} ({host.get('machine')}), {host.get('cpu')}, {host.get('cores')} cores")
    if a.get("retention_days"):
        out.append(f"- Retention {a['retention_days']} day(s); backfill: {meta.get('backfill', '–')}")
    if "startup_s" in meta:
        out.append(f"- Startup {meta['startup_s']} s; restart after backfill {meta.get('restart_s', '–')} s; "
                   f"monitor create p95 {fmt(meta.get('seed_create_p95_ms'), 0)} ms")  # fmt: skip
    out += ["", "## Resources", "", "| | start | end | max |", "|---|---|---|---|"]
    out.append(f"| RSS (MB) | {fmt(r['rss_start_mb'])} | {fmt(r['rss_end_mb'])} | {fmt(r['rss_max_mb'])} |")
    out.append(f"| cgroup memory incl. page cache (MB) | | | {fmt(r['cgroup_max_mb'])} |")
    out.append(f"| CPU (% of one core) | avg {fmt(r['cpu_avg_pct'])} | p95 {fmt(r['cpu_p95_pct'])} | |")
    out.append(f"| Open FDs / threads | | | {fmt(r['fds_max'])} / {fmt(r['threads_max'])} |")
    out.append(f"| DB file (MB) | {fmt(r['db_start_mb'], 2)} | {fmt(r['db_end_mb'], 2)} | growth {fmt(r['db_growth_mb_per_h'], 2)} MB/h |")
    out.append(f"| WAL (MB) | | | {fmt(r['wal_max_mb'], 2)} |")
    out.append(f"| Heartbeat rows | {fmt(r['heartbeats_start'])} | {fmt(r['heartbeats_end'])} | {fmt(r['heartbeat_rows_per_h'])} rows/h |")
    out += ["", "## Scheduler", ""]
    out.append(f"- {fmt(r['checks_per_s'])} checks/s recorded (– = `/metrics` never answered in time)")
    out.append(f"- Start delay vs schedule (`vigilant_check_delay_seconds`, {fmt(r['delay_checks'], 0)} checks): "
               f"p50 {fmt(r['delay_p50_s'], 3)} s, p95 {fmt(r['delay_p95_s'], 3)} s, p99 {fmt(r['delay_p99_s'], 3)} s, "
               f"over 1 s: {fmt(r['delay_over_1s'], 0)}")  # fmt: skip
    out.append(f"- Heartbeat gap minus interval (1 s resolution): median p50 {fmt(r['gap_p50_median_s'])} s, "
               f"p95 of the 10-s samples' p95 {fmt(r['gap_p95_typical_s'])} s, worst sample {fmt(r['gap_p95_worst_s'])} s, max {fmt(r['gap_max_s'])} s, gaps over 5 s: {r['gap_over5']}")  # fmt: skip
    out += ["", "## Latency (ms, whole run)", "", "| class | requests | p50 | p95 | p99 | max | errors |",
            "|---|---|---|---|---|---|---|"]  # fmt: skip
    for name, c in r["classes"].items():
        out.append(f"| {name} | {c['n']} | {fmt(c['p50'])} | {fmt(c['p95'])} | {fmt(c['p99'])} | {fmt(c['max'])} | {c['errors']} |")
    out.append("")
    out.append(f"Requests {r['requests']}, errors {r['errors']} ({r['error_rate'] * 100:.3f}%), dropped by the in-flight cap {r['dropped']}.")
    samples = {n: c["error_samples"] for n, c in r["classes"].items() if c["error_samples"]}
    if samples:
        out += ["", "Error samples:", ""]
        for n, s in samples.items():
            out.append(f"- {n}: " + "; ".join(sorted(set(s))[:5]))
    out += ["", "## Live dashboards and notifications", ""]
    s = r["sse"]
    out.append(f"- SSE: {s['events']} renders received ({r['sse_events_per_s']}/s, {r['sse_mb_per_s']} MB/s), "
               f"reconnects {s['reconnects']}, refused {s['refused']}")  # fmt: skip
    out.append(f"- Webhook deliveries by event: {r['deliveries'] or '–'}; app counters sent {fmt(r['notif_sent'], 0)}, failed {fmt(r['notif_failed'], 0)}")
    out.append(f"- Delivery latency (event time → sink): p50 {fmt(r['notif_latency_p50_s'], 3)} s, p95 {fmt(r['notif_latency_p95_s'], 3)} s")
    out.append(f"- Detection (target flips → webhook, 30 s interval, no retries): p50 {fmt(r['detect_p50_s'])} s, "
               f"p95 {fmt(r['detect_p95_s'])} s, max {fmt(r['detect_max_s'])} s")  # fmt: skip
    out += ["", "## Retention runs", ""]
    out += [f"- `{ln[-220:]}`" for ln in r["retention_runs"]] or ["- none in this run"]
    out += ["", "## Server warnings and errors", ""]
    out += [f"- {n}× {msg}" for msg, n in r["log_warnings"]] or ["- none"]
    out += ["", "## Thresholds", "", "| check | value | limit | |", "|---|---|---|---|"]
    for v in r["thresholds"]:
        out.append(f"| {v['check']} | {fmt(v['value'], 3)} | {v['limit']} | {'ok' if v['ok'] else '**FAIL**'} |")
    plans = meta.get("query_plans")
    if plans:
        out += ["", "## Query plans", ""]
        for name, plan in plans.items():
            out.append(f"- {name}: {' / '.join(plan) if isinstance(plan, list) else plan}")
    if "crash" in meta:
        out += ["", "## Harness error", "", "```", meta["crash"], "```"]
    return "\n".join(out) + "\n"
