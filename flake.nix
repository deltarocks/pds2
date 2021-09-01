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
          nativeBuildInputs = [
            pkgs.nodePackages.node-gyp
            (pkgs.tree-sitter.overrideAttrs (prev:
              let
                src = pkgs.fetchFromGitHub {
                  owner = "tree-sitter";
                  repo = "tree-sitter";
                  rev = "master";
                  hash = "sha256-qS5fepa9FCGZXy9IQLFK1bR8Vsb5rUiyWGBTrP6jVFE=";
                }; in
              {
                inherit src;
                cargoDeps = prev.cargoDeps.overrideAttrs (_: {
                  inherit src;
                  outputHash = "sha256-lJXwHyaUKyjcaskzRlQs2UN+p5w2nJhyu1KU1HhV/Xs=";
                });
              }))
            pkgs.python3
            pkgs.binutils
            pkgs.pkgconfig
            pkgs.openssl
            ((pkgs.rustChannelOf { date = "2021-08-16"; channel = "nightly"; }).default.override {
              extensions = [ "rust-src" ];
            })
          ];
        };
      });
}
