{
  description = "faff — a TUI for running several Claude Code agents in parallel on one repo";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = {
    self,
    nixpkgs,
    flake-utils,
  }:
    flake-utils.lib.eachDefaultSystem (
      system: let
        pkgs = import nixpkgs {inherit system;};

        # Runtime dependencies faff shells out to by bare name. jj drives the
        # workspaces and wezterm drives the panes. `claude` is intentionally NOT
        # here: faff spawns it via `wezterm cli spawn -- claude ...`, so it runs
        # in the user's interactive shell inside the pane, not from faff's PATH.
        runtimeDeps = [
          pkgs.jujutsu # provides `jj`
          pkgs.wezterm # provides `wezterm`
        ];

        cargoToml = pkgs.lib.importTOML ./Cargo.toml;

        faff = pkgs.rustPlatform.buildRustPackage {
          pname = cargoToml.package.name;
          version = cargoToml.package.version;

          src = self;

          cargoLock.lockFile = ./Cargo.lock;

          nativeBuildInputs = [pkgs.makeWrapper];

          # rusqlite's `bundled` feature compiles SQLite from C; the stdenv cc
          # covers that, no extra buildInputs needed.

          # The test suite (115 tests) shells out to a real `jj`, which is not
          # available in the build sandbox, so skip it during the Nix build.
          doCheck = false;

          postInstall = ''
            wrapProgram $out/bin/faff \
              --prefix PATH : ${pkgs.lib.makeBinPath runtimeDeps}
          '';

          meta = {
            description = "TUI for running several Claude Code agents in parallel on one repo, each in its own jj workspace and WezTerm pane";
            homepage = "https://github.com/Jezza/faff";
            mainProgram = "faff";
          };
        };
      in {
        packages.default = faff;
        packages.faff = faff;

        apps.default = {
          type = "app";
          program = "${faff}/bin/faff";
        };

        devShells.default = pkgs.mkShell {
          inputsFrom = [faff];
          packages =
            runtimeDeps
            ++ [
              pkgs.cargo
              pkgs.rustc
              pkgs.clippy
              pkgs.rustfmt
            ];
        };
      }
    );
}
