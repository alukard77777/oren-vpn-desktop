#!/usr/bin/env python3
"""Installed Fedora smoke support; all subscriptions and user data are synthetic.

GUI checks use the actual installed handler and packaged daemon. No application
test overrides, privileged host launch, or Connect action are used.
"""
import argparse
import ctypes
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import re
import secrets
import select
import signal
import stat
import subprocess
import sys
import threading
import time
from urllib.parse import quote, urlsplit
import xml.etree.ElementTree as ET

APP = "/usr/lib/multicore/multicore"
DAEMON = "/usr/lib/multicore/runtime/multicore-daemon"
HOST = "/usr/lib/multicore/runtime/multicore-core-host"
CORES = ("/usr/lib/multicore/cores/mihomo", "/usr/lib/multicore/cores/xray")
ROUTES = ("/sub/cold-decline?token=a%2Fb&next=1", "/sub/warm-approved?token=a%2Fb&next=1")


def install_uri(source):
    url = urlsplit(source)
    if (url.scheme != "http" or url.hostname != "127.0.0.1" or not url.port
            or url.username or url.password or url.fragment
            or url.path not in {"/sub/cold-decline", "/sub/warm-approved"}):
        raise ValueError("Only synthetic loopback subscription URLs are allowed")
    return "multicore://install-sub?url=" + quote(source, safe="")


def subscription_body():
    yaml = ("proxies:\n  - name: Local test node\n    type: socks5\n"
            "    server: 127.0.0.1\n    port: 9\nproxy-groups:\n"
            "  - name: Main\n    type: select\n    proxies: [Local test node]\n"
            "rules:\n  - MATCH,Main\n")
    return json.dumps([yaml, {"outbounds": [{"protocol": "freedom", "tag": "direct"}]}]).encode()


def modal_colors_match(colors):
    # app.slint: centered 420x232 modal; elevated #1c2026, selected Yes
    # fill #4c9dff18 composited over elevated. Test blank insets, never text.
    expected = [(28, 32, 38)] * 7 + [(33, 44, 58)] * 2
    return len(colors) == len(expected) and all(
        all(abs(actual - target) <= 3 for actual, target in zip(color, wanted))
        for color, wanted in zip(colors, expected)
    )


def command(args, timeout=10, required=True, redirect=None):
    result = subprocess.run(args, stdout=redirect if redirect else subprocess.PIPE,
                            stderr=redirect if redirect else subprocess.PIPE,
                            text=True, timeout=timeout, check=False)
    if required and result.returncode:
        # Never format full argv/environment: xauth and the daemon use credentials.
        raise RuntimeError(f"{Path(args[0]).name} failed (exit {result.returncode})")
    return result


def wait_for(predicate, failure, seconds=20):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        value = predicate()
        if value:
            return value
        time.sleep(0.1)
    raise RuntimeError(failure)


def verify_install():
    files = command(["rpm", "-ql", "multicore"]).stdout.splitlines()
    if not files:
        raise RuntimeError("Installed RPM inventory is empty")
    for name in files:
        path = Path(name)
        info = path.lstat()
        mode = stat.S_IMODE(info.st_mode)
        if info.st_uid != 0 or info.st_gid != 0 or mode & 0o6000:
            raise RuntimeError("RPM path has unsafe ownership or set-ID mode")
        if stat.S_ISLNK(info.st_mode):
            continue
        expected = 0o755 if stat.S_ISDIR(info.st_mode) or name in {APP, DAEMON, HOST, *CORES} else 0o644
        if not (stat.S_ISDIR(info.st_mode) or stat.S_ISREG(info.st_mode)) or mode != expected:
            raise RuntimeError("RPM path has an unexpected type or mode")
        if stat.S_ISREG(info.st_mode) and command(["getcap", str(path)]).stdout.strip():
            raise RuntimeError("RPM payload has file capabilities")
    for executable in (APP, DAEMON, HOST, *CORES):
        if executable not in files or stat.S_IMODE(Path(executable).stat().st_mode) != 0o755:
            raise RuntimeError("Installed executable is missing or is not mode 0755")
    launcher = Path("/usr/bin/multicore")
    if not launcher.is_symlink() or str(launcher.resolve()) != APP or str(launcher) not in files:
        raise RuntimeError("Installed launcher does not resolve to the private runtime")
    desktop = Path("/usr/share/applications/multicore.desktop")
    fields = desktop.read_text().splitlines()
    if "Exec=multicore %u" not in fields or "MimeType=x-scheme-handler/multicore;" not in fields:
        raise RuntimeError("Installed desktop URI contract differs")
    policy = Path("/usr/share/polkit-1/actions/io.github.fatyzzz.multicore.core-host.policy")
    actions = ET.parse(policy).getroot().findall("action")
    if len(actions) != 1 or actions[0].get("id") != "io.github.fatyzzz.multicore.core-host":
        raise RuntimeError("Unexpected polkit action")
    action = actions[0]
    annotation = action.find("annotate[@key='org.freedesktop.policykit.exec.path']")
    if annotation is None or annotation.text != HOST:
        raise RuntimeError("Polkit action does not target the installed core host")
    for name, value in (("allow_any", "auth_admin"), ("allow_inactive", "auth_admin"),
                        ("allow_active", "auth_admin_keep")):
        if action.findtext("defaults/" + name) != value:
            raise RuntimeError("Unexpected polkit authorization default")
    required = [str(desktop), str(policy), "/usr/share/icons/hicolor/scalable/apps/multicore.svg"]
    required += [str(path) for path in Path("/usr/share/licenses/multicore").glob("**/*") if path.is_file()]
    if len(required) <= 3 or any(name not in files for name in required):
        raise RuntimeError("Missing package-owned metadata or licenses")
    print("PASS: installed inventory, ownership/modes/capabilities, desktop and polkit metadata", flush=True)


def process_identity(pid):
    """Start time prevents sending a later signal to a recycled PID."""
    try:
        process = Path("/proc") / str(pid)
        fields = (process / "stat").read_text().rsplit(")", 1)[1].split()
        if fields[0] == "Z":
            return None
        return pid, fields[19], process.stat().st_uid
    except (OSError, IndexError):
        return None


def home_processes(uid, home):
    result = {}
    marker = ("HOME=" + home).encode()
    for entry in Path("/proc").iterdir():
        if not entry.name.isdigit():
            continue
        pid = int(entry.name)
        identity = process_identity(pid)
        if not identity or identity[2] != uid:
            continue
        try:
            if marker not in (entry / "environ").read_bytes().split(b"\0"):
                continue
            result[identity] = os.readlink(entry / "exe")
        except OSError:
            continue
    return result


def stop_processes(records):
    records = set(records)
    for sig, seconds in ((signal.SIGTERM, 4), (signal.SIGKILL, 2)):
        for identity in records:
            if process_identity(identity[0]) == identity:
                try:
                    os.kill(identity[0], sig)
                except ProcessLookupError:
                    pass
        deadline = time.monotonic() + seconds
        while time.monotonic() < deadline:
            records = {item for item in records if process_identity(item[0]) == item}
            if not records:
                return
            time.sleep(0.1)
    if records:
        raise RuntimeError("An owned process did not stop")


def cleanup_user(uid, home):
    # This is solely an emergency cleanup for the shell's freshly allocated user.
    if (os.geteuid() != 0 or uid <= 0 or os.environ.get("MULTICORE_DISPOSABLE_RPM_TEST") != "1"
            or not (Path("/.dockerenv").is_file() or Path("/run/.containerenv").is_file())
            or not re.fullmatch(r"/tmp/multicore-rpm-smoke\.[A-Za-z0-9]+/home", home)
            or Path(home).stat().st_uid != uid):
        raise RuntimeError("Refusing cleanup outside the isolated test user/home")
    stop_processes(home_processes(uid, home))
    if home_processes(uid, home):
        raise RuntimeError("The isolated test user still has live processes")


class WindowPixels:
    """Read Xvfb's 24-bit window pixels without a screenshot/image dependency."""

    def __init__(self):
        self.x = ctypes.CDLL("libX11.so.6")
        self.x.XOpenDisplay.argtypes = [ctypes.c_char_p]
        self.x.XOpenDisplay.restype = ctypes.c_void_p
        self.x.XGetImage.argtypes = [ctypes.c_void_p, ctypes.c_ulong, ctypes.c_int, ctypes.c_int,
                                    ctypes.c_uint, ctypes.c_uint, ctypes.c_ulong, ctypes.c_int]
        self.x.XGetImage.restype = ctypes.c_void_p
        self.x.XGetPixel.argtypes = [ctypes.c_void_p, ctypes.c_int, ctypes.c_int]
        self.x.XGetPixel.restype = ctypes.c_ulong
        self.x.XDestroyImage.argtypes = [ctypes.c_void_p]
        self.x.XCloseDisplay.argtypes = [ctypes.c_void_p]
        self.display = self.x.XOpenDisplay(None)
        if not self.display:
            raise RuntimeError("Cannot open isolated Xvfb display")

    def capture(self, window, output=None):
        geometry = command(["xdotool", "getwindowgeometry", "--shell", str(window)]).stdout
        fields = dict(line.split("=", 1) for line in geometry.splitlines() if "=" in line)
        width, height = int(fields["WIDTH"]), int(fields["HEIGHT"])
        if width < 700 or height < 600 or width > 1280 or height > 900:
            return False
        bitmap = self.x.XGetImage(self.display, window, 0, 0, width, height,
                                 ctypes.c_ulong(-1).value, 2)
        if not bitmap:
            raise RuntimeError("Xvfb window capture failed")
        try:
            def pixel(x, y):
                value = self.x.XGetPixel(bitmap, x, y)
                return (value >> 16) & 255, (value >> 8) & 255, value & 255
            cx, cy = width // 2, height // 2
            points = [(-198, -70), (198, -70), (-198, 0), (198, 0),
                      (-198, 70), (198, 70), (0, -108), (110, 60), (174, 60)]
            modal = modal_colors_match([pixel(cx + x, cy + y) for x, y in points])
            if output:
                data = bytearray()
                for y in range(height):
                    for x in range(width):
                        data.extend(pixel(x, y))
                Path(output).write_bytes(f"P6\n{width} {height}\n255\n".encode() + data)
            return modal
        finally:
            self.x.XDestroyImage(bitmap)

    def close(self):
        if self.display:
            self.x.XCloseDisplay(self.display)
            self.display = None


def run_session(evidence):
    import resource

    uid, home = os.geteuid(), os.environ["HOME"]
    runtime = Path(os.environ["XDG_RUNTIME_DIR"])
    if uid == 0 or runtime.stat().st_uid != uid or stat.S_IMODE(runtime.stat().st_mode) != 0o700:
        raise RuntimeError("GUI must use a nonroot user and a private XDG_RUNTIME_DIR")
    evidence.mkdir(mode=0o700, parents=True, exist_ok=True)
    if set(home_processes(uid, home).values()) & {APP, DAEMON}:
        raise RuntimeError("An application already exists in the isolated test home")
    for name in os.environ:
        if name.startswith("MULTICORE_"):
            raise RuntimeError("Application overrides are not allowed in the installed smoke")
    # Bound diagnostic output inherited by the GUI/Xvfb without limiting dnf's
    # package downloads. The synthetic profile and each screenshot are much smaller.
    resource.setrlimit(resource.RLIMIT_FSIZE, (16 * 1024 * 1024, 16 * 1024 * 1024))
    requests = []
    body = subscription_body()

    class Feed(BaseHTTPRequestHandler):
        def do_GET(self):
            requests.append(("GET", self.path))
            accepted = self.path in ROUTES
            self.send_response(200 if accepted else 404)
            self.send_header("Content-Type", "application/json")
            self.send_header("profile-title", "Fedora synthetic deep link smoke")
            self.send_header("Content-Length", str(len(body) if accepted else 0))
            self.end_headers()
            if accepted:
                self.wfile.write(body)

        def do_POST(self):
            requests.append(("POST", self.path))
            self.send_error(405)

        def log_message(self, *_):
            pass

        def setup(self):
            super().setup()
            self.connection.settimeout(5)

    feed = ThreadingHTTPServer(("127.0.0.1", 0), Feed)
    threading.Thread(target=feed.serve_forever, daemon=True).start()
    xvfb = None
    pixels = None
    window = None
    xlog = (evidence / "xvfb.log").open("wb")
    app_log = (evidence / "desktop.log").open("wb")
    # Keep the display credential outside the uploaded evidence, including on a
    # hard timeout that prevents the finally block from running.
    auth = runtime / "smoke.Xauthority"
    try:
        auth.touch(mode=0o600)
        cookie = secrets.token_hex(16)
        command(["xauth", "-f", str(auth), "add", ":0", ".", cookie])
        reader, writer = os.pipe()
        try:
            xvfb = subprocess.Popen(["Xvfb", "-displayfd", str(writer), "-screen", "0", "1280x900x24",
                                     "-nolisten", "tcp", "-noreset", "-auth", str(auth)],
                                    pass_fds=(writer,), stdout=xlog, stderr=xlog)
        finally:
            os.close(writer)
        try:
            if not select.select([reader], [], [], 10)[0]:
                raise RuntimeError("Xvfb did not allocate a display")
            number = os.read(reader, 32).decode().strip()
            if not number.isdigit():
                raise RuntimeError("Xvfb display allocation failed")
        finally:
            os.close(reader)
        os.environ["DISPLAY"] = ":" + number
        os.environ["XAUTHORITY"] = str(auth)
        command(["xauth", "-f", str(auth), "add", os.environ["DISPLAY"], ".", cookie])
        pixels = WindowPixels()
        handler = command(["xdg-mime", "query", "default", "x-scheme-handler/multicore"]).stdout.strip()
        if handler != "multicore.desktop":
            raise RuntimeError("Installed multicore scheme handler is not discoverable")

        def app_processes():
            return {identity: exe for identity, exe in home_processes(uid, home).items()
                    if exe in {APP, DAEMON, HOST, *CORES}}

        def singleton_pair():
            processes = app_processes()
            desktops = [key for key, exe in processes.items() if exe == APP]
            daemons = [key for key, exe in processes.items() if exe == DAEMON]
            if any(exe == HOST or exe in CORES for exe in processes.values()):
                raise RuntimeError("The non-live smoke unexpectedly launched a network core or privileged host")
            return (desktops[0], daemons[0]) if len(desktops) == len(daemons) == 1 else None

        def find_window(desktop):
            matches = command(["xdotool", "search", "--onlyvisible", "--name", "^MultiCore$"], required=False)
            for value in matches.stdout.split():
                owner = command(["xdotool", "getwindowpid", value], required=False)
                if owner.returncode == 0 and owner.stdout.strip() == str(desktop[0]):
                    return int(value)
            return None

        data = Path(os.environ["XDG_DATA_HOME"]) / "multicore"

        def saved_sources():
            return [json.loads(path.read_text())["source_url"]
                    for path in data.rglob("subscription.json")]

        def assert_no_import():
            if requests or saved_sources():
                raise RuntimeError("A subscription was fetched or persisted before consent")

        base = f"http://127.0.0.1:{feed.server_port}"
        # The launched GUI inherits the descriptors. Real files prevent waiting
        # for EOF on a pipe held open by that intentionally long-lived process.
        command(["gio", "open", install_uri(base + ROUTES[0])], redirect=app_log)
        desktop, daemon = wait_for(singleton_pair, "Packaged desktop/daemon did not remain alive")
        window = wait_for(lambda: find_window(desktop), "Installed desktop handler did not open its window")
        command(["xdotool", "windowsize", str(window), "1000", "760"])
        command(["xdotool", "windowmove", str(window), "0", "0"])

        def wait_modal(name):
            wait_for(lambda: pixels.capture(window), "Consent modal is not visually present: " + name)
            wait_for(lambda: singleton_pair() == (desktop, daemon),
                     "URI activation did not retain the original desktop/daemon")
            pixels.capture(window, evidence / (name + ".ppm"))
            time.sleep(0.5)
            assert_no_import()

        def consent(approve):
            if not pixels.capture(window):
                raise RuntimeError("Refusing keyboard input without a verified consent modal")
            command(["xdotool", "windowfocus", "--sync", str(window)])
            if command(["xdotool", "getwindowfocus"]).stdout.strip() != str(window):
                raise RuntimeError("Refusing keyboard input outside the owned window")
            keys = ["Tab", "Return"] if approve else ["Return"]
            command(["xdotool", "key", "--clearmodifiers", "--delay", "150", *keys])
            wait_for(lambda: not pixels.capture(window), "Consent action did not close the modal")

        wait_modal("cold-consent")
        consent(False)
        time.sleep(0.7)
        assert_no_import()
        print("PASS: cold installed gio dispatch, visible consent, No, zero subscription requests", flush=True)

        approved = base + ROUTES[1]
        command(["gio", "open", install_uri(approved)], redirect=app_log)
        wait_modal("warm-consent")
        consent(True)

        def imported():
            index = data / "profiles" / "index.json"
            if not index.exists() or not json.loads(index.read_text())["profiles"]:
                return False
            # The authoritative index is published after the snapshot files.
            sources = saved_sources()
            if sources != [approved]:
                raise RuntimeError("Persisted source differs from the approved synthetic URI")
            return len(json.loads(index.read_text())["profiles"]) == 1

        wait_for(imported, "Yes did not persist one profile and its exact subscription source")
        if not requests or any(item != ("GET", ROUTES[1]) for item in requests):
            raise RuntimeError("Unexpected subscription request or URI decoding")
        deadline = time.monotonic() + 2
        while time.monotonic() < deadline:
            if singleton_pair() != (desktop, daemon):
                raise RuntimeError("Installed desktop/daemon stopped after import")
            time.sleep(0.1)
        pixels.capture(window, evidence / "imported-profile.ppm")
        (evidence / "result.json").write_text(json.dumps({
            "handler": handler, "desktop_uid": uid, "daemon_uid": uid,
            "cold_decline_requests": 0, "warm_accept_requests": len(requests),
            "persisted_profiles": 1, "exact_synthetic_source": True, "connected": False,
        }, indent=2) + "\n")
        print("PASS: warm installed gio dispatch, Yes, exact loopback fetch/source persistence, one live desktop/daemon", flush=True)
    except BaseException:
        if pixels and window:
            try:
                pixels.capture(window, evidence / "failure.ppm")
            except Exception:
                pass
        raise
    finally:
        # Only this fresh user's private HOME and installed executable paths are
        # eligible; signal individual PIDs after rechecking their start times.
        try:
            owned = {key for key, exe in home_processes(uid, home).items()
                     if exe in {APP, DAEMON, HOST, *CORES}}
            stop_processes(owned)
        finally:
            if pixels:
                pixels.close()
            if xvfb and xvfb.poll() is None:
                xvfb.terminate()
                try:
                    xvfb.wait(timeout=4)
                except subprocess.TimeoutExpired:
                    xvfb.kill()
                    xvfb.wait(timeout=2)
            xlog.close()
            app_log.close()
            feed.shutdown()
            feed.server_close()
            # The X cookie is only needed while the test display is alive.
            auth.unlink(missing_ok=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument("--verify-install", action="store_true")
    mode.add_argument("--session", action="store_true")
    mode.add_argument("--cleanup-user", type=int)
    parser.add_argument("--test-home")
    parser.add_argument("--evidence", type=Path)
    args = parser.parse_args()
    if args.verify_install:
        verify_install()
    elif args.session:
        if args.evidence is None:
            parser.error("--session requires --evidence")

        def interrupted(*_):
            raise RuntimeError("GUI smoke interrupted or timed out")

        signal.signal(signal.SIGTERM, interrupted)
        run_session(args.evidence)
    else:
        if not args.test_home:
            parser.error("--cleanup-user requires --test-home")
        cleanup_user(args.cleanup_user, args.test_home)


if __name__ == "__main__":
    try:
        main()
    except Exception as error:
        # CalledProcessError/TimeoutExpired contain full argv; avoid those dumps.
        message = "command timed out" if isinstance(error, subprocess.TimeoutExpired) else str(error)
        print("FAIL: " + message, file=sys.stderr)
        sys.exit(1)
