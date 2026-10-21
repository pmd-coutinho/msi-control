{
  description = "msi-control: msi-ec + MUX + tuned/PPD in one daemon, CLI and GTK4 GUI";
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  outputs = { self, nixpkgs }:
    let
      system = "x86_64-linux";
      pkgs = nixpkgs.legacyPackages.${system};
    in {
      packages.${system}.default = pkgs.callPackage ./nix/package.nix { };
      nixosModules.default = import ./nix/module.nix self;
      devShells.${system}.default = pkgs.mkShell {
        nativeBuildInputs = with pkgs; [ cargo rustc clippy rustfmt rust-analyzer pkg-config ];
        buildInputs = with pkgs; [ gtk4 libadwaita glib ];
      };
    };
}
