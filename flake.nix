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
      self,
      nixpkgs,
      flake-utils,
      rust-overlay,
      ...
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        overlays = [ (import rust-overlay) ];
        pkgs = import nixpkgs {
          inherit system overlays;
        };

        rustToolchain = pkgs.rust-bin.stable."1.74.0".default.override {
          extensions = [
            "rustfmt"
            "clippy"
            "rust-src"
            "rust-docs"
            "rust-analyzer"
          ];
        };

        # Build with the pinned toolchain rather than whatever nixpkgs ships, so the
        # package and the dev shell cannot disagree about the compiler.
        rustPlatform = pkgs.makeRustPlatform {
          cargo = rustToolchain;
          rustc = rustToolchain;
        };

        src = pkgs.lib.cleanSourceWith {
          src = ./.;
          filter =
            path: type:
            let
              name = baseNameOf (toString path);
            in
            name != "target"
            && name != "mutants.out"
            && name != "coding_challenge.md"
            && pkgs.lib.cleanSourceFilter path type;
        };

        package =
          {
            features ? [ ],
          }:
          rustPlatform.buildRustPackage {
            pname = "transaction-solver";
            version = "0.1.0";
            inherit src;

            cargoLock.lockFile = ./Cargo.lock;
            buildFeatures = features;
            checkFeatures = features;

            meta = {
              description = "A toy payments engine that applies a CSV transaction log to client accounts";
              license = pkgs.lib.licenses.mit;
              mainProgram = "transaction_solver";
            };
          };

        transactionSolver = package { };
      in
      {
        packages = {
          default = transactionSolver;
          transaction-solver = transactionSolver;
          # The same engine with withdrawal disputes compiled in.
          with-dispute-withdraw = package { features = [ "dispute-withdraw" ]; };
        };

        # `nix build` runs the test suite in the derivation's check phase, so this is the
        # whole suite, hermetically, on the pinned toolchain. CI runs exactly this, which
        # is the point: `nix flake check` locally gives the same answer as the pipeline.
        checks = {
          default = transactionSolver;
          with-dispute-withdraw = package { features = [ "dispute-withdraw" ]; };

          # Lints both configurations. `--all-features` on its own would only ever check
          # the one where withdrawal disputes are compiled in.
          clippy = rustPlatform.buildRustPackage {
            pname = "transaction-solver-clippy";
            version = "0.1.0";
            inherit src;
            cargoLock.lockFile = ./Cargo.lock;
            nativeBuildInputs = [ rustToolchain ];
            buildPhase = ''
              runHook preBuild
              cargo clippy --all-targets -- -D warnings
              cargo clippy --all-targets --all-features -- -D warnings
              runHook postBuild
            '';
            doCheck = false;
            installPhase = "touch $out";
          };

          # `rustfmt` directly rather than `cargo fmt`: the latter shells out to
          # `cargo metadata`, which wants to resolve the dependency graph, and a build
          # sandbox has no network.
          rustfmt = pkgs.runCommand "check-rustfmt" { nativeBuildInputs = [ rustToolchain ]; } ''
            find ${src} -name '*.rs' -exec rustfmt --check --edition 2021 {} +
            touch $out
          '';

          nixfmt = pkgs.runCommand "check-nixfmt" { nativeBuildInputs = [ pkgs.nixfmt ]; } ''
            nixfmt --check ${src}/flake.nix
            touch $out
          '';
        };

        devShells.default = pkgs.mkShell {
          nativeBuildInputs = with pkgs; [
            pkg-config
            rustToolchain

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

        apps.default = {
          type = "app";
          program = pkgs.lib.getExe transactionSolver;
        };

        # A reproducible benchmark: `nix run .#bench`.
        #
        # Every parameter has a fixed default, and the generator is seeded, so two runs on
        # two machines process byte-identical input. The input is cached under BENCH_DIR
        # and keyed by its parameters, so re-running does not regenerate a gigabyte.
        #
        # Override with environment variables:
        #   BYTES=256MiB MIX=settled CLIENTS=1000 SEED=42 nix run .#bench
        apps.bench = {
          type = "app";
          program = "${
            pkgs.writeShellApplication {
              name = "bench";
              runtimeInputs = with pkgs; [
                rustToolchain
                time
                coreutils
                gawk
              ];
              text = ''
                seed="''${SEED:-20260816}"
                bytes="''${BYTES:-1GiB}"
                clients="''${CLIENTS:-65535}"
                mix="''${MIX:-balanced}"
                workdir="''${BENCH_DIR:-''${TMPDIR:-/tmp}/transaction-solver-bench}"

                mkdir -p "$workdir"
                input="$workdir/tx-$mix-$clients-$seed-$bytes.csv"
                accounts="$workdir/accounts.csv"
                rejected="$workdir/rejected.log"
                timing="$workdir/timing.txt"

                echo "building release binaries..." >&2
                cargo build --release --locked --quiet
                cargo build --release --locked --quiet --example generate_transactions

                if [[ -f "$input" ]]; then
                  echo "reusing cached input" >&2
                else
                  echo "generating $input" >&2
                  ./target/release/examples/generate_transactions \
                    --seed "$seed" --bytes "$bytes" --clients "$clients" --mix "$mix" > "$input"
                fi

                input_bytes="$(stat -c %s "$input")"
                input_records="$(( $(wc -l < "$input") - 1 ))"

                printf '\n  seed      %s\n' "$seed"
                printf '  mix       %s, %s clients\n' "$mix" "$clients"
                printf '  input     %s, %s records\n' \
                  "$(numfmt --to=iec "$input_bytes")" "$input_records"
                printf '  input sha %s\n' "$(sha256sum "$input" | cut -c1-32)"

                command time -o "$timing" -f '%e %M %U %S' \
                  ./target/release/transaction_solver "$input" > "$accounts" 2> "$rejected"
                read -r elapsed rss user sys < "$timing"

                printf '\n  wall      %s s\n' "$elapsed"
                printf '  cpu       %s s user, %s s sys\n' "$user" "$sys"
                printf '  peak rss  %s\n' "$(numfmt --to=iec "$(( rss * 1024 ))")"
                awk -v b="$input_bytes" -v r="$input_records" -v e="$elapsed" \
                  'BEGIN { if (e <= 0) e = 0.001;
                           printf "  rate      %.0f records/s, %.0f MB/s\n", r / e, b / e / 1000000 }'

                printf '\n  accounts  %s\n' "$(( $(wc -l < "$accounts") - 1 ))"
                printf '  rejected  %s\n' "$(wc -l < "$rejected")"
                printf '  output sha %s\n' "$(sha256sum "$accounts" | cut -c1-32)"
              '';
            }
          }/bin/bench";
        };

        apps.fmt = {
          type = "app";
          program = "${
            pkgs.writeShellApplication {
              name = "fmt";
              runtimeInputs = with pkgs; [
                rustToolchain
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
    );
}
