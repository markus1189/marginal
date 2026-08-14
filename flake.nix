{
  description = "marginal — block-range annotation for markdown, in a terminal";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = { self, nixpkgs }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" "x86_64-darwin" "aarch64-darwin" ];
      forAll = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});

      fs = nixpkgs.lib.fileset;

      # Every check sees the smallest set of files that can change its result,
      # so editing the README no longer rebuilds clippy. `src = ./.` used to put
      # all ~78K of README/STATUS/AGENTS/docs prose into the crate's build hash.
      crate = fs.unions [ ./Cargo.toml ./Cargo.lock ./src ];
      # `cargo metadata`, which cargo-deny shells out to, refuses a manifest
      # with no target — so main.rs has to be present even though nothing reads
      # it. Listing only the entry point keeps edits to the other modules from
      # re-running the check. If a src/lib.rs is ever added this fails loudly
      # with "no targets specified", which is the right way to find out.
      graph = fs.unions [ ./Cargo.toml ./Cargo.lock ./deny.toml ./src/main.rs ];
      manifest = ./Cargo.toml;
      extension = ./.pi/extensions;
      # typos reads prose too. Subtracting the lock files rather than listing
      # what to include means new docs are covered the day they are added.
      everything = fs.difference ./. (fs.unions [ ./Cargo.lock ./flake.lock ]);

      sourceOf = fileset: fs.toSource { root = ./.; inherit fileset; };

      # A check that needs the resolved dependency graph. The vendor directory
      # is a fixed-output derivation shared by all of them, so it is fetched
      # once no matter how many checks ask for it.
      cargoCheck = pkgs: { name, fileset, tools ? [ ], script }:
        pkgs.stdenv.mkDerivation {
          name = "marginal-check-${name}";
          src = sourceOf fileset;
          cargoDeps = pkgs.rustPlatform.importCargoLock { lockFile = ./Cargo.lock; };
          nativeBuildInputs = [ pkgs.cargo pkgs.rustc pkgs.rustPlatform.cargoSetupHook ] ++ tools;
          buildPhase = ''
            runHook preBuild
            ${script}
            runHook postBuild
          '';
          installPhase = ''
            runHook preInstall
            touch $out
            runHook postInstall
          '';
          dontFixup = true;
        };

      # A check that only needs files and one tool. The tree is copied writable
      # because some tools want to create scratch files next to the sources.
      toolCheck = pkgs: { name, fileset, tools, script }:
        pkgs.runCommand "marginal-check-${name}" { nativeBuildInputs = tools; } ''
          cp -r ${sourceOf fileset} tree
          chmod -R u+w tree
          cd tree
          ${script}
          touch $out
        '';
    in
    {
      packages = forAll (pkgs: rec {
        marginal = pkgs.rustPlatform.buildRustPackage {
          pname = "marginal";
          version = "0.1.0";
          src = sourceOf crate;
          cargoLock.lockFile = ./Cargo.lock;
        };
        default = marginal;
      });

      # `nix flake check` is the whole suite. ./check is a thin wrapper over it
      # plus the one check that cannot run in a sandbox — see below.
      checks = forAll (pkgs: {
        # buildRustPackage leaves doCheck at true, so building the package
        # compiles it *and* runs `cargo test`. There is no separate test check
        # because this one already is it.
        build = self.packages.${pkgs.system}.marginal;

        fmt = cargoCheck pkgs {
          name = "fmt";
          fileset = crate;
          tools = [ pkgs.rustfmt ];
          script = "cargo fmt --check";
        };

        clippy = cargoCheck pkgs {
          name = "clippy";
          fileset = crate;
          tools = [ pkgs.clippy ];
          script = "cargo clippy --all-targets --offline -- -D warnings";
        };

        # `advisories` is deliberately missing: it fetches the RustSec database
        # over the network, which the build sandbox forbids, and pinning the
        # database would mean naming cargo-deny's internal db-path hash
        # directory. ./check and the scheduled CI job run it where there is a
        # network; the three offline checks run on every build.
        deny = cargoCheck pkgs {
          name = "deny";
          fileset = graph;
          tools = [ pkgs.cargo-deny ];
          script = "cargo deny --offline check bans licenses sources";
        };

        machete = toolCheck pkgs {
          name = "machete";
          fileset = crate;
          tools = [ pkgs.cargo pkgs.cargo-machete ];
          script = "cargo machete";
        };

        taplo = toolCheck pkgs {
          name = "taplo";
          fileset = manifest;
          tools = [ pkgs.taplo ];
          script = "taplo fmt --check Cargo.toml";
        };

        typos = toolCheck pkgs {
          name = "typos";
          fileset = everything;
          tools = [ pkgs.typos ];
          script = "typos";
        };

        # Previously skipped whenever node was absent, which was always, locally.
        extension = toolCheck pkgs {
          name = "extension";
          fileset = extension;
          tools = [ pkgs.nodejs ];
          script = "node --test .pi/extensions/marginal-annotate.test.mjs";
        };
      });

      devShells = forAll (pkgs: {
        default = pkgs.mkShell {
          packages = with pkgs; [
            cargo rustc rustfmt clippy rust-analyzer
            # checks — see ./check, which defers to `nix flake check`
            cargo-deny      # advisories, licenses, duplicate deps
            cargo-machete   # unused dependencies
            typos           # spelling, config in _typos.toml
            taplo           # TOML formatting
            # Runs the .pi extension's tests. Not a build dependency of the
            # binary, but it is a dependency of a test we commit, and leaving it
            # out made the suite environment-dependent.
            nodejs
            # not wired into the checks; slow, run by hand
            cargo-mutants   # does the suite actually catch anything?
            bacon           # watch loop
          ];
        };
      });
    };
}
