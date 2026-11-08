# fil built from source with Nix (docs/m4/packaging.md): `nix build`, `nix run . -- --version`,
# `nix profile install github:anthonytw/fil/vX.Y.Z`. The toolchain is the one rust-toolchain.toml
# pins, from rust-overlay, because the workspace's rust-version is that toolchain and nixpkgs' rustc
# may be older. Like `cargo install`, this builds the client and the remote for this machine only:
# there is no bundle for `fil install`, which then uploads this binary or one named with --binary.
{
  description = "fil: a remote shell for development over SSH that keeps your place";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    { self, nixpkgs, rust-overlay }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" "x86_64-darwin" "aarch64-darwin" ];
      forAll = f: nixpkgs.lib.genAttrs systems (system: f (import nixpkgs {
        inherit system;
        overlays = [ rust-overlay.overlays.default ];
      }));
      cargoToml = builtins.fromTOML (builtins.readFile ./Cargo.toml);
      channel = (builtins.fromTOML (builtins.readFile ./rust-toolchain.toml)).toolchain.channel;
    in
    {
      packages = forAll (pkgs:
        let
          toolchain = pkgs.rust-bin.stable.${channel}.minimal;
          rustPlatform = pkgs.makeRustPlatform { cargo = toolchain; rustc = toolchain; };
        in
        {
          fil = rustPlatform.buildRustPackage {
            pname = "fil-shell";
            version = cargoToml.workspace.package.version;
            src = pkgs.lib.cleanSource ./.;
            cargoLock.lockFile = ./Cargo.lock;
            cargoBuildFlags = [ "-p" "fil-shell" "--bin" "fil" ];
            # The workspace's tests need PTYs, sockets and real time; `just ci` runs them.
            doCheck = false;
            # Not a release build (no remap of the builder's paths): `fil install` will not upload it.
            meta = {
              description = "Remote shell for development over SSH that keeps your place";
              homepage = "https://github.com/anthonytw/fil";
              license = pkgs.lib.licenses.gpl3Plus;
              mainProgram = "fil";
              platforms = systems;
            };
          };
          default = self.packages.${pkgs.stdenv.hostPlatform.system}.fil;
        });

      apps = forAll (pkgs: {
        default = {
          type = "app";
          program = "${self.packages.${pkgs.stdenv.hostPlatform.system}.fil}/bin/fil";
        };
      });
    };
}
