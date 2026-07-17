# Multiplayer

*This chapter is not yet written.*

It will cover running a map over LAN — the `--listen` / `--connect` host
flags — how `local_player()` gates which side a client may act for, how an
observer differs from a player, and how to read a desync when one happens.
The transport is deterministic lockstep over QUIC: only inputs cross the
wire, and each client re-derives identical state.
