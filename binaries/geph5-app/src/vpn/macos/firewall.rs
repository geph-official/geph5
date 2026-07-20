//! Fail-closed PF kill switch as typed rules in a dedicated anchor, via the
//! `pfctl` crate. The anchor call is injected in-kernel (`try_add_anchor`), so
//! the main ruleset — including the system's `com.apple` anchors — is never
//! replaced or reloaded.
//!
//! Policy (first match wins; all pass/block rules are `quick` except the final
//! default deny): loopback and the tun pass freely; the engine's own traffic
//! passes by uid; DHCP/DHCPv6 handshakes pass so leases survive; port 53 to
//! anywhere else is dropped to plug DNS leaks; LAN ranges optionally pass; and
//! everything else outbound is dropped.

use anyhow::Context;
use ipnetwork::IpNetwork;
use pfctl::{
    AnchorKind, Direction, DropAction, FilterRuleAction, FilterRuleBuilder, PfCtl, Proto,
    RulesetKind, StatePolicy, TcpFlag, TcpFlags, Uid,
};

const ANCHOR: &str = "geph";

const LAN_NETS: [&str; 8] = [
    "10.0.0.0/8",
    "172.16.0.0/12",
    "192.168.0.0/16",
    "169.254.0.0/16",
    "224.0.0.0/4",
    "fc00::/7",
    "fe80::/10",
    "ff00::/8",
];

/// What teardown needs to know: whether PF was already enabled before we
/// touched it (if so, leave it enabled for whoever else relies on it).
#[derive(Clone, Copy)]
pub struct PfState {
    pf_was_enabled: bool,
}

fn ctl() -> anyhow::Result<PfCtl> {
    PfCtl::new().context("opening /dev/pf")
}

/// Stateful-pass TCP rules match on the initial SYN, like pfctl(8)'s implicit
/// `flags S/SA`.
fn syn_only() -> TcpFlags {
    TcpFlags::new(&[TcpFlag::Syn], &[TcpFlag::Syn, TcpFlag::Ack])
}

/// Enable PF (recording its prior state) and load the kill-switch rules into
/// our anchor. Idempotent: reapplying replaces the anchor's rules in place.
pub fn apply(utun: &str, uid: u32, allow_lan: bool) -> anyhow::Result<PfState> {
    let mut pf = ctl()?;
    let pf_was_enabled = pf.is_enabled().unwrap_or(true);
    pf.try_enable().context("enabling PF")?;
    pf.try_add_anchor(ANCHOR, AnchorKind::Filter)
        .context("adding PF anchor")?;

    let mut rules: Vec<pfctl::FilterRule> = Vec::new();
    for iface in ["lo0", utun] {
        rules.push(
            FilterRuleBuilder::default()
                .action(FilterRuleAction::Pass)
                .quick(true)
                .interface(iface)
                .keep_state(StatePolicy::Keep)
                .tcp_flags(syn_only())
                .build()?,
        );
    }
    for proto in [Proto::Tcp, Proto::Udp, Proto::Icmp] {
        let mut b = FilterRuleBuilder::default();
        b.action(FilterRuleAction::Pass)
            .direction(Direction::Out)
            .quick(true)
            .proto(proto)
            .user(Uid::from(uid))
            .keep_state(StatePolicy::Keep);
        if proto == Proto::Tcp {
            b.tcp_flags(syn_only());
        }
        rules.push(b.build()?);
    }
    for (from, to) in [(68u16, 67u16), (546, 547)] {
        rules.push(
            FilterRuleBuilder::default()
                .action(FilterRuleAction::Pass)
                .direction(Direction::Out)
                .quick(true)
                .proto(Proto::Udp)
                .from(pfctl::Port::from(from))
                .to(pfctl::Port::from(to))
                .keep_state(StatePolicy::Keep)
                .build()?,
        );
    }
    for proto in [Proto::Tcp, Proto::Udp] {
        rules.push(
            FilterRuleBuilder::default()
                .action(FilterRuleAction::Drop(DropAction::Drop))
                .quick(true)
                .proto(proto)
                .to(pfctl::Port::from(53))
                .build()?,
        );
    }
    if allow_lan {
        for net in LAN_NETS {
            let net: IpNetwork = net.parse().expect("static LAN net");
            rules.push(
                FilterRuleBuilder::default()
                    .action(FilterRuleAction::Pass)
                    .direction(Direction::Out)
                    .quick(true)
                    .to(pfctl::Ip::from(net))
                    .keep_state(StatePolicy::Keep)
                    .tcp_flags(syn_only())
                    .build()?,
            );
        }
    }
    rules.push(
        FilterRuleBuilder::default()
            .action(FilterRuleAction::Drop(DropAction::Drop))
            .direction(Direction::Out)
            .build()?,
    );

    let mut change = pfctl::AnchorChange::new();
    change.set_filter_rules(rules);
    pf.set_rules(ANCHOR, change)
        .context("loading kill-switch rules")?;
    Ok(PfState { pf_was_enabled })
}

/// Flush and remove our anchor; disable PF only if it was disabled before we
/// enabled it.
pub fn teardown(state: PfState) {
    let Ok(mut pf) = ctl() else { return };
    let _ = pf.flush_rules(ANCHOR, RulesetKind::Filter);
    let _ = pf.try_remove_anchor(ANCHOR, AnchorKind::Filter);
    if !state.pf_was_enabled {
        let _ = pf.try_disable();
    }
}

/// Remove any anchor left by a crashed prior instance. PF's enabled state is
/// left alone: we cannot know who else relies on it.
pub fn teardown_stale() {
    let Ok(mut pf) = ctl() else { return };
    let _ = pf.flush_rules(ANCHOR, RulesetKind::Filter);
    let _ = pf.try_remove_anchor(ANCHOR, AnchorKind::Filter);
}
