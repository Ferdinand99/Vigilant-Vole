"""The Vigilant Vole instance under test: a Docker container (the release
image), a local binary, or an already running server. Each knows how to start,
stop and restart it and what it can measure about it."""

from __future__ import annotations

import json
import os
import shutil
import socket
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request
from pathlib import Path
from typing import Any

HERE = Path(__file__).resolve().parent
DB_NAME = "vigilant-vole.db"
CLK_TCK = 100  # Linux USER_HZ


def free_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def run(cmd: list[str], check: bool = True, timeout: float = 120) -> str:
    res = subprocess.run(cmd, capture_output=True, text=True, timeout=timeout, encoding="utf-8", errors="replace")
    if check and res.returncode != 0:
        raise RuntimeError(f"{' '.join(cmd)} failed ({res.returncode}): {res.stderr.strip() or res.stdout.strip()}")
    return res.stdout


def wait_healthy(base_url: str, timeout: float = 120, alive: Any = None) -> None:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if alive is not None and not alive():
            raise RuntimeError("server exited during startup")
        try:
            with urllib.request.urlopen(base_url + "/healthz", timeout=2) as resp:  # noqa: S310 - local
                if resp.status == 200:
                    return
        except (urllib.error.URLError, ConnectionError, TimeoutError, OSError):
            pass
        time.sleep(0.25)
    raise RuntimeError(f"{base_url} did not become healthy within {timeout:.0f}s")


def parse_proc(stat: str, status: str) -> dict[str, float]:
    """CPU seconds, RSS, peak RSS and threads from /proc/<pid>/{stat,status}."""
    out: dict[str, float] = {}
    fields = stat[stat.rfind(")") + 2 :].split()
    out["cpu_seconds"] = (int(fields[11]) + int(fields[12])) / CLK_TCK  # utime + stime
    for line in status.splitlines():
        key, _, value = line.partition(":")
        if key == "VmRSS":
            out["rss_mb"] = int(value.split()[0]) / 1024
        elif key == "VmHWM":
            out["rss_peak_mb"] = int(value.split()[0]) / 1024
        elif key == "Threads":
            out["threads"] = int(value)
    return out


class Instance:
    """Base: an instance we can only talk to over HTTP."""

    kind = "external"
    base_url = ""
    target_host = "127.0.0.1"  # how the server reaches the fake targets
    dns_ip = "127.0.0.1"  # same, as an IP (dns monitors need one)

    def start(self) -> None:
        wait_healthy(self.base_url)

    def stop(self) -> None:
        pass

    def restart(self) -> None:
        raise RuntimeError(f"{self.kind} instances cannot be restarted by the harness")

    def cleanup(self) -> None:
        pass

    def proc_stats(self) -> dict[str, float]:
        return {}

    def db(self, *args: str) -> dict[str, Any]:
        return {}

    def logs(self) -> str:
        return ""

    def describe(self) -> dict[str, Any]:
        return {"instance": self.kind, "url": self.base_url}


class ExternalInstance(Instance):
    def __init__(self, url: str, target_host: str, dns_ip: str | None) -> None:
        self.base_url = url.rstrip("/")
        self.target_host = target_host
        self.dns_ip = dns_ip or target_host


class DockerInstance(Instance):
    """The release image with VPS-like limits, a named volume for /data and a
    small Python sidecar on the same volume for read-only database probes."""

    kind = "docker"

    def __init__(self, image: str, cpus: str, memory: str, run_id: str, env: dict[str, str], sidecar_image: str) -> None:
        self.image = image
        self.cpus = cpus
        self.memory = memory
        self.name = f"vv-load-{run_id}"
        self.sidecar = f"{self.name}-db"
        self.volume = f"{self.name}-data"
        self.port = free_port()
        self.base_url = f"http://127.0.0.1:{self.port}"
        self.env = env
        self.sidecar_image = sidecar_image
        self.target_host = "host.docker.internal"

    def start(self) -> None:
        run(["docker", "volume", "create", self.volume])
        cmd = [
            "docker", "run", "-d", "--name", self.name,
            "--cpus", self.cpus, "--memory", self.memory, "--memory-swap", self.memory,
            "-p", f"127.0.0.1:{self.port}:3001",
            "--add-host", "host.docker.internal:host-gateway",
            "-v", f"{self.volume}:/data",
        ]  # fmt: skip
        for key, value in self.env.items():
            cmd += ["-e", f"{key}={value}"]
        # The harness posts to /setup right away: no setup token.
        cmd += ["-e", "VV_SETUP_OPEN=1"]
        run(cmd + [self.image])
        wait_healthy(self.base_url, alive=self._running)
        hosts = run(["docker", "exec", self.name, "cat", "/etc/hosts"])
        for line in hosts.splitlines():
            parts = line.split()
            if len(parts) >= 2 and "host.docker.internal" in parts[1:]:
                self.dns_ip = parts[0]
                break
        run(["docker", "run", "-d", "--name", self.sidecar, "-v", f"{self.volume}:/data",
             "--entrypoint", "sleep", self.sidecar_image, "infinity"], timeout=600)  # fmt: skip
        run(["docker", "cp", str(HERE / "dbstats.py"), f"{self.sidecar}:/dbstats.py"])

    def _running(self) -> bool:
        out = run(["docker", "inspect", "-f", "{{.State.Running}}", self.name], check=False)
        return out.strip() == "true"

    def stop(self) -> None:
        run(["docker", "stop", "-t", "30", self.name], check=False, timeout=90)

    def restart(self) -> None:
        run(["docker", "start", self.name])
        wait_healthy(self.base_url, alive=self._running)

    def cleanup(self) -> None:
        run(["docker", "rm", "-f", self.name, self.sidecar], check=False)
        run(["docker", "volume", "rm", "-f", self.volume], check=False)

    def proc_stats(self) -> dict[str, float]:
        script = (
            "cat /proc/1/stat; echo @@; cat /proc/1/status; echo @@; ls /proc/1/fd | wc -l; echo @@; "
            "cat /sys/fs/cgroup/memory.current 2>/dev/null || echo -1; echo @@; "
            "grep -E '^(anon|file) ' /sys/fs/cgroup/memory.stat 2>/dev/null || true"
        )
        try:
            out = run(["docker", "exec", self.name, "sh", "-c", script], timeout=20)
        except (RuntimeError, subprocess.TimeoutExpired):
            return {}
        stat, status, fds, cg, cgstat = (part.strip() for part in out.split("@@"))
        stats = parse_proc(stat, status)
        stats["fds"] = int(fds)
        if int(cg) >= 0:
            stats["cgroup_mb"] = int(cg) / 1048576
        for line in cgstat.splitlines():
            key, value = line.split()
            stats[f"cgroup_{key}_mb"] = int(value) / 1048576
        return stats

    def db(self, *args: str) -> dict[str, Any]:
        out = run(["docker", "exec", self.sidecar, "python3", "/dbstats.py", args[0], f"/data/{DB_NAME}", *args[1:]],
                  timeout=1800)  # fmt: skip
        return json.loads(out)

    def logs(self) -> str:
        res = subprocess.run(["docker", "logs", self.name], capture_output=True, text=True, encoding="utf-8",
                             errors="replace")  # fmt: skip
        return res.stdout + res.stderr

    def describe(self) -> dict[str, Any]:
        info = json.loads(run(["docker", "info", "--format", "{{json .}}"]))
        image = json.loads(run(["docker", "image", "inspect", self.image]))[0]
        return {
            "instance": "docker",
            "image": self.image,
            "image_id": image.get("Id", "")[:19],
            "image_arch": image.get("Architecture"),
            "limits": f"--cpus {self.cpus} --memory {self.memory}",
            "docker_host": f"{info.get('OperatingSystem')} {info.get('Architecture')}, "
            f"{info.get('NCPU')} CPUs, {int(info.get('MemTotal', 0)) / 2**30:.1f} GiB",
        }


class LocalInstance(Instance):
    """A binary started by the harness with a throw-away DATA_DIR."""

    kind = "local"

    def __init__(self, binary: str, env: dict[str, str]) -> None:
        self.binary = binary
        self.port = free_port()
        self.base_url = f"http://127.0.0.1:{self.port}"
        self.data_dir = Path(tempfile.mkdtemp(prefix="vv-load-"))
        self.env = {**os.environ, **env, "DATA_DIR": str(self.data_dir), "PORT": str(self.port)}
        for key in ("VV_SETUP_TOKEN", "VV_CONFIG_FILE", "VV_TRUST_PROXY"):
            self.env.pop(key, None)
        # The harness posts to /setup right away: no setup token.
        self.env["VV_SETUP_OPEN"] = "1"
        self.proc: subprocess.Popen[bytes] | None = None
        self.log_path = self.data_dir / "server.log"

    def start(self) -> None:
        log = open(self.log_path, "ab")  # noqa: SIM115 - closed with the process
        self.proc = subprocess.Popen([self.binary], cwd=self.data_dir, env=self.env, stdout=log,
                                     stderr=subprocess.STDOUT)  # fmt: skip
        wait_healthy(self.base_url, alive=lambda: self.proc is not None and self.proc.poll() is None)

    def stop(self) -> None:
        if self.proc and self.proc.poll() is None:
            self.proc.terminate()
            try:
                self.proc.wait(timeout=30)
            except subprocess.TimeoutExpired:
                self.proc.kill()
                self.proc.wait(timeout=10)

    def restart(self) -> None:
        self.start()

    def cleanup(self) -> None:
        self.stop()
        shutil.rmtree(self.data_dir, ignore_errors=True)

    def proc_stats(self) -> dict[str, float]:
        if self.proc is None or not sys.platform.startswith("linux"):
            return {}
        base = Path(f"/proc/{self.proc.pid}")
        try:
            stats = parse_proc((base / "stat").read_text(), (base / "status").read_text())
            stats["fds"] = len(os.listdir(base / "fd"))
        except OSError:
            return {}
        return stats

    def db(self, *args: str) -> dict[str, Any]:
        out = run([sys.executable, str(HERE / "dbstats.py"), args[0], str(self.data_dir / DB_NAME), *args[1:]],
                  timeout=1800)  # fmt: skip
        return json.loads(out)

    def logs(self) -> str:
        try:
            return self.log_path.read_text(encoding="utf-8", errors="replace")
        except OSError:
            return ""

    def describe(self) -> dict[str, Any]:
        return {"instance": "local", "binary": self.binary, "data_dir": str(self.data_dir)}
