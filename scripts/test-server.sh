#!/usr/bin/env bash
# Manage the gsvn Subversion test server (Apache + mod_dav_svn in Docker).
#
#   scripts/test-server.sh up       build image if needed, generate repos if
#                                   missing, start container
#   scripts/test-server.sh down     stop and remove the container
#   scripts/test-server.sh status   show container state and probe the URLs
#   scripts/test-server.sh logs     follow the container logs
#   scripts/test-server.sh rebuild  force an image rebuild (then run 'up')
#
# Environment overrides:
#   GSVN_TEST_REPOS  repos directory (default: <repo>/target/test-repos)
#   GSVN_HTTP_PORT   host HTTP port  (default: 8080)
#   GSVN_HTTPS_PORT  host HTTPS port (default: 8443)
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
IMAGE="gsvn-test-svn:latest"
NAME="gsvn-test-svn"
REPOS="${GSVN_TEST_REPOS:-$ROOT/target/test-repos}"
HTTP_PORT="${GSVN_HTTP_PORT:-8080}"
HTTPS_PORT="${GSVN_HTTPS_PORT:-8443}"
ALL_REPOS=(small wide deep history)

build() {
    docker build -t "$IMAGE" "$ROOT/test-server"
}

ensure_image() {
    # Rebuild when the image is missing or older than its build context.
    local created ctx_mtime
    if ! created=$(docker image inspect -f '{{.Created}}' "$IMAGE" 2>/dev/null); then
        build
        return
    fi
    created=$(date -d "$created" +%s)
    ctx_mtime=$(stat -c %Y "$ROOT/test-server/Dockerfile" "$ROOT/test-server/httpd.conf" | sort -n | tail -1)
    if (( ctx_mtime > created )); then
        build
    fi
}

ensure_repos() {
    local missing=()
    for r in "${ALL_REPOS[@]}"; do
        [[ -d "$REPOS/$r" ]] || missing+=("$r")
    done
    if (( ${#missing[@]} )); then
        local IFS=,
        echo "Generating missing test repos: ${missing[*]}"
        python3 "$ROOT/scripts/make-test-repos.py" "$REPOS" --only "${missing[*]}"
    fi
}

running() {
    [[ "$(docker inspect -f '{{.State.Running}}' "$NAME" 2>/dev/null)" == "true" ]]
}

up() {
    ensure_image
    ensure_repos
    if running; then
        echo "$NAME already running"
        return
    fi
    docker rm -f "$NAME" >/dev/null 2>&1 || true
    # Run as the host user so the bind-mounted repos (owned by that user) are
    # readable; mod_dav_svn only needs read access for a read-only browser.
    # ':z' relabels for SELinux hosts that enforce container labels.
    docker run -d --name "$NAME" \
        --user "$(id -u):$(id -g)" \
        -p "127.0.0.1:$HTTP_PORT:8080" \
        -p "127.0.0.1:$HTTPS_PORT:8443" \
        -v "$REPOS:/repos:ro,z" \
        "$IMAGE" >/dev/null
    for _ in $(seq 1 50); do
        if curl -fsS -o /dev/null "http://127.0.0.1:$HTTP_PORT/svn/" 2>/dev/null; then
            echo "$NAME up:"
            urls
            return
        fi
        sleep 0.2
    done
    echo "server did not come up; logs:" >&2
    docker logs "$NAME" >&2
    exit 1
}

urls() {
    echo "  http://localhost:$HTTP_PORT/svn/<repo>    anonymous"
    echo "  http://localhost:$HTTP_PORT/auth/<repo>   Basic auth test/test (realm \"gsvn test\")"
    echo "  http://localhost:$HTTP_PORT/slow/<repo>   anonymous, 64 KiB/s bandwidth limit"
    echo "  https://localhost:$HTTPS_PORT/svn/<repo>  self-signed cert (also /auth/<repo>)"
    echo "  repos: $(ls "$REPOS" 2>/dev/null | tr '\n' ' ')"
}

down() {
    docker rm -f "$NAME" >/dev/null 2>&1 && echo "$NAME removed" || echo "$NAME not running"
}

status() {
    if running; then
        echo "$NAME running"
        urls
        local code
        code=$(curl -s -o /dev/null -w '%{http_code}' "http://127.0.0.1:$HTTP_PORT/svn/small/") || true
        echo "  probe /svn/small/: HTTP $code"
    else
        echo "$NAME not running"
        return 1
    fi
}

case "${1:-}" in
    up) up ;;
    down) down ;;
    status) status ;;
    logs) docker logs -f "$NAME" ;;
    rebuild) build ;;
    *) echo "usage: $0 {up|down|status|logs|rebuild}" >&2; exit 2 ;;
esac
