#!/usr/bin/env bash
# Run oj's build, tests and dev servers on Linux from a Mac, inside a container
# (Docker, or Colima's containerd through `colima nerdctl`).
#
#   tools/linux.sh deps                      Linux node_modules for the e2e fixtures
#   tools/linux.sh build [cargo args]        cargo build -p oj (or the given args)
#   tools/linux.sh run [opts] -- CMD...      run CMD from the repo root
#   tools/linux.sh shell [opts]              an interactive shell
#
# opts: --cpus N, --memory SIZE (a sandbox's limits), -p PORT (publish it),
#       --modules DIR (give DIR its own Linux node_modules; repeatable).
#
# $HOME is mounted at its own path, so absolute paths and worktree symlinks
# resolve as on the Mac. Linux artifacts never land in the checkout: the
# repo's target/ and every node_modules/ are named volumes in the container,
# keyed on the directory's path, and caches live in the container's own home.
set -euo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
image=""

if command -v docker >/dev/null 2>&1 && docker info >/dev/null 2>&1; then
    ct=(docker)
elif command -v colima >/dev/null 2>&1; then
    colima status >/dev/null 2>&1 || colima start
    ct=(colima nerdctl --)
else
    echo "tools/linux.sh: needs Docker or Colima (brew install colima)" >&2
    exit 1
fi

# A volume name for a host directory: readable, plus a short hash of its path.
volume_for() {
    local dir="$1" tag
    tag="$(printf '%s' "$dir" | shasum | cut -c1-8)"
    echo "oj-linux-$(basename "$(dirname "$dir")")-$(basename "$dir")-$tag" | tr -c 'a-zA-Z0-9_.\n-' '-'
}

# The image is tagged by its inputs, so a toolchain bump rebuilds it.
ensure_image() {
    local rust playwright
    rust="$(sed -n 's/^channel = "\(.*\)"/\1/p' "$repo/rust-toolchain.toml")"
    playwright="$(sed -n 's/.*"playwright": "^\{0,1\}\([0-9.]*\)".*/\1/p' "$repo/e2e/package.json")"
    image="oj-linux:$(cat "$repo/tools/linux/Dockerfile" <(echo "$rust $playwright") | shasum | cut -c1-12)"
    if "${ct[@]}" image inspect "$image" >/dev/null 2>&1; then
        return
    fi
    "${ct[@]}" build -t "$image" \
        --build-arg "RUST_TOOLCHAIN=$rust" \
        --build-arg "PLAYWRIGHT_VERSION=$playwright" \
        "$repo/tools/linux"
}

# The package dirs whose node_modules must be Linux builds.
module_dirs=("$repo/e2e" "$repo/playground")
for fixture in "$repo"/e2e/fixtures/*/; do
    [ -f "$fixture/package.json" ] && module_dirs+=("${fixture%/}")
done

cpus="" memory="" ports=() cmd=()
parse_opts() {
    while [ $# -gt 0 ]; do
        case "$1" in
            --cpus) cpus="$2"; shift 2 ;;
            --memory) memory="$2"; shift 2 ;;
            -p) ports+=("$2"); shift 2 ;;
            --modules) module_dirs+=("$(cd "$2" && pwd -P)"); shift 2 ;;
            --) shift; cmd=("$@"); return ;;
            *) cmd=("$@"); return ;;
        esac
    done
}

container() {
    ensure_image
    local args=(run --rm --init -w "$repo"
        -v "$HOME:$HOME"
        -v "$(volume_for "$repo/target"):$repo/target"
        -v oj-linux-cargo-registry:/usr/local/cargo/registry
        -v oj-linux-cargo-git:/usr/local/cargo/git
        -v oj-linux-home:/root
        -e HOME=/root
        -e PLAYWRIGHT_BROWSERS_PATH=/root/ms-playwright)
    local dir modules
    for dir in "${module_dirs[@]}"; do
        modules="$dir/node_modules"
        # A worktree's node_modules is often a symlink into the main
        # checkout: mount where it points, which the $HOME mount also holds.
        if [ -L "$modules" ]; then
            modules="$(cd "$modules" 2>/dev/null && pwd -P)" || continue
        fi
        args+=(-v "$(volume_for "$modules"):$modules")
    done
    [ -n "$cpus" ] && args+=(--cpus "$cpus")
    [ -n "$memory" ] && args+=(--memory "$memory")
    local port
    for port in "${ports[@]+"${ports[@]}"}"; do
        args+=(-p "$port:$port")
    done
    [ -t 0 ] && [ -t 1 ] && args+=(-it)
    # nerdctl notes every named volume that already exists; that's expected.
    "${ct[@]}" "${args[@]}" "$image" "$@" 2> >(grep -v 'will be returned as-is' >&2)
}

sub="${1:-}"
[ $# -gt 0 ] && shift
case "$sub" in
    deps)
        parse_opts "$@"
        script='set -e
for dir in "$@"; do
    if [ -f "$dir/package-lock.json" ]; then
        npm ci --prefix "$dir"
    else
        npm install --no-package-lock --prefix "$dir"
    fi
done
npx --prefix e2e playwright install chromium'
        container bash -c "$script" deps "${module_dirs[@]}"
        ;;
    build)
        # Every argument is cargo's (`-p` names a package here).
        [ $# -eq 0 ] && set -- -p oj
        container cargo build "$@"
        ;;
    run)
        parse_opts "$@"
        [ ${#cmd[@]} -eq 0 ] && { echo "tools/linux.sh run: no command" >&2; exit 2; }
        container "${cmd[@]}"
        ;;
    shell)
        parse_opts "$@"
        container bash
        ;;
    *)
        sed -n '2,12p' "$0" | sed 's/^# \{0,1\}//'
        exit 2
        ;;
esac
