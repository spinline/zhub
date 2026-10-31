# Scenes validation — 2026-10-31

- Matter-enabled host tests: 105 passed.
- Default host tests: 103 passed.
- Static MIPS release built with Matter using nightly-2026-10-16 and Zig 0.15.2.
- Installed binary SHA-256: `19145ad15d2010cba52723585494fe751f75a40785ec0a634b8774b9254a6c7b`.
- Pre-deployment binary, startup script and persisted state backup:
  `/opt/var/backups/zhub/scenes-20261031-141145/`.

Real-device checks used the existing two-light group, ID 3. Scene ID 254 was
selected only after reading both devices' tables and confirming it was unused.
No permanent scene or zauto rule was created by these checks.

Verified on both Hue endpoints:

1. Enhanced Add Scene with a 0.2-second transition.
2. Store Scene, followed by View Scene: both devices retained 0.2 seconds.
3. Rename and refresh: metadata name survived reading the device tables.
4. Add Scene with each lamp's existing on/off, brightness and color temperature.
5. Group recall, followed by acknowledged individual recalls of the same scene.
6. Scene rename sent through the actual MQTT broker and observed through the API.
7. Remove Scene and read both device tables again: the temporary entry was gone.

A membership read immediately after startup timed out on one lamp. Retrying
after the Zigbee routes settled succeeded. The failure was returned to the
client rather than treated as an empty scene table.

Afterwards, device identities (15), group definitions (3), network parameters
and Matter endpoint assignments matched the pre-deployment snapshot. The
Matter bridge reported running with both controller records loaded. Zauto
and zhub were running; no temporary scene metadata remained on disk.

The served HTML matched the local source and its JavaScript passed `node --check`.
Interactive visual verification was unavailable because local browser use was
disabled in the task settings. Matter controller commands were not exercised.

Final validation before committing, after adding scene prediction metadata:
Matter-enabled host tests passed (107 tests) with nightly-2026-10-16, and the
web UI JavaScript syntax check passed. The later production binary, including
scene prediction metadata, has SHA-256
`66c4119dddb930c919c2001051a28cb4002bfba78f2fb4007f6409aa9f955fca`.
The earlier binary hash and 105-test result above describe the initial scene
deployment, before that metadata extension.
