//! What the control channel says: typed outbound signalling packets and the
//! channel plans that turn their channel numbers into frequencies.
//!
//! Replaces trunk-recorder's `P25Parser`. Where it reads a field wrong the
//! type here documents the difference (unit-to-unit grants have no service
//! options; `SCCB_EXP`'s receive channel is not a control channel; reserved
//! TDMA channel types have no plan; protected and vendor packets are not
//! read as standard ones).
//!
//! A system usually has several control channels in view, and each
//! announces the same plans. [`SystemPlans`] is shared by all of a system's
//! control channels: a channel resolves against its own announcements
//! first, then its site's, then whatever any site of the system announced
//! last, so a channel heard before its own `IDEN_UP` still resolves.

pub mod iden;
pub mod osp;

use std::collections::HashMap;

pub use iden::{Access, Channel, ChannelId, ChannelPlan, Iden, IdenTable};
pub use osp::{GroupChannel, Osp, Response, ServiceOptions, Site};

/// Where a channel number's plan came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Resolution {
    /// This control channel announced it.
    Own(Channel),
    /// Another control channel of the same site did.
    Site(Channel),
    /// Another site of the system did.
    System(Channel),
    Unknown,
}

impl Resolution {
    pub const fn channel(self) -> Option<Channel> {
        match self {
            Self::Own(c) | Self::Site(c) | Self::System(c) => Some(c),
            Self::Unknown => None,
        }
    }
}

/// Plans one system's control channels have announced.
#[derive(Debug, Default)]
pub struct SystemPlans {
    sites: HashMap<Site, IdenTable>,
    latest: IdenTable,
    latest_from: [Option<Site>; 16],
    /// Two sites announced different plans for one identifier.
    pub conflicts: u64,
}

impl SystemPlans {
    pub fn sites(&self) -> impl Iterator<Item = (&Site, &IdenTable)> {
        self.sites.iter()
    }
}

/// One control channel's view: its site and what it announced itself.
#[derive(Debug, Default)]
pub struct ControlChannelPlans {
    site: Option<Site>,
    own: IdenTable,
}

impl ControlChannelPlans {
    pub const fn site(&self) -> Option<Site> {
        self.site
    }

    pub const fn own(&self) -> &IdenTable {
        &self.own
    }

    /// Takes in what `osp` says about plans or the site.
    pub fn learn(&mut self, osp: &Osp, system: &mut SystemPlans) {
        match *osp {
            Osp::IdenUpdate {
                iden,
                plan: Some(plan),
            } => {
                self.own.set(iden, plan);
                if let Some(site) = self.site {
                    system.sites.entry(site).or_default().set(iden, plan);
                }
                let i = iden.get() as usize;
                let differs = system.latest.get(iden).is_some_and(|p| *p != plan);
                if differs && system.latest_from[i].is_some() && system.latest_from[i] != self.site
                {
                    system.conflicts += 1;
                }
                system.latest.set(iden, plan);
                system.latest_from[i] = self.site;
            }
            Osp::RfssStatus { site, .. } if self.site != Some(site) => {
                self.site = Some(site);
                let table = system.sites.entry(site).or_default();
                for i in 0..16 {
                    let iden = Iden::new(i);
                    if let Some(p) = self.own.get(iden) {
                        table.set(iden, *p);
                    }
                }
            }
            _ => {}
        }
    }

    pub fn resolve(&self, id: ChannelId, system: &SystemPlans) -> Resolution {
        if let Some(c) = self.own.resolve(id) {
            return Resolution::Own(c);
        }
        if let Some(c) = self
            .site
            .and_then(|s| system.sites.get(&s))
            .and_then(|t| t.resolve(id))
        {
            return Resolution::Site(c);
        }
        system
            .latest
            .resolve(id)
            .map_or(Resolution::Unknown, Resolution::System)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PLAN: ChannelPlan = ChannelPlan {
        base_hz: 762_006_250,
        spacing_hz: 12_500,
        tx_offset_hz: 30_000_000,
        bandwidth_hz: 12_500,
        access: Access::Tdma { slots: 2 },
    };
    const SITE: Site = Site {
        system: 0x5e4,
        rfss: 1,
        site: 1,
    };
    const CH: ChannelId = ChannelId::new(0x37ec);

    fn iden_up(plan: ChannelPlan) -> Osp {
        Osp::IdenUpdate {
            iden: Iden::new(3),
            plan: Some(plan),
        }
    }

    fn rfss(site: Site) -> Osp {
        Osp::RfssStatus {
            site,
            channel: ChannelId::new(0x1670),
        }
    }

    #[test]
    fn a_control_channel_uses_its_own_plans_first() {
        let mut sys = SystemPlans::default();
        let mut a = ControlChannelPlans::default();
        assert_eq!(a.resolve(CH, &sys), Resolution::Unknown);
        a.learn(&iden_up(PLAN), &mut sys);
        assert_eq!(a.resolve(CH, &sys), Resolution::Own(PLAN.resolve(CH)));
    }

    #[test]
    fn plans_are_shared_across_a_systems_control_channels() {
        let mut sys = SystemPlans::default();
        let (mut a, mut b, mut c) = Default::default();
        let other = Site { site: 2, ..SITE };
        ControlChannelPlans::learn(&mut a, &rfss(SITE), &mut sys);
        a.learn(&iden_up(PLAN), &mut sys);
        // Same site, nothing announced yet: the site's plan.
        ControlChannelPlans::learn(&mut b, &rfss(SITE), &mut sys);
        assert_eq!(b.resolve(CH, &sys), Resolution::Site(PLAN.resolve(CH)));
        // Another site, nothing announced yet: the system's.
        ControlChannelPlans::learn(&mut c, &rfss(other), &mut sys);
        assert_eq!(c.resolve(CH, &sys), Resolution::System(PLAN.resolve(CH)));
        assert_eq!(sys.conflicts, 0);
    }

    #[test]
    fn sites_disagreeing_is_counted_and_each_keeps_its_own() {
        let mut sys = SystemPlans::default();
        let (mut a, mut b) = (
            ControlChannelPlans::default(),
            ControlChannelPlans::default(),
        );
        let moved = ChannelPlan {
            base_hz: PLAN.base_hz + 1_000_000,
            ..PLAN
        };
        a.learn(&rfss(SITE), &mut sys);
        a.learn(&iden_up(PLAN), &mut sys);
        b.learn(&rfss(Site { site: 2, ..SITE }), &mut sys);
        b.learn(&iden_up(moved), &mut sys);
        assert_eq!(sys.conflicts, 1);
        assert_eq!(a.resolve(CH, &sys), Resolution::Own(PLAN.resolve(CH)));
        assert_eq!(b.resolve(CH, &sys), Resolution::Own(moved.resolve(CH)));
    }

    #[test]
    fn learning_the_site_late_files_earlier_plans_under_it() {
        let mut sys = SystemPlans::default();
        let mut a = ControlChannelPlans::default();
        a.learn(&iden_up(PLAN), &mut sys);
        a.learn(&rfss(SITE), &mut sys);
        let mut b = ControlChannelPlans::default();
        b.learn(&rfss(SITE), &mut sys);
        assert_eq!(b.resolve(CH, &sys), Resolution::Site(PLAN.resolve(CH)));
    }
}
