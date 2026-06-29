#!/usr/bin/env bash
set -euo pipefail
# dont export big captures like `out=$(run_vm ...)` into the environment.
set +a
# extglob enables the *(...) pattern strip_ansi uses for pure-bash CSI stripping
shopt -s extglob
# note: needs `sudo modprobe vhost_vsock`!

log() {
    printf "\n\033[1;36m>>> %s\033[0m\n" "$*"
}

_strip_ansi_stream() {
    local line
    while IFS= read -r line || [ -n "$line" ]; do
        line=${line//$'\e'\[*([0-9;])[a-zA-Z]/}
        line=${line//$'\e'\([a-zA-Z]/}
        printf '%s\n' "$line"
    done
}

strip_ansi() {
    if [ "$#" -gt 0 ]; then
        local f
        for f in "$@"; do
            [ -r "$f" ] && _strip_ansi_stream < "$f"
        done
    else
        _strip_ansi_stream
    fi
}

# check_needles OUT NEEDLES...
# each needle is an extended regex matched per line (mirrors `grep -qE`).
check_needles() {
    local out="$1"
    shift

    local -a lines
    mapfile -t lines <<< "$out"

    # strip ANSI per line (each line is short, so this stays cheap)
    local i l
    for i in "${!lines[@]}"; do
        l=${lines[i]}
        l=${l//$'\e'\[*([0-9;])[a-zA-Z]/}
        l=${l//$'\e'\([a-zA-Z]/}
        lines[i]=$l
    done

    local needle missing=0 line hit
    for needle in "$@"; do
        hit=0
        for line in "${lines[@]}"; do
            if [[ $line =~ $needle ]]; then
                hit=1
                break
            fi
        done
        if [ "$hit" -eq 0 ]; then
            echo "error: output missing '$needle'" >&2
            missing=1
        fi
    done

    if [ "$missing" -ne 0 ]; then
        printf '%s\n' "${lines[@]}" >&2
        return 1
    fi
}

declare -a TEST_NAMES=()
declare -a TEST_STATUSES=()
declare -a TEST_TIMES=()
SKIP_TEST_RC=200

get_time_ms() {
    local t="${EPOCHREALTIME:-}"
    if [[ "$t" == *.* ]]; then
        local secs="${t%.*}"
        local subs="${t#*.}"
        subs="${subs:0:3}"
        while [ "${#subs}" -lt 3 ]; do
            subs="${subs}0"
        done
        echo "${secs}${subs}"
    else
        echo "$(date +%s)000"
    fi
}

format_duration() {
    local ms=$1
    local secs=$((ms / 1000))
    local rem=$((ms % 1000))
    printf "%d.%03ds" "$secs" "$rem"
}

print_summary() {
    if [ "${#TEST_NAMES[@]}" -eq 0 ]; then
        return
    fi
    printf "\n"
    log "test summary"
    echo "========================================="
    local passed_count=0
    local skipped_count=0
    local failed_count=0
    local total_time=0
    for i in "${!TEST_NAMES[@]}"; do
        local name="${TEST_NAMES[$i]}"
        local status="${TEST_STATUSES[$i]}"
        local duration_ms="${TEST_TIMES[$i]}"
        local duration_str
        duration_str=$(format_duration "$duration_ms")

        local status_color="\033[0;32m"
        if [ "$status" = "Failed" ]; then
            status_color="\033[0;31m"
            failed_count=$((failed_count + 1))
        elif [ "$status" = "Skipped" ]; then
            status_color="\033[0;33m"
            skipped_count=$((skipped_count + 1))
        else
            passed_count=$((passed_count + 1))
        fi
        total_time=$((total_time + duration_ms))

        printf "  %-30s %b%-8b\033[0m  %s\n" "$name" "$status_color" "$status" "$duration_str"
    done
    echo "-----------------------------------------"
    local total_tests="${#TEST_NAMES[@]}"
    local total_time_str
    total_time_str=$(format_duration "$total_time")
    printf "  total: %d tests, %d passed, %d skipped, %d failed\n" "$total_tests" "$passed_count" "$skipped_count" "$failed_count"
    printf "  total execution time: %s\n" "$total_time_str"
    echo "========================================="
}

skip_test() {
    echo "skipped: $*"
    return "$SKIP_TEST_RC"
}

host_is_nixos() {
    [ -e /etc/NIXOS ] && return 0
    [ -r /etc/os-release ] && grep -q '^ID=nixos$' /etc/os-release
}

JOBS="${JOBS:-4}"
while [[ $# -gt 0 ]]; do
    case "$1" in
        -j | --jobs)
            JOBS="$2"
            shift 2
            ;;
        --jobs=*)
            JOBS="${1#*=}"
            shift
            ;;
        --only)
            TEST_ONLY="$2"
            shift 2
            ;;
        --only=*)
            TEST_ONLY="${1#*=}"
            shift
            ;;
        *)
            echo "unknown argument: $1" >&2
            echo "usage: $0 [-j N|--jobs N] [--only TEST]" >&2
            exit 1
            ;;
    esac
done
if ! [[ "$JOBS" =~ ^[0-9]+$ ]] || [ "$JOBS" -lt 1 ]; then
    echo "error: --jobs must be a positive integer (got '$JOBS')" >&2
    exit 1
fi

pick_free_port() {
    local port
    for _ in $(seq 1 50); do
        port=$(((RANDOM % 16384) + 20000))
        if ! (exec 3<>"/dev/tcp/127.0.0.1/$port") 2>/dev/null; then
            echo "$port"
            return 0
        fi
    done
    echo "error: could not find a free port for the cache" >&2
    return 1
}

SUCCESS=0
rm -rf /tmp/test-spindle-microvm-logs

log "setup local cache & temp environment"
TEMP_DIR=$(mktemp -d -t test-spindle-microvm-XXXXXX)

if [ ! -e /dev/vsock ]; then
    echo "error: /dev/vsock is missing; run sudo modprobe vhost_vsock" >&2
    exit 1
fi

log "build spindle & microvm image tarball"
nix develop --command go build -o spindle/spindle-microvm-run ./cmd/spindle-microvm-run
TARBALL_PATH=$(nix build .#spindle-nixos-image-tarball --no-link --print-out-paths)
mkdir -p "$TEMP_DIR/image"
tar -C "$TEMP_DIR/image" -xzf "$TARBALL_PATH"
IMAGE_SPEC_JSON="$TEMP_DIR/image/spec.json"

log "build alpine microvm image tarball"
ALPINE_TARBALL_PATH=$(nix build .#spindle-alpine-image-tarball --no-link --print-out-paths)
mkdir -p "$TEMP_DIR/alpine-image"
tar -C "$TEMP_DIR/alpine-image" -xzf "$ALPINE_TARBALL_PATH"
ALPINE_IMAGE_SPEC_JSON="$TEMP_DIR/alpine-image/spec.json"

kill_temp_dir_procs() {
    if [ -f "$TEMP_DIR/ncps.pid" ]; then
        kill "$(cat "$TEMP_DIR/ncps.pid")" 2>/dev/null || true
    fi
    pkill -TERM -f "$TEMP_DIR" 2>/dev/null || true
    local i
    for i in $(seq 1 20); do
        pgrep -f "$TEMP_DIR" >/dev/null 2>&1 || break
        sleep 0.25
    done
    pkill -KILL -f "$TEMP_DIR" 2>/dev/null || true
}

collect_logs() {
    echo "test failed. copying logs to /tmp/test-spindle-microvm-logs"
    mkdir -p /tmp/test-spindle-microvm-logs
    local f
    for f in "$TEMP_DIR"/*; do
        [ -f "$f" ] && cp "$f" /tmp/test-spindle-microvm-logs/
    done
    local work logf
    for work in "$TEMP_DIR"/work-*; do
        [ -d "$work" ] || continue
        for logf in "$work"/*.log; do
            [ -f "$logf" ] || continue
            strip_ansi "$logf" > "/tmp/test-spindle-microvm-logs/$(basename "$work")-$(basename "$logf")"
        done
    done
}

CLEANED=0
cleanup() {
    [ "$CLEANED" -eq 1 ] && return
    CLEANED=1

    print_summary
    log "cleaning up..."

    local jobs_pids
    jobs_pids=$(jobs -p)
    [ -n "$jobs_pids" ] && kill $jobs_pids 2>/dev/null || true

    kill_temp_dir_procs

    [ "$SUCCESS" -ne 1 ] && collect_logs

    chmod -R +w "$TEMP_DIR" 2>/dev/null || true
    rm -rf "$TEMP_DIR"
    echo "done"
}
trap cleanup EXIT
# route signals through the EXIT trap so an interrupt still tears down VMs.
trap 'exit 130' INT
trap 'exit 143' TERM

CACHE_PORT=$(pick_free_port)
./spindle/engines/microvm/start-test-cache.sh "$TEMP_DIR" "$CACHE_PORT"
source "$TEMP_DIR/env.sh"

run_vm() {
    local name=""
    local timeout="60s"
    local upload=0
    local upload_url=""
    local activate=""
    local no_cache=0
    local db=""
    local spec="$IMAGE_SPEC_JSON"

    while [[ $# -gt 0 ]]; do
        case "$1" in
            --spec)
                spec="$2"
                shift 2
                ;;
            --name)
                name="$2"
                shift 2
                ;;
            --timeout)
                timeout="$2"
                shift 2
                ;;
            --upload)
                upload=1
                shift
                ;;
            --upload-url)
                upload=1
                upload_url="$2"
                shift 2
                ;;
            --activate)
                activate="$2"
                shift 2
                ;;
            --no-cache)
                no_cache=1
                shift
                ;;
            --db)
                db="$2"
                shift 2
                ;;
            --)
                shift
                break
                ;;
            *)
                echo "unknown argument: $1" >&2
                exit 1
                ;;
        esac
    done

    local work_dir="$TEMP_DIR/work-${name}"
    mkdir -p "$work_dir"

    local args=(
        --image-spec "$spec"
        --work-dir "$work_dir"
        --exec-timeout "$timeout"
        --port "${SPINDLE_TEST_VSOCK_PORT:-10240}"
        --memory-mib 4096
    )

    if [ "$no_cache" -eq 0 ]; then
        args+=(
            --cache-read-url "$CACHE_URL"
            --cache-trusted-public-key "$CACHE_PUBKEY"
        )
    fi

    if [ "$upload" -eq 1 ]; then
        if [ -z "$upload_url" ]; then
            upload_url="$CACHE_UPLOAD_URL?secret-key=$CACHE_SECRET_KEY_PATH"
        fi
        args+=(
            --cache-upload-url "$upload_url"
        )
    fi

    if [ -n "$activate" ]; then
        args+=(
            --activate-config "$activate"
        )
    fi

    if [ -n "$db" ]; then
        args+=(
            --db "$db"
        )
    fi

    local out
    if ! out=$(spindle/spindle-microvm-run "${args[@]}" -- "$@" 2>&1); then
        echo "$out" | strip_ansi >&2
        strip_ansi "$work_dir/serial.log" >&2
        strip_ansi "$work_dir/qemu.log" >&2
        return 1
    fi
    echo "$out"
}

run_test_job() {
    local name="$1"
    local func="$2"
    local port="$3"
    export SPINDLE_TEST_VSOCK_PORT="$port"

    local logfile="$TEMP_DIR/test-${name}.log"
    local start
    start=$(get_time_ms)
    log "[$name] start (vsock port $port)"

    local status="Passed"
    if "$func" > "$logfile" 2>&1; then
        status="Passed"
    else
        local rc=$?
        if [ "$rc" -eq "$SKIP_TEST_RC" ]; then
            status="Skipped"
        else
            status="Failed"
        fi
    fi

    local duration_ms=$(($(get_time_ms) - start))
    printf '%s\t%s\n' "$status" "$duration_ms" > "$TEMP_DIR/test-${name}.status"

    local duration_str
    duration_str=$(format_duration "$duration_ms")
    if [ "$status" = "Failed" ]; then
        printf "\n\033[0;31m>>> [%s] FAILED (%s)\033[0m\n" "$name" "$duration_str"
        strip_ansi "$logfile" || true
    elif [ "$status" = "Skipped" ]; then
        printf "\n\033[0;33m>>> [%s] skipped (%s)\033[0m\n" "$name" "$duration_str"
        strip_ansi "$logfile" || true
    else
        printf "\n\033[0;32m>>> [%s] passed (%s)\033[0m\n" "$name" "$duration_str"
    fi
}

# schedules every selected test across at most $JOBS concurrent VMs (each on its
# own vsock port), then aggregates the per-test status files into the summary
# arrays. returns 1 if any test failed, 0 otherwise.
run_tests() {
    local base_port=10240
    local idx=0
    local running=0

    for func in "${TESTS[@]}"; do
        local name="${func#test_}"
        name="${name//_/-}"
        if [ -n "${TEST_ONLY:-}" ] && [ "${TEST_ONLY}" != "$name" ]; then
            continue
        fi

        run_test_job "$name" "$func" "$((base_port + idx))" &
        idx=$((idx + 1))
        running=$((running + 1))

        if [ "$running" -ge "$JOBS" ]; then
            wait -n || true
            running=$((running - 1))
        fi
    done
    wait

    local failed=0
    for func in "${TESTS[@]}"; do
        local name="${func#test_}"
        name="${name//_/-}"
        if [ -n "${TEST_ONLY:-}" ] && [ "${TEST_ONLY}" != "$name" ]; then
            continue
        fi

        local statusfile="$TEMP_DIR/test-${name}.status"
        if [ ! -f "$statusfile" ]; then
            TEST_NAMES+=("$name")
            TEST_STATUSES+=("Failed")
            TEST_TIMES+=(0)
            failed=1
            continue
        fi

        local status duration_ms
        IFS=$'\t' read -r status duration_ms < "$statusfile"
        TEST_NAMES+=("$name")
        TEST_STATUSES+=("$status")
        TEST_TIMES+=("$duration_ms")
        if [ "$status" = "Failed" ]; then
            failed=1
        fi
    done

    return "$failed"
}

test_realize() {
    local test_store_path
    test_store_path=$(nix-build -E 'with import <nixpkgs> {}; writeText "test-file" "hello from cache"' --no-out-link)
    nix copy --to "$CACHE_UPLOAD_URL?secret-key=$CACHE_SECRET_KEY_PATH" "$test_store_path"

    local out
    out=$(run_vm --name "realize" --timeout "60s" -- /run/current-system/sw/bin/bash -lc '
set -euo pipefail
store_path=$1
cache_url=$(sed -n "s/^extra-substituters = //p" /run/spindle/nix.conf)
cache_url=${cache_url%% *}
if [ -z "$cache_url" ]; then
    echo "error: cache URL not found in /run/spindle/nix.conf" >&2
    exit 1
fi

http_version=$(/run/current-system/sw/bin/curl --http2-prior-knowledge -fsS -o /dev/null -w "%{http_version}" "$cache_url/nix-cache-info")
echo "http_version=$http_version"
case "$http_version" in
    2|2.0) ;;
    *)
        echo "error: cache proxy did not negotiate HTTP/2 (got $http_version)" >&2
        exit 1
        ;;
esac

if [ "$(stat -c %d /tmp)" = "$(stat -c %d /workspace)" ]; then
    echo "tmp_on_persist=yes"
else
    echo "tmp_on_persist=no"
fi

/run/current-system/sw/bin/nix-store --realise "$store_path" >/dev/null
' bash "$test_store_path") || return 1

    check_needles "$out" "^http_version=2(\\.0)?$" "^tmp_on_persist=yes$" || return 1
    echo "success: store path realized from cache and cache proxy accepted cleartext HTTP/2"
}

test_build_upload() {
    local nix_expr='with import <nixpkgs> {}; writeText "uploaded-test-file" "hello from vm upload"'
    local out
    out=$(run_vm --name "build-upload" --timeout "120s" --upload -- /run/current-system/sw/bin/bash -l -c "nix-build -E '$nix_expr' --no-out-link") || return 1

    local built_path
    built_path=$(echo "$out" | strip_ansi | grep -v '\.drv' | grep -o '/nix/store/[a-z0-9]*-uploaded-test-file' | head -n 1 || true)
    if [ -z "$built_path" ]; then
        echo "error: could not find built store path in vm output" >&2
        return 1
    fi
    echo "extracted path: $built_path"

    local hash
    hash=$(basename "$built_path" | cut -d'-' -f1)
    if ! curl -s -f "$CACHE_URL/${hash}.narinfo" > /dev/null; then
        echo "error: built store path was not uploaded to the binary cache" >&2
        return 1
    fi
    echo "success: store path uploaded to cache"
}

test_ssh_store_upload() {
    if ! host_is_nixos; then
        skip_test "ssh store upload smoke only runs on nixos hosts"
    fi

    run_ssh_store_upload_case() {
        local target="$1"
        local label="$2"
        local name="uploaded-test-file-${label}"
        local content="hello from vm upload via ${label}"
        local out
        out=$(run_vm --name "$label" --timeout "180s" --upload-url "$target" -- /run/current-system/sw/bin/bash -l -c "nix-build -E 'with import <nixpkgs> {}; writeText \"$name\" \"$content\"' --no-out-link") || return 1

        local built_path
        built_path=$(echo "$out" | strip_ansi | grep -v '\.drv' | grep -o "/nix/store/[a-z0-9]*-${name}" | head -n 1 || true)
        if [ -z "$built_path" ]; then
            echo "error: could not find built store path in vm output for $label" >&2
            echo "$out" | strip_ansi >&2
            return 1
        fi
        if [ ! -e "$built_path" ]; then
            echo "error: uploaded path missing from host store for $label: $built_path" >&2
            return 1
        fi
        if [ "$(cat "$built_path")" != "$content" ]; then
            echo "error: uploaded path content mismatch for $label" >&2
            echo "path=$built_path" >&2
            return 1
        fi
        echo "success: store path uploaded to $target via spindle"
    }

    run_ssh_store_upload_case "ssh-ng://localhost" "ssh-ng-upload"
    run_ssh_store_upload_case "ssh://localhost" "ssh-upload"
}

test_networking() {
    local hello_path
    hello_path=$(nix-build -E 'with import <nixpkgs> {}; hello' --no-out-link)

    local out
    out=$(run_vm --name "networking" --timeout "120s" --no-cache -- /run/current-system/sw/bin/bash -c "/run/current-system/sw/bin/curl -I --connect-timeout 1 -m 1 http://10.0.2.2:$CACHE_PORT; /run/current-system/sw/bin/nix-store --realise $hello_path") || return 1

    if echo "$out" | grep -qi -E "unreachable|timeout|failed to connect|timed out" || echo "$out" | grep -q "exited with code"; then
        echo "success: host network access blocked"
    else
        echo "error: guest vm accessed host network or returned unexpected output" >&2
        echo "$out" | strip_ansi >&2
        return 1
    fi
    echo "success: guest vm reached the internet and substituted hello"
}

test_substitution_and_no_upload() {
    local hello_path
    hello_path=$(nix-build -E 'with import <nixpkgs> {}; hello' --no-out-link)

    local out
    out=$(run_vm --name "nixpkgs-hello" --timeout "180s" --upload -- /run/current-system/sw/bin/nix-store --realise "$hello_path") || return 1

    # substituted from our proxy, and nothing uploaded back to the cache.
    check_needles "$out" "copying path.*hello" "cache uploaded: 0" || return 1
    echo "success: hello package substituted from upstream cache and was not uploaded"
}

# a pinned registry, reused by the dependency and registry-pin tests.
ACTIVATION_REGISTRY='"registry": {
        "nixpkgs": "github:nixos/nixpkgs/nixos-unstable",
        "my-nixpkgs": "nixpkgs"
    }'

test_activation_services() {
    local config='{
        "services": {
            "openssh": {
                "enable": true,
                "authorizedKeysFiles": ["/etc/ssh/authorized_keys"]
            }
        }
    }'
    local out
    out=$(run_vm --name "activation-services" --timeout "300s" --activate "$config" -- /run/current-system/sw/bin/systemctl is-active sshd) || return 1
    check_needles "$out" "^active$" || return 1
    echo "success: openssh service active after activation"
}

test_activation_dependencies() {
    # pkg-config + openssl as bare dependencies (resolved via the pinned nixpkgs
    # registry); their job is to prove the activation devshell exports build env
    # vars like PKG_CONFIG_PATH, not just PATH. hello comes in via the github
    # flakeref and (separately) the my-nixpkgs alias.
    local config='{
        '"$ACTIVATION_REGISTRY"',
        "dependencies": [
            "pkg-config",
            "openssl",
            "github:nixos/nixpkgs#hello",
            "my-nixpkgs#hello"
        ]
    }'
    # dependencies live in the activation devshell now, not systemPackages, so a
    # step picks them up by sourcing the materialised env (this is what the
    # engine's RunStep does automatically; the CLI runner does not, so we do it
    # here). this script is reused across the build and cache-hit runs below.
    local job='
env_file=/run/spindle/devshell-env.sh
[ -f "$env_file" ] && echo "env_file=present" || echo "env_file=missing"

# mirror RunStep
. "$env_file"
export PATH="$PATH:/run/current-system/sw/bin:/nix/var/nix/profiles/default/bin"

set -euo pipefail
# bare deps: pkg-config must be on PATH, and it must locate openssl through the
# devshell-provided PKG_CONFIG_PATH (openssls headers live in a separate `dev`
# output, so this also proves that output got realised into the devshell).
echo "pkgconfig=$(pkg-config --version)"
pkg-config --exists openssl && echo "openssl_found=yes"
echo "openssl_version=$(pkg-config --modversion openssl)"
inc=$(pkg-config --variable=includedir openssl)
echo "includedir=$inc"
[ -f "$inc/openssl/ssl.h" ] && echo "dev_headers=found"

# flakeref + aliased deps
echo "hello=$(hello)"
'

    local db_path="$TEMP_DIR/activation-dependencies.db"

    # first run: build the config, materialise the devshell env profile, upload
    # its closure, and record both toplevel + env profile in the db.
    local out
    out=$(run_vm --name "activation-dependencies" --timeout "600s" --activate "$config" --db "$db_path" --upload -- /run/current-system/sw/bin/bash -l -c "$job") || return 1

    check_needles "$out" \
        "env_file=present" "pkgconfig=[0-9]" "openssl_found=yes" \
        "openssl_version=[0-9]" "dev_headers=found" "hello=Hello, world!" || return 1
    echo "success: bare (pkg-config + openssl, dev headers found via PKG_CONFIG_PATH), flakeref, and aliased deps all resolved"

    # second run: same config + db, no upload. the devshell .drv is absent (no
    # eval happens on a cache hit), so the env must be re-read from the cached
    # env profile. the deps must still resolve exactly as on the build run.
    out=$(run_vm --name "activation-dependencies-cached" --timeout "300s" --activate "$config" --db "$db_path" -- /run/current-system/sw/bin/bash -l -c "$job") || return 1

    check_needles "$out" \
        "realizing cached NixOS config" \
        "env_file=present" "pkgconfig=[0-9]" "openssl_found=yes" \
        "openssl_version=[0-9]" "dev_headers=found" "hello=Hello, world!" || return 1
    echo "success: cache-hit run re-read the devshell env from the cached profile (no drv) and all deps resolved"
}

test_activation_registry_pin() {
    # the nixpkgs the image itself was built from; the registry override must NOT
    # resolve to this. deterministic (locked in the repo flake), so safe to compare.
    local base_nixpkgs
    base_nixpkgs=$(nix eval --raw --impure --expr '(builtins.getFlake (toString ./.)).inputs.nixpkgs.outPath')

    local config='{
        '"$ACTIVATION_REGISTRY"'
    }'
    # the pinned nixpkgs must reach the system nix config: resolving it via the
    # flakes CLI must not error "is not locked", and it must win the <nixpkgs> nixPath.
    local out
    out=$(run_vm --name "activation-registry-pin" --timeout "300s" --activate "$config" -- /run/current-system/sw/bin/bash -l -c '
set -euo pipefail
echo "lib_version=$(nix eval --raw nixpkgs#lib.version)"
echo "nix_path=$(nix eval --raw --impure --expr "toString <nixpkgs>")"
') || return 1

    check_needles "$out" "lib_version=[0-9]" || return 1
    local clean guest_nixpath
    clean=$(echo "$out" | strip_ansi)
    guest_nixpath=$(echo "$clean" | sed -n 's/^nix_path=//p' | head -n1)
    if [ -z "$guest_nixpath" ] || [ "$guest_nixpath" = "$base_nixpkgs" ]; then
        echo "error: guest <nixpkgs> nixPath did not resolve to the registry override (got '$guest_nixpath', base '$base_nixpkgs')" >&2
        return 1
    fi
    echo "success: pinned nixpkgs registry reached the guest nix config (flakes CLI + <nixpkgs> nixPath)"
}

test_activation_cache_substitution() {
    # a unique path that only exists in the configured (workflow) cache; the host
    # seeds it so the guest can prove it substitutes through the read proxy.
    local test_store_path
    test_store_path=$(nix-build -E 'with import <nixpkgs> {}; writeText "activation-cache-test" "hello from the workflow cache"' --no-out-link)
    nix copy --to "$CACHE_UPLOAD_URL?secret-key=$CACHE_SECRET_KEY_PATH" "$test_store_path"

    # a trivial config: this test only cares that the read proxy serves the
    # workflow-cache path during an activated run.
    local out
    out=$(run_vm --name "activation-cache-substitution" --timeout "300s" --activate '{}' -- /run/current-system/sw/bin/bash -l -c '
set -euo pipefail
store_path=$1
# the unique path only exists in the configured cache, so realising it proves it
# was substituted through the read proxy and not built or found elsewhere.
nix-store --realise "$store_path" >/dev/null
echo "substituted=$(cat "$store_path")"
' bash "$test_store_path") || return 1

    check_needles "$out" "substituted=hello from the workflow cache" || return 1
    echo "success: unique path substituted from the workflow cache through the read proxy"
}

test_activation_docker() {
    local config='{
        "virtualisation": {
            "docker": { "enable": true }
        }
    }'
    # docker.service is up, but the daemon socket can lag a beat behind activation;
    # wait for it to answer, then pull+run a real image. this drives the slimmed
    # kernel modules: overlay.ko storage plus bridge/iptables networking out of the
    # pruned tree, with outbound DNS/network over the guest slirp link.
    local out
    out=$(run_vm --name "activation-docker" --timeout "600s" --activate "$config" -- /run/current-system/sw/bin/bash -l -c '
set -euo pipefail
echo "docker_unit=$(systemctl is-active docker)"
for i in $(seq 1 60); do docker info >/dev/null 2>&1 && break; sleep 1; done
docker info >/dev/null
echo "storage_driver=$(docker info --format "{{.Driver}}")"
docker run --rm alpine cat /etc/alpine-release | sed "s/^/alpine_release=/"
docker run --rm alpine echo container-ran-ok
') || return 1

    check_needles "$out" \
        "^docker_unit=active$" "storage_driver=overlay(2|fs)" \
        "alpine_release=[0-9]+\." "container-ran-ok" || return 1
    echo "success: docker service active, pulled and ran an alpine container on the overlay storage driver"
}

test_activation_cached_realize() {
    local config='{
        "services": {
            "openssh": {
                "enable": true,
                "authorizedKeysFiles": ["/etc/ssh/authorized_keys"]
            }
        }
    }'
    local db_path="$TEMP_DIR/activation-cached.db"

    # first run: build the config, upload its closure, and record the toplevel in
    # the db. nothing cached yet, so this builds from scratch.
    local out
    out=$(run_vm --name "activation-cached-first" --timeout "600s" --activate "$config" --db "$db_path" --upload -- /run/current-system/sw/bin/systemctl is-active sshd) || return 1
    check_needles "$out" "^active$" || return 1

    # second run: same config + db, no upload. must realize the recorded toplevel
    # from the cache instead of rebuilding, and the cached system must come up.
    out=$(run_vm --name "activation-cached-second" --timeout "300s" --activate "$config" --db "$db_path" -- /run/current-system/sw/bin/systemctl is-active sshd) || return 1

    check_needles "$out" "realizing cached NixOS config" "^active$" || return 1
    echo "success: second run realized the cached NixOS config from the cache and sshd came up"
}

test_alpine() {
    local hello_path
    hello_path=$(nix-build -E 'with import <nixpkgs> {}; hello' --no-out-link)

    local out
    out=$(run_vm --spec "$ALPINE_IMAGE_SPEC_JSON" --name "alpine" --timeout "180s" --no-cache -- /bin/sh -lc '
set -eu
export HOME=/workspace
hello_path=$1
echo "release=$(cat /etc/alpine-release)"
echo "user=$(id -un)"
git version
bash -c "echo bash=\$BASH_VERSION"
touch /workspace/write-test
echo "workspace writable"
git ls-remote https://tangled.org/@tangled.org/core HEAD >/dev/null
echo "git over https ok"
apk add make
echo "apk ok"
# substitute a real package from cache.nixos.org over HTTPS and run it
nix-store --realise "$hello_path" >/dev/null
echo "ran=$("$hello_path/bin/hello")"
' sh "$hello_path") || return 1

    check_needles "$out" \
        "release=" "user=spindle-workflow" "git version" "bash=" \
        "workspace writable" "git over https ok" "apk ok" "ran=Hello, world!" || return 1
    echo "success: alpine guest booted, ran as workflow user, wrote workspace, cloned + installed over the network, and substituted+ran a package from cache.nixos.org over HTTPS"
}

test_alpine_podman() {
    # install podman via apk and run a real container as the workflow user.
    # rootless podman lives entirely in the writable workspace (storage + runroot
    # under XDG dirs there), uses podman's default storage driver, and pulls over
    # the guest network like the other alpine tests.
    local out
    out=$(run_vm --spec "$ALPINE_IMAGE_SPEC_JSON" --name "alpine-podman" --timeout "300s" --no-cache -- /bin/sh -lc '
set -eu
# no env setup here on purpose: shuttle seeds USER/LOGNAME/HOME/SHELL from the
# workflow users passwd entry and provisions XDG_RUNTIME_DIR, so rootless podman
# works out of the box. asserting those below doubles as a check on that.

# shadow-uidmap ships newuidmap/newgidmap which rootless podman uses to apply the
# /etc/subuid + /etc/subgid ranges baked into the image.
apk add podman shadow-uidmap
echo "user=$(id -un) USER=$USER HOME=$HOME XDG_RUNTIME_DIR=$XDG_RUNTIME_DIR"
echo "newuidmap=$(command -v newuidmap)"
echo "podman_version=$(podman --version)"

podman info >/dev/null
echo "storage_driver=$(podman info --format "{{.Store.GraphDriverName}}")"

podman run --rm --network=host docker.io/library/alpine cat /etc/alpine-release | sed "s/^/container_release=/"
podman run --rm --network=host docker.io/library/alpine echo container-ran-ok
' sh) || return 1

    check_needles "$out" \
        "user=spindle-workflow USER=spindle-workflow HOME=/workspace XDG_RUNTIME_DIR=/run/user/970" \
        "newuidmap=/" "podman_version=" \
        "storage_driver=" "container_release=[0-9]+\." "container-ran-ok" || return 1
    echo "success: alpine guest installed podman via apk and pulled + ran a rootless container"
}

# asserts a store path's narinfo shows up in the local cache, retrying briefly
# since the post-build-hook enqueues uploads asynchronously.
cache_has_path() {
    local path="$1"
    local hash
    hash=$(basename "$path" | cut -d'-' -f1)
    local i
    for i in $(seq 1 20); do
        if curl -s -f "$CACHE_URL/${hash}.narinfo" > /dev/null; then
            return 0
        fi
        sleep 0.5
    done
    return 1
}

test_alpine_nix() {
    local test_store_path
    test_store_path=$(nix-build -E 'with import <nixpkgs> {}; writeText "alpine-nix-test" "hello from cache to alpine"' --no-out-link)
    nix copy --to "$CACHE_UPLOAD_URL?secret-key=$CACHE_SECRET_KEY_PATH" "$test_store_path"

    # exercise the full local-cache path: daemon connectivity, substitution,
    # store-db queries, and a build via *both* the classic (nix-build) and the
    # new flakes/nix-command (nix build) frontends. the two build derivations
    # use distinct names so we can confirm each got uploaded back to the cache.
    local out
    out=$(run_vm --spec "$ALPINE_IMAGE_SPEC_JSON" --name "alpine-nix" --timeout "180s" --upload -- /bin/sh -lc '
set -eu
export HOME=/workspace
store_path=$1

echo "nix_version=$(nix --version | head -n1)"
{ nix store info >/dev/null 2>&1 || nix store ping >/dev/null 2>&1; } && echo "daemon=ok"

# substitute a path from the cache and query the store db about it
nix-store --realise "$store_path" >/dev/null
echo "substituted=$(cat "$store_path")"
echo "requisites=$(nix-store --query --requisites "$store_path" | wc -l | tr -d " ")"
nix path-info --json "$store_path" >/dev/null && echo "path_info=ok"

# build via the new CLI; the substituted path is declared as a real input
# (builtins.storePath) so nix must realise it into the build sandbox first.
# heredoc is unquoted (the outer guest script is single-quoted, so a quoted
# delimiter would close it), hence \$ escapes what nix/the builder must expand.
export DEP="$store_path"
cat > /workspace/new.nix <<NIXEOF
let dep = builtins.storePath (builtins.getEnv "DEP"); in
derivation {
  name = "alpine-nix-build-new";
  system = "x86_64-linux";
  builder = "/bin/sh";
  # the sandbox only provides the sh builtin shell (no coreutils in PATH), so
  # stick to builtins: the build only succeeds if nix realised the declared
  # storePath dependency into the sandbox, where [ -r ] can see it.
  args = [ "-c" "[ -r \${dep} ] && echo via-nix-build-with-dep > \$out" ];
}
NIXEOF
new_path=$(nix build --impure --file /workspace/new.nix --no-link --print-out-paths)
echo "new_path=$new_path"
echo "new_content=$(tr "\n" "|" < "$new_path")"

# build via the classic CLI
cat > /workspace/old.nix <<NIXEOF
derivation {
  name = "alpine-nix-build-old";
  system = "x86_64-linux";
  builder = "/bin/sh";
  args = [ "-c" "echo built-on-alpine > \$out" ];
}
NIXEOF
old_path=$(nix-build /workspace/old.nix --no-out-link)
echo "old_path=$old_path"
' sh "$test_store_path") || return 1

    check_needles "$out" \
        "daemon=ok" "substituted=hello from cache to alpine" "path_info=ok" \
        "requisites=[1-9][0-9]*" "new_content=via-nix-build-with-dep" || return 1

    local clean new_path old_path
    clean=$(echo "$out" | strip_ansi)
    new_path=$(echo "$clean" | grep -o 'new_path=/nix/store/[a-z0-9]*-alpine-nix-build-new' | cut -d= -f2)
    old_path=$(echo "$clean" | grep -o 'old_path=/nix/store/[a-z0-9]*-alpine-nix-build-old' | cut -d= -f2)
    if [ -z "$new_path" ] || [ -z "$old_path" ]; then
        echo "error: could not extract both built store paths from alpine guest output" >&2
        return 1
    fi
    if ! cache_has_path "$new_path"; then
        echo "error: nix-build (new CLI) output was not uploaded to the cache" >&2
        return 1
    fi
    if ! cache_has_path "$old_path"; then
        echo "error: nix-build (classic CLI) output was not uploaded to the cache" >&2
        return 1
    fi
    echo "success: alpine guest substituted, queried the store db, built via both CLIs, and uploaded both outputs"
}

TESTS=(
    test_alpine
    test_alpine_nix
    test_alpine_podman
    test_realize
    test_build_upload
    test_ssh_store_upload
    test_networking
    test_substitution_and_no_upload
    test_activation_services
    test_activation_dependencies
    test_activation_registry_pin
    test_activation_cache_substitution
    test_activation_docker
    test_activation_cached_realize
)

log "running ${#TESTS[@]} tests"
if ! run_tests; then
    exit 1
fi

SUCCESS=1
log "passed!!"
