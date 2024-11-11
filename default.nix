{pkgs ? import <nixpkgs> {}}:
with pkgs; let
  python-app = python3Packages.buildPythonApplication {
    pname = "testsearch";
    version = "0.1.0";

    src = ./.;

    pyproject = true;

    dependencies = with python3Packages; [
      setuptools
      tree-sitter
      tree-sitter-python
      (iterfzf.overrideAttrs (prev: {
        doInstallCheck = false;
      }))
    ];

    propagatedBuildInputs = [
      fd
    ];
  };

  rust-app = rustPlatform.buildRustPackage {
    pname = "testsearch";
    version = "0.1.0";

    src = ./.;

    cargoLock.lockFile = ./Cargo.lock;
  };
in
  rust-app
