//! One explicit desktop intention, fenced by the paired owner and server version.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Version {
    Session(u64),
    Legacy(u64),
}

impl Version {
    pub fn revision(&self) -> u64 {
        match self {
            Self::Session(n) | Self::Legacy(n) => *n,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Intent {
    pub serial: u64,
    pub owner: u64,
    pub enabled: bool,
    pub version: Version,
    pub control_id: String,
}

#[derive(Default)]
pub struct Pending {
    serial: u64,
    current: Option<Intent>,
}

impl Pending {
    pub fn issue(
        &mut self,
        owner: u64,
        enabled: bool,
        version: Version,
        namespace: &str,
    ) -> Intent {
        self.serial = self.serial.wrapping_add(1);
        let intent = Intent {
            serial: self.serial,
            owner,
            enabled,
            version,
            control_id: format!("{namespace}-{}", self.serial),
        };
        self.current = Some(intent.clone());
        intent
    }
    pub fn current(&self) -> Option<&Intent> {
        self.current.as_ref()
    }
    pub fn matches(&self, intent: &Intent) -> bool {
        self.current.as_ref() == Some(intent)
    }
    pub fn clear(&mut self) {
        self.current = None;
    }
    pub fn finish(&mut self, intent: &Intent) {
        if self.matches(intent) {
            self.clear();
        }
    }
    pub fn observe(&mut self, owner: u64, version: &Version) {
        if self.current.as_ref().is_some_and(|intent| {
            intent.owner != owner
                || std::mem::discriminant(&intent.version) != std::mem::discriminant(version)
                || version.revision() > intent.version.revision()
        }) {
            self.clear();
        }
    }
    pub fn holds_local_pause(&self, owner: u64, version: &Version) -> bool {
        self.current.as_ref().is_some_and(|intent| {
            intent.owner == owner && !intent.enabled && &intent.version == version
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn uncertain_pause_retries_the_same_identity_and_version() {
        let mut p = Pending::default();
        let original = p.issue(1, false, Version::Session(26), "desktop-1");
        p.observe(1, &Version::Session(26));
        assert_eq!(p.current(), Some(&original));
        assert!(p.holds_local_pause(1, &Version::Session(26)));
    }
    #[test]
    fn a_new_remote_revision_cancels_the_old_pause_instead_of_rebasing_it() {
        let mut p = Pending::default();
        p.issue(1, false, Version::Session(26), "desktop-1");
        p.observe(1, &Version::Session(28));
        assert!(p.current().is_none());
    }
    #[test]
    fn stale_snapshots_cannot_remove_a_newer_pending_action() {
        let mut p = Pending::default();
        let action = p.issue(1, false, Version::Session(28), "desktop-1");
        p.observe(1, &Version::Session(26));
        assert_eq!(p.current(), Some(&action));
    }
    #[test]
    fn a_new_intention_replaces_the_old_identity_and_late_ack_cannot_clear_it() {
        let mut p = Pending::default();
        let first = p.issue(1, true, Version::Session(26), "desktop-1");
        let next = p.issue(1, false, Version::Session(26), "desktop-1");
        assert_ne!(first.control_id, next.control_id);
        p.finish(&first);
        assert_eq!(p.current(), Some(&next));
    }
    #[test]
    fn changing_pairing_owner_or_protocol_revokes_pending_work() {
        let mut p = Pending::default();
        p.issue(1, false, Version::Session(26), "desktop-1");
        p.observe(2, &Version::Session(26));
        assert!(p.current().is_none());
        p.issue(2, false, Version::Legacy(9), "desktop-2");
        p.observe(2, &Version::Session(1));
        assert!(p.current().is_none());
    }
}
