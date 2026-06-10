{
  description = "monada — deterministic lockstep voxel game engine (roxlap-rendered)";

  inputs = {
    nixpkgs.url = "flake:nixpkgs";
    # Pinned nightly Rust comes from rust-overlay, driven by
    # rust-toolchain.toml. monada inherits roxlap's wasm-threads
    # toolchain requirements (`-Z build-std` + `rust-src`) because
    # monada-web (M4) reuses roxlap's wasm-bindgen-rayon path.
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { self, nixpkgs, rust-overlay }:
    let
      forAllSystems = f:
        nixpkgs.lib.genAttrs [ "x86_64-linux" "aarch64-linux" "x86_64-darwin" "aarch64-darwin" ]
          (system: f {
            pkgs = import nixpkgs {
              inherit system;
              overlays = [ rust-overlay.overlays.default ];
            };
          });
    in {
      devShells = forAllSystems ({ pkgs }:
        let
          # Runtime libs the host renderer dlopens on Linux. Needed from
          # M1 onward (monada-host = winit + softbuffer + roxlap, mirroring
          # roxlap-host / roxlap-cave-demo); harmless to ship earlier so
          # the shell is ready when the render bridge lands. X11 *and*
          # Wayland are listed so the demo Just Works on either backend;
          # macOS uses CoreGraphics/Metal natively and needs none.
          linuxRuntimeLibs = with pkgs; [
            libxkbcommon
            wayland
            libx11
            libxcursor
            libxi
            libxrandr
            libxcb
            # roxlap-gpu's wgpu backend dlopens libvulkan.so.1 to reach
            # the Mesa/Nvidia ICDs; the loader must be on LD_LIBRARY_PATH
            # for our non-NixOS-managed binaries to find it.
            vulkan-loader
          ];

          # Single source of truth: the same rust-toolchain.toml cargo
          # reads. Bundles rust-src (for `-Z build-std`) and the
          # wasm32-unknown-unknown target. Native builds + tests behave
          # identically to stable on this nightly (M0 uses no nightly
          # features off the wasm target).
          rustToolchain =
            pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
        in {
          default = pkgs.mkShell {
            packages = with pkgs; [
              rustToolchain
              pkg-config
              # wasm32-unknown-unknown needs an LLD-class linker; nixpkgs
              # rustc doesn't bundle rust-lld, so provide the system one
              # (cargo finds it via the bare `lld` on PATH).
              lld
              # monada-web (M4) browser build + tests: wasm-bindgen-cli
              # produces the JS shim and the test runner; Node executes it;
              # trunk is the dev-server / bundler.
              wasm-bindgen-cli
              nodejs
              trunk
            ] ++ pkgs.lib.optionals pkgs.stdenv.isLinux linuxRuntimeLibs;

            # mkShell only sets PATH / PKG_CONFIG_PATH; the dlopen'd render
            # libs above need an explicit search path. macOS skips this —
            # the Cocoa / Metal frameworks are always on the dyld path.
            shellHook = pkgs.lib.optionalString pkgs.stdenv.isLinux ''
              export LD_LIBRARY_PATH="${pkgs.lib.makeLibraryPath linuxRuntimeLibs}:''${LD_LIBRARY_PATH:-}"
            '';
          };

          # Dedicated cargo-fuzz shell — the LLVM/clang sanitizer stack is
          # heavy and only the `monada-fixed/fuzz` targets need it, so it
          # stays out of the default shell. Enter with `nix develop .#fuzz`,
          # then run targets from the crate that owns them:
          #   cd crates/monada-fixed && cargo fuzz run roundtrip
          # (The pinned toolchain is already nightly, so no `+nightly`.)
          fuzz = pkgs.mkShell {
            packages = with pkgs; [
              rustToolchain
              cargo-fuzz
              # clang + llvm provide the sanitizer/coverage instrumentation
              # and `llvm-symbolizer` for readable crash backtraces.
              clang
              llvm
            ];
          };
        });

      formatter = forAllSystems ({ pkgs }: pkgs.nixpkgs-fmt);
    };
}
