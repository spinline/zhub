# Hue Wall Switch direct binding test — 2026-10-31

Tested RDM001 firmware 2.85.5 and RDM004 firmware 2.85.1 on the existing Zigbee network. Each switch's actual binding table was read before changes and checked against the saved table after restoration.

A temporary group contained one lamp. The switch's endpoint 1 On/Off (0x0006) and Level Control (0x0008) client clusters were bound to that group; On/Off and manufacturer-specific 0xfc00 coordinator bindings were temporarily removed, keeping battery reporting. The corresponding zauto light automation was disabled during each test.

RDM001 was tested temporarily in single_push_button mode and restored to single_rocker. RDM004 retained dual_push_button throughout; the user tested the left and right inputs with short presses, a requested five-second hold, and double presses. For both models the user observed only switching. Captured group frames contained On (0x01) and Off With Effect (0x40) on cluster 0x0006. No Level Control or Scenes cluster commands were observed during these tests. This is evidence for the tested firmware and configurations, not proof of all possible firmware behavior. Traces for the two RDM004 input tests were collected in sequential windows; group packets themselves do not identify the physical input.

For RDM001 the user also reported switching during an explicitly verified zhub stop window. An earlier stop attempt had failed and was excluded. For RDM004 zhub was verified stopped and automatically restarted after 40 seconds, but no user response to the offline observation question was received before restoring the settings; do not claim a user-confirmed offline result for RDM004. Its captured group commands originate from the switch rather than zauto.

Both devices' original binding tables were verified after restoration. Temporary groups were removed, lamp settings restored, light automations enabled as before, and diagnostic logging removed. The original production binary was restored and its SHA-256 verified:

`66c4119dddb930c919c2001051a28cb4002bfba78f2fb4007f6409aa9f955fca`

No firmware updates or factory resets were performed. The result supports keeping software double-press/hold automation for the current functionality; direct binding alone did not reproduce dimming or scene selection in the tested setup.
