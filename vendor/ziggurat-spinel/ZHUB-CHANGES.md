# Changes in zhub's copy

Copied from https://github.com/zigpy/ziggurat at b82229ade28bf9b077a19afaec7d85166f721cdd
(`crates/ziggurat-spinel`, Apache-2.0, see LICENSE.md) and used through `[patch]` in
zhub's Cargo.toml.

- `src/client.rs`: a transmit (`STREAM_RAW`) waits `TX_TIMEOUT` (250 ms) for the RCP's
  TX-done status instead of the 2 s command timeout, unless it is a delayed (CSL)
  transmit. Other commands keep 2 s.
- `Cargo.toml`: workspace fields made concrete; the one workspace dependency comes from
  the same git revision; dev-dependencies (tests not copied) dropped.
