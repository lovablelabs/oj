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
      mkOj = pkgs:
        pkgs.rustPlatform.buildRustPackage {
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
          # transitive dep) whose build script runs bindgen.
          nativeBuildInputs = [ pkgs.pkg-config pkgs.rustPlatform.bindgenHook ];
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
          # The test suite drives real Node sidecars and network fixtures; it
          # runs in CI, not inside the sandboxed nix build.
          doCheck = false;
          meta = {
            description = "An experimental Rust-native build tool for React apps";
            homepage = "https://github.com/lovablelabs/oj";
            license = nixpkgs.lib.licenses.mit;
            mainProgram = "oj";
          };
        };
    in
    {
      packages = forAllSystems (pkgs: rec {
        oj = mkOj pkgs;
        default = oj;
      });
      overlays.default = final: prev: { oj = mkOj final; };
    };
}
