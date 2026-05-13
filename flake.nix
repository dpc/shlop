{
  description = "tau";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-25.11";
    flake-utils.url = "github:numtide/flake-utils";
    flakebox.url = "github:rustshop/flakebox";
  };

  outputs =
    {
      self,
      nixpkgs,
      flake-utils,
      flakebox,
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = nixpkgs.legacyPackages.${system};
        projectName = "tau";

        flakeboxLib = flakebox.lib.mkLib pkgs {
          config = {
            github.ci.buildOutputs = [ ".#ci.workspace" ];
            just.importPaths = [ "justfile.custom.just" ];
            just.rules.watch.enable = false;
          };
        };

        buildPaths = [
          "Cargo.toml"
          "Cargo.lock"
          "config"
          "crates"
        ];

        buildSrc = flakeboxLib.filterSubPaths {
          root = builtins.path {
            name = projectName;
            path = ./.;
          };
          paths = buildPaths;
        };

        # Placeholders are 40 / 16 raw bytes that the binary embeds via
        # a `static [u8; N]` in `crates/tau-harness/src/version.rs`.
        # The strings below MUST byte-for-byte match those statics, and
        # the substituted values MUST be the same length so `bbe` can
        # patch them in place without shifting any file offsets.
        #
        # Why the unique `__TAU_BUILD…` prefix: short, "ASCII-table-ish"
        # placeholders (e.g. `0123456`) collide with natural byte runs
        # in the binary (base64 alphabets, hex digit tables) and bbe
        # would silently corrupt them.
        tauBuildRevisionPlaceholder = "__TAU_BUILD_GIT_REVISION_PLACEHOLDER____";
        tauBuildDatePlaceholder = "__TAU_BUILD_DATE";
        tauBuildRevision =
          if (self ? rev) && (builtins.stringLength self.rev == 40) then
            self.rev
          else if (self ? dirtyRev) && (builtins.stringLength self.dirtyRev == 46) then
            "${builtins.substring 0 16 self.dirtyRev}00000000${builtins.substring 24 16 self.dirtyRev}"
          else if (self ? dirtyRev) && (builtins.stringLength self.dirtyRev == 40) then
            self.dirtyRev
          else
            tauBuildRevisionPlaceholder;
        tauBuildDate =
          if self ? lastModifiedDate then
            "${builtins.substring 0 4 self.lastModifiedDate}-${builtins.substring 4 2 self.lastModifiedDate}-${
              builtins.substring 6 2 self.lastModifiedDate
            } ${builtins.substring 8 2 self.lastModifiedDate}:${builtins.substring 10 2 self.lastModifiedDate}"
          else
            tauBuildDatePlaceholder;

        replaceTauBuildInfo =
          package:
          pkgs.stdenv.mkDerivation {
            pname = projectName;
            version = package.version;

            dontUnpack = true;
            dontStrip = true;

            nativeBuildInputs = [ pkgs.bbe ];

            # `bbe` itself silently no-ops when its pattern isn't found,
            # which is exactly how the previous LTO-eats-the-placeholder
            # bug shipped. Track per-placeholder hit counts and require
            # at least one substitution across all executables; also
            # assert no placeholder bytes remain after patching.
            installPhase = ''
              cp -a ${package} $out
              chmod -R u+w $out
              revision_hits=0
              date_hits=0
              for path in $(${pkgs.findutils}/bin/find $out -type f -executable); do
                had_revision=0
                had_date=0
                if grep -aqF '${tauBuildRevisionPlaceholder}' "$path"; then
                  had_revision=1
                fi
                if grep -aqF '${tauBuildDatePlaceholder}' "$path"; then
                  had_date=1
                fi
                ${pkgs.bbe}/bin/bbe \
                  -e 's/${tauBuildRevisionPlaceholder}/${tauBuildRevision}/' \
                  -e 's/${tauBuildDatePlaceholder}/${tauBuildDate}/' \
                  "$path" -o ./tmp
                cat ./tmp > "$path"
                if [ "$had_revision" = 1 ]; then
                  if grep -aqF '${tauBuildRevisionPlaceholder}' "$path"; then
                    echo "error: revision placeholder still present in $path after bbe" >&2
                    exit 1
                  fi
                  revision_hits=$((revision_hits + 1))
                fi
                if [ "$had_date" = 1 ]; then
                  if grep -aqF '${tauBuildDatePlaceholder}' "$path"; then
                    echo "error: date placeholder still present in $path after bbe" >&2
                    exit 1
                  fi
                  date_hits=$((date_hits + 1))
                fi
              done
              if [ "$revision_hits" = 0 ]; then
                echo "error: revision placeholder '${tauBuildRevisionPlaceholder}' not found in any executable under $out" >&2
                echo "       (likely the compiler optimized it out — check crates/tau-harness/src/version.rs)" >&2
                exit 1
              fi
              if [ "$date_hits" = 0 ]; then
                echo "error: date placeholder '${tauBuildDatePlaceholder}' not found in any executable under $out" >&2
                echo "       (likely the compiler optimized it out — check crates/tau-harness/src/version.rs)" >&2
                exit 1
              fi
            '';
          };

        multiBuild = (flakeboxLib.craneMultiBuild { }) (
          craneLib':
          let
            craneLib = craneLib'.overrideArgs {
              pname = projectName;
              src = buildSrc;
              nativeBuildInputs = [ ];
            };
          in
          rec {
            workspaceDeps = craneLib.buildWorkspaceDepsOnly { };

            workspace = craneLib.buildWorkspace {
              cargoArtifacts = workspaceDeps;
            };

            tests = craneLib.cargoNextest {
              cargoArtifacts = workspace;
              cargoNextestExtraArgs = "--no-tests=pass";
            };

            clippy = craneLib.cargoClippy {
              cargoArtifacts = workspaceDeps;
              cargoClippyExtraArgs = "-- -D warnings";
            };

            tau = replaceTauBuildInfo (
              craneLib.buildPackage {
                cargoArtifacts = workspaceDeps;
                cargoExtraArgs = "-p tau";
              }
            );
          }
        );

        site = pkgs.runCommand "tau-agent-site" { } ''
          mkdir -p $out/share/tau-agent-site
          cp -r ${./site}/* $out/share/tau-agent-site/
        '';
      in
      {
        packages.default = multiBuild.tau;
        packages.tau = multiBuild.tau;
        packages.site = site;

        ci = {
          inherit (multiBuild) workspace clippy tests;
        };

        legacyPackages = multiBuild;

        devShells = flakeboxLib.mkShells {
          channel = "latest";
          components = flakeboxLib.config.toolchain.components ++ [
            "rustc-codegen-cranelift-preview"
          ];
          packages = [ pkgs.taplo ];
        };
      }
    );
}
