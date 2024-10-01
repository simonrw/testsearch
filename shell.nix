{ pkgs ? import <nixpkgs> { } }:
with pkgs;
mkShell {
  packages = [
    python3
    python3Packages.venvShellHook
    uv
  ];

  venvDir = ".venv";

  postVenvCreation = ''
  '';

  postShellHook = ''
    export VENV_DIR=$VIRTUAL_ENV
  '';
}

