
{ pkgs ? import <nixpkgs> {} }:
  pkgs.mkShell {
    # build tools
    nativeBuildInputs = with pkgs; [
      pkg-config
    ];

    # libraries/dependencies
    buildInputs = with pkgs; [
      openssl
    ];
}
