{
  description = "Unique network node";
  inputs.flake-utils.url = "github:numtide/flake-utils";
  inputs.rust-overlay = { url = "github:oxalica/rust-overlay"; flake = false; };
  outputs = { self, nixpkgs, rust-overlay, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs
          {
            inherit system; overlays = [ (import rust-overlay) ];
          };
      in
      {
        devShell = pkgs.mkShell {
          nativeBuildInputs = with pkgs; [
            nodePackages.node-gyp
            tree-sitter
            python3
            binutils
            cargo-edit
            pkgconfig
            openssl
            lld
            ((rustChannelOf { date = "2022-11-10"; channel = "nightly"; }).default.override {
              extensions = [ "rust-src" ];
            })
          ];
        };
      });
}
