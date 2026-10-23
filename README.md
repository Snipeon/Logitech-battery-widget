# Peripheral Battery Widget

This Rainmeter skin shows battery percentage and charging state for supported Logitech wireless devices. A small Rust helper reads the devices directly using HID++; some supported headsets use Logitech's Centurion report format. The package does not install LogitechBattery, HeadsetControl, or G HUB.

## What happens when the skin runs

1. When Rainmeter refreshes the skin, its RunCommand measure starts `@Resources\headset-battery-bridge-rs.exe` as a hidden background helper. The helper keeps running while the skin is loaded.
2. The helper checks connected Logitech HID interfaces. A Logitech vendor ID (`046D`) is only a candidate filter: a device is tracked only if it answers one of the battery queries the helper supports.
3. On the first pass, after the connected HID paths change, and at the configured rediscovery interval, the helper probes direct HID++ devices and receiver slots `1`–`6`. It also checks Centurion interfaces. Once a device is found, its open HID handle and battery feature are kept for later polls.
4. On each battery poll, the helper reads level and charge state, then writes the latest device list to `%LOCALAPPDATA%\PeripheralBatteryWidget\devices.tsv`.
5. `Grid.lua` reads that file on Rainmeter's update tick and fills the cards. If no device rows are available, it displays a waiting message.

The helper refreshes the connected HID path list on each poll so it can notice interface changes quickly. Full name and feature discovery is slower, so it runs only after a path change or when its timer expires.

## Supported readings and names

For HID++ devices, the helper looks for battery features in this order: `0x1F20` (headset voltage and status), `0x1004` (Unified Battery), `0x1001` (Battery Voltage), and `0x1000` (Battery Status). It also reads Centurion battery reports for devices that use that protocol. A device appears only after a supported query returns battery data.

The helper prefers the device name reported by HID++ feature `0x0005`. If that name is generic, it matches the HID interface path to Windows PnP information and tries the Friendly Name, then the USB product name. This helps replace labels such as a generic driver name with the Windows name for the matching interface. If no better name is available, it keeps the HID++ name or a generic fallback.

Devices that report voltage instead of a percentage use an estimated percentage. Charging, full, and charging fault states are shown when the device's protocol reports them. Devices using other battery protocols or firmware formats may not be supported.

The helper gives each physical device a stable ID where possible so duplicate HID interfaces can be merged while two devices with the same model name remain separate. Rows are sorted by device name before they are written.

## Layouts

- `PeripheralBatteryWidget-1x1.ini` contains one card.
- `PeripheralBatteryWidget-2x1.ini` contains two cards.
- `PeripheralBatteryWidget.ini` contains four cards in a 2x2 grid.

Each layout defines only the card meters it uses. The Lua script uses the selected layout dimensions to resize the background and place the available cards.

## Change the timings

Edit `@Resources\Settings.inc`:

| Setting | Purpose | Default |
| --- | --- | ---: |
| `RainmeterUpdateMilliseconds` | How often Rainmeter updates the display and reads the snapshot file. Refresh the skin after changing it. | `1000` ms |
| `PollIntervalSeconds` | How often the helper asks tracked devices for fresh battery data. | `3` seconds |
| `DiscoveryIntervalSeconds` | How often the helper repeats full device, name, and battery-feature discovery when HID paths have not changed. | `60` seconds |
| `SnapshotCacheSeconds` | How long the last reading stays in the snapshot after a device stops responding. The card shows `RETRYING` while that reading is stale. | `60` seconds |
| `DiagnosticLogging` | Set to `1` to log helper startup, each device rediscovery result, HID initialization failure, and cache expiry to `diagnostics.log`; set to `0` to stop writing diagnostic entries. | `0` (off) |

The Rust helper rereads its settings while running, so timing and logging changes take effect without restarting it. Missing or invalid values use the defaults; Rust timing values are limited to 1–86400 seconds. Rainmeter reads its update interval when the skin loads, so refresh the skin after changing that value.

## Runtime files

- `%LOCALAPPDATA%\PeripheralBatteryWidget\devices.tsv` contains the current device rows used by the skin.
- `%LOCALAPPDATA%\PeripheralBatteryWidget\diagnostics.log` records helper startup, discovery, and cache-expiry information only when `DiagnosticLogging=1`. The log rotates at 512 KB when logging is enabled. Turning logging off stops new entries but keeps any existing log for later troubleshooting.

## Install or rebuild

1. Copy this folder into Rainmeter's `Skins` directory.
2. Refresh Rainmeter and load `PeripheralBatteryWidget\PeripheralBatteryWidget.ini`, or choose one of the smaller layout INIs.
3. Keep `headset-battery-bridge-rs.exe` beside the `@Resources` folder so the skin can start it.

The supplied helper executable is ready to run. To rebuild it with Rust installed, run these commands from the skin folder:

```powershell
cargo build --release --manifest-path "@Resources\headset-battery-bridge-rs\Cargo.toml"
Copy-Item "@Resources\headset-battery-bridge-rs\target\release\headset-battery-bridge.exe" "@Resources\headset-battery-bridge-rs.exe"
```
