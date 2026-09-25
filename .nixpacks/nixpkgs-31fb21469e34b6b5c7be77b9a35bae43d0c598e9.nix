{ }:

let pkgs = import (fetchTarball "https://github.com/NixOS/nixpkgs/archive/31fb21469e34b6b5c7be77b9a35bae43d0c598e9.tar.gz") { overlays = [ (import (builtins.fetchTarball "https://github.com/railwayapp/nix-npm-overlay/archive/main.tar.gz")) ]; };
in with pkgs;
  let
    APPEND_LIBRARY_PATH = "${lib.makeLibraryPath [  ] }";
    myLibraries = writeText "libraries" ''
      export LD_LIBRARY_PATH="${APPEND_LIBRARY_PATH}:$LD_LIBRARY_PATH"
      
    '';
  in
    buildEnv {
      name = "31fb21469e34b6b5c7be77b9a35bae43d0c598e9-env";
      paths = [
        (runCommand "31fb21469e34b6b5c7be77b9a35bae43d0c598e9-env" { } ''
          mkdir -p $out/etc/profile.d
          cp ${myLibraries} $out/etc/profile.d/31fb21469e34b6b5c7be77b9a35bae43d0c598e9-env.sh
        '')
        bun nodejs_18
      ];
    }
