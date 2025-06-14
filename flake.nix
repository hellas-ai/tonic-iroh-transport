{
  description = "Gate - P2P AI Compute Network";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay.url = "github:oxalica/rust-overlay";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, rust-overlay, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        overlays = [ (import rust-overlay) ];
        pkgs = import nixpkgs {
          inherit system overlays;
        };

        rustToolchain = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
      in
      {
        devShells.default = pkgs.mkShell {
          buildInputs = with pkgs; [
            rustToolchain
            openssl
            pkg-config
            gh
            pre-commit
            cargo-expand
            cargo-udeps
            cargo-outdated
            cargo-release
            cargo-machete
            cargo-unused-features
            protobuf
          ];

          RUST_LOG = "info";

          shellHook = ''
            echo "RUST_LOG set to: $RUST_LOG"
          '';
        };
      });
}
