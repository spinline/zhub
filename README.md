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
- Thermostats: SONOFF TRVZB (ZCL thermostat cluster) and Tuya thermostats (datapoints),
  including weekly schedules where supported.
- Hue lights can be factory reset by serial number (like the Hue Bridge).
- Web UI at `http://<router>:8099` (no authentication; local network only).

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

Defined devices: Philips Hue LTA009, SONOFF SNZB-02D, SONOFF SNZB-02P, SONOFF TRVZB,
Tuya ZWT07 (`_TZE200_g9a3awaj`).

## Hardware

SONOFF Dongle Plus MG24 with Silicon Labs OpenThread RCP 3.1.1 (921600 baud, no flow
control). On Keenetic the "USB to Serial Port Converters" component must be installed.

Known issue: some sleepy devices (SONOFF TRVZB) cannot join the dongle directly with a
MAC association; pair them through a router (e.g. a Hue light). After that they can
rejoin the dongle directly.

## Build and install

```sh
./build.sh
cat target/mips-unknown-linux-musl/release/zhub | ssh -p 222 root@192.168.1.1 'cat > /opt/bin/zhub && chmod +x /opt/bin/zhub'
cat deploy/S99zhub | ssh -p 222 root@192.168.1.1 'cat > /opt/etc/init.d/S99zhub && chmod +x /opt/etc/init.d/S99zhub'
ssh -p 222 root@192.168.1.1 /opt/etc/init.d/S99zhub restart
```

Log: `/opt/var/log/zhub.log`. `--sniff` logs MAC command frames for diagnostics (the
radio does not acknowledge frames while sniffing; use it only briefly).
