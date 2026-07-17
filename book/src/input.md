# Input, actions, bindings

*This chapter is not yet written.*

It will cover the map's **local script layer** — a second script scope that
runs per client and handles input, selection gestures, camera, and UI
without ever touching the synchronized simulation directly. Its entry points
are `local_init`, `local_frame(dt)`, `local_tick(dt)`, `action(id, down)`,
and `pointer(button, point, entity)`.

It will also cover **declared actions**: `[[action]]` tables in the manifest
name rebindable inputs (`button`, `axis`, `axis2`) with default keys, which
the local layer reads by name (`action_down`, `action_axis2`, …) and which
players rebind through the host. Cursor handling gets a first-class pick API
(`pick_ground`, `pick_entity`, `aim_yaw`) whose results are already
quantized to simulation types, ready to become command payloads.

The design and current state live in
[`docs/plans/input-bindings.md`](https://github.com/NCrashed/monada/blob/master/docs/plans/input-bindings.md);
the action-RPG and chess maps are the worked examples.
