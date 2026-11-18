//! Step-0 exploration for the MCHOSE V9 PRO 2.4 GHz dongle (VID 291D, PID 385D).
//!
//! The dongle is a C-Media composite device: USB audio plus one HID interface with
//! vendor-defined collections (usage pages 0xFF21, 0xFF22, 0xFF90, 0xFF82), consumer
//! control and telephony. Windows reports no standard battery for it, so any battery
//! data must live in one of these collections.
//!
//! Modes:
//!   cargo run --example mchose_probe           V9 battery/EQ exchange on every collection
//!   cargo run --example mchose_probe -- --poll poll feature report 0xED on 0xFF21 once/second
//!   cargo run --example mchose_probe -- --full scan every report id on every collection
//!
//! Protocol (github.com/JoaoKSS/MCHOSE_v9_PRO_Controller, same 291D:385D device):
//! write output report 0x55 + "65 01", read 0x55 reply: byte1 0x65, byte2 level, byte3
//! state (2 discharging, 3 charging, 4 full, 26 asleep). EQ: 0x55 + "11" -> mode byte 2.
//! Firmware: send feature report "AA 01 00", get feature report 0xAA -> dongle fw 8..10,
//! headset fw 10..12.

use hidapi::HidApi;
use std::time::{Duration, Instant};

const VID: u16 = 0x291D;

fn main() {
    let api = HidApi::new().expect("hidapi");
    let arg = std::env::args().nth(2).unwrap_or_default();
    let mut found = false;
    for d in api.device_list().filter(|d| d.vendor_id() == VID) {
        found = true;
        println!(
            "pid={:04X} if={} usage_page={:04X} usage={:04X} product={:?}",
            d.product_id(),
            d.interface_number(),
            d.usage_page(),
            d.usage(),
            d.product_string().unwrap_or("?"),
        );
        match arg.as_str() {
            "--full" => scan(d, &api),
            "--poll" if d.usage_page() == 0xFF21 => poll(d, &api),
            "--poll" => {}
            _ => exchange(d, &api),
        }
    }
    if !found {
        println!("no VID {VID:04X} device present");
    }
}

/// The V9 exchange: output report 0x55 on whatever collection accepts it.
fn exchange(d: &hidapi::DeviceInfo, api: &HidApi) {
    let dev = match d.open_device(api) {
        Ok(d) => d,
        Err(e) => return println!("    open failed: {e}"),
    };
    for (name, req) in [
        ("battery", [0x55u8, 0x65, 0x01]),
        ("eq", [0x55u8, 0x11, 0x00]),
    ] {
        let mut tx = [0u8; 64];
        tx[..req.len()].copy_from_slice(&req);
        match dev.write(&tx) {
            Ok(_) => {}
            Err(e) => {
                println!("    {name}: write failed: {e}");
                continue;
            }
        }
        let mut rx = [0u8; 64];
        let end = Instant::now() + Duration::from_millis(500);
        while Instant::now() < end {
            match dev.read_timeout(&mut rx, 50) {
                Ok(n) if n > 0 => println!("    {name} reply ({n}B): {:02X?}", &rx[..n.min(24)]),
                _ => {}
            }
        }
    }
}

/// Read feature report 0xED every second for 60 s, printing only changes plus a heartbeat.
fn poll(d: &hidapi::DeviceInfo, api: &HidApi) {
    let dev = match d.open_device(api) {
        Ok(d) => d,
        Err(e) => return println!("    open failed: {e}"),
    };
    println!(
        "    polling feature report 0xED (60 s; toggle headset power / charging to see bytes move)"
    );
    let mut last = Vec::new();
    let end = Instant::now() + Duration::from_secs(60);
    while Instant::now() < end {
        let mut buf = [0u8; 65];
        buf[0] = 0xED;
        match dev.get_feature_report(&mut buf) {
            Ok(n) => {
                let cur = buf[..n].to_vec();
                if cur != last {
                    println!("    {:02X?}", &buf[..n]);
                    last = cur;
                }
            }
            Err(e) => println!("    read failed: {e}"),
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}

/// Read-only scan: every report id via get-feature/get-input-report, then 5 s passive listen.
fn scan(d: &hidapi::DeviceInfo, api: &HidApi) {
    let dev = match d.open_device(api) {
        Ok(d) => d,
        Err(e) => return println!("    open failed: {e}"),
    };
    for id in 0u16..=0xFF {
        let mut buf = [0u8; 65];
        buf[0] = id as u8;
        if let Ok(n) = dev.get_feature_report(&mut buf) {
            println!(
                "    feature id {id:#04X} ({n}B): {:02X?}",
                &buf[..n.min(24)]
            );
        }
        let mut buf = [0u8; 65];
        buf[0] = id as u8;
        if let Ok(n) = dev.get_input_report(&mut buf) {
            println!(
                "    input   id {id:#04X} ({n}B): {:02X?}",
                &buf[..n.min(24)]
            );
        }
    }
    print!("    listening 5s for unsolicited reports...");
    let mut buf = [0u8; 256];
    let end = Instant::now() + Duration::from_secs(5);
    while Instant::now() < end {
        if let Ok(n) = dev.read_timeout(&mut buf, 100) {
            if n > 0 {
                print!(" {:02X?}", &buf[..n.min(32)]);
            }
        }
    }
    println!(" done");
}
