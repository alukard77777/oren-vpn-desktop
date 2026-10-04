#!/usr/bin/env python3
"""REALITY compatibility lab: real Xray servers of the generations that break clients, in Docker,
and the cores MULTICORE runs in front of them, one HTTPS probe per (server, client) cell.

A minimal port of SlothClash's tests/reality-lab (https://github.com/Nemu-x/SlothClash, PR #81).
The expected result of every cell comes from a model of the server generations, so a regression
in either direction fails the run:

  * Xray < 25.0 silently drops a ClientHello whose REALITY key is the X25519MLKEM768 share;
  * Xray >= 26.9.8 (XTLS/REALITY 8cdf7bf) rejects a ClientHello without an X25519MLKEM768 share;
  * a server with minClientVer hands an older client version to its cover site (a timeout).

Clients under test:
  --patched  the Mihomo built by scripts/build-mihomo.sh (linux-amd64): auto, off and on policies
  --stock    the unpatched MetaCubeX Mihomo of the same tag (expected to fail 26.9.9 and minver)
  --xray     the Xray client MULTICORE bundles (normal mode speaks REALITY through Xray)

usage: scripts/reality-lab.py [--patched BIN] [--stock BIN|--fetch-stock] [--xray BIN|--fetch-xray]
needs: Linux, Docker, internet (servers fall back to www.cloudflare.com; probes fetch
       https://www.gstatic.com/generate_204 through the tunnel).
"""

import argparse
import concurrent.futures
import gzip
import hashlib
import io
import json
import os
import shutil
import socket
import subprocess
import sys
import tempfile
import time
import urllib.request
import zipfile

# Lab-only key pair and ids (never used outside this test; same as SlothClash's lab).
PRIVATE_KEY = "KMrJVES1iBgdUD96IaYtAQTlv2BJonjum3X83DXtOGQ"
PUBLIC_KEY = "WB52-may9oKJkdKL4cbLSLg_t41INOVet9-1ey1RLnk"
UUID = "3f1c9a3e-5c6a-4d1e-9d8b-2a7e4f0b1c55"
SHORT_ID = "0123456789abcdef"
SNI = "www.cloudflare.com"
PROBE_URL = "https://www.gstatic.com/generate_204"
INNER_PORT = 10000

SERVER_VERSIONS = ["24.12.31", "26.9.9"]
VARIANTS = {"plain": {}, "minver": {"minClientVer": "25.1.1"}}

# The cores MULTICORE shipped before the patch series, and the Xray it bundles (pinned bytes).
STOCK_MIHOMO = (
    "https://github.com/MetaCubeX/mihomo/releases/download/v1.19.32/mihomo-linux-amd64-compatible-v1.19.32.gz",
    "ba3ce607747a07f948fc35780e108a4a7c7f552a38b9bd4d115f313ebcb89c20",
    "7a0d59da2e678d56c899a3db996a2ad8963286c4f3634b0451435db248f13fa1",
)
ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))


def version_tuple(text):
    return tuple(int(part) for part in text.split("."))


def accepts_share(server_version, mlkem):
    """Whether an Xray server of this version completes REALITY with (mlkem) or without the share."""
    if version_tuple(server_version) < (25, 0, 0):
        return not mlkem
    if version_tuple(server_version) >= (26, 9, 8):
        return mlkem
    return True


def version_gate(variant, client_version):
    if variant == "minver":
        return version_tuple(client_version) >= (25, 1, 1)
    return True


def expect_patched(policy):
    def model(row):
        if not version_gate(row["variant"], "26.9.9"):
            return False
        if row["forced"] or policy == "on":
            return accepts_share(row["version"], True)  # non-chrome prints are swapped to chrome
        if policy == "off":
            return accepts_share(row["version"], False)
        return True  # auto: one of the two shares works on every generation
    return model


def expect_stock(row):
    if not version_gate(row["variant"], "1.8.2"):
        return False
    return accepts_share(row["version"], row["forced"] and row["fingerprint"] == "chrome")


def expect_xray(client_version):
    # Xray's own client: its version, and modern fingerprints (chrome, firefox) that carry
    # X25519MLKEM768 first and X25519 second; the REALITY key is the X25519 share when present,
    # so a pre-25 server (which knows only X25519) authenticates it too.
    def model(row):
        return version_gate(row["variant"], client_version)
    return model


def sha256(path):
    digest = hashlib.sha256()
    with open(path, "rb") as stream:
        for block in iter(lambda: stream.read(1 << 20), b""):
            digest.update(block)
    return digest.hexdigest()


def download(url, want):
    with urllib.request.urlopen(url, timeout=120) as response:
        data = response.read()
    got = hashlib.sha256(data).hexdigest()
    if got != want:
        sys.exit(f"{url}: SHA-256 {got} does not match the pinned {want}")
    return data


def fetch_stock(work):
    url, archive_sha, exe_sha = STOCK_MIHOMO
    path = os.path.join(work, "mihomo-stock")
    with open(path, "wb") as out:
        out.write(gzip.decompress(download(url, archive_sha)))
    if sha256(path) != exe_sha:
        sys.exit("stock Mihomo executable does not match its pin")
    os.chmod(path, 0o755)
    return path


def fetch_xray(work):
    manifest = json.load(open(os.path.join(ROOT, "packaging", "linux", "versions.json"), encoding="utf-8"))
    asset = manifest["components"]["xray"]["asset"]
    archive = zipfile.ZipFile(io.BytesIO(download(asset["url"], asset["sha256"])))
    path = os.path.join(work, "xray-client")
    with open(path, "wb") as out:
        out.write(archive.read(asset["entry"]))
    if sha256(path) != asset["executableSha256"]:
        sys.exit("Xray executable does not match its pin")
    os.chmod(path, 0o755)
    return path, manifest["components"]["xray"]["version"].lstrip("v")


def free_port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def wait_tcp(port, seconds):
    deadline = time.time() + seconds
    while time.time() < deadline:
        try:
            with socket.create_connection(("127.0.0.1", port), timeout=1):
                return
        except OSError:
            time.sleep(0.2)
    sys.exit(f"127.0.0.1:{port} not reachable after {seconds}s")


def docker(*args, check=True):
    return subprocess.run(["docker", *args], check=check, capture_output=True, text=True)


def start_servers(work, run_id, cleanup):
    servers = []
    for version in SERVER_VERSIONS:
        for variant, extra in VARIANTS.items():
            reality = {
                "show": False, "dest": SNI + ":443", "xver": 0, "serverNames": [SNI],
                "privateKey": PRIVATE_KEY, "shortIds": [SHORT_ID], **extra,
            }
            config = {
                "log": {"loglevel": "warning"},
                "dns": {"servers": ["https://1.1.1.1/dns-query", "https://8.8.8.8/dns-query"], "queryStrategy": "UseIPv4"},
                "inbounds": [{
                    "listen": "0.0.0.0", "port": INNER_PORT, "protocol": "vless",
                    "settings": {"clients": [{"id": UUID, "flow": "xtls-rprx-vision"}], "decryption": "none"},
                    "streamSettings": {"network": "tcp", "security": "reality", "realitySettings": reality},
                }],
                "outbounds": [{"protocol": "freedom", "settings": {"domainStrategy": "UseIPv4"}}],
            }
            directory = os.path.join(work, f"server-{version}-{variant}")
            os.makedirs(directory)
            with open(os.path.join(directory, "config.json"), "w", encoding="utf-8") as out:
                json.dump(config, out, indent=1)
            name = f"multicore-reality-lab-{run_id}-{version}-{variant}"
            docker("run", "-d", "--name", name, "-p", f"127.0.0.1::{INNER_PORT}",
                   "-v", f"{directory}:/etc/xray:ro", f"teddysun/xray:{version}")
            cleanup.append(lambda name=name: docker("rm", "-f", name, check=False))
            port_line = docker("port", name, f"{INNER_PORT}/tcp").stdout.strip().splitlines()[0]
            servers.append({"version": version, "variant": variant, "port": int(port_line.rsplit(":", 1)[1])})
    for server in servers:
        wait_tcp(server["port"], 30)
    return servers


def rows_for(servers):
    rows = []
    for server in servers:
        for fingerprint, forced in (("chrome", False), ("chrome", True), ("firefox", False)):
            rows.append({**server, "fingerprint": fingerprint, "forced": forced})
    return rows


def row_name(row):
    name = f"{row['version']}-{row['variant']}-{row['fingerprint']}"
    return name + ("-forced" if row["forced"] else "")


def start_mihomo(binary, keys, rows, work, label, cleanup):
    home = os.path.join(work, f"mihomo-{label}")
    os.makedirs(home)
    proxies, listeners, ports = [], [], []
    for index, row in enumerate(rows):
        options = {"public-key": PUBLIC_KEY, "short-id": SHORT_ID}
        if row["forced"]:
            options["support-x25519mlkem768"] = True
        name = f"p{index}"
        proxies.append({
            "name": name, "type": "vless", "server": "127.0.0.1", "port": row["port"], "uuid": UUID,
            "flow": "xtls-rprx-vision", "tls": True, "servername": SNI, "client-fingerprint": row["fingerprint"],
            "network": "tcp", "udp": True, "reality-opts": options,
        })
        port = free_port()
        ports.append(port)
        listeners.append({"name": "l" + name, "type": "mixed", "listen": "127.0.0.1", "port": port, "proxy": name})
    config = {
        "mixed-port": 0, "log-level": "info", "mode": "global", "ipv6": False,
        "dns": {"enable": True, "nameserver": ["1.1.1.1"]},
        "proxies": proxies, "listeners": listeners, "rules": ["MATCH,DIRECT"], **keys,
    }
    with open(os.path.join(home, "config.yaml"), "w", encoding="utf-8") as out:
        json.dump(config, out, indent=1)  # JSON is valid YAML
    log = open(os.path.join(home, "core.log"), "w", encoding="utf-8")
    process = subprocess.Popen([binary, "-d", home], stdout=log, stderr=subprocess.STDOUT)
    cleanup.append(lambda: (process.kill(), process.wait(), log.close()))
    wait_tcp(ports[-1], 20)
    return ports, os.path.join(home, "core.log")


def start_xray(binary, rows, work, cleanup):
    home = os.path.join(work, "xray-client")
    os.makedirs(home)
    inbounds, outbounds, rules, ports = [], [], [], []
    for index, row in enumerate(rows):
        port = free_port()
        ports.append(port)
        inbounds.append({"tag": f"in{index}", "listen": "127.0.0.1", "port": port, "protocol": "http"})
        outbounds.append({
            "tag": f"out{index}", "protocol": "vless",
            "settings": {"vnext": [{"address": "127.0.0.1", "port": row["port"],
                                    "users": [{"id": UUID, "flow": "xtls-rprx-vision", "encryption": "none"}]}]},
            "streamSettings": {"network": "tcp", "security": "reality", "realitySettings": {
                "serverName": SNI, "fingerprint": row["fingerprint"], "publicKey": PUBLIC_KEY, "shortId": SHORT_ID}},
        })
        rules.append({"type": "field", "inboundTag": [f"in{index}"], "outboundTag": f"out{index}"})
    config = {"log": {"loglevel": "warning"}, "inbounds": inbounds, "outbounds": outbounds, "routing": {"rules": rules}}
    path = os.path.join(home, "config.json")
    with open(path, "w", encoding="utf-8") as out:
        json.dump(config, out, indent=1)
    log = open(os.path.join(home, "xray.log"), "w", encoding="utf-8")
    process = subprocess.Popen([binary, "run", "-c", path], stdout=log, stderr=subprocess.STDOUT)
    cleanup.append(lambda: (process.kill(), process.wait(), log.close()))
    wait_tcp(ports[-1], 20)
    return ports, os.path.join(home, "xray.log")


def probe(port, attempts=3):
    """Whether any of `attempts` HTTPS requests (each a fresh REALITY handshake) returns 204."""
    proxy = f"http://127.0.0.1:{port}"
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({"http": proxy, "https": proxy}))
    for _ in range(attempts):
        try:
            with opener.open(PROBE_URL, timeout=8) as response:
                if response.status == 204:
                    return True
        except Exception:  # noqa: BLE001 - any failure is a failed probe
            pass
    return False


def evaluate(label, rows, ports, model, log_path):
    with concurrent.futures.ThreadPoolExecutor(max_workers=len(rows)) as pool:
        results = list(pool.map(probe, ports))
    mismatches = 0
    for row, got in zip(rows, results):
        want = model(row)
        mark = "  " if got == want else "!!"
        mismatches += got != want
        print(f"{mark} {label:<22} {row_name(row):<34} got={'OK' if got else 'FAIL':<4} want={'OK' if want else 'FAIL'}")
    print(f"== {label}: {sum(results)}/{len(rows)} OK, {mismatches} mismatches")
    if mismatches:
        with open(log_path, encoding="utf-8", errors="replace") as log:
            lines = [line.rstrip() for line in log if "REALITY" in line or "level=error" in line]
        print("\n".join(lines[-40:]))
    return mismatches


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--patched")
    parser.add_argument("--stock")
    parser.add_argument("--fetch-stock", action="store_true")
    parser.add_argument("--xray")
    parser.add_argument("--fetch-xray", action="store_true")
    args = parser.parse_args()
    if shutil.which("docker") is None:
        sys.exit("docker not found")

    work = tempfile.mkdtemp(prefix="multicore-reality-lab-")
    os.chmod(work, 0o755)
    cleanup = []
    try:
        stock = fetch_stock(work) if args.fetch_stock else args.stock
        xray, xray_version = (None, None)
        if args.fetch_xray:
            xray, xray_version = fetch_xray(work)
        elif args.xray:
            xray = args.xray
            xray_version = subprocess.run([xray, "version"], capture_output=True, text=True).stdout.split()[1]
        if not (args.patched or stock or xray):
            sys.exit("nothing to test: pass --patched, --stock/--fetch-stock and/or --xray/--fetch-xray")

        run_id = format(int(time.time() * 1000) % 10**8, "x")
        servers = start_servers(work, run_id, cleanup)
        rows = rows_for(servers)
        scenarios = []
        if args.patched:
            scenarios += [
                ("patched/no-keys", args.patched, {}, expect_patched("auto")),
                ("patched/off", args.patched, {"reality-mlkem": "off"}, expect_patched("off")),
                ("patched/on", args.patched, {"reality-mlkem": "on"}, expect_patched("on")),
            ]
        if stock:
            # The keys MULTICORE may pass through: an unpatched core must ignore them.
            scenarios.append(("stock/with-keys", stock, {"reality-mlkem": "on", "reality-client-version": "26.9.9"}, expect_stock))
        mismatches = 0
        for label, binary, keys, model in scenarios:
            ports, log_path = start_mihomo(binary, keys, rows, work, label.replace("/", "-"), cleanup)
            mismatches += evaluate(label, rows, ports, model, log_path)
            cleanup.pop()()
        if xray:
            xray_rows = [row for row in rows if not row["forced"]]
            ports, log_path = start_xray(xray, xray_rows, work, cleanup)
            mismatches += evaluate(f"xray {xray_version}", xray_rows, ports, expect_xray(xray_version), log_path)
        print("PASS: every cell matches the model" if mismatches == 0 else f"FAIL: {mismatches} cells differ from the model")
        return 1 if mismatches else 0
    finally:
        for step in reversed(cleanup):
            try:
                step()
            except Exception:  # noqa: BLE001 - best-effort teardown
                pass
        shutil.rmtree(work, ignore_errors=True)


if __name__ == "__main__":
    sys.exit(main())
