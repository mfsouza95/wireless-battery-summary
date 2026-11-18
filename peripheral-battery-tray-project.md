# Peripheral Battery Tray App — Project Summary

## 1. Goal

A single Windows tray icon that shows the battery level of all my wireless peripherals (all on 2.4 GHz dongles) in one popup, and draws my attention when something is running low.

Reference project: <https://github.com/renanleonel/windows-taskbar-battery> (Rust; Logitech HID++ and Bluetooth; one widget per device embedded in the Windows 11 taskbar).

## 2. Desired behavior

- **One tray icon** instead of one icon per device.
- **Click the icon** → a small popup lists every connected device with its icon, name, percentage and a bar.
- **At or below 20%** on any device → a warning badge is added to the main icon.
- **At or below 10%** on any device → the main icon changes to that device's own icon (mouse, headset, ...), ideally in red.
- If several devices are low, the icon reflects the lowest one.
- Devices that are charging do not trigger warnings.
- Charging badge (⚡) is optional and low priority: most of my devices only charge over the cable, so they leave the dongle when plugged in.

### Small design details

- Hysteresis: clear the warning a bit above the threshold (e.g. 22%) so the icon doesn't flicker.
- A sleeping Logitech device keeps its last known level, so it can still trigger a warning.
- Devices that don't report battery are shown greyed out as "battery not reported" and never trigger warnings.

## 3. Approach

**Fork the reference repo** rather than start from scratch.

| Part of the repo | Plan |
| --- | --- |
| Logitech HID++ code, device model, polling | Keep |
| Bluetooth / Xbox paths | Keep if useful, otherwise ignore |
| Taskbar-embedded widget (`widget.rs`) | **Replace** with a tray icon + popup |
| Renderer (`render.rs`) | Reuse for drawing the popup and the generated icon |

Replacing the taskbar embedding removes the hackiest part of the original (an undocumented approach that can break with Windows updates).

### Tray icon

- Use `Shell_NotifyIcon`; generate the icon image at runtime (16×16, DPI-scaled) from the current state: plain → warning badge → device icon.
- Popup: a tool window (no taskbar button), anchored with `Shell_NotifyIconGetRect`, closed when it loses focus.
- Windows 11 gotcha: new tray icons start in the overflow area. Pin it once in *Settings › Personalization › Taskbar › Other system tray icons*, and mention this in the README.

### Adding devices

Each new device type is a small module next to the `logitech/` code that returns a list of `Observation`s. The popup and warning logic don't care where a reading came from.

## 4. Devices and status

| Device | Connection | Battery reporting | Status |
| --- | --- | --- | --- |
| Logitech PRO X 2 | LIGHTSPEED receiver C54D | HID++ 0x1004 (percent) | ✅ Confirmed: 38% |
| Logitech G502 X PLUS | LIGHTSPEED receiver C547 | HID++ 0x1004 (percent) | ✅ Confirmed: 68% |
| MCHOSE V9 PRO headset | 2.4 GHz dongle (also BT / wired) | Vendor HID protocol | ✅ Confirmed: 60% discharging |
| Keyboard RD75 PRO (Womier, QMK/VIA) | 2.4 GHz dongle (also BT / wired) | Unknown | Not arrived yet |
| EasySMX X20 controller | 2.4 GHz dongle (also BT / wired) | Unknown, possibly none | Not arrived yet |

### Step 0 findings (2026-11-18)

- **Both Logitech mice work out of the box** with the repo's existing code
  (`cargo run -- --once`). Each has its own receiver (C54D / C547); both report a
  true percentage via HID++ feature 0x1004.
- **MCHOSE V9 PRO reports battery** over its 2.4 GHz dongle (VID 291D, PID 385D,
  C-Media composite: USB audio + one HID interface). Windows exposes **no**
  standard battery for it (WinRT `Battery` enumeration = 0 devices). The working
  exchange, on the HID collection usage page **0xFF90** usage 0x0001: write a
  64-byte output report `55 65 01`, read `55 65 <level> <state>` — state 2 =
  discharging, 3 = charging, 4 = full, 26 = asleep. Also `55 11` → EQ mode, and
  feature report `AA 01 00` → dongle/headset firmware. Protocol documented by
  github.com/JoaoKSS/MCHOSE_v9_PRO_Controller (same VID:PID) and the
  `65 01`-frame notes in the HaloBattery community fork.
- The dongle's other collections: telephony (0x000B), consumer (0x000C), vendor
  0xFF21 / 0xFF22 / 0xFF82 — none answer the `0x55` report; 0xFF21 has a feature
  report 0xED that returns data but did not track battery level (purpose
  unknown, ignored).
- Exploration tooling: `cargo run --example mchose_probe` (V9 exchange),
  `-- --poll` (0xED watch), `-- --full` (report-id scan).

Notes from research:

- I found **no public protocol documentation** for the headset, keyboard or controller dongles, and no sign of HeadsetControl support for the V9 PRO.
- MCHOSE provides vendor software under "Drivers & Firmwares" on mchose.store.
- EasySMX X20: reviews say its 4 LEDs do not show the real charge level and a buyer asked for a battery indicator, so it may not report battery over 2.4 GHz at all.
- Womier RD75 Pro: the manual-style battery check I found (Fn+Enter) was for a similar Royal Kludge keyboard and is done on the keyboard itself, so it says nothing about what the PC receives.

## 5. Exploration tactics (per device)

The core question: **does the battery percentage reach the PC at all?** If the vendor's own app can't show it, the device can probably be written off.

1. **Vendor app check.** Install the vendor software, connect via the 2.4 GHz dongle, and see whether it shows a battery percentage.
   - Headset: MCHOSE driver software
   - Keyboard: Womier / VIA configurator
   - Controller: EasySMX Keylinker
2. **Windows check.** Look at the dongle's entries in Device Manager (HID, Sound devices). Some dongles expose a standard battery property, which would need no reverse engineering. For the controller, also test what `Windows.Gaming.Input` / XInput report.
3. **HID dump.** Enumerate the dongle's HID interfaces (vendor ID, product ID, usage pages). A vendor-defined interface (usage page `0xFF00` or similar) is where a battery report would likely live. A small Python script using `hidapi` is enough.
4. **Traffic capture** (only if step 1 shows a percentage). Run USBPcap + Wireshark while the vendor app is open, and change the battery state (turn the device off/on, charge it briefly). Compare captures to find the bytes that track the level, then replay the request.
5. **Check granularity.** Is it a real percentage or coarse steps (e.g. 4 levels)? Coarse steps change how the 20% / 10% thresholds behave for that device.

## 6. Risks

- A device that genuinely doesn't expose its battery can't be supported, no matter the effort.
- The vendor app may read the battery over a private channel that is harder to reverse engineer.
- Hardware testing is the slow part of the project, not the UI.

## 7. Next steps

1. ~~Headset: vendor app, Windows check, HID dump~~ **Done** — protocol confirmed on hardware; no vendor app or USBPcap needed (community driver already documented it).
2. Fork the reference repo and replace the taskbar widget with the tray icon + popup, running with just the two Logitech mice.
3. Add the headset module if the exploration succeeds.
4. When the keyboard and controller arrive, repeat the exploration for each.
5. Decide later whether the charging badge is worth adding.
