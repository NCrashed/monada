# The monada Map-Maker's Book

monada is a deterministic voxel game engine. It runs *maps* — self-contained
archives that carry their own rules, art, and interaction as scripts. The
engine itself knows no genre: chess, an action-RPG, and a strategy game are
all just maps, and the host that runs them is the same binary in every case.

This book is for people writing those maps. It covers the map archive
format, the scripting API, the determinism rules that let a match stay in
sync across machines, and the input and UI systems — each with a small,
runnable example you can build on.

## Who this is for

You should be comfortable reading code. The scripting language is
[Rhai](https://rhai.rs) — small, dynamically typed, and close enough to
JavaScript or Lua that you can follow the examples without prior exposure.
You do not need to know Rust; the engine internals stay behind the API.

## How the examples work

Every example in this book is a real, runnable map under
[`book/examples/`](https://github.com/NCrashed/monada/tree/master/book/examples)
in the repository — not an inline snippet. Continuous integration packs and
runs each one on every supported platform, so an example that appears here
is one that actually loads and ticks. When a chapter shows code, it is
pulled directly from the example's source, so the two cannot drift apart.

## A note on determinism

The one idea that shapes everything else: the simulation must produce
bit-identical results on every machine, every run. That is what lets two
players re-derive the same game state from the same inputs (lockstep
networking) and what lets a recorded match replay exactly. Much of what
looks like a restriction in the API — fixed-point numbers instead of
floats, a seeded RNG, no hash-map iteration — exists to hold that line. The
[determinism chapter](determinism.md) makes the rules explicit; the earlier
chapters simply follow them.
