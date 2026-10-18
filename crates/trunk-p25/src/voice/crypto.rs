//! Whether a voice call is encrypted, from its encryption sync.
//!
//! Every superframe (an `LDU2` in phase 1, a 2V burst in phase 2) carries the
//! ESS, and a Reed–Solomon decode at its correction limit occasionally turns
//! a noisy one into a valid-looking word with a random ALGID. Real encryption
//! repeats its ALGID and key ID in every superframe (only the MI changes), so
//! a call counts as encrypted once the same pair decodes twice running.

use super::EncryptionSync;

/// An ALGID/KeyID pair seen in the ESS.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Encryption {
    pub algid: u8,
    pub keyid: u16,
}

impl From<&EncryptionSync> for Encryption {
    fn from(es: &EncryptionSync) -> Self {
        Self {
            algid: es.algid,
            keyid: es.keyid,
        }
    }
}

/// Confirms a call's encryption from its successive ESS decodes.
#[derive(Clone, Copy, Debug, Default)]
pub struct EncryptionTracker {
    /// The last ESS decoded (encrypted or clear).
    last: Option<Encryption>,
    /// The encryption last confirmed.
    confirmed: Option<Encryption>,
    /// ESS decodes that were not confirmed by the next.
    pub unconfirmed: u64,
}

impl EncryptionTracker {
    /// A decoded ESS. Returns the encryption when this decode confirms one
    /// not confirmed before.
    pub fn observe(&mut self, es: &EncryptionSync) -> Option<Encryption> {
        let seen = Encryption::from(es);
        let repeated = self.last == Some(seen);
        if let Some(last) = self.last
            && !repeated
            && last.algid != super::signalling::ALGID_CLEAR
        {
            self.unconfirmed += 1;
        }
        self.last = Some(seen);
        if repeated && es.encrypted() && self.confirmed != Some(seen) {
            self.confirmed = Some(seen);
            return Some(seen);
        }
        None
    }

    /// A superframe whose ESS did not decode breaks the run.
    pub const fn missed(&mut self) {
        self.last = None;
    }

    /// The encryption confirmed on this call, if any.
    pub const fn confirmed(&self) -> Option<Encryption> {
        self.confirmed
    }

    /// A new call.
    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn es(algid: u8, keyid: u16, mi: u8) -> EncryptionSync {
        EncryptionSync {
            mi: [mi; 9],
            algid,
            keyid,
        }
    }

    #[test]
    fn encryption_counts_once_it_repeats() {
        let mut t = EncryptionTracker::default();
        assert_eq!(t.observe(&es(0x84, 0xa100, 1)), None);
        // The MI changes every superframe; ALGID and KeyID do not.
        let aes = Encryption {
            algid: 0x84,
            keyid: 0xa100,
        };
        assert_eq!(t.observe(&es(0x84, 0xa100, 2)), Some(aes));
        // Reported once.
        assert_eq!(t.observe(&es(0x84, 0xa100, 3)), None);
        assert_eq!(t.confirmed(), Some(aes));
    }

    #[test]
    fn a_lone_miscorrection_does_not_count() {
        let mut t = EncryptionTracker::default();
        for (mi, algid) in (0u8..).zip([0x80, 0x80, 0xfe, 0x80, 0x80, 0x29, 0x80]) {
            assert_eq!(t.observe(&es(algid, 0, mi)), None);
        }
        assert_eq!(t.confirmed(), None);
        assert_eq!(t.unconfirmed, 2);
    }

    #[test]
    fn a_missed_superframe_breaks_the_run() {
        let mut t = EncryptionTracker::default();
        t.observe(&es(0x84, 1, 0));
        t.missed();
        assert_eq!(t.observe(&es(0x84, 1, 1)), None);
        assert!(t.observe(&es(0x84, 1, 2)).is_some());
    }

    #[test]
    fn clear_voice_never_counts() {
        let mut t = EncryptionTracker::default();
        for i in 0..5 {
            assert_eq!(t.observe(&es(0x80, 0, i)), None);
        }
        assert_eq!(t.unconfirmed, 0);
    }
}
