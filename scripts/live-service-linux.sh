#!/usr/bin/env bash
# Live test of the always-ready MultiCore service on Linux (deb / rpm).
#
# DESTRUCTIVE: installs and removes the multicore package and starts/stops multicore.service.
# Run it on a disposable host: WSL Debian with systemd, or the Fedora systemd container below.
#
# Usage:
#   live-service-linux.sh run <package.deb|package.rpm> [options]
#       On a systemd host (PID 1 = systemd), as a normal user with passwordless sudo. Installs the
#       package, checks the unit and the socket, drives the daemon, removes the package.
#   live-service-linux.sh fedora-container <package.rpm> [options]
#       On a Docker host: builds a Fedora 43 image with systemd, boots it as a privileged
#       container, runs "run" inside as the user "tester" and copies the results out.
#
# Options (run):
#   --subscription URL   import URL instead of the built-in local fixture (Mihomo-only, DIRECT)
#   --traffic URL|none   fetched while connected (default https://www.gstatic.com/generate_204)
#   --keep-installed     skip the removal phase
#   --out DIR            results directory (default ./live-service-linux-result)
#
# The service only answers a daemon started from /usr/lib/multicore/runtime/multicore-daemon by a
# uid that owns an active or online logind session. Without one (plain `wsl -e bash`, docker exec)
# this script opens a holder session with `systemd-run -p PAMName=login` (needs pam_systemd, e.g.
# Debian's libpam-systemd) and closes it at the end.
#
# Result: <out>/live-service-result.json, one {"check","ok","detail"} per step; exit 1 on failure.
set -euo pipefail

SELF="$(realpath "${BASH_SOURCE[0]}")"
DAEMON=/usr/lib/multicore/runtime/multicore-daemon
SOCKET=/run/multicore/service.sock
UNIT=multicore.service

usage() { sed -n '2,27p' "$SELF" >&2; exit 2; }

# ------------------------------------------------------------------ fedora-container (Docker host)

fedora_container() {
    [[ $# -ge 1 && "$1" == *.rpm && -f "$1" ]] || usage
    local rpm name image out
    rpm="$(realpath "$1")"; shift
    image=multicore-live-fedora:43
    name="multicore-live-fedora-$$"
    out="$PWD/live-service-linux-result-fedora"
    docker build -t "$image" - <<'DOCKERFILE'
FROM fedora:43
RUN dnf -y install systemd systemd-pam dbus-broker sudo procps-ng iproute curl python3 findutils util-linux shadow-utils \
 && dnf clean all \
 && useradd -m tester \
 && echo 'tester ALL=(ALL) NOPASSWD: ALL' >/etc/sudoers.d/tester \
 && chmod 0440 /etc/sudoers.d/tester
STOPSIGNAL SIGRTMIN+3
CMD ["/sbin/init"]
DOCKERFILE
    docker run -d --name "$name" --privileged --cgroupns=host \
        -v /sys/fs/cgroup:/sys/fs/cgroup:rw --tmpfs /run --tmpfs /run/lock --tmpfs /tmp \
        "$image" >/dev/null
    # shellcheck disable=SC2064
    trap "docker rm -f '$name' >/dev/null 2>&1 || true" EXIT
    local state=''
    for _ in $(seq 60); do
        state="$(docker exec "$name" systemctl is-system-running 2>/dev/null || true)"
        [[ "$state" == running || "$state" == degraded ]] && break
        sleep 1
    done
    [[ "$state" == running || "$state" == degraded ]] || { echo "FAIL: container systemd is '$state'" >&2; exit 1; }
    docker cp "$rpm" "$name:/home/tester/$(basename "$rpm")"
    docker cp "$SELF" "$name:/home/tester/live-service-linux.sh"
    docker exec "$name" chown tester:tester "/home/tester/$(basename "$rpm")" /home/tester/live-service-linux.sh
    local status=0
    docker exec -u tester -e HOME=/home/tester -w /home/tester "$name" \
        bash /home/tester/live-service-linux.sh run "/home/tester/$(basename "$rpm")" --out /home/tester/result "$@" || status=$?
    rm -rf "$out"
    docker cp "$name:/home/tester/result" "$out" || true
    echo "results: $out"
    exit "$status"
}

# ------------------------------------------------------------------ run (systemd host)

OUT="$PWD/live-service-linux-result"
SUBSCRIPTION=''
TRAFFIC='https://www.gstatic.com/generate_204'
KEEP_INSTALLED=0
PACKAGE=''
RESULTS=''
TOKEN=''
BASE=''
DAEMON_PID=''
FIXTURE_PID=''
REVISION_FILE=''
HOLDER_UNIT=''
DATA=''
BACKUP=''
WARM_PIDS=''
NODES_BEFORE=0

log() { printf '%s %s\n' "$(date +%T)" "$*" | tee -a "$OUT/driver.log" >&2; }

record() { # name ok(0|1) detail
    python3 -c 'import json,sys; print(json.dumps({"check":sys.argv[1],"ok":sys.argv[2]=="1","detail":sys.argv[3]}))' \
        "$1" "$2" "$3" >>"$RESULTS"
    if [[ "$2" == 1 ]]; then log "PASS $1: $3"; else log "FAIL $1: $3"; fi
}

# check NAME FUNCTION [ARGS...]: the function prints its detail and returns non-zero on failure.
# It runs in this shell (not a subshell) so the state it sets (daemon pid, token, warm pids)
# survives. Functions below never rely on `set -e` (it is off inside `if`): every step checks.
check() {
    local name="$1"; shift
    local output="$OUT/.check-output" status=0
    if "$@" >"$output" 2>&1; then status=1; fi
    local detail; detail="$(tr '\n' ' ' <"$output" | sed 's/ *$//')"
    record "$name" "$status" "$detail"
    [[ "$status" == 1 ]]
}

finish_results() {
    python3 - "$RESULTS" "$OUT/live-service-result.json" <<'PY'
import json, sys
rows = [json.loads(line) for line in open(sys.argv[1]) if line.strip()]
json.dump(rows, open(sys.argv[2], "w"), indent=2)
failed = [row["check"] for row in rows if not row["ok"]]
print(f"{len(rows) - len(failed)}/{len(rows)} checks passed" + (f"; failed: {', '.join(failed)}" if failed else ""))
PY
}

jget() { # python expression over d (parsed JSON on stdin)
    python3 -c 'import json,sys
d=json.load(sys.stdin)
v=eval(sys.argv[1])
print(json.dumps(v) if not isinstance(v,str) else v)' "$1"
}

api() { # METHOD PATH [BODY] -> body on stdout, non-zero on a non-2xx answer
    local method="$1" path="$2" body="${3-}" code tmp
    tmp="$(mktemp)"
    local args=(-sS --noproxy '*' --max-time 120 -X "$method" -H "Authorization: Bearer $TOKEN" -o "$tmp" -w '%{http_code}')
    [[ -n "$body" ]] && args+=(-H 'Content-Type: application/json' --data "$body")
    code="$(curl "${args[@]}" "$BASE$path" || echo 000)"
    local text; text="$(cat "$tmp")"; rm -f "$tmp"
    if [[ "$code" != 2* ]]; then echo "$method $path -> $code $text"; return 1; fi
    printf '%s' "$text"
}

status_field() { local s; s="$(api GET /v1/status)" || return 1; jget "d.get('$1')" <<<"$s"; }

wait_until() { # SECONDS DESCRIPTION COMMAND...
    local seconds="$1" what="$2"; shift 2
    local deadline=$((SECONDS + seconds))
    while ((SECONDS < deadline)); do
        if "$@" >/dev/null 2>&1; then return 0; fi
        sleep 0.5
    done
    echo "timed out after ${seconds}s waiting for $what"
    return 1
}

service_core_pids() { { pgrep -u 0 -x mihomo || true; pgrep -u 0 -x xray || true; } | sort -n | paste -sd, -; }
cores_running() { [[ -n "$(service_core_pids)" ]]; }
cores_stopped() { [[ -z "$(service_core_pids)" ]]; }
is_connected() { [[ "$(status_field state)" == connected ]]; }
is_disconnected_warm() { [[ "$(status_field state)" != connected && "$(status_field state)" != connecting && "$(status_field cores_ready)" == true ]]; }
is_ready() { [[ "$(status_field cores_ready)" == true ]]; }
tun_up() { ip -o link show dev MultiCore 2>/dev/null | grep -Eq '[<,]UP[,>]'; }
tun_down() { ! tun_up; }
pids_changed_from() { local now; now="$(service_core_pids)"; [[ -n "$now" && "$now" != "$1" ]] && is_ready; }
free_port() { python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1])'; }

session_state() { loginctl show-user "$(id -u)" -p State --value 2>/dev/null || true; }
session_ok() { [[ "$(session_state)" == active || "$(session_state)" == online ]]; }

start_fixture() {
    REVISION_FILE="$OUT/fixture-revision"
    echo 1 >"$REVISION_FILE"
    cat >"$OUT/fixture.py" <<'PY'
import http.server, sys
revision_file = sys.argv[1]
class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        revision = int(open(revision_file).read().strip() or 1)
        nodes = ["live-direct-a"] + (["live-direct-b"] if revision >= 2 else [])
        lines = ["mode: rule", "proxies:"]
        lines += ["  - {name: %s, type: direct, udp: true}" % node for node in nodes]
        lines += ["proxy-groups:", "  - {name: LiveTest, type: select, proxies: [%s]}" % ", ".join(nodes)]
        lines += ["rules:", "  - MATCH,LiveTest"]
        body = ("\n".join(lines) + "\n").encode()
        self.send_response(200)
        self.send_header("Content-Type", "text/yaml; charset=utf-8")
        self.send_header("Profile-Title", "MultiCore live test")
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Connection", "close")
        self.end_headers()
        self.wfile.write(body)
    def log_message(self, *args):
        pass
server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
print(server.server_address[1], flush=True)
server.serve_forever()
PY
    python3 "$OUT/fixture.py" "$REVISION_FILE" >"$OUT/fixture.port" 2>"$OUT/fixture.err" &
    FIXTURE_PID=$!
    wait_until 10 'fixture port' test -s "$OUT/fixture.port" || return 1
    SUBSCRIPTION="http://127.0.0.1:$(head -n1 "$OUT/fixture.port")/sub/live-test"
    echo "$SUBSCRIPTION"
}

start_daemon() {
    [[ -x "$DAEMON" ]] || { echo "missing $DAEMON"; return 1; }
    TOKEN="$(head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n')"
    local stamp; stamp="$(date +%H%M%S%N)"
    local ready="$OUT/daemon-$stamp.stdout" errors="$OUT/daemon-$stamp.stderr"
    # Exactly how the desktop starts it; the service resolves the staged generation from this
    # uid's data root (XDG_DATA_HOME from the daemon's environment, else ~/.local/share).
    MULTICORE_DAEMON_TOKEN="$TOKEN" MULTICORE_DAEMON_ADDR=127.0.0.1:0 MULTICORE_DAEMON_DATA_DIR="$DATA" \
        MULTICORE_DAEMON_READY_STDOUT=1 MULTICORE_MIHOMO_CONTROLLER_ADDR="127.0.0.1:$(free_port)" \
        "$DAEMON" >"$ready" 2>"$errors" &
    DAEMON_PID=$!
    local line=''
    for _ in $(seq 300); do
        line="$(head -n1 "$ready" 2>/dev/null || true)"
        [[ -n "$line" ]] && break
        kill -0 "$DAEMON_PID" 2>/dev/null || { echo "daemon exited: $(head -c 2000 "$errors")"; return 1; }
        sleep 0.1
    done
    [[ "$line" =~ ^MULTICORE_READY\ (127\.0\.0\.1:[0-9]+)$ ]] || { echo "invalid readiness line '$line'"; return 1; }
    BASE="http://${BASH_REMATCH[1]}"
    echo "pid $DAEMON_PID at $BASE"
}

stop_daemon() { # SIGNAL
    [[ -n "$DAEMON_PID" ]] || return 0
    kill "-$1" "$DAEMON_PID" 2>/dev/null || true
    for _ in $(seq 100); do kill -0 "$DAEMON_PID" 2>/dev/null || break; sleep 0.1; done
    wait "$DAEMON_PID" 2>/dev/null || true
    DAEMON_PID=''
}

save_evidence() {
    local label="$1"
    api GET '/v1/events?after=0' >"$OUT/$label-events.json" 2>/dev/null || true
    api GET /v1/diagnostics >"$OUT/$label-diagnostics.json" 2>/dev/null || true
}

# ---------------------------------------------------------------- checks

c_preflight() {
    [[ "$(ps -p 1 -o comm=)" == systemd ]] || { echo 'PID 1 is not systemd (WSL: [boot] systemd=true in /etc/wsl.conf, then wsl --shutdown)'; return 1; }
    sudo -n true 2>/dev/null || { echo 'passwordless sudo is required'; return 1; }
    local tool
    for tool in curl python3 ip pgrep loginctl systemctl systemd-run stat; do
        command -v "$tool" >/dev/null || { echo "missing tool: $tool"; return 1; }
    done
    [[ "$(id -u)" != 0 ]] || { echo 'run as a normal user, not root'; return 1; }
    ! systemctl cat "$UNIT" >/dev/null 2>&1 || { echo "$UNIT already exists: use a clean host"; return 1; }
    ! pgrep -x mihomo >/dev/null || { echo 'mihomo is already running'; return 1; }
    echo "user $(id -un), $(. /etc/os-release && echo "$PRETTY_NAME")"
}

c_install() {
    case "$PACKAGE" in
        *.deb) sudo DEBIAN_FRONTEND=noninteractive apt-get install -y "$PACKAGE" >"$OUT/install.log" 2>&1 || { tail -n 20 "$OUT/install.log"; return 1; } ;;
        *.rpm) sudo dnf install -y "$PACKAGE" >"$OUT/install.log" 2>&1 || { tail -n 20 "$OUT/install.log"; return 1; } ;;
    esac
    echo "installed $(basename "$PACKAGE")"
}

c_unit_enabled() {
    local enabled; enabled="$(systemctl is-enabled "$UNIT" 2>&1 || true)"
    [[ "$enabled" == enabled ]] || { echo "is-enabled: $enabled"; return 1; }
    wait_until 30 "$UNIT active" systemctl is-active --quiet "$UNIT" || { systemctl status "$UNIT" --no-pager 2>&1 | tail -n 15; return 1; }
    echo 'enabled, active'
}

c_unit_hardening() {
    local show; show="$(systemctl show "$UNIT" -p ExecStart -p User -p ProtectSystem -p ProtectHome -p NoNewPrivileges -p CapabilityBoundingSet)"
    grep -q 'multicore-core-host --service' <<<"$show" || { echo "ExecStart: $show"; return 1; }
    grep -qx 'ProtectSystem=strict' <<<"$show" || { echo 'ProtectSystem is not strict'; return 1; }
    grep -qx 'ProtectHome=read-only' <<<"$show" || { echo 'ProtectHome is not read-only'; return 1; }
    grep -qx 'NoNewPrivileges=yes' <<<"$show" || { echo 'NoNewPrivileges is not yes'; return 1; }
    local caps; caps="$(sed -n 's/^CapabilityBoundingSet=//p' <<<"$show")"
    grep -qw cap_net_admin <<<"$caps" || { echo "no cap_net_admin: $caps"; return 1; }
    ! grep -qw cap_sys_admin <<<"$caps" || { echo "cap_sys_admin in the bounding set: $caps"; return 1; }
    echo "caps: $caps"
}

c_socket_perms() {
    wait_until 15 "$SOCKET" test -S "$SOCKET" || return 1
    local socket dir state
    socket="$(stat -c '%a %U %F' "$SOCKET")"
    dir="$(stat -c '%a %U' /run/multicore)"
    state="$(sudo stat -c '%a %U' /var/lib/multicore)"
    [[ "$socket" == '666 root socket' ]] || { echo "socket: $socket"; return 1; }
    [[ "$dir" == '755 root' ]] || { echo "/run/multicore: $dir"; return 1; }
    [[ "$state" == '700 root' ]] || { echo "/var/lib/multicore: $state"; return 1; }
    echo "socket $socket, run dir $dir, state dir $state"
}

c_session() {
    if session_ok; then echo "existing logind state: $(session_state)"; return 0; fi
    HOLDER_UNIT="multicore-live-session-$$"
    sudo systemd-run --quiet --unit="$HOLDER_UNIT" --uid="$(id -u)" --gid="$(id -g)" \
        -p PAMName=login -p Type=simple /bin/sleep 3600 || { echo 'systemd-run failed'; return 1; }
    wait_until 20 'a logind session' session_ok || { echo "logind state stays '$(session_state)' (is pam_systemd installed?)"; return 1; }
    echo "holder session $HOLDER_UNIT, logind state $(session_state)"
}

c_no_cores_before_daemon() { cores_stopped || { echo "service cores already running: $(service_core_pids)"; return 1; }; echo idle; }

c_service_mode() {
    local mode; mode="$(status_field service_mode)" || { echo "$mode"; return 1; }
    [[ "$mode" == true ]] || { echo 'daemon did not select the service (service_mode=false)'; return 1; }
    echo 'service_mode=true'
}

c_import() {
    local body; body="$(python3 -c 'import json,sys; print(json.dumps({"url":sys.argv[1]}))' "$SUBSCRIPTION")"
    local status; status="$(api POST /v1/subscriptions/import "$body")" || { echo "$status"; return 1; }
    echo "state=$(jget "d['state']" <<<"$status")"
}

c_cores_warm() {
    wait_until 90 'cores_ready' is_ready || return 1
    [[ "$(status_field state)" != connected ]] || { echo 'connected on its own'; return 1; }
    WARM_PIDS="$(service_core_pids)"
    [[ -n "$WARM_PIDS" ]] || { echo 'cores_ready but no root mihomo/xray'; return 1; }
    local user_cores; user_cores="$(pgrep -u "$(id -u)" -x mihomo || true)"
    [[ -z "$user_cores" ]] || { echo "user-owned mihomo: $user_cores"; return 1; }
    echo "root cores $WARM_PIDS"
}

c_catalog() {
    local catalog; catalog="$(api GET /v1/catalog)" || { echo "$catalog"; return 1; }
    NODES_BEFORE="$(jget 'sum(len(g["nodes"]) for g in d["groups"])' <<<"$catalog")"
    ((NODES_BEFORE >= 1)) || { echo 'catalog has no nodes'; return 1; }
    echo "nodes=$NODES_BEFORE"
}

c_latency() {
    local response; response="$(api POST /v1/latencies '{}')" || { echo "$response"; return 1; }
    local count; count="$(jget 'len(d["entries"])' <<<"$response")"
    ((count >= 1)) || { echo 'no latency entries'; return 1; }
    [[ "$(status_field state)" != connected ]] || { echo 'latency probe connected the tunnel'; return 1; }
    jget '" ".join("%s:%s" % (e["status"], e["latency_ms"]) for e in d["entries"])' <<<"$response"
}

c_dashboard() {
    local response url; response="$(api GET /v1/dashboard-url)" || { echo "$response"; return 1; }
    url="$(jget "d['url']" <<<"$response")"
    [[ "$url" =~ ^http://127\.0\.0\.1:([0-9]+)/ui/.*secret=([0-9a-f]+) ]] || { echo 'unexpected dashboard URL shape'; return 1; }
    local port="${BASH_REMATCH[1]}" secret="${BASH_REMATCH[2]}" code
    code="$(curl -sS --noproxy '*' -o /dev/null -w '%{http_code}' -H "Authorization: Bearer $secret" "http://127.0.0.1:$port/version")"
    [[ "$code" == 200 ]] || { echo "GET /version -> $code"; return 1; }
    code="$(curl -sS --noproxy '*' -o /dev/null -w '%{http_code}' -X PATCH -H "Authorization: Bearer $secret" --data '{"mode":"global"}' "http://127.0.0.1:$port/configs")"
    [[ "$code" == 403 ]] || { echo "PATCH /configs -> $code"; return 1; }
    code="$(curl -sS --noproxy '*' -o /dev/null -w '%{http_code}' -X POST -H "Authorization: Bearer $secret" --data '{}' "http://127.0.0.1:$port/upgrade")"
    [[ "$code" == 403 ]] || { echo "POST /upgrade -> $code"; return 1; }
    echo 'GET /version=200 PATCH /configs=403 POST /upgrade=403'
}

c_connect() {
    local started=$SECONDS response
    response="$(api POST /v1/connect)" || { echo "$response"; return 1; }
    wait_until 60 'state=connected' is_connected || return 1
    ! pgrep -x pkexec >/dev/null || { echo 'a pkexec prompt is running (fallback instead of the service)'; return 1; }
    local now; now="$(service_core_pids)"
    [[ "$now" == "$WARM_PIDS" ]] || { echo "cores restarted on connect: $WARM_PIDS -> $now"; return 1; }
    wait_until 30 'MultiCore link up' tun_up || return 1
    echo "connected in $((SECONDS - started))s without pkexec, warm cores reused, MultiCore up"
}

c_traffic() {
    local code; code="$(curl -sS -o /dev/null -w '%{http_code}' --max-time 20 "$TRAFFIC" || echo 000)"
    [[ "$code" =~ ^[23] ]] || { echo "HTTP $code"; return 1; }
    echo "HTTP $code"
}

c_disconnect_keeps_cores() {
    local response; response="$(api POST /v1/disconnect)" || { echo "$response"; return 1; }
    wait_until 30 'disconnected and warm' is_disconnected_warm || return 1
    local now; now="$(service_core_pids)"
    [[ "$now" == "$WARM_PIDS" ]] || { echo "cores changed on disconnect: $WARM_PIDS -> $now"; return 1; }
    wait_until 30 'MultiCore link down' tun_down || return 1
    echo "cores $now kept, MultiCore down"
}

c_refresh_while_connected() {
    [[ -n "$REVISION_FILE" ]] || { echo 'needs the built-in fixture'; return 1; }
    local response before after nodes state
    response="$(api POST /v1/connect)" || { echo "$response"; return 1; }
    wait_until 60 'state=connected' is_connected || return 1
    before="$(service_core_pids)"
    echo 2 >"$REVISION_FILE"
    response="$(api POST /v1/subscriptions/refresh)" || { echo "$response"; return 1; }
    wait_until 90 'new cores after refresh' pids_changed_from "$before" || return 1
    after="$(service_core_pids)"
    state="$(status_field state)"
    nodes="$(api GET /v1/catalog | jget 'sum(len(g["nodes"]) for g in d["groups"])')"
    ((nodes > NODES_BEFORE)) || { echo "catalog did not change ($nodes nodes)"; return 1; }
    response="$(api POST /v1/disconnect)" || { echo "$response"; return 1; }
    wait_until 60 'disconnected and warm' is_disconnected_warm || return 1
    WARM_PIDS="$(service_core_pids)"
    echo "cores $before -> $after, nodes $NODES_BEFORE -> $nodes, state after refresh=$state"
}

c_quit_stops_cores() {
    save_evidence first-daemon
    cores_running || { echo 'cores were not running before quit'; return 1; }
    stop_daemon TERM
    wait_until 30 'service cores to stop' cores_stopped || return 1
    systemctl is-active --quiet "$UNIT" || { echo 'service stopped instead of idling'; return 1; }
    echo 'cores stopped, service idle'
}

c_restart_rewarms() {
    start_daemon || return 1
    wait_until 90 'cores_ready' is_ready || return 1
    service_core_pids
}

c_crash_stops_cores() {
    local response; response="$(api POST /v1/connect)" || { echo "$response"; return 1; }
    wait_until 60 'state=connected' is_connected || return 1
    wait_until 30 'MultiCore link up' tun_up || return 1
    save_evidence second-daemon
    stop_daemon KILL
    wait_until 30 'service cores to stop after the crash' cores_stopped || return 1
    wait_until 30 'MultiCore link gone after the crash' tun_down || return 1
    systemctl is-active --quiet "$UNIT" || { echo 'service did not survive the client crash'; return 1; }
    echo 'cores stopped, MultiCore gone, service active'
}

c_remove() {
    case "$PACKAGE" in
        *.deb) sudo DEBIAN_FRONTEND=noninteractive apt-get remove -y multicore >"$OUT/remove.log" 2>&1 || { tail -n 20 "$OUT/remove.log"; return 1; } ;;
        *.rpm) sudo dnf remove -y multicore >"$OUT/remove.log" 2>&1 || { tail -n 20 "$OUT/remove.log"; return 1; } ;;
    esac
    local enabled active
    enabled="$(systemctl is-enabled "$UNIT" 2>&1 || true)"
    active="$(systemctl is-active "$UNIT" 2>&1 || true)"
    [[ "$enabled" != enabled ]] || { echo 'unit still enabled'; return 1; }
    [[ "$active" != active ]] || { echo 'unit still active'; return 1; }
    [[ ! -e "$SOCKET" ]] || { echo "$SOCKET left behind"; return 1; }
    cores_stopped || { echo "cores left: $(service_core_pids)"; return 1; }
    echo "is-enabled=$enabled is-active=$active"
}

c_purge_state() {
    if [[ "$PACKAGE" == *.deb ]]; then
        sudo DEBIAN_FRONTEND=noninteractive apt-get purge -y multicore >>"$OUT/remove.log" 2>&1 || { tail -n 20 "$OUT/remove.log"; return 1; }
    fi
    ! sudo test -e /var/lib/multicore || { echo '/var/lib/multicore left behind'; return 1; }
    ! systemctl cat "$UNIT" >/dev/null 2>&1 || { echo "$UNIT still known to systemd"; return 1; }
    echo '/var/lib/multicore removed, unit gone'
}

c_user_data_kept() {
    [[ -d "$DATA/profiles" ]] || { echo "$DATA/profiles removed by the package manager"; return 1; }
    echo "$DATA kept"
}

cleanup() {
    stop_daemon KILL
    [[ -n "$FIXTURE_PID" ]] && kill "$FIXTURE_PID" 2>/dev/null || true
    [[ -n "$HOLDER_UNIT" ]] && sudo systemctl stop "$HOLDER_UNIT" 2>/dev/null || true
    if [[ -n "$DATA" && "$KEEP_INSTALLED" == 0 ]]; then
        rm -rf "$DATA"
        [[ -n "$BACKUP" ]] && mv "$BACKUP" "$DATA"
    fi
    return 0
}

run() {
    [[ $# -ge 1 ]] || usage
    PACKAGE="$(realpath "$1")"; shift
    [[ -f "$PACKAGE" && ( "$PACKAGE" == *.deb || "$PACKAGE" == *.rpm ) ]] || { echo "not a .deb/.rpm: $PACKAGE" >&2; exit 2; }
    while [[ $# -gt 0 ]]; do
        case "$1" in
            --subscription) SUBSCRIPTION="$2"; shift 2 ;;
            --traffic) TRAFFIC="$2"; [[ "$TRAFFIC" == none ]] && TRAFFIC=''; shift 2 ;;
            --keep-installed) KEEP_INSTALLED=1; shift ;;
            --out) OUT="$2"; shift 2 ;;
            *) usage ;;
        esac
    done
    mkdir -p "$OUT"
    OUT="$(realpath "$OUT")"
    RESULTS="$OUT/results.jsonl"
    : >"$RESULTS"
    : >"$OUT/driver.log"
    local base_data="${XDG_DATA_HOME:-}"
    [[ "$base_data" == /* ]] || base_data="$HOME/.local/share"
    DATA="$base_data/multicore"
    trap cleanup EXIT

    check preflight c_preflight || { finish_results; exit 1; }
    if [[ -e "$DATA" ]]; then
        BACKUP="$DATA.live-backup-$(date +%Y%m%d-%H%M%S)"
        mv "$DATA" "$BACKUP"
        log "moved existing data to $BACKUP"
    fi
    if check install c_install; then
        check unit-enabled-and-active c_unit_enabled || true
        check unit-hardening c_unit_hardening || true
        check socket-and-state-permissions c_socket_perms || true
        if check logind-session c_session; then
            check no-cores-before-daemon c_no_cores_before_daemon || true
            if [[ -z "$SUBSCRIPTION" ]]; then check fixture-subscription start_fixture || true; fi
            if check daemon-start start_daemon; then
                check service-mode c_service_mode || true
                if check import c_import && check cores-warm-before-connect c_cores_warm; then
                    check catalog-before-connect c_catalog || true
                    check latency-before-connect c_latency || true
                    check dashboard-read-only c_dashboard || true
                    if check connect-tun-without-pkexec c_connect; then
                        if [[ -n "$TRAFFIC" ]]; then check traffic c_traffic || true; fi
                        check disconnect-keeps-cores c_disconnect_keeps_cores || true
                    fi
                    check refresh-while-connected-reprepares c_refresh_while_connected || true
                    check quit-stops-cores c_quit_stops_cores || true
                    if check daemon-restart-rewarms c_restart_rewarms; then
                        check crash-while-connected-stops-cores c_crash_stops_cores || true
                    fi
                fi
            fi
            stop_daemon KILL
        fi
        if [[ "$KEEP_INSTALLED" == 0 ]]; then
            check remove-disables-unit c_remove || true
            check purge-removes-state c_purge_state || true
            check user-data-kept c_user_data_kept || true
        fi
    fi
    local summary; summary="$(finish_results)"
    log "$summary"
    echo "results: $OUT/live-service-result.json"
    grep -q '"ok": false' "$RESULTS" && exit 1
    exit 0
}

case "${1-}" in
    run) shift; run "$@" ;;
    fedora-container) shift; fedora_container "$@" ;;
    *) usage ;;
esac
