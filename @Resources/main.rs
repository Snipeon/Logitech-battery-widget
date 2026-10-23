#![windows_subsystem = "windows"]

use hidapi::{HidApi, HidDevice};
// Standard library tools for device data, settings, files, paths, and timers.
use std::{
    collections::HashMap,
    env, fs,
    fs::OpenOptions,
    io::Write,
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

#[cfg(windows)]
// Windows SetupAPI is used to find the friendly name for each HID interface.
use windows_sys::Win32::{
    Devices::{
        DeviceAndDriverInstallation::{
            SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInterfaces, SetupDiGetClassDevsW,
            SetupDiGetDeviceInterfaceDetailW, SetupDiGetDeviceRegistryPropertyW,
            DIGCF_DEVICEINTERFACE, DIGCF_PRESENT, SPDRP_DEVICEDESC, SPDRP_FRIENDLYNAME,
            SP_DEVICE_INTERFACE_DATA, SP_DEVICE_INTERFACE_DETAIL_DATA_W, SP_DEVINFO_DATA,
        },
        HumanInterfaceDevice::GUID_DEVINTERFACE_HID,
    },
    Foundation::HWND,
};

// Defaults used if a setting is missing or cannot be read from Settings.inc.
const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(3);
const DEFAULT_REDISCOVERY_INTERVAL: Duration = Duration::from_secs(60);
const DEFAULT_KEEP_LAST_READING: Duration = Duration::from_secs(60);
const LOGITECH_VID: u16 = 0x046d;

// Background settings the user can change in the shared settings file.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Settings {
    poll_interval: Duration,
    rediscovery_interval: Duration,
    keep_last_reading: Duration,
    // False by default so diagnostics do not grow unless the user enables them.
    diagnostic_logging: bool,
}

impl Default for Settings {
    fn default() -> Self {
        // Keep the helper running with the known-good timings if config is unavailable.
        Self {
            poll_interval: DEFAULT_POLL_INTERVAL,
            rediscovery_interval: DEFAULT_REDISCOVERY_INTERVAL,
            keep_last_reading: DEFAULT_KEEP_LAST_READING,
            diagnostic_logging: false,
        }
    }
}

// Find Settings.inc beside the helper executable in the @Resources folder.
fn settings_path() -> PathBuf {
    env::current_exe()
        .ok()
        .and_then(|executable| executable.parent().map(|dir| dir.join("Settings.inc")))
        .unwrap_or_else(|| PathBuf::from("Settings.inc"))
}

// Read the shared INI-style file and use defaults for invalid or missing values.
fn load_settings(path: &Path) -> Settings {
    // Keep safe defaults for missing or malformed values. Values are read again
    // each poll, so editing Settings.inc takes effect without restarting.
    let mut settings = Settings::default();
    let Ok(contents) = fs::read_to_string(path) else {
        return settings;
    };

    for line in contents.lines() {
        let line = line.split(';').next().unwrap_or("").trim();
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim();
        if key == "DiagnosticLogging" {
            settings.diagnostic_logging = match value {
                "1" => true,
                "0" => false,
                _ => settings.diagnostic_logging,
            };
            continue;
        }
        let Ok(seconds) = value.parse::<u64>() else {
            continue;
        };
        // Keep intervals within a useful range and prevent a zero-delay loop.
        let duration = Duration::from_secs(seconds.clamp(1, 86_400));
        match key {
            "PollIntervalSeconds" => settings.poll_interval = duration,
            "DiscoveryIntervalSeconds" => settings.rediscovery_interval = duration,
            "SnapshotCacheSeconds" => settings.keep_last_reading = duration,
            _ => {}
        }
    }
    settings
}

// These are the battery states the widget can display.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ChargeState {
    Discharging,
    Charging,
    Full,
    Fault,
}

impl ChargeState {
    // Convert the state to the short text written into the snapshot file.
    fn as_tsv(self) -> &'static str {
        match self {
            Self::Discharging => "discharging",
            Self::Charging => "charging",
            Self::Full => "full",
            Self::Fault => "fault",
        }
    }
}

// One successful battery reading, ready to be written for the widget.
#[derive(Clone, Debug)]
struct BatteryDevice {
    // HID++ unit ID when available, otherwise the interface path plus slot.
    // This keeps two same-model devices separate while merging duplicate HID
    // collections that resolve to the same physical peripheral.
    id: String,
    // The name shown in the widget. Prefer the name reported by the peripheral
    // over a generic receiver or Windows driver label.
    name: String,
    percent: u8,
    charge_state: ChargeState,
}

// Start the helper, discover devices, poll them, and refresh the snapshot.
fn main() {
    // Rainmeter starts this process as a hidden sidecar. It owns the polling
    // loop and writes a small TSV snapshot that the skin's Lua script reads.
    let Some(local_app_data) = env::var_os("LOCALAPPDATA") else {
        return;
    };

    // Keep runtime data outside the skin folder so Rainmeter can read it without
    // modifying the installed package. The log records polling and HID errors.
    let output = PathBuf::from(local_app_data)
        .join("PeripheralBatteryWidget")
        .join("devices.tsv");
    let diagnostics = output.with_file_name("diagnostics.log");
    // Load timings before the first discovery and keep reloading them in the loop.
    let settings_file = settings_path();
    let mut settings = load_settings(&settings_file);
    if let Some(parent) = output.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if settings.diagnostic_logging {
        log_diagnostic(
            &diagnostics,
            &format!(
                "helper_started poll_interval_seconds={} rediscovery_interval_seconds={} cache_ttl_seconds={}",
                settings.poll_interval.as_secs(),
                settings.rediscovery_interval.as_secs(),
                settings.keep_last_reading.as_secs()
            ),
        );
    }

    // Keep HID handles and feature indexes open between battery polls. Device
    // discovery is slower and runs only after a topology change or on a timer.
    let Ok(mut api) = HidApi::new() else {
        if settings.diagnostic_logging {
            log_diagnostic(&diagnostics, "hidapi_initialization_failed");
        }
        return;
    };
    // Keep discovered HID handles so each poll can go straight to battery data.
    let mut tracked = Vec::new();
    let mut known_paths = Vec::new();
    let mut next_discovery = Instant::now();
    let mut snapshot_cache = HashMap::new();
    loop {
        // Re-read timing settings so edits take effect without restarting the helper.
        let updated_settings = load_settings(&settings_file);
        if updated_settings.rediscovery_interval != settings.rediscovery_interval {
            next_discovery = Instant::now() + updated_settings.rediscovery_interval;
        }
        settings = updated_settings;

        let poll_started = Instant::now();
        let now = Instant::now();
        // Refresh the list cheaply; only run the slower full scan when it changed or its timer expires.
        if api.refresh_devices().is_ok() {
            let paths = candidate_paths(&api);
            let signature: Vec<_> = paths.iter().map(|p| p.stable_path.clone()).collect();
            let topology_changed = signature != known_paths;
            if topology_changed || now >= next_discovery {
                tracked = discover_logitech_devices(&api, paths);
                known_paths = signature;
                next_discovery = Instant::now() + settings.rediscovery_interval;
                if settings.diagnostic_logging {
                    log_diagnostic(
                        &diagnostics,
                        &format!("discovery_complete tracked_devices={}", tracked.len()),
                    );
                }
            }
        }

        // Battery reads use the retained path, slot, and feature index. This
        // avoids repeating receiver scans and name/feature queries every 3s.
        let devices = tracked
            .iter()
            .filter_map(TrackedDevice::poll)
            .collect::<Vec<_>>();

        // The snapshot layer holds last-good values briefly to smooth over
        // missed responses, marks them stale, and removes expired devices.
        write_snapshot(
            &output,
            &diagnostics,
            &mut snapshot_cache,
            &devices,
            settings.keep_last_reading,
            settings.poll_interval,
            settings.diagnostic_logging,
        );

        // Sleep after polling; poll-start spacing includes time spent reading
        // each currently tracked device.
        thread::sleep(
            settings
                .poll_interval
                .saturating_sub(poll_started.elapsed()),
        );
    }
}

// LogitechBatteryMonitor's HID++ and Centurion discovery/battery logic,
// implemented directly in this sidecar instead of running its tray app.
// Describes which protocol the helper should use for a discovered device.
#[derive(Clone, Copy, Debug)]
enum TrackedProtocol {
    Hidpp {
        device_index: u8,
        feature: u16,
        feature_index: u8,
    },
    Centurion,
}

// A device that has passed discovery and is ready for repeated battery reads.
struct TrackedDevice {
    hid: HidDevice,
    id: String,
    name: String,
    protocol: TrackedProtocol,
}

impl TrackedDevice {
    // Read one current battery result using the protocol saved during discovery.
    fn poll(&self) -> Option<BatteryDevice> {
        let (percent, charge_state) = match self.protocol {
            TrackedProtocol::Hidpp {
                device_index,
                feature,
                feature_index,
            } => Hidpp(&self.hid).read_battery(device_index, feature, feature_index)?,
            TrackedProtocol::Centurion => read_centurion_battery(&self.hid)?,
        };
        Some(BatteryDevice {
            id: self.id.clone(),
            name: self.name.clone(),
            percent,
            charge_state,
        })
    }
}

// HID interface details needed to open a device and choose a useful name.
struct HidPath {
    path: std::ffi::CString,
    stable_path: String,
    usage_page: u16,
    product_name: Option<String>,
}

// Keep Logitech interfaces that are likely to support HID++ or Centurion queries.
fn candidate_paths(api: &HidApi) -> Vec<HidPath> {
    let mut paths: Vec<_> = api
        .device_list()
        .filter(|d| {
            d.vendor_id() == LOGITECH_VID
                && ((0xff00..=0xffff).contains(&d.usage_page()) || d.usage_page() == 0x0059)
        })
        .map(|d| HidPath {
            path: d.path().to_owned(),
            stable_path: d.path().to_string_lossy().to_lowercase(),
            usage_page: d.usage_page(),
            product_name: d.product_string().map(str::to_owned),
        })
        .collect();
    // A consistent order makes path comparisons reliable between scans.
    paths.sort_by(|left, right| left.stable_path.cmp(&right.stable_path));
    paths
}

// Probe candidate HID interfaces and keep only devices that answer battery queries.
fn discover_logitech_devices(api: &HidApi, paths: Vec<HidPath>) -> Vec<TrackedDevice> {
    // Build one Windows PnP lookup table so a generic HID++ name can be
    // replaced with the Friendly Name belonging to that exact HID path. This
    // SetupAPI scan happens only during rediscovery, never every battery poll.
    let pnp_names = windows_hid_friendly_names();
    let mut devices = Vec::new();
    for candidate in paths {
        let HidPath {
            path,
            stable_path,
            usage_page,
            product_name,
        } = candidate;
        let pnp_friendly_name = pnp_names.get(&stable_path).map(String::as_str);
        // A HID path is only a transport. We still have to speak HID++ to find
        // out whether a Logitech device behind it provides battery information.
        let Ok(hid) = api.open_path(&path) else {
            continue;
        };
        // Centurion devices use their own report format instead of HID++.
        if usage_page == 0xffa0 {
            let id = format!("{stable_path}#centurion");
            let name = preferred_name(
                "Logitech wireless headset",
                pnp_friendly_name,
                product_name.as_deref(),
            );
            add_unique_tracked(
                &mut devices,
                TrackedDevice {
                    hid,
                    id,
                    name,
                    protocol: TrackedProtocol::Centurion,
                },
            );
            continue;
        }
        let channel = Hidpp(&hid);
        // Prefer the direct transport when it actually exposes battery data.
        // Only scan receiver slots if this HID path is not a direct battery device.
        if channel.probe(0xff) {
            if let Some((name, id, feature, feature_index)) =
                channel.discover_device(0xff, &stable_path)
            {
                let name = preferred_name(&name, pnp_friendly_name, product_name.as_deref());
                add_unique_tracked(
                    &mut devices,
                    TrackedDevice {
                        hid,
                        id,
                        name,
                        protocol: TrackedProtocol::Hidpp {
                            device_index: 0xff,
                            feature,
                            feature_index,
                        },
                    },
                );
                continue;
            }
        }
        // Receiver slots use indexes 1 through 6; each responding slot is checked separately.
        for index in 1..=6 {
            if channel.probe(index) {
                if let Some((name, id, feature, feature_index)) =
                    channel.discover_device(index, &stable_path)
                {
                    let name = preferred_name(&name, pnp_friendly_name, product_name.as_deref());
                    if let Ok(slot_hid) = api.open_path(&path) {
                        add_unique_tracked(
                            &mut devices,
                            TrackedDevice {
                                hid: slot_hid,
                                id,
                                name,
                                protocol: TrackedProtocol::Hidpp {
                                    device_index: index,
                                    feature,
                                    feature_index,
                                },
                            },
                        );
                    }
                }
            }
        }
    }
    devices
}

// Keep a specific name reported by the device; use Windows/product names only for generic ones.
fn preferred_name(reported: &str, pnp_name: Option<&str>, product_name: Option<&str>) -> String {
    if !is_generic_device_name(reported) {
        return reported.trim().to_string();
    }
    // Prefer the Windows friendly label for this exact interface, then its USB product name.
    pnp_name
        .map(str::trim)
        .filter(|name| !name.is_empty() && !is_generic_device_name(name))
        .or_else(|| {
            product_name
                .map(str::trim)
                .filter(|name| !name.is_empty() && !is_generic_device_name(name))
        })
        .unwrap_or(reported)
        .trim()
        .to_string()
}

// Avoid showing one physical device more than once through multiple HID interfaces.
fn add_unique_tracked(devices: &mut Vec<TrackedDevice>, device: TrackedDevice) {
    if !devices.iter().any(|existing| existing.id == device.id) {
        devices.push(device);
    }
}

// LogitechBatteryMonitor also handles some newer wireless headsets through
// Centurion rather than HID++. This read-only 0x51/64-byte request is retained
// on the tracked device and repeated without re-running discovery.
// Ask a Centurion device for its battery level and charging state.
fn read_centurion_battery(hid: &HidDevice) -> Option<(u8, ChargeState)> {
    let mut request = [0u8; 64];
    request[0] = 0x51;
    request[1] = 0x08;
    request[3] = 0x03;
    request[4] = 0x1a;
    request[6] = 0x03;
    request[8] = 0x04;
    request[9] = 0x0a;
    hid.write(&request).ok()?;

    for _ in 0..8 {
        let mut response = [0u8; 64];
        let count = hid.read_timeout(&mut response, 250).ok()?;
        if count >= 13 && response[0] == 0x51 && response[1] == 0x0b && response[8] == 0x04 {
            let percent = response[10].min(100);
            let charge_state = match response[12] {
                0x02 => ChargeState::Charging,
                0x03 => ChargeState::Full,
                _ => ChargeState::Discharging,
            };
            return Some((percent, charge_state));
        }
        // ACK and power-event frames precede the battery response on some
        // firmwares. Keep reading past them; an explicit offline frame is not
        // battery data and is retried on the next poll.
    }
    None
}

// Detect common driver/receiver labels that do not identify the actual device model.
fn is_generic_device_name(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    name == "logitech wireless headset"
        || name.contains("vhf driver")
        || name.contains("hid-compliant")
        || name.contains("usb input device")
        || name == "usb receiver"
        || name.starts_with("logitech hid++ device")
}

#[cfg(windows)]
// Match Windows HID paths to their PnP Friendly Names for name fallback.
fn windows_hid_friendly_names() -> HashMap<String, String> {
    use std::{mem, ptr, slice};

    // Ask Windows for all currently connected HID interfaces. Each entry has
    // a device path matching the path hidapi opens for the HID++ conversation.
    let mut names = HashMap::new();
    unsafe {
        let set = SetupDiGetClassDevsW(
            &GUID_DEVINTERFACE_HID,
            ptr::null(),
            0 as HWND,
            DIGCF_PRESENT | DIGCF_DEVICEINTERFACE,
        );
        if set == -1 {
            return names;
        }

        // Walk each connected HID interface and ask Windows for its path and name.
        let mut index = 0;
        loop {
            let mut interface = SP_DEVICE_INTERFACE_DATA {
                cbSize: mem::size_of::<SP_DEVICE_INTERFACE_DATA>() as u32,
                ..Default::default()
            };
            if SetupDiEnumDeviceInterfaces(
                set,
                ptr::null(),
                &GUID_DEVINTERFACE_HID,
                index,
                &mut interface,
            ) == 0
            {
                break;
            }
            index += 1;

            // Windows first tells us how much memory the path structure needs.
            let mut required = 0u32;
            let _ = SetupDiGetDeviceInterfaceDetailW(
                set,
                &interface,
                ptr::null_mut(),
                0,
                &mut required,
                ptr::null_mut(),
            );
            if required < mem::size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32 {
                continue;
            }

            // Use usize storage so the buffer is suitably aligned for Windows' structure.
            let words = (required as usize + mem::size_of::<usize>() - 1) / mem::size_of::<usize>();
            let mut storage = vec![0usize; words];
            let detail = storage.as_mut_ptr().cast::<u8>();
            // The path begins immediately after cbSize in the variable-length structure.
            *(detail.cast::<u32>()) = mem::size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32;
            let mut device_info = SP_DEVINFO_DATA {
                cbSize: mem::size_of::<SP_DEVINFO_DATA>() as u32,
                ..Default::default()
            };
            if SetupDiGetDeviceInterfaceDetailW(
                set,
                &interface,
                detail.cast::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>(),
                required,
                ptr::null_mut(),
                &mut device_info,
            ) == 0
            {
                continue;
            }

            let path_ptr = detail.add(4).cast::<u16>();
            let max_units = required.saturating_sub(4) as usize / mem::size_of::<u16>();
            let mut path_len = 0;
            while path_len < max_units && *path_ptr.add(path_len) != 0 {
                path_len += 1;
            }
            if path_len == 0 || path_len >= max_units {
                continue;
            }
            let pnp_path =
                String::from_utf16_lossy(slice::from_raw_parts(path_ptr, path_len)).to_lowercase();

            // Friendly Name is the user-facing Windows label. Some HID
            // collections have no Friendly Name, so use Device Description.
            let friendly_name = read_pnp_name(set, &device_info, SPDRP_FRIENDLYNAME)
                .or_else(|| read_pnp_name(set, &device_info, SPDRP_DEVICEDESC));
            if let Some(name) = friendly_name.filter(|name| !name.trim().is_empty()) {
                names.insert(pnp_path, name.trim().to_string());
            }
        }
        let _ = SetupDiDestroyDeviceInfoList(set);
    }
    names
}

#[cfg(not(windows))]
// Other operating systems do not have the Windows friendly-name lookup.
fn windows_hid_friendly_names() -> HashMap<String, String> {
    HashMap::new()
}

#[cfg(windows)]
// Read one text label from the Windows device-properties table.
unsafe fn read_pnp_name(
    set: isize,
    device_info: &SP_DEVINFO_DATA,
    property: u32,
) -> Option<String> {
    use std::{mem, slice};

    // First call asks Windows for the buffer size; the second gets the UTF-16 name.
    let mut required = 0u32;
    let _ = SetupDiGetDeviceRegistryPropertyW(
        set,
        device_info,
        property,
        std::ptr::null_mut(),
        std::ptr::null_mut(),
        0,
        &mut required,
    );
    if !(2..=65536).contains(&required) {
        return None;
    }
    let mut bytes = vec![0u8; required as usize];
    if SetupDiGetDeviceRegistryPropertyW(
        set,
        device_info,
        property,
        std::ptr::null_mut(),
        bytes.as_mut_ptr(),
        required,
        &mut required,
    ) == 0
    {
        return None;
    }
    let units = slice::from_raw_parts(
        bytes.as_ptr().cast::<u16>(),
        bytes.len() / mem::size_of::<u16>(),
    );
    let end = units
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(units.len());
    Some(String::from_utf16_lossy(&units[..end]))
}

// Small wrapper that sends HID++ messages through an already-open HID handle.
struct Hidpp<'a>(&'a HidDevice);

impl Hidpp<'_> {
    // Check whether this direct address or receiver slot responds as a HID++ device.
    fn probe(&self, device: u8) -> bool {
        self.request_with_timeout(device, 0, 1, &[0, 0, 0xaa], 300)
            .is_some_and(|response| response[0] >= 2)
    }

    // Find a supported battery feature, device name, stable ID, and feature index.
    fn discover_device(&self, device: u8, stable_path: &str) -> Option<(String, String, u16, u8)> {
        // Match the reference monitor's feature priority. 0x1F20 is used by
        // Logitech headsets such as the G933; omitting it makes those headsets
        // invisible even though they answer HID++ correctly.
        let (feature, feature_index) =
            [0x1f20u16, 0x1004, 0x1001, 0x1000]
                .into_iter()
                .find_map(|feature| {
                    self.feature_index(device, feature)
                        .map(|idx| (feature, idx))
                })?;
        // Ask the peripheral for its own name. If it doesn't implement the
        // naming feature, fall back to a transport/receiver-slot description.
        let name = self.device_name(device).unwrap_or_else(|| {
            if device == 0xff {
                "Logitech HID++ device (direct)".to_string()
            } else {
                format!("Logitech HID++ device (receiver slot {device})")
            }
        });
        let id = self
            .device_unit_id(device)
            .map(|unit| format!("unit:{unit:08x}"))
            .unwrap_or_else(|| format!("{stable_path}#{device:02x}"));
        Some((name, id, feature, feature_index))
    }

    // Send a normal HID++ request with the usual response timeout.
    fn request(&self, device: u8, feature: u8, function: u8, params: &[u8]) -> Option<[u8; 16]> {
        self.request_with_timeout(device, feature, function, params, 800)
    }

    // Build a HID++ packet, send it, and wait for the matching response.
    fn request_with_timeout(
        &self,
        device: u8,
        feature: u8,
        function: u8,
        params: &[u8],
        timeout_ms: u64,
    ) -> Option<[u8; 16]> {
        const REPORT_ID: u8 = 0x11;
        const SOFTWARE_ID: u8 = 0x07;
        let sw = (function << 4) | SOFTWARE_ID;
        let mut packet = [0u8; 20];
        packet[0] = REPORT_ID;
        packet[1] = device;
        packet[2] = feature;
        packet[3] = sw;
        // Copy only what fits in the HID++ parameter area of this packet.
        let count = params.len().min(16);
        packet[4..4 + count].copy_from_slice(&params[..count]);
        self.0.write(&packet).ok()?;

        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        let mut report = [0u8; 64];
        // Ignore unrelated reports until this request's response arrives or times out.
        while Instant::now() < deadline {
            let remaining = (deadline - Instant::now()).as_millis().clamp(1, 100) as i32;
            let n = self.0.read_timeout(&mut report, remaining).ok()?;
            if n < 7 || !matches!(report[0], 0x10..=0x12) || report[1] != device {
                continue;
            }
            if (report[2] == 0xff || report[2] == 0x8f) && report[3] == feature && report[4] == sw {
                return None;
            }
            if report[2] == feature && report[3] == sw {
                let mut response = [0u8; 16];
                let count = (n - 4).min(16);
                response[..count].copy_from_slice(&report[4..4 + count]);
                return Some(response);
            }
        }
        None
    }

    // Ask the device for the short index used to call one of its battery features.
    fn feature_index(&self, device: u8, feature: u16) -> Option<u8> {
        let response = self.request(device, 0, 0, &[(feature >> 8) as u8, feature as u8])?;
        (response[0] != 0).then_some(response[0])
    }

    // Read the device-reported name in chunks from its HID++ Device Name feature.
    fn device_name(&self, device: u8) -> Option<String> {
        // HID++ feature 0x0005 is Device Name. First find its feature index.
        let index = self.feature_index(device, 0x0005)?;
        // Function 0 reports the name length; cap it at 64 bytes so a malformed
        // device response cannot cause an unexpectedly large allocation.
        let first = self.request(device, index, 0, &[])?;
        let target_len = (first[0] as usize).min(64);
        if target_len == 0 {
            return None;
        }
        let mut bytes = Vec::with_capacity(target_len);
        while bytes.len() < target_len {
            // Function 1 returns the next name chunk, beginning at the offset
            // currently stored in bytes.len(). Repeat until the name is complete.
            let response = self.request(device, index, 1, &[bytes.len() as u8])?;
            let remaining = target_len - bytes.len();
            bytes.extend(response.into_iter().take(remaining).take_while(|b| *b != 0));
            if bytes.is_empty() {
                return None;
            }
        }
        let name = String::from_utf8_lossy(&bytes).trim().to_string();
        (!name.is_empty()).then_some(name)
    }

    // Read a stable ID so two interfaces for the same device can be combined.
    fn device_unit_id(&self, device: u8) -> Option<u32> {
        // Device Information (0x0003), GetDeviceInfo. Bytes 1..4 of the
        // parameter block are the unit ID. It is stable across HID collections
        // and receiver/direct transports for the same physical device.
        let index = self.feature_index(device, 0x0003)?;
        let info = self.request(device, index, 0, &[])?;
        let unit = u32::from_be_bytes([info[1], info[2], info[3], info[4]]);
        (unit != 0).then_some(unit)
    }

    // Read level and charging state using whichever supported battery feature was found.
    fn read_battery(&self, device: u8, feature: u16, index: u8) -> Option<(u8, ChargeState)> {
        match feature {
            // Logitech headset BatteryStatus1f20: one voltage reading and a
            // bit-field status byte. Full (0x07) also has the charging bit set,
            // so check full and fault before checking charging.
            0x1f20 => {
                let response = self.request(device, index, 0, &[])?;
                let mv = u16::from_be_bytes([response[0], response[1]]) as i32;
                let status = response[2];
                let state = if status & 0x08 != 0 {
                    ChargeState::Fault
                } else if status & 0x04 != 0 {
                    ChargeState::Full
                } else if status & 0x02 != 0 {
                    ChargeState::Charging
                } else {
                    ChargeState::Discharging
                };
                Some((percent_from_voltage(mv), state))
            }
            0x1004 => {
                // Unified Battery can report an exact percentage or only coarse level flags.
                let capabilities = self.request(device, index, 0, &[])?;
                let response = self.request(device, index, 1, &[])?;
                let level_flags = response[1];
                let charge_status = response[2];
                let state = match charge_status {
                    0 => ChargeState::Discharging,
                    1 | 2 => ChargeState::Charging,
                    3 => ChargeState::Full,
                    _ => ChargeState::Discharging,
                };
                let percent = if capabilities[1] & 0x02 != 0 && response[0] != 0 {
                    response[0].min(100)
                } else if level_flags == 0x08 {
                    100
                } else if level_flags == 0x04 {
                    50
                } else if level_flags == 0x02 {
                    15
                } else {
                    5
                };
                Some((percent, state))
            }
            0x1000 => {
                // Battery Status reports a percentage and a numeric charge state.
                let response = self.request(device, index, 0, &[])?;
                let state = match response[2] {
                    0 => ChargeState::Discharging,
                    1 | 2 | 4 => ChargeState::Charging,
                    3 => ChargeState::Full,
                    _ => ChargeState::Discharging,
                };
                Some((response[0].min(100), state))
            }
            0x1001 => {
                // Battery Voltage reports voltage and whether the device is charging.
                let response = self.request(device, index, 0, &[])?;
                let mv = u16::from_be_bytes([response[0], response[1]]) as i32;
                let flags = response[2];
                // BatteryVoltage only defines the bit-7 charging flag; it has
                // no reliable full state, so preserve the two-state semantics.
                let state = if flags & 0x80 != 0 {
                    ChargeState::Charging
                } else {
                    ChargeState::Discharging
                };
                Some((percent_from_voltage(mv), state))
            }
            _ => None,
        }
    }
}

// Estimate a percentage from battery voltage for devices that do not report one directly.
fn percent_from_voltage(mv: i32) -> u8 {
    // Known voltage/percentage points; readings between them are interpolated below.
    const CURVE: [(i32, i32); 13] = [
        (4186, 100),
        (4067, 90),
        (3989, 80),
        (3922, 70),
        (3859, 60),
        (3811, 50),
        (3778, 40),
        (3751, 30),
        (3717, 20),
        (3671, 10),
        (3646, 5),
        (3579, 2),
        (3500, 0),
    ];
    if mv >= CURVE[0].0 {
        return 100;
    }
    if mv <= CURVE[CURVE.len() - 1].0 {
        return 0;
    }
    for pair in CURVE.windows(2) {
        let (high_mv, high_pct) = pair[0];
        let (low_mv, low_pct) = pair[1];
        if mv >= low_mv {
            let ratio = (mv - low_mv) as f32 / (high_mv - low_mv) as f32;
            return (low_pct as f32 + ratio * (high_pct - low_pct) as f32 + 0.5) as u8;
        }
    }
    0
}

// Append a timestamped diagnostic line and rotate the file when it grows too large.
fn log_diagnostic(path: &Path, message: &str) {
    const MAX_LOG_BYTES: u64 = 512 * 1024;
    if fs::metadata(path).is_ok_and(|metadata| metadata.len() >= MAX_LOG_BYTES) {
        let previous = path.with_file_name("diagnostics.log.1");
        let _ = fs::remove_file(&previous);
        let _ = fs::rename(path, previous);
    }
    let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) else {
        return;
    };
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let _ = writeln!(
        file,
        "unix={}.{:03} {message}",
        elapsed.as_secs(),
        elapsed.subsec_millis()
    );
}

// Keep recent readings through brief failures, then write the current device list for Rainmeter.
fn write_snapshot(
    path: &Path,
    diagnostics: &Path,
    cache: &mut HashMap<String, (BatteryDevice, Instant)>,
    devices: &[BatteryDevice],
    keep_last_reading: Duration,
    poll_interval: Duration,
    diagnostic_logging: bool,
) {
    // A transient HID timeout should not make a tile disappear. Keep its last row
    // for the configured cache period and mark it stale while waiting for a new read.
    let now = Instant::now();
    for device in devices {
        // Replacing a key refreshes that device's last-seen time and reading.
        cache.insert(device.id.clone(), (device.clone(), now));
    }
    // Keep missed readings temporarily, then remove them after the TTL and log
    // that removal. A stale row stays visible with its last reading meanwhile.
    let expired: Vec<_> = cache
        .iter()
        .filter(|(_, (_, last_seen))| now.duration_since(*last_seen) > keep_last_reading)
        .map(|(key, (device, _))| (key.clone(), device.name.clone()))
        .collect();
    for (key, name) in expired {
        if diagnostic_logging {
            log_diagnostic(
                diagnostics,
                &format!("cache_expired source=logitech name={name:?}"),
            );
        }
        cache.remove(&key);
    }
    let mut rows = cache
        .iter()
        .map(|(_, (d, last_seen))| {
            let stale = now.duration_since(*last_seen) >= poll_interval + Duration::from_secs(2);
            // The Lua reader expects exactly four tab-separated columns:
            // name, percentage (blank if unavailable), charging flag, stale flag.
            (
                format!("{}:{}", d.name.to_lowercase(), d.id),
                format!(
                    "{}\t{}\t{}\t{}\r\n",
                    d.name,
                    d.percent,
                    d.charge_state.as_tsv(),
                    u8::from(stale)
                ),
            )
        })
        .collect::<Vec<_>>();
    // Keep the tile order predictable by sorting device names alphabetically.
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    let rows = rows.into_iter().map(|(_, row)| row).collect::<String>();
    let _ = fs::write(path, &rows);
}
