{pkgs ? import <nixpkgs> {}}:
with pkgs; let
  pytest-collect-formatter = python3Packages.buildPythonPackage rec {
    pname = "pytest-collect-formatter";
    version = "0.4.0";

    src = fetchPypi {
      inherit pname version;
      hash = "sha256-jYp3qn5x7ZdFscIBckjZ7ksRiGGanSaLNNtxSvA6FAo=";
    };

    buildInputs = with python3Packages; [
      pip
    ];

    propagatedBuildInputs = with python3Packages; [
      dicttoxml
      pyyaml
    ];
  };

  custom-python = python3.withPackages (ps:
    with ps; [
      pytest
      pytest-collect-formatter
    ]);
in
  mkShell rec {
    packages =
      [
        hyperfine
        rustup
        clang
        custom-python
      ]
      ++ lib.optionals stdenv.isDarwin (with darwin.apple_sdk.frameworks; [
        libiconv
      ])
      ++ lib.optionals stdenv.isLinux [
        mold
      ];

    shellHook = ''
      export RUST_BUILD_BASE="$HOME/.cache/rust-builds"
      WORKSPACE_ROOT=$(cargo metadata --no-deps --offline 2>/dev/null | jq -r ".workspace_root")
      PACKAGE_BASENAME=$(basename $WORKSPACE_ROOT)

      # Run cargo with target set to $RUST_BUILD_BASE/$PACKAGE_BASENAME
      export CARGO_TARGET_DIR="$RUST_BUILD_BASE/$PACKAGE_BASENAME"
    '';

    env = {
      RUST_SRC_PATH = "${rustPlatform.rustLibSrc}";
      LD_LIBRARY_PATH = lib.makeLibraryPath packages;
    };
  }
