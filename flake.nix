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

        rustToolchain = pkgs.rust-bin.stable."1.71.0".default.override {
          extensions = [
            "rustfmt"
            "clippy"
            "rust-src"
            "rust-docs"
            "rust-analyzer"
          ];
        };
      in
      {
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

          ];

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
