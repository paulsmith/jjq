# ABOUTME: Nix flake providing a Rust dev environment and build output for jjq.
# ABOUTME: Includes rustc, cargo, and standard build inputs; produces the jjq binary as a package.
{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  };

  outputs =
    { self, nixpkgs }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "x86_64-darwin"
        "aarch64-darwin"
      ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
      version = (builtins.fromTOML (builtins.readFile ./Cargo.toml)).package.version;
    in
    {
      packages = forAllSystems (pkgs:
        let
          jjq = pkgs.rustPlatform.buildRustPackage {
            pname = "jjq";
            inherit version;
            src = ./.;
            cargoLock.lockFile = ./Cargo.lock;
            nativeBuildInputs = [ pkgs.installShellFiles ];
            postInstall = ''
              installManPage docs/jjq.1
            '';
            # e2e tests require jj, git, and go; skip in sandbox build
            doCheck = false;
          };
        in
        {
          default = jjq;

          tarball = pkgs.runCommand "jjq-tarball" { } ''
            dir="jjq-${version}-${pkgs.stdenv.hostPlatform.system}"
            mkdir -p "$dir/bin" "$dir/share/man/man1"
            cp ${jjq}/bin/jjq "$dir/bin/"
            cp ${jjq}/share/man/man1/jjq.1.gz "$dir/share/man/man1/"
            cp ${./install.sh} "$dir/install"
            chmod 755 "$dir/install"
            tar czf "$out" "$dir"
          '';
        });

      devShells = forAllSystems (pkgs: {
        default = pkgs.mkShell {
          packages = with pkgs; [
            rustc
            cargo
            rust-analyzer
            clippy
            rustfmt
          ];
        };
      });
    };
}
