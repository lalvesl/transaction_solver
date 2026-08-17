# Everything about building the crate: the pinned toolchain, the build platform layered
# on top of it, the filtered source, and the package itself.
#
# `flake.nix` imports this and does nothing but wire the results into outputs.
{ pkgs }:

let
  inherit (pkgs) lib;

  rustToolchain = pkgs.rust-bin.stable."1.74.0".default.override {
    extensions = [
      "rustfmt"
      "clippy"
      "rust-src"
      "rust-docs"
      "rust-analyzer"
    ];
  };

  # Build with the pinned toolchain rather than whatever nixpkgs ships, so the package
  # and the dev shell cannot disagree about the compiler.
  rustPlatform = pkgs.makeRustPlatform {
    cargo = rustToolchain;
    rustc = rustToolchain;
  };

  # A whitelist, not an exclude-list. The package's store path is a hash of everything in
  # here, so anything included that the compiler never reads is a spurious rebuild waiting
  # to happen. Under the old exclude-list filter, appending a line to bench/history.jsonl
  # rebuilt the engine — and the benchmark runner appends one at the end of every run, so
  # two consecutive benchmarks were never timing the same binary.
  src = lib.fileset.toSource {
    root = ../.;
    fileset = lib.fileset.unions [
      ../Cargo.toml
      ../Cargo.lock
      ../.cargo
      ../src
      ../tests
      ../examples
    ];
  };

  cargoLock.lockFile = ../Cargo.lock;

  package =
    {
      features ? [ ],
    }:
    rustPlatform.buildRustPackage {
      pname = "transaction-solver";
      version = "0.1.0";
      inherit src cargoLock;

      buildFeatures = features;
      checkFeatures = features;

      meta = {
        description = "A toy payments engine that applies a CSV transaction log to client accounts";
        license = lib.licenses.mit;
        mainProgram = "transaction_solver";
      };
    };

  transactionSolver = package { };
  withDisputeWithdraw = package { features = [ "dispute-withdraw" ]; };
in
{
  inherit
    rustToolchain
    rustPlatform
    src
    cargoLock
    package
    transactionSolver
    withDisputeWithdraw
    ;

  checks = {
    # Lints both configurations. `--all-features` on its own would only ever check the
    # one where withdrawal disputes are compiled in.
    clippy = rustPlatform.buildRustPackage {
      pname = "transaction-solver-clippy";
      version = "0.1.0";
      inherit src cargoLock;

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
    # `cargo metadata`, which wants to resolve the dependency graph, and a build sandbox
    # has no network.
    rustfmt = pkgs.runCommand "check-rustfmt" { nativeBuildInputs = [ rustToolchain ]; } ''
      find ${src} -name '*.rs' -exec rustfmt --check --edition 2021 {} +
      touch $out
    '';
  };
}
