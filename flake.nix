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
