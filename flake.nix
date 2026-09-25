{
  description = "An experimental Rust-native build tool for React apps";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  };

  outputs = { self, nixpkgs }:
    let
      version = (builtins.fromTOML (builtins.readFile ./Cargo.toml)).workspace.package.version;
      systems = [ "x86_64-linux" "aarch64-linux" "x86_64-darwin" "aarch64-darwin" ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
      # Only what the cargo build reads: a README or e2e change must not
      # change the derivation, or every non-Rust commit forces consumers
      # into a full rebuild.
      src = nixpkgs.lib.fileset.toSource {
        root = ./.;
        fileset = nixpkgs.lib.fileset.unions [
          ./Cargo.toml
          ./Cargo.lock
          ./crates
        ];
      };
      # The v8 crate's build script downloads a prebuilt static lib, which the
      # nix sandbox forbids; fetch it as a fixed-output derivation and hand it
      # over via RUSTY_V8_ARCHIVE (the same approach nixpkgs uses for deno).
      # On a rusty_v8 bump: update the version and re-prefetch the four hashes
      # (nix store prefetch-file <url>).
      rustyV8Version = "150.4.0";
      rustyV8Hashes = {
        aarch64-darwin = "sha256-Wu/9jVoMG3msHXCvg9WxkJllX9nGRaeU3EPxAfd5g4w=";
        x86_64-darwin = "sha256-p1AnH+xrIRRX7Qpc99LqsZJLJlYhqC2oarlZ1v8II+Q=";
        aarch64-linux = "sha256-U54oOBWjlqV5bzKFi0LlF7hY66rqqtBdAykO6MhkpSc=";
        x86_64-linux = "sha256-9IdiyhDR8fxgWkQcWuQw7Izh6egPFNePvELLh4wwtHY=";
      };
      rustyV8Target = {
        aarch64-darwin = "aarch64-apple-darwin";
        x86_64-darwin = "x86_64-apple-darwin";
        aarch64-linux = "aarch64-unknown-linux-gnu";
        x86_64-linux = "x86_64-unknown-linux-gnu";
      };
      rustyV8Archive = pkgs: pkgs.fetchurl {
        # The "_simdutf" archive variant matches the v8 crate's feature set here
        # (deno_core enables v8/simdutf; build.rs appends "_simdutf" to the name).
        url = "https://github.com/denoland/rusty_v8/releases/download/v${rustyV8Version}/librusty_v8_simdutf_release_${rustyV8Target.${pkgs.stdenv.hostPlatform.system}}.a.gz";
        hash = rustyV8Hashes.${pkgs.stdenv.hostPlatform.system};
      };
      # Pinned V8 snapshot bundles (issue #211): snapshot CREATION is not
      # run-deterministic (rusty_v8 serializes live embedder memory into the
      # blob), so reproducible builds consume a prebuilt per-target bundle via
      # crates/oj_deno_snapshots. The bundles are GitHub release assets (same
      # shape as RUSTY_V8_ARCHIVE above: fetched by SRI hash, uniquely named,
      # never overwritten — old revisions keep building), so ~8MB of generated
      # artifacts stay out of the git tree. The release pipeline regenerates
      # them: on a version tag, release.yml compares the pinned manifest with
      # Cargo.lock and, when a deno_runtime / deno_core bump made it stale,
      # harvests a fresh bundle onto that version's release and opens a PR
      # updating release/file/hash here (tools/gen-snapshot-pin.sh is the
      # manual fallback). Systems without a pin build the snapshot live, like
      # plain cargo does — their output is then not bit-reproducible, which
      # only the darwin cache's two-builder agreement actually requires today.
      snapshotPins = {
        aarch64-darwin = {
          release = "snapshot-pins";
          file = "oj-snapshot-pin-aarch64-apple-darwin-dc0.412.0-6f8e4d21.tar.gz";
          hash = "sha256-RTe+9f4HGqaIm417kZ49C8HbYtGX02gi3Jij6L/e3ko=";
        };
      };
      snapshotPin = pkgs:
        let pin = snapshotPins.${pkgs.stdenv.hostPlatform.system};
        in pkgs.runCommand "oj-snapshot-pin" { } ''
          mkdir -p "$out"
          tar -xzf ${pkgs.fetchurl {
            url = "https://github.com/lovablelabs/oj/releases/download/${pin.release}/${pin.file}";
            hash = pin.hash;
          }} -C "$out"
        '';
      mkOj = pkgs:
        pkgs.rustPlatform.buildRustPackage ({
          pname = "oj";
          inherit version src;
          # All dependencies are crates.io; the checked-in lockfile is the
          # single source of truth, so no fixed-output hash to maintain.
          cargoLock.lockFile = ./Cargo.lock;
          # deno_core's extension! records each extension JS file by absolute
          # path via env!("CARGO_MANIFEST_DIR"), which --remap-path-prefix
          # cannot rewrite (env! expands before remapping). Compile from store
          # paths instead of the (randomized) build directory so the recorded
          # paths — and therefore the binary — are identical on every builder:
          # the vendor tree is used straight from the store (the symlink is
          # realpath-resolved by cargoSetupHook), and the workspace itself is
          # built from $src via --manifest-path.
          cargoDepsHook = ''
            ln -s "$cargoDeps" "$sourceRoot/nix-vendor"
            cargoVendorDir=nix-vendor
          '';
          cargoBuildFlags = [ "--manifest-path" "${src}/Cargo.toml" "-p" "oj" ];
          # bindgenHook provides libclang for libsqlite3-sys (a deno_runtime
          # transitive dep) whose build script runs bindgen. nukeReferences
          # breaks the compile-time-only store-path references (see postFixup).
          nativeBuildInputs = [ pkgs.pkg-config pkgs.rustPlatform.bindgenHook pkgs.nukeReferences ]
            # signIfRequired for the post-nuke re-sign; python3 for the LC_UUID
            # rewrite; patchelf for the RUNPATH keep-list.
            ++ nixpkgs.lib.optionals pkgs.stdenv.isDarwin [ pkgs.darwin.autoSignDarwinBinariesHook pkgs.python3 ]
            ++ nixpkgs.lib.optionals pkgs.stdenv.isLinux [ pkgs.patchelf ];
          buildInputs = [ pkgs.openssl pkgs.sqlite ] ++ nixpkgs.lib.optionals pkgs.stdenv.isDarwin [ pkgs.libiconv ];
          RUSTY_V8_ARCHIVE = rustyV8Archive pkgs;
          # The build directory name varies between nix implementations (and is
          # randomized on some), and rustc embeds dependency source paths from
          # the vendored tree in panic locations — remap them so the output is
          # independent of where it was built (bit-reproducibility, #188).
          preBuild = ''
            # importCargoLock's shipped config names the vendor dir relative to
            # the build dir (where the skipped copy would have been); point it
            # at the store tree instead.
            sed -i 's|directory = "cargo-vendor-dir"|directory = "'"$cargoDeps"'"|' \
              "$NIX_BUILD_TOP/.cargo/config.toml"
            # Subprocesses that run from the store workspace (cargo-auditable's
            # cargo metadata) can't discover the config by walking up from
            # their cwd; CARGO_HOME config is loaded from anywhere.
            export CARGO_HOME="$NIX_BUILD_TOP/.cargo"
            # The workspace builds from the read-only store src, so the target
            # dir must be redirected somewhere writable — and stay at the
            # cwd-relative target/ that cargoInstallHook expects.
            export CARGO_TARGET_DIR="$PWD/target"
            export RUSTFLAGS="''${RUSTFLAGS:+$RUSTFLAGS }--remap-path-prefix $NIX_BUILD_TOP=/build"
            export NIX_CFLAGS_COMPILE="''${NIX_CFLAGS_COMPILE:-} -ffile-prefix-map=$NIX_BUILD_TOP=/build"
          '';
          # A raw $NIX_BUILD_TOP path in the binary escaped the remapping
          # (env!()-derived strings do) and makes the output depend on where
          # it was built; fail rather than ship it. When the build dir IS the
          # remap target (the Linux sandbox builds in a constant /build),
          # remapped and raw paths are the same constant bytes — deterministic
          # either way, and innocent strings like "src/build.rs" would match —
          # so there is nothing to check.
          postInstall = ''
            if [ "$NIX_BUILD_TOP" != /build ] && grep -aqF "$NIX_BUILD_TOP" "$out/bin/oj"; then
              echo "error: build dir $NIX_BUILD_TOP leaked into bin/oj; the build is not reproducible" >&2
              strings "$out/bin/oj" | grep -F "$NIX_BUILD_TOP" | head -5 >&2 || true
              exit 1
            fi
          '';
          # Compiling from store paths embeds $src and vendor-crate store paths
          # in the binary, and the reference scanner would promote the whole
          # vendor tree (~1.2GiB) to a RUNTIME dependency of every consumer.
          # The strings are compile-time-only (deno_core reads them while
          # building the snapshot; dead bytes at runtime — a crates.io-built oj
          # carries ~/.cargo paths and runs fine without them), so break the
          # references: nuke-refs rewrites their hash part to a constant,
          # keeping the bytes deterministic while restoring the ~170MiB
          # closure. The binary's REAL link-time references (dylib install
          # names, the ELF interpreter and RUNPATH) are collected first and
          # kept. Runs in postFixup, i.e. after darwin's auto-signing, so the
          # mutated binary is re-signed explicitly.
          postFixup = ''
            keep=""
            if [ "$(uname)" = Darwin ]; then
              for p in $(otool -L "$out/bin/oj" | grep -o '/nix/store/[a-z0-9]\{32\}-[^/ ]*' | sort -u); do
                keep="$keep -e $p"
              done
              # LC_RPATH entries are load commands too (otool -l, not -L);
              # today's binary has none, but a future store-pathed rpath must
              # survive the nuke like the dylib install names do.
              for p in $(otool -l "$out/bin/oj" | grep -A2 LC_RPATH | grep ' path /nix/store/' | grep -o '/nix/store/[a-z0-9]\{32\}-[^/ ]*' | sort -u); do
                keep="$keep -e $p"
              done
            else
              for p in $( (patchelf --print-rpath "$out/bin/oj" | tr ':' '\n'; patchelf --print-interpreter "$out/bin/oj" 2>/dev/null) | grep -o '^/nix/store/[a-z0-9]\{32\}-[^/]*' | sort -u ); do
                keep="$keep -e $p"
              done
            fi
            nuke-refs $keep "$out/bin/oj"
            for bad in "$cargoDeps" ${src}; do
              h=$(basename "$bad" | cut -c1-32)
              if grep -aqF "$h" "$out/bin/oj"; then
                echo "error: build-input store path $bad still referenced from bin/oj after nuke-refs" >&2
                exit 1
              fi
            done
            if [ "$(uname)" = Darwin ]; then
              # ld64 seeds LC_UUID from link-time state that includes the
              # randomized build-dir object paths (strip removes the paths,
              # the UUID survives), so two builders differed in exactly the
              # UUID plus the code directory's page-0 hash over it. Rewrite
              # it as a hash of the post-strip content (UUID and signature
              # regions zeroed in the hash input): identical across builders,
              # still unique per real change. The re-sign below covers it.
              python3 - "$out/bin/oj" <<'PYUUID'
import hashlib, struct, sys
p = sys.argv[1]
d = bytearray(open(p, "rb").read())
assert struct.unpack_from("<I", d, 0)[0] == 0xFEEDFACF, "not a 64-bit macho"
ncmds = struct.unpack_from("<I", d, 16)[0]
off = 32
uuid_off = None
sig_off = len(d)
for _ in range(ncmds):
    cmd, size = struct.unpack_from("<II", d, off)
    if cmd == 0x1B:
        uuid_off = off + 8
    if cmd == 0x1D:
        sig_off = struct.unpack_from("<II", d, off + 8)[0]
    off += size
assert uuid_off is not None, "no LC_UUID"
h = hashlib.sha256()
h.update(d[:uuid_off])
h.update(bytes(16))
h.update(d[uuid_off + 16 : sig_off])
d[uuid_off : uuid_off + 16] = h.digest()[:16]
open(p, "wb").write(d)
PYUUID
              if type -t signIfRequired > /dev/null; then
                signIfRequired "$out/bin/oj"
              elif command -v codesign > /dev/null; then
                codesign -f -s - "$out/bin/oj"
              else
                echo "error: no darwin signing helper available after mutating bin/oj" >&2
                exit 1
              fi
            fi
          '';
          # The test suite drives real Node sidecars and network fixtures; it
          # runs in CI, not inside the sandboxed nix build.
          doCheck = false;
          meta = {
            description = "An experimental Rust-native build tool for React apps";
            homepage = "https://github.com/lovablelabs/oj";
            license = nixpkgs.lib.licenses.mit;
            mainProgram = "oj";
          };
        } // nixpkgs.lib.optionalAttrs (snapshotPins ? ${pkgs.stdenv.hostPlatform.system}) {
          OJ_SNAPSHOT_ARCHIVE = snapshotPin pkgs;
        });
    in
    {
      packages = forAllSystems (pkgs: rec {
        oj = mkOj pkgs;
        default = oj;
      });
      overlays.default = final: prev: { oj = mkOj final; };
    };
}
