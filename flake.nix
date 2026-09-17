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
          cargoBuildFlags = [ "-p" "oj" ];
          nativeBuildInputs = [ pkgs.pkg-config ];
          buildInputs = [ pkgs.openssl ] ++ nixpkgs.lib.optionals pkgs.stdenv.isDarwin [ pkgs.libiconv ];
          RUSTY_V8_ARCHIVE = rustyV8Archive pkgs;
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
