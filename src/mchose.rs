//! MCHOSE wireless headsets on their 2.4 GHz dongles (V9 PRO measured: 291D:385D).
//!
//! The dongle is a C-Media composite device (USB audio + one HID interface with several
//! vendor-defined collections). Windows exposes no standard battery for it. The battery
//! channel is the HID collection on usage page 0xFF90: write a 64-byte output report
//! `55 65 01` and read the `55 65 <level> <state>` input report back. State byte:
//! 2 discharging, 3 charging, 4 full, 26 asleep.
//!
//! Protocol from github.com/JoaoKSS/MCHOSE_v9_PRO_Controller (same VID:PID), matching the
//! `65 01`-frame notes in the HaloBattery community fork. `55 11` returns the EQ mode and
//! feature report `AA 01 00` the dongle/headset firmware versions — unused here.
//!
//! A headset that is off or out of range stops the dongle from answering; we then report
//! Unreachable so the tracker keeps the last known level dimmed, like a sleeping Logitech.

use crate::device::{Kind, Observation, State};
use crate::diag;
use hidapi::{HidApi, HidDevice};
use std::time::{Duration, Instant};

const VID: u16 = 0x291D;
const USAGE_PAGE: u16 = 0xFF90;
const REPORT: u8 = 0x55;
const CMD_STATUS: u8 = 0x65;
const ATTEMPTS: u8 = 2;
const READ: Duration = Duration::from_millis(400);

const DISCHARGING: u8 = 2;
const CHARGING: u8 = 3;
const FULL: u8 = 4;
const ASLEEP: u8 = 26;

pub fn scan(api: &HidApi) -> Vec<Observation> {
    let mut out = Vec::new();
    for d in api
        .device_list()
        .filter(|d| d.vendor_id() == VID && d.usage_page() == USAGE_PAGE)
    {
        let pid = d.product_id();
        let key = format!("mchose:{pid:04x}:{}", out.len());
        let dev = match api.open_path(d.path()) {
            Ok(dev) => dev,
            Err(e) => {
                diag::event(&key, format!("open failed: {e}"));
                continue;
            }
        };
        let state = match battery(&dev) {
            Ok((level, state)) => match state {
                DISCHARGING | FULL => {
                    let s = if state == FULL { "full" } else { "discharging" };
                    diag::event(&key, format!("battery via report 0x{REPORT:02X} ({level}%, {s})"));
                    State::Battery { percent: level, charging: false }
                }
                CHARGING => {
                    diag::event(&key, format!("battery via report 0x{REPORT:02X} ({level}%, charging)"));
                    State::Battery { percent: level, charging: true }
                }
                ASLEEP => {
                    diag::event(&key, "headset asleep");
                    State::Unreachable
                }
                s => {
                    diag::event(&key, format!("unexpected charge state {s} (level {level}%)"));
                    continue;
                }
            },
            Err(why) => {
                // Dongle present, headset not answering: keep the last reading.
                diag::event(&key, why);
                State::Unreachable
            }
        };
        let name = d.product_string().unwrap_or_default().trim().to_string();
        out.push(Observation {
            id: key,
            name: Some(if name.is_empty() { "MCHOSE headset".into() } else { name }),
            kind: Some(Kind::Headset),
            state,
        });
    }
    out
}

/// (level, state) from the `65 01` status exchange, or why there's no reading.
fn battery(dev: &HidDevice) -> Result<(u8, u8), String> {
    let mut tx = [0u8; 64];
    tx[0] = REPORT;
    tx[1] = CMD_STATUS;
    tx[2] = 0x01;
    let mut last = None;
    for _ in 0..ATTEMPTS {
        dev.write(&tx).map_err(|e| format!("write failed: {e}"))?;
        let deadline = Instant::now() + READ;
        let mut rx = [0u8; 64];
        while Instant::now() < deadline {
            match dev.read_timeout(&mut rx, 50) {
                Ok(0) => continue,
                Ok(n) => {
                    last = Some((rx[0], rx[1]));
                    if n >= 4 && rx[0] == REPORT && rx[1] == CMD_STATUS && rx[2] <= 100 {
                        return Ok((rx[2], rx[3]));
                    }
                }
                Err(e) => return Err(format!("read failed: {e}")),
            }
        }
    }
    Err(match last {
        Some((a, b)) => format!("no status reply (last report: {a:02X} {b:02X} …)"),
        None => "no reply".into(),
    })
}

#[cfg(test)]
mod tests {
    // The parse is inlined in `battery`'s match; state mapping is exercised through it.
    #[test]
    fn state_constants_match_documented_protocol() {
        assert_eq!(
            (
                super::DISCHARGING,
                super::CHARGING,
                super::FULL,
                super::ASLEEP
            ),
            (2, 3, 4, 26)
        );
    }
}
