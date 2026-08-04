{
  description = "Storz & Bickel vaporizer control: storz-rs library, volcano CLI, volcano-mcp MCP server, volcano-daemon";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  };

  outputs = { self, nixpkgs }:
    let
      forAllSystems = nixpkgs.lib.genAttrs [ "x86_64-linux" "aarch64-linux" "x86_64-darwin" "aarch64-darwin" ];
      pkgsFor = system: nixpkgs.legacyPackages.${system};
    in
    {
      packages = forAllSystems (system:
        let
          pkgs = pkgsFor system;
          volcano-suite = pkgs.rustPlatform.buildRustPackage {
            pname = "volcano-suite";
            version = "0.1.0";
            src = self;
            cargoLock.lockFile = ./Cargo.lock;
            cargoBuildFlags = [ "--workspace" ];
            nativeBuildInputs = with pkgs; [ pkg-config installShellFiles ];
            buildInputs = with pkgs; [ dbus ];
            # The workspace's own tests only; doctests hit the network-free sandbox fine but BLE integration is hardware-only anyway.
            doCheck = false;
            # Only volcano carries the CompleteEnv hook; COMPLETE=zsh against the other bins would run them for real.
            postInstall = ''
              installShellCompletion --cmd volcano --zsh <(COMPLETE=zsh "$out/bin/volcano")
            '';
            meta = {
              description = "Control Storz & Bickel vaporizers (Volcano Hybrid, Venty, Crafty) over BLE";
              license = pkgs.lib.licenses.mit;
              mainProgram = "volcano";
            };
          };
        in
        {
          default = volcano-suite;
          volcano = volcano-suite;
        });

      apps = forAllSystems (system:
        let
          suite = self.packages.${system}.default;
          app = program: { type = "app"; program = "${suite}/bin/${program}"; };
        in
        {
          default = app "volcano";
          volcano = app "volcano";
          volcano-mcp = app "volcano-mcp";
          volcano-daemon = app "volcano-daemon";
        });

      devShells = forAllSystems (system:
        let pkgs = pkgsFor system;
        in {
          default = pkgs.mkShell {
            nativeBuildInputs = with pkgs; [ pkg-config ];
            buildInputs = with pkgs; [ dbus ];
          };
        });
    };
}
