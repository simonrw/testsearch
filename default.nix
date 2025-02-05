{pkgs ? import <nixpkgs> {}}:
with pkgs;
  rustPlatform.buildRustPackage {
    pname = "testsearch";
    version = "0.1.0";

    src = ./.;

    cargoLock.lockFile = ./Cargo.lock;

    nativeBuildInputs = [
      installShellFiles
    ];

    postInstall = ''
      installShellCompletion --cmd testsearch \
        --bash <($out/bin/testsearch completion bash) \
        --zsh <($out/bin/testsearch completion zsh) \
        --fish <($out/bin/testsearch completion fish)
    '';
  }
