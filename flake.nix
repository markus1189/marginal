{
  description = "marginal — block-range annotation for markdown, in a terminal";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = { self, nixpkgs }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" "x86_64-darwin" "aarch64-darwin" ];
      forAll = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
    in
    {
      packages = forAll (pkgs: rec {
        marginal = pkgs.rustPlatform.buildRustPackage {
          pname = "marginal";
          version = "0.1.0";
          src = ./.;
          cargoLock.lockFile = ./Cargo.lock;
        };
        default = marginal;
      });

      devShells = forAll (pkgs: {
        default = pkgs.mkShell {
          packages = with pkgs; [
            cargo rustc rustfmt clippy rust-analyzer
            # checks — see ./check
            cargo-deny      # advisories, licenses, duplicate deps
            cargo-machete   # unused dependencies
            typos           # spelling, config in _typos.toml
            taplo           # TOML formatting
            # not wired into ./check; slow, run by hand
            cargo-mutants   # does the suite actually catch anything?
            bacon           # watch loop
          ];
        };
      });
    };
}
