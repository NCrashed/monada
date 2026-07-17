# API reference

*This chapter is not yet written.*

It will list every host function a map script can call, grouped by layer —
simulation (deterministic, hashed), local (per-client input and queries),
and presentation (render / audio / HUD) — with each function's signature and
its determinism notes.

A CI check will keep this reference in step with the engine: it diffs the set
of functions the reference documents against the set the scripting backend
actually registers, and fails if either side has an entry the other lacks.
Until that lands, the authoritative list is the registration code in
[`monada-script`](https://github.com/NCrashed/monada/blob/master/crates/monada-script/src/rhai_backend.rs).
