# Commands and lockstep

*This chapter is not yet written.*

It will cover the `command(player, verb, target, arg)` handler — the only
way player input reaches the simulation — and how the same command stream
drives turn-based and real-time maps alike. It will also cover the rule that
matters most for networked play: **the client has zero authority**, so a map
validates every command (ownership, legality, range) in its handler, exactly
as the chess map validates each move.

Replays and the determinism goldens both build on this: a match is fully
described by its seed and its command stream.
