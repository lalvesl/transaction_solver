{
  description = "Some transaction solver";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      nixpkgs,
      flake-utils,
      rust-overlay,
      ...
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [ (import rust-overlay) ];
        };

        # The crate: toolchain, build platform, package and lints.
        rust = import ./nix/rust.nix { inherit pkgs; };
        # The benchmark: generator, store-resident inputs and runners.
        bench = import ./nix/bench.nix { inherit pkgs rust; };
      in
      {
        packages = {
          default = rust.transactionSolver;
          transaction-solver = rust.transactionSolver;
          # The same engine with withdrawal disputes compiled in.
          with-dispute-withdraw = rust.withDisputeWithdraw;
        }
        // bench.packages;

        # `nix build` runs the test suite in the derivation's check phase, so this is the
        # whole suite, hermetically, on the pinned toolchain. CI runs exactly this, which
        # is the point: `nix flake check` locally gives the same answer as the pipeline.
        checks = {
          default = rust.transactionSolver;
          with-dispute-withdraw = rust.withDisputeWithdraw;

          # Its own source: rust.src is narrowed to what the compiler reads, which is
          # deliberately not the Nix files.
          nixfmt =
            let
              nixSrc = pkgs.lib.fileset.toSource {
                root = ./.;
                fileset = pkgs.lib.fileset.unions [
                  ./flake.nix
                  ./nix
                ];
              };
            in
            pkgs.runCommand "check-nixfmt" { nativeBuildInputs = [ pkgs.nixfmt ]; } ''
              find ${nixSrc} -name '*.nix' -exec nixfmt --check {} +
              touch $out
            '';
        }
        // rust.checks
        // bench.checks;

        apps = {
          default = {
            type = "app";
            program = pkgs.lib.getExe rust.transactionSolver;
          };

          fmt = {
            type = "app";
            program = "${
              pkgs.writeShellApplication {
                name = "fmt";
                runtimeInputs = with pkgs; [
                  rust.rustToolchain
                  nixfmt
                  taplo
                  prettier
                  jq
                ];
                text = ''
                  nixfmt .
                  cargo fmt --all
                  taplo fmt
                  prettier --write "**/*.md"
                  while IFS= read -r -d "" f; do
                    tmp="$(mktemp)"
                    jq . "$f" > "$tmp" && mv "$tmp" "$f"
                  done < <(find . -name "*.jsonl" -not -path "./.git/*" -not -path "./target/*" -print0)
                '';
              }
            }/bin/fmt";
          };
        }
        // bench.apps;

        devShells.default = pkgs.mkShell {
          nativeBuildInputs = with pkgs; [
            pkg-config
            rust.rustToolchain

            # Code quality
            cargo-mutants
            cargo-deny
            taplo
            nixpkgs-fmt

            # Testing
            cargo-nextest

            gh
          ];
        };
      }
    );
}
