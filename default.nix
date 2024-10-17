{ pkgs ? import <nixpkgs> {} }:
with pkgs;
python3Packages.buildPythonApplication {
  pname = "testsearch";
  version = "0.1.0";

  src = ./.;

  pyproject = true;

  dependencies = with python3Packages; [
    setuptools
    tree-sitter
    tree-sitter-python
    iterfzf
  ];

  propagatedBuildInputs = [
    fd
  ];
}
