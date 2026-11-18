//! Low-battery toast notifications.
//!
//! Unpackaged apps can show toasts once their AppUserModelID is registered under
//! HKCU\Software\Classes\AppUserModelId\<id> with a DisplayName.

use crate::device::{Reading, Status};
use crate::settings;
use std::collections::HashSet;
use windows::core::HSTRING;
use windows::Data::Xml::Dom::XmlDocument;
use windows::UI::Notifications::{ToastNotification, ToastNotificationManager};

pub const APP_ID: &str = "TaskbarBattery";
/// Toast alerts fire at or below this level (the tray icon's warning thresholds
/// are separate, see tray.rs).
const LOW_BATTERY: u8 = 15;
/// Re-arm an alert once the level is back above this (avoids repeats around the threshold).
const REARM_ABOVE: u8 = LOW_BATTERY + 5;

#[derive(Default)]
pub struct LowBatteryAlerts {
    alerted: HashSet<String>,
}

impl LowBatteryAlerts {
    pub fn check(&mut self, readings: &[Reading]) {
        for r in readings {
            let Status::Battery { percent, charging, stale } = r.status else { continue };
            if charging || percent > REARM_ABOVE {
                self.alerted.remove(&r.id);
            } else if percent <= LOW_BATTERY && !stale && !self.alerted.contains(&r.id) {
                self.alerted.insert(r.id.clone());
                if settings::alerts_enabled() {
                    let body = format!("{} is at {percent}%. Time to charge it.", r.name);
                    if let Err(e) = show("Low battery", &body) {
                        crate::diag::event("notify", format!("toast failed: {e}"));
                    }
                }
            }
        }
    }
}

pub fn show(title: &str, body: &str) -> windows::core::Result<()> {
    let xml = format!(
        "<toast><visual><binding template=\"ToastGeneric\"><text>{}</text><text>{}</text></binding></visual></toast>",
        escape(title),
        escape(body)
    );
    let doc = XmlDocument::new()?;
    doc.LoadXml(&HSTRING::from(xml))?;
    let toast = ToastNotification::CreateToastNotification(&doc)?;
    ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(APP_ID))?.Show(&toast)
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}
