# Linux from a Mac

CI and the sandboxes oj ships to run Linux. `tools/linux.sh` runs oj's build,
tests and dev servers there from a Mac, in a container: Docker if it is
running, else Colima (`brew install colima`; the script starts the VM).

```sh
tools/linux.sh deps                 # Linux node_modules for e2e/, playground/, the fixtures
tools/linux.sh build                # cargo build -p oj
tools/linux.sh run -- node e2e/start-hmr-gate.mjs
tools/linux.sh shell                # bash in the repo root
```

The first call builds the image (Node 22, the toolchain in
`rust-toolchain.toml`, clang, Bun, Chromium's libraries). It is tagged by its
inputs, so a toolchain bump rebuilds it on the next call.

## What lives where

- `$HOME` is mounted at its own path, so absolute paths, worktree symlinks and
  apps outside the repo resolve as on the Mac.
- `target/` and every `node_modules/` the script knows are named volumes,
  keyed on the directory's path: Linux builds never overwrite the Mac's, and
  each worktree gets its own. The e2e scripts build and find
  `target/debug/oj` as usual.
- Cargo's registry, npm's and Bun's caches and Playwright's browsers live in
  volumes too, so later runs start warm.

An app outside the repo needs its own Linux `node_modules`:

```sh
tools/linux.sh run --modules ~/apps/shop -- npm ci --prefix ~/apps/shop
```

## A sandbox's limits

`--cpus` and `--memory` cap the container, and `-p` publishes a port to the
Mac:

```sh
tools/linux.sh build --release -p oj
tools/linux.sh run --cpus 2 --memory 4g -p 8080 --modules ~/apps/shop -- \
  target/release/oj dev --host 0.0.0.0 --port 8080 ~/apps/shop
```

The VM bounds what a container can get: Colima's default is 4 CPUs and 8 GB
(`colima stop && colima start --cpus 8 --memory 16` for more).

To compare two builds, build each in its own worktree and run both under the
same limits. Measure inside the container (`ps`, `/proc`, `/usr/bin/time -v`),
not on the Mac, where the VM's numbers mix every container together.
