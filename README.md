# zhub

A lightweight Zigbee hub that runs on the router itself, built on the
[Ziggurat](https://github.com/zigpy/ziggurat) Zigbee stack. No Home Assistant or
Zigbee2MQTT needed: a single static binary with an embedded web UI.

- Forms a network and keeps the network key, frame counters and stack tables in
  `/opt/var/lib/zhub/state.json`.
- Interviews joining devices (ZDO), binds their clusters to the hub and configures
  reporting.
- Lights: on/off, brightness, color temperature, startup and transition settings, effects.
- Sensors: temperature, humidity, occupancy, IAS zones (contact, motion, water), battery.
- Remotes and switch modules: button presses become the `action` value.
- Groups: lights in a Zigbee group take one multicast command (on/off, brightness,
  color temperature); membership lives on the devices (Groups cluster).
- MQTT (optional), laid out like Zigbee2MQTT: `zigbee2mqtt/<name>` state, `…/set`
  commands (also `state: ON/OFF/TOGGLE` and `brightness`), `…/action` button presses,
  `bridge/state|devices|groups`, and Home Assistant discovery (lights and groups as lights,
  thermostats as climate, settings as number/select/switch, buttons as device triggers).
  Live updates, including every button press, also stream over `/api/events`.
- Bindings: a device's binding table can be read and edited (to the hub, a device or a
  group), so e.g. a switch controls lights directly.
- Thermostats: SONOFF TRVZB (ZCL thermostat cluster) and Tuya thermostats (datapoints),
  both with weekly schedules (the TRVZB takes a schedule but cannot report it, so the UI
  shows what was saved from the hub).
- Hue lights can be factory reset by serial number (like the Hue Bridge).
- Web UI at `http://<router>:8099`. Without a password anyone on the network can control
  every device; like Zigbee2MQTT's auth token, Settings → Web UI password protects the page
  and its API (one password, remembered per browser; kept as a salted hash). A forgotten
  password is removed by starting zhub once with `--reset-ui-password`. Do not expose the UI
  to the internet.
- Settings page: hub and radio firmware info, network parameters, transmit power,
  availability limits (cards show Offline), permit join duration, log level with a live
  log viewer, restart, Hue reset.
- Values for sleepy devices are written at once and repeated until taken; ones still
  waiting survive a restart (kept in the state file).
- Matter bridge (optional, `--features matter`): lights, thermostats and temperature
  sensors appear in Apple Home, Google Home or Home Assistant; a group can be shown as one
  light. It uses the test vendor ID, so controllers treat it as uncertified.

## Architecture

Following Zigbee2MQTT: the hub knows no device types; devices are defined as data under
`src/devices/`.

- **Definition**: fingerprint (model or manufacturer name), vendor and description,
  card layout, features, extra bindings, time base.
- **Feature**: key, label, description, kind (numeric/binary/enum/schedule), access,
  where it shows in the UI, and its source (a ZCL attribute, a light command or a Tuya
  datapoint). Decoding, writing and the UI schema all come from the same definition.
- **Common building blocks** (`devices/common.rs`): features of the standard clusters;
  devices without a definition get one built from the clusters they offer.
- Configuration (binding, reporting, reads) is derived from the features; writes to
  sleepy devices are queued and performed when the device wakes up.

Adding a device: open its definition in Zigbee2MQTT (`zigbee-herdsman-converters`),
write the same information as a `Definition` in `devices/<vendor>.rs` and add it to the
list in `devices/mod.rs`. Feature labels and descriptions are taken from Zigbee2MQTT.

Defined devices: Philips Hue LTA005, LTA009, LWA017, RDM001/RDM004 (wall switch module), SONOFF SNZB-02D,
SNZB-02P, TRVZB, Tuya ZWT07 (`_TZE200_g9a3awaj`).

## Hardware

SONOFF ZBDongle-E ("Zigbee 3.0 USB Dongle Plus V2", EFR32MG21) with Nerivec's OpenThread
RCP build `sonoff_zbdonglee_openthread_rcp_2026.6.2_3.1.2_460800_sw_flow.gbl`: 460800 baud,
XON/XOFF flow control (`--baudrate 460800 --flow-control software`; both must match the
firmware build). On Keenetic the "USB to Serial Port Converters" component must be
installed. The dongle is a USB 2.0 device; a USB 3 port only adds 2.4 GHz noise.

Known issue: some sleepy devices (SONOFF TRVZB) cannot join the dongle directly with a
MAC association; pair them through a router (e.g. a Hue light). After that they can
rejoin the dongle directly.

## Build and install

`./build.sh` builds in Docker. `./build-native.sh` builds on the host instead (Rust nightly
via rustup, zig 0.15.2 in `.tools/`); it is faster and is the one that fits the Matter build
(rs-matter needs more memory than Docker Desktop's default VM gives).

```sh
./build.sh
cat target/mips-unknown-linux-musl/release/zhub | ssh -p 222 root@192.168.1.1 'cat > /opt/bin/zhub && chmod +x /opt/bin/zhub'
cat deploy/S99zhub | ssh -p 222 root@192.168.1.1 'cat > /opt/etc/init.d/S99zhub && chmod +x /opt/etc/init.d/S99zhub'
ssh -p 222 root@192.168.1.1 /opt/etc/init.d/S99zhub restart
```

MQTT broker on the router (optional; Home Assistant or another automation platform connects
to it): Entware's `mosquitto-nossl` (no TLS, ~110 KB) with `deploy/mosquitto.conf`, which
listens on the home network and localhost only and requires a password. This build stores
passwords in plain text, so the file is readable by the broker user only:

```sh
opkg install mosquitto-nossl
cat deploy/mosquitto.conf | ssh -p 222 root@192.168.1.1 'cat > /opt/etc/mosquitto/mosquitto.conf'
# on the router: one line per user, "name:password"
mkdir -p /opt/var/lib/mosquitto && chown nobody:nobody /opt/var/lib/mosquitto
umask 077; echo "zhub:<password>" > /opt/etc/mosquitto/passwd; chown nobody:nobody /opt/etc/mosquitto/passwd
/opt/etc/init.d/S80mosquitto restart
```

Then Settings → MQTT in zhub: broker 127.0.0.1, user zhub.

Log: `/opt/var/log/zhub.log`. `--sniff` logs MAC command frames for diagnostics (the
radio does not acknowledge frames while sniffing; use it only briefly).

## Credits

zhub builds on the work of these projects:

- [Ziggurat](https://github.com/zigpy/ziggurat) (Apache-2.0): the Zigbee stack zhub runs
  on, from the zigpy project. `vendor/ziggurat-spinel` is a modified copy of its Spinel
  client (see `vendor/ziggurat-spinel/ZHUB-CHANGES.md`).
- [rs-matter](https://github.com/project-chip/rs-matter) (Apache-2.0): the Rust Matter
  stack behind the Matter bridge.
- [Zigbee2MQTT](https://github.com/Koenkk/zigbee2mqtt) and
  [zigbee-herdsman-converters](https://github.com/Koenkk/zigbee-herdsman-converters) (MIT) by
  Koen Kanters and contributors: the device definitions, feature names and descriptions, and
  the light command behavior are derived from zigbee-herdsman-converters; the MQTT topics
  and payloads follow Zigbee2MQTT so that tools made for it work with zhub.
- [Nerivec's silabs-firmware-builder](https://github.com/Nerivec/silabs-firmware-builder):
  the OpenThread RCP firmware zhub is tested with.

## License

Apache License 2.0, see [LICENSE](LICENSE). Third-party notices are in [NOTICE](NOTICE).
