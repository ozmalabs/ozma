#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
#
# Clean-host Phase 4 harness.
#
# Builds the Rust node binaries, runs a bounded (or full) test selection,
# checks both binaries answer --help, and smoke-starts ozma-node with synthetic
# providers. By default this runs in a rootless Podman/Docker container so the
# host is never modified. Use --host to build/test in the current tree instead.
#
# Usage:
#   scripts/clean-host/run.sh                    # auto: rootless container
#   scripts/clean-host/run.sh --container        # force container
#   scripts/clean-host/run.sh --host             # build/test on the host
#   scripts/clean-host/run.sh --check-deps       # dependency report only
#   scripts/clean-host/run.sh --full             # cargo test --workspace
#   scripts/clean-host/run.sh --no-smoke         # skip the start/health smoke
#   scripts/clean-host/run.sh --rebuild          # force an image rebuild
#   scripts/clean-host/run.sh --clean            # remove image and target cache
#   scripts/clean-host/run.sh --runtime podman   # pick a container runtime
#
# Exit status: 0 PASS; 1 FAIL; 2 usage/configuration error.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
cd "$REPO_ROOT"

RUNTIME="${OZMA_CLEAN_HOST_RUNTIME:-}"
MODE=auto
FULL=0
SMOKE=1
CHECK_DEPS_ONLY=0
REBUILD=0
CLEAN=0
ALLOW_ROOTFUL=0
IMAGE="${OZMA_CLEAN_HOST_IMAGE:-localhost/ozma-clean-host:1.95}"
TARGET_DIR="${OZMA_CLEAN_HOST_TARGET:-$REPO_ROOT/target/clean-host}"
LOG_DIR="${OZMA_CLEAN_HOST_LOG_DIR:-/tmp/ozma-clean-host-$(id -u)/logs}"

usage() {
    sed -n '2,20p' "$0" | sed 's/^# \{0,1\}//'
}

while [ "$#" -gt 0 ]; do
    case "$1" in
        --container) MODE=container ;;
        --host) MODE=host ;;
        --auto) MODE=auto ;;
        --runtime) shift; RUNTIME="${1:-}" ;;
        --runtime=*) RUNTIME="${1#*=}" ;;
        --image) shift; IMAGE="${1:-}" ;;
        --image=*) IMAGE="${1#*=}" ;;
        --full) FULL=1 ;;
        --subset) FULL=0 ;;
        --no-smoke) SMOKE=0 ;;
        --smoke) SMOKE=1 ;;
        --check-deps) CHECK_DEPS_ONLY=1 ;;
        --rebuild) REBUILD=1 ;;
        --clean) CLEAN=1 ;;
        --allow-rootful) ALLOW_ROOTFUL=1 ;;
        -h|--help) usage; exit 0 ;;
        *)
            echo "unknown argument: $1" >&2
            usage >&2
            exit 2
            ;;
    esac
    shift
done

if [ "$CHECK_DEPS_ONLY" = "1" ]; then
    exec "$SCRIPT_DIR/check-deps.sh"
fi

mkdir -p "$LOG_DIR"

find_runtime() {
    if [ -n "$RUNTIME" ]; then
        if ! command -v "$RUNTIME" >/dev/null 2>&1; then
            echo "Requested container runtime '$RUNTIME' was not found." >&2
            return 1
        fi
        echo "$RUNTIME"
        return 0
    fi
    for candidate in podman docker; do
        if command -v "$candidate" >/dev/null 2>&1; then
            echo "$candidate"
            return 0
        fi
    done
    return 1
}

is_rootless() {
    runtime=$1
    case "$runtime" in
        podman)
            [ "$(podman info --format '{{.Host.Security.Rootless}}' 2>/dev/null)" = "true" ]
            ;;
        docker)
            docker info --format '{{.SecurityOptions}}' 2>/dev/null | grep -q 'rootless'
            ;;
        *)
            return 1
            ;;
    esac
}

image_exists() {
    runtime=$1
    case "$runtime" in
        podman) podman image exists "$IMAGE" >/dev/null 2>&1 ;;
        docker) docker image inspect "$IMAGE" >/dev/null 2>&1 ;;
        *) return 1 ;;
    esac
}

build_image() {
    runtime=$1
    log="$LOG_DIR/container-build.log"
    echo "Building $IMAGE with $runtime (log: $log)"
    if "$runtime" build -t "$IMAGE" -f "$SCRIPT_DIR/Containerfile" "$SCRIPT_DIR" 2>&1 | tee "$log"; then
        return 0
    fi
    echo "" >&2
    echo "Image build failed. See $log." >&2
    echo "If this host cannot use containers, run the same checks without one:" >&2
    echo "  scripts/clean-host/run.sh --host" >&2
    return 1
}

run_in_container() {
    runtime=$1
    if ! is_rootless "$runtime"; then
        if [ "$ALLOW_ROOTFUL" = "0" ]; then
            echo "FAIL  $runtime is not rootless." >&2
            echo "This harness will not use a rootful container daemon by default." >&2
            echo "Use a rootless Podman/Docker, --host, or --allow-rootful explicitly." >&2
            return 1
        fi
        echo "WARN  $runtime is not rootless; continuing because --allow-rootful was set." >&2
    fi

    if ! image_exists "$runtime"; then
        build_image "$runtime" || return 1
    elif [ "$REBUILD" = "1" ]; then
        build_image "$runtime" || return 1
    fi

    mkdir -p "$TARGET_DIR" "$TARGET_DIR/home" "$TARGET_DIR/cargo-home"
    log="$LOG_DIR/run.log"
    if [ "$runtime" = "podman" ]; then
        # Rootless + label=disable keeps the harness from relabelling the
        # checkout or the 2 GB host Cargo cache. No daemon/root access is used.
        args=(run --rm --userns=keep-id --security-opt label=disable -w /src
            -e CARGO_TARGET_DIR=/src/target/clean-host
            -e CARGO_HOME=/cargo-home
            -e HOME=/tmp/ozma-clean-home
            -e OZMA_CLEAN_HOST_FULL="$FULL"
            -e OZMA_CLEAN_HOST_SMOKE="$SMOKE"
            -v "$REPO_ROOT:/src"
            -v "$TARGET_DIR/home:/tmp/ozma-clean-home"
            -v "$TARGET_DIR/cargo-home:/cargo-home"
            -v "$TARGET_DIR:/src/target/clean-host")
        if [ -d "$HOME/.cargo/registry" ]; then
            args+=(-v "$HOME/.cargo/registry:/cargo-home/registry")
        fi
        if [ -d "$HOME/.cargo/git" ]; then
            args+=(-v "$HOME/.cargo/git:/cargo-home/git")
        fi
    else
        args=(run --rm --user "$(id -u):$(id -g)" -w /src
            -e CARGO_TARGET_DIR=/src/target/clean-host
            -e CARGO_HOME=/cargo-home
            -e HOME=/tmp/ozma-clean-home
            -e OZMA_CLEAN_HOST_FULL="$FULL"
            -e OZMA_CLEAN_HOST_SMOKE="$SMOKE"
            -v "$REPO_ROOT:/src"
            -v "$TARGET_DIR/home:/tmp/ozma-clean-home"
            -v "$TARGET_DIR/cargo-home:/cargo-home"
            -v "$TARGET_DIR:/src/target/clean-host")
        if [ -d "$HOME/.cargo/registry" ]; then
            args+=(-v "$HOME/.cargo/registry:/cargo-home/registry")
        fi
        if [ -d "$HOME/.cargo/git" ]; then
            args+=(-v "$HOME/.cargo/git:/cargo-home/git")
        fi
    fi

    echo "Runtime: $runtime (rootless)"
    echo "Image:   $IMAGE"
    echo "Repo:    $REPO_ROOT (mounted at /src)"
    echo "Target:  $TARGET_DIR"
    echo "Log:     $log"
    echo ""

    "$runtime" "${args[@]}" "$IMAGE" /src/scripts/clean-host/in-container.sh 2>&1 | tee "$log"
    status=${PIPESTATUS[0]}
    echo ""
    if [ "$status" -eq 0 ]; then
        echo "RESULT: PASS (log: $log)"
    else
        echo "RESULT: FAIL (log: $log)" >&2
    fi
    return "$status"
}

run_on_host() {
    log="$LOG_DIR/host-run.log"
    echo "Runtime: host (no container)"
    echo "Repo:    $REPO_ROOT"
    echo "Target:  $TARGET_DIR"
    echo "Log:     $log"
    echo ""
    CARGO_TARGET_DIR="$TARGET_DIR" \
    OZMA_CLEAN_HOST_FULL="$FULL" \
    OZMA_CLEAN_HOST_SMOKE="$SMOKE" \
        "$SCRIPT_DIR/in-container.sh" 2>&1 | tee "$log"
    status=${PIPESTATUS[0]}
    echo ""
    if [ "$status" -eq 0 ]; then
        echo "RESULT: PASS (log: $log)"
    else
        echo "RESULT: FAIL (log: $log)" >&2
    fi
    return "$status"
}

status=0

if [ "$MODE" = "host" ]; then
    run_on_host || status=$?
else
    runtime=$(find_runtime) || {
        if [ "$MODE" = "container" ]; then
            echo "FAIL  no container runtime found (podman or docker)." >&2
            echo "Install a rootless runtime, or run: scripts/clean-host/run.sh --host" >&2
            exit 1
        fi
        echo "No container runtime found; falling back to --host." >&2
        run_on_host || status=$?
        runtime=""
    }
    if [ -n "$runtime" ]; then
        run_in_container "$runtime" || status=$?
    fi
fi

if [ "$CLEAN" = "1" ]; then
    echo ""
    echo "Cleaning harness artifacts..."
    if [ -n "${runtime:-}" ] && command -v "$runtime" >/dev/null 2>&1; then
        case "$runtime" in
            podman) podman rmi -f "$IMAGE" >/dev/null 2>&1 || true ;;
            docker) docker rmi -f "$IMAGE" >/dev/null 2>&1 || true ;;
        esac
        echo "  removed image $IMAGE (if present)"
    fi
    rm -rf "$TARGET_DIR"
    echo "  removed $TARGET_DIR"
    echo "  logs kept at $LOG_DIR"
fi

exit "$status"
