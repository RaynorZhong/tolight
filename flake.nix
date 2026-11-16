{
  description = "tolight: pinned Rust, PostgreSQL 18, and TypeScript web checks";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/2074cc672349f14240fb7ea90d509e0b643e9dd4";
    rust-overlay = {
      url = "github:oxalica/rust-overlay/5b87ee0513c29127a01ad06e9cb362ea5ce91e78";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { nixpkgs, rust-overlay, ... }:
    let
      systems = [ "aarch64-darwin" "x86_64-darwin" "aarch64-linux" "x86_64-linux" ];
      eachSystem = f: nixpkgs.lib.genAttrs systems (system: f system);
    in {
      devShells = eachSystem (system:
        let
          pkgs = import nixpkgs { inherit system; overlays = [ (import rust-overlay) ]; };
          rust = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
          python = pkgs.python313.withPackages (ps: [ ps.cryptography ]);
          sqlx = assert pkgs.sqlx-cli.version == "0.9.0"; pkgs.sqlx-cli;
        in {
          default = pkgs.mkShell {
            packages = [ rust pkgs.cargo-deny sqlx pkgs.postgresql_18 pkgs.nodejs_26 python pkgs.caddy pkgs.curl pkgs.git ];
            shellHook = ''
              export CARGO_HOME="$PWD/.local/cargo"
              export PYTHON=python3
            '';
          };
        });
    };
}
