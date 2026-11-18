//! Source-independent device model, and the tracker that merges scans over time.

use std::collections::{HashMap, HashSet};

/// Also the display order on the taskbar.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum Kind {
    Headset,
    Earbuds,
    Mouse,
    Keyboard,
    Gamepad,
    Other,
}

impl Kind {
    pub fn from_name(name: &str) -> Kind {
        let n = name.to_ascii_lowercase();
        if ["headset", "headphone", "airpods max"].iter().any(|k| n.contains(k)) {
            Kind::Headset
        } else if ["earbud", "buds", "airpods", "earphone", "in-ear"].iter().any(|k| n.contains(k)) {
            Kind::Earbuds
        } else if n.contains("mouse") {
            Kind::Mouse
        } else if n.contains("keyboard") || n.contains("keys") {
            Kind::Keyboard
        } else if n.contains("controller") || n.contains("gamepad") {
            Kind::Gamepad
        } else {
            Kind::Other
        }
    }
}

/// What a source saw for one device during one scan.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum State {
    Battery { percent: u8, charging: bool },
    /// Known to exist but didn't answer (asleep, or off; often indistinguishable).
    Unreachable,
    /// Definitely not connected (e.g. headset off while its dongle is plugged).
    Off,
}

#[derive(Clone)]
pub struct Observation {
    /// Stable per device across scans, e.g. "logi:c539:2:01" or "ble:f4ee25fbba46".
    pub id: String,
    /// None when the source can't identify the device right now (only updates known devices).
    pub name: Option<String>,
    pub kind: Option<Kind>,
    pub state: State,
    /// `Some(true)` when reached directly (e.g. charging cable), `Some(false)`
    /// when reached through a receiver/dongle, `None` when the source can't tell.
    /// Used to merge the same physical device seen via two transports.
    pub direct: Option<bool>,
    /// Other ids this device may be observed under (e.g. a Bluetooth device
    /// reachable over both `ble:` and `bt:` transports). The tracker folds all
    /// of them into one row so per-id settings follow the device.
    pub aliases: Vec<String>,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Status {
    /// `stale`: last known reading of a device that's asleep right now.
    Battery { percent: u8, charging: bool, stale: bool },
    NotConnected,
}

impl std::fmt::Display for Status {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Status::Battery { percent, charging, stale } => {
                write!(f, "{percent}%")?;
                if *charging {
                    write!(f, " (charging)")?;
                }
                if *stale {
                    write!(f, " (asleep)")?;
                }
                Ok(())
            }
            Status::NotConnected => write!(f, "not connected"),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Reading {
    /// Current transport id (the source that last won a merge).
    pub id: String,
    /// Every transport id this row has been seen under, starting with `id`.
    /// Grows as the same device appears on cable and dongle; used so settings
    /// like the hidden-device list survive a transport flip.
    pub ids: Vec<String>,
    pub name: String,
    pub kind: Kind,
    pub status: Status,
    /// See `Observation::direct`.
    pub direct: Option<bool>,
}

impl Reading {
    /// Hidden by the user if any id this row has ever gone by is hidden.
    pub fn is_hidden(&self, hidden: &[String]) -> bool {
        self.ids.iter().any(|id| hidden.contains(id))
    }
}

/// Scans an entry may go unobserved before being dropped: one scan of grace
/// covers a flaky enumeration, anything more means the source is really gone.
const GRACE: u8 = 1;

/// Remembers every device seen this session, so a device that's present but not
/// answering (asleep, or off behind a powered dongle) shows as "not connected"
/// and a sleeping one keeps its last battery reading. Entries whose source no
/// longer enumerates at all (dongle unplugged, cable removed) are dropped, and
/// the same physical device seen via two transports is merged into one row.
#[derive(Default)]
pub struct Tracker {
    known: Vec<Reading>,
    /// Consecutive scans each id went unobserved; dropped past `GRACE`.
    missed: HashMap<String, u8>,
}

/// Ranks statuses for merging duplicates: live beats stale beats absent, a
/// charging reading beats a discharging one at the same freshness, and at the
/// same rank the lower percentage wins when discharging (the conservative
/// choice for warnings) and the higher percentage wins when charging.
fn rank(s: &Status) -> (u8, u8) {
    match s {
        Status::Battery { percent, charging: true, stale: false, .. } => (3, *percent),
        Status::Battery { percent, stale: false, .. } => (2, 255 - percent),
        // Stale readings rank lowest; a stale "charging" flag can't be trusted,
        // so it ties on the conservative discharging side (lower wins).
        Status::Battery { percent, .. } => (1, 255 - percent),
        Status::NotConnected => (0, 0),
    }
}

impl Tracker {
    pub fn update(&mut self, observations: Vec<Observation>) -> Vec<Reading> {
        // Transport id -> row, so an observation finds its row in O(1) — even
        // under an alias id the row gained in an earlier merge.
        let mut by_id: HashMap<String, usize> = HashMap::new();
        for (i, r) in self.known.iter().enumerate() {
            for id in &r.ids {
                by_id.insert(id.clone(), i);
            }
        }
        let mut seen = HashSet::new();
        for o in observations {
            // Only the observed id counts as seen; a sibling row that shares an
            // alias but wasn't observed ages out through the grace counter. (The
            // fold below inserts o.id into r.ids, so the grace check sees it.)
            seen.insert(o.id.clone());
            self.missed.remove(&o.id);
            // Fall back to the observation's declared aliases, so a source that
            // learned a device under a second transport folds into the row that
            // already owns that alias instead of spawning a duplicate.
            let pos = by_id
                .get(&o.id)
                .or_else(|| o.aliases.iter().find_map(|a| by_id.get(a)))
                .copied();
            let status = match (o.state, pos.map(|p| self.known[p].status)) {
                (State::Battery { percent, charging }, _) => Status::Battery { percent, charging, stale: false },
                // Asleep: battery hasn't changed, keep the last reading but mark it.
                (State::Unreachable, Some(Status::Battery { percent, charging, .. })) => {
                    Status::Battery { percent, charging, stale: true }
                }
                (State::Unreachable | State::Off, _) => Status::NotConnected,
            };
            match pos {
                Some(p) => {
                    let r = &mut self.known[p];
                    r.status = status;
                    r.id = o.id.clone(); // stay anchored to the source that answered
                    if let Some(name) = o.name {
                        r.name = name;
                    }
                    if let Some(kind) = o.kind {
                        r.kind = kind;
                    }
                    // The observed id and its declared aliases all belong to
                    // this row now (`ids` only ever grows, so `direct` below is
                    // stable once it becomes `None`). If another row claimed
                    // the alias earlier, `by_id` reassigns it here — that orphan
                    // flips to NotConnected for a scan, then ages out. Correct;
                    // the brief UI glitch is the price of resolving the claim.
                    for id in std::iter::once(&o.id).chain(&o.aliases) {
                        if !r.ids.contains(id) {
                            by_id.insert(id.clone(), p);
                            r.ids.push(id.clone());
                        }
                    }
                    // A merged row (several transport ids) keeps `direct`
                    // unknown so it is never re-paired with a different
                    // same-name device on the opposite transport.
                    r.direct = if r.ids.len() == 1 { o.direct.or(r.direct) } else { None };
                }
                None => {
                    let Some(name) = o.name else { continue };
                    let kind = o.kind.unwrap_or_else(|| Kind::from_name(&name));
                    let mut ids = o.aliases;
                    ids.insert(0, o.id.clone());
                    for id in &ids {
                        by_id.insert(id.clone(), self.known.len());
                    }
                    // Same rule as the update branch: a row that already covers
                    // several transport ids is transport-ambiguous.
                    let direct = if ids.len() > 1 { None } else { o.direct };
                    self.known.push(Reading { id: o.id, ids, name, kind, status, direct });
                }
            }
        }
        // Sources only emit observations while physically present (receiver or
        // dongle opened), so silence means gone: report "not connected" for the
        // grace scans, then drop the entry.
        let mut i = 0;
        while i < self.known.len() {
            if !self.known[i].ids.iter().any(|id| seen.contains(id)) {
                let miss = self.missed.entry(self.known[i].id.clone()).or_default();
                *miss += 1;
                if *miss > GRACE {
                    let id = self.known.remove(i).id;
                    self.missed.remove(&id);
                    continue;
                }
                self.known[i].status = Status::NotConnected;
            }
            i += 1;
        }

        // The same physical device can appear via two transports at once (e.g.
        // charging cable + wireless dongle). Group by (name, kind): a direct
        // row merges with one wireless row per pair — a second same-name
        // dongle is a distinct unit and keeps its row, and unknown transport
        // never merges. The winner is adopted wholesale (id included), so the
        // row stays anchored to whichever source is actually answering.
        let mut groups: HashMap<(String, Kind), Vec<Reading>> = HashMap::new();
        for r in std::mem::take(&mut self.known) {
            groups.entry((r.name.clone(), r.kind)).or_default().push(r);
        }
        for (_, group) in groups {
            let mut direct = Vec::new();
            let mut wireless = Vec::new();
            for r in group {
                match r.direct {
                    Some(true) => direct.push(r),
                    Some(false) => wireless.push(r),
                    None => self.known.push(r),
                }
            }
            while !direct.is_empty() && !wireless.is_empty() {
                let a = direct.pop().unwrap();
                let b = wireless.pop().unwrap();
                let (mut winner, mut loser) = if rank(&a.status) >= rank(&b.status) { (a, b) } else { (b, a) };
                self.missed.remove(&loser.id);
                for id in loser.ids.drain(..) {
                    if !winner.ids.contains(&id) {
                        winner.ids.push(id);
                    }
                }
                winner.direct = None;
                self.known.push(winner);
            }
            self.known.extend(direct);
            self.known.extend(wireless);
        }

        // Entries can be removed or have their id replaced in the loops above;
        // keep the missed counter in sync so a dead id doesn't accumulate counts.
        let alive: HashSet<&str> = self.known.iter().flat_map(|r| r.ids.iter().map(String::as_str)).collect();
        self.missed.retain(|id, _| alive.contains(id.as_str()));

        self.known.sort_by(|a, b| a.kind.cmp(&b.kind).then_with(|| a.name.cmp(&b.name)));
        self.known.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obs(state: State) -> Observation {
        Observation { id: "x".into(), name: Some("Mouse".into()), kind: Some(Kind::Mouse), state, direct: None, aliases: vec![] }
    }

    #[test]
    fn kind_from_name() {
        assert_eq!(Kind::from_name("Renan's AirPods"), Kind::Earbuds);
        assert_eq!(Kind::from_name("AirPods Max"), Kind::Headset);
        assert_eq!(Kind::from_name("Galaxy Buds2 Pro"), Kind::Earbuds);
        assert_eq!(Kind::from_name("PRO X Wireless Gaming Headset"), Kind::Headset);
        assert_eq!(Kind::from_name("MX Master 3S Mouse"), Kind::Mouse);
    }

    #[test]
    fn sleeping_device_keeps_last_reading_dimmed() {
        let mut t = Tracker::default();
        t.update(vec![obs(State::Battery { percent: 80, charging: false })]);
        let r = t.update(vec![obs(State::Unreachable)]);
        assert_eq!(r[0].status, Status::Battery { percent: 80, charging: false, stale: true });
    }

    #[test]
    fn vanished_device_is_dropped_after_grace() {
        let mut t = Tracker::default();
        t.update(vec![obs(State::Battery { percent: 80, charging: false })]);
        // Source absent: one grace scan as "not connected", then it's dropped.
        assert_eq!(t.update(vec![])[0].status, Status::NotConnected);
        assert!(t.update(vec![]).is_empty());
    }

    #[test]
    fn same_device_via_two_transports_merges() {
        let mut t = Tracker::default();
        let mut cable = obs(State::Battery { percent: 40, charging: true });
        cable.id = "logi:c095:2:ff".into();
        cable.direct = Some(true);
        let mut dongle = obs(State::Battery { percent: 40, charging: false });
        dongle.id = "logi:c547:2:01".into();
        dongle.direct = Some(false);
        let r = t.update(vec![cable, dongle]);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].status, Status::Battery { percent: 40, charging: true, stale: false });
    }

    #[test]
    fn merge_prefers_live_reading_over_stale() {
        let mut t = Tracker::default();
        let mut cable = obs(State::Unreachable);
        cable.id = "logi:c095:2:ff".into();
        cable.direct = Some(true);
        let mut dongle = obs(State::Battery { percent: 67, charging: false });
        dongle.id = "logi:c547:2:01".into();
        dongle.direct = Some(false);
        let r = t.update(vec![cable, dongle]);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].status, Status::Battery { percent: 67, charging: false, stale: false });
    }

    #[test]
    fn two_identical_models_on_own_dongles_dont_merge() {
        let mut t = Tracker::default();
        let mut a = obs(State::Battery { percent: 40, charging: false });
        a.id = "logi:c54d:2:01".into();
        a.direct = Some(false);
        let mut b = obs(State::Battery { percent: 60, charging: false });
        b.id = "logi:c53f:1:02".into();
        b.direct = Some(false);
        // Same name and kind, but both via receivers: two distinct units.
        assert_eq!(t.update(vec![a, b]).len(), 2);
    }

    #[test]
    fn observation_under_alias_id_updates_same_row() {
        let mut t = Tracker::default();
        let mut cable = obs(State::Battery { percent: 40, charging: true });
        cable.id = "logi:c095:2:ff".into();
        cable.direct = Some(true);
        let mut dongle = obs(State::Battery { percent: 40, charging: false });
        dongle.id = "logi:c547:2:01".into();
        dongle.direct = Some(false);
        t.update(vec![cable, dongle]);
        // Cable unplugged; the dongle id is now only an alias of the merged row.
        // An observation under it must still find the row — not spawn a new one.
        let mut next = obs(State::Battery { percent: 55, charging: false });
        next.id = "logi:c547:2:01".into();
        next.direct = Some(false);
        let r = t.update(vec![next]);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].status, Status::Battery { percent: 55, charging: false, stale: false });
    }

    #[test]
    fn merged_row_remembers_every_transport_id() {
        let mut t = Tracker::default();
        let mut cable = obs(State::Battery { percent: 40, charging: true });
        cable.id = "logi:c095:2:ff".into();
        cable.direct = Some(true);
        let mut dongle = obs(State::Battery { percent: 40, charging: false });
        dongle.id = "logi:c547:2:01".into();
        dongle.direct = Some(false);
        let r = t.update(vec![cable, dongle]);
        // Hiding either transport id hides the row, even after the winner's
        // id changes.
        assert!(r[0].is_hidden(&["logi:c095:2:ff".into()]));
        assert!(r[0].is_hidden(&["logi:c547:2:01".into()]));
        assert!(!r[0].is_hidden(&["logi:0000:0:00".into()]));
    }

    #[test]
    fn unknown_transport_never_merges() {
        let mut t = Tracker::default();
        let mut a = obs(State::Battery { percent: 40, charging: false });
        a.id = "x".into();
        let mut b = obs(State::Battery { percent: 60, charging: false });
        b.id = "y".into();
        // Same name and kind, transport unknown on both: can't prove it's the
        // same device, so keep both rows.
        assert_eq!(t.update(vec![a, b]).len(), 2);
    }

    #[test]
    fn merged_row_never_merges_again() {
        let mut t = Tracker::default();
        let mut cable = obs(State::Battery { percent: 40, charging: true });
        cable.id = "logi:c095:2:ff".into();
        cable.direct = Some(true);
        let mut dongle = obs(State::Battery { percent: 40, charging: false });
        dongle.id = "logi:c547:2:01".into();
        dongle.direct = Some(false);
        t.update(vec![cable, dongle]);
        // Later: only the cable answers, and a second same-model mouse appears
        // on its own dongle. The merged row is anchored to the cable transport
        // but must not swallow the second device.
        let mut cable_again = obs(State::Battery { percent: 39, charging: true });
        cable_again.id = "logi:c095:2:ff".into();
        cable_again.direct = Some(true);
        let mut other = obs(State::Battery { percent: 60, charging: false });
        other.id = "logi:c53f:1:02".into();
        other.direct = Some(false);
        assert_eq!(t.update(vec![cable_again, other]).len(), 2);
    }

    #[test]
    fn observation_aliases_share_one_row() {
        let mut t = Tracker::default();
        let mut ble = obs(State::Battery { percent: 50, charging: false });
        ble.id = "ble:f4ee25fbba46".into();
        ble.name = Some("Headset".into());
        ble.kind = Some(Kind::Headset);
        ble.aliases = vec!["bt:f4ee25fbba46".into()];
        t.update(vec![ble]);
        // Same device next scan via the other transport: one row, both ids.
        let mut bt = obs(State::Battery { percent: 51, charging: false });
        bt.id = "bt:f4ee25fbba46".into();
        bt.name = Some("Headset".into());
        bt.kind = Some(Kind::Headset);
        bt.aliases = vec!["ble:f4ee25fbba46".into()];
        let r = t.update(vec![bt]);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].status, Status::Battery { percent: 51, charging: false, stale: false });
        assert!(r[0].is_hidden(&["ble:f4ee25fbba46".into()]));
        assert!(r[0].is_hidden(&["bt:f4ee25fbba46".into()]));
    }

    #[test]
    fn observation_folds_into_row_that_owns_its_alias() {
        let mut t = Tracker::default();
        // A row created before aliases existed (or by a build that didn't set
        // them): its ids list only knows the classic Bluetooth id.
        let mut bt = obs(State::Battery { percent: 50, charging: false });
        bt.id = "bt:f4ee25fbba46".into();
        bt.name = Some("Headset".into());
        bt.kind = Some(Kind::Headset);
        t.update(vec![bt]);
        // The same device now answers over BLE and declares bt: as an alias:
        // it must fold into the existing row, not spawn a second one.
        let mut ble = obs(State::Battery { percent: 51, charging: false });
        ble.id = "ble:f4ee25fbba46".into();
        ble.name = Some("Headset".into());
        ble.kind = Some(Kind::Headset);
        ble.aliases = vec!["bt:f4ee25fbba46".into()];
        let r = t.update(vec![ble]);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].status, Status::Battery { percent: 51, charging: false, stale: false });
        // The row adopts the observed id and stays anchored to the live transport.
        assert_eq!(r[0].id, "ble:f4ee25fbba46");
        assert!(r[0].is_hidden(&["ble:f4ee25fbba46".into()]));
        assert!(r[0].is_hidden(&["bt:f4ee25fbba46".into()]));
    }

    #[test]
    fn merge_prefers_higher_percent_when_charging() {
        let mut t = Tracker::default();
        // Both live and charging: the higher reading wins — it's the one that
        // reflects actual charging progress, and charging devices never warn.
        let mut cable = obs(State::Battery { percent: 60, charging: true });
        cable.id = "logi:c095:2:ff".into();
        cable.direct = Some(true);
        let mut dongle = obs(State::Battery { percent: 80, charging: true });
        dongle.id = "logi:c547:2:01".into();
        dongle.direct = Some(false);
        let r = t.update(vec![cable, dongle]);
        assert_eq!(r[0].status, Status::Battery { percent: 80, charging: true, stale: false });
    }

    #[test]
    fn new_row_with_aliases_is_transport_ambiguous() {
        // Any future source can combine `direct` and `aliases`; a row that
        // covers several transport ids must never be re-paired on transport.
        let mut t = Tracker::default();
        let mut o = obs(State::Battery { percent: 50, charging: false });
        o.id = "x:a".into();
        o.direct = Some(true);
        o.aliases = vec!["x:b".into()];
        let r = t.update(vec![o]);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].direct, None);
    }

    #[test]
    fn merge_prefers_lower_percent_on_equal_freshness() {
        let mut t = Tracker::default();
        // Both live and discharging: the conservative (lower) reading wins.
        let mut cable = obs(State::Battery { percent: 60, charging: false });
        cable.id = "logi:c095:2:ff".into();
        cable.direct = Some(true);
        let mut dongle = obs(State::Battery { percent: 40, charging: false });
        dongle.id = "logi:c547:2:01".into();
        dongle.direct = Some(false);
        let r = t.update(vec![cable, dongle]);
        assert_eq!(r[0].status, Status::Battery { percent: 40, charging: false, stale: false });
    }

    #[test]
    fn unknown_unreachable_device_is_ignored() {
        let mut t = Tracker::default();
        let mut o = obs(State::Unreachable);
        o.name = None;
        assert!(t.update(vec![o]).is_empty());
    }
}
