# Getting started

## Build the host

monada builds with a recent Rust toolchain. From a clone of the repository:

```console
$ cargo build --release -p monada-host
```

On Linux the default build links the platform audio backend (ALSA), so
install its development headers first — `libasound2-dev` on Debian/Ubuntu —
or build with `--no-default-features` to compile the audio calls out.

## Run a bundled map

Two complete maps ship in the workspace and double as worked examples later
in this book: a turn-based chess game and a real-time co-op action-RPG.

```console
$ cargo run --release -p monada-chess
$ cargo run --release -p monada-rpg
```

Each opens a window with the map already loaded. Arrow keys orbit the
camera and `Esc` quits; `F2` opens a key-bindings panel where you can rebind
any control — engine or map — and the change is saved for next time. The
rest of the controls belong to the map (see
[Input, actions, bindings](input.md)).

## Run a map archive directly

The host also loads a packed map archive, or a map directory it packs on the
fly, via `--map`:

```console
$ cargo run --release -p monada-host -- --map book/examples/01-hello-voxels
```

That runs [the first example](hello-voxels.md) — a floor and a single cube
that walks across it. It is the smallest thing that counts as a map, and the
next two chapters build it up from the archive format on down.
