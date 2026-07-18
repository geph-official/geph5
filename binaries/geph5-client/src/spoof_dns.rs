use std::{net::Ipv4Addr, sync::Arc};

use anyctx::AnyCtx;
use bytes::Bytes;
use moka::{notification::RemovalCause, sync::Cache};
use rand::Rng;
use simple_dns::{Packet, QTYPE};

use crate::{Config, client::CtxField};

/// Fake-DNS pool: `198.18.0.0/15`, the RFC 2544 benchmarking range. It is reserved
/// (never a real destination) yet is ordinary *routable unicast*, so unlike the
/// old `240.0.0.0/4` (Class E) pool the Windows TCP/IP stack will actually send to
/// it. On Class E, Windows fails `connect()` instantly with `WSAENETUNREACH`
/// before a packet ever reaches the tun, so under VPN mode every spoofed name was
/// blackholed and the machine looked like it had lost all internet.
const FAKE_DNS_BASE: u32 = u32::from_be_bytes([198, 18, 0, 0]);
const FAKE_DNS_MASK: u32 = u32::from_be_bytes([255, 254, 0, 0]); // /15

/// Cap on live fake mappings. Kept well under the /15's 131072 addresses so that
/// random free-address probing stays cheap (~1-2 tries) even when the cache is
/// full, and so the two direction caches never exhaust the pool.
const FAKE_DNS_CAPACITY: u64 = 60_000;

/// Reverse map fake IP -> name. Given generous headroom so it is pruned *only* by
/// the forward cache's eviction listener (below) and never self-evicts, which
/// would otherwise strand a still-live forward mapping.
static FAKE_DNS_BACKWARD: CtxField<Cache<Ipv4Addr, String>> =
    |_| Cache::new(FAKE_DNS_CAPACITY * 2);

/// Forward map name -> fake IP, and the source of truth for capacity. Its
/// eviction listener drops the matching reverse entry so the two stay consistent
/// as LRU-style eviction reclaims old names.
static FAKE_DNS_FORWARD: CtxField<Cache<String, Ipv4Addr>> = |ctx| {
    let backward = ctx.get(FAKE_DNS_BACKWARD).clone();
    Cache::builder()
        .max_capacity(FAKE_DNS_CAPACITY)
        .eviction_listener(move |_name: Arc<String>, ip: Ipv4Addr, _cause: RemovalCause| {
            backward.invalidate(&ip);
        })
        .build()
};

pub fn fake_dns_backtranslate(ctx: &AnyCtx<Config>, fake: Ipv4Addr) -> Option<String> {
    tracing::trace!(fake = debug(fake), "attempting to backtranslate");
    ctx.get(FAKE_DNS_BACKWARD).get(&fake)
}

pub fn fake_dns_allocate(ctx: &AnyCtx<Config>, dns_name: &str) -> Ipv4Addr {
    let backward = ctx.get(FAKE_DNS_BACKWARD);
    ctx.get(FAKE_DNS_FORWARD)
        .get_with(dns_name.to_string(), || {
            // Pick a random unused address in the pool. Because the cache holds
            // far fewer entries than the pool size, a free slot is normally found
            // on the first try; the bounded attempt count only guards against
            // pathological states so this can never spin forever.
            let mut ip_addr = random_pool_addr();
            for _ in 0..64 {
                if backward.get(&ip_addr).is_none() {
                    break;
                }
                ip_addr = random_pool_addr();
            }
            backward.insert(ip_addr, dns_name.to_string());
            tracing::debug!(
                from = debug(dns_name),
                to = debug(ip_addr),
                "created fake dns mapping",
            );
            ip_addr
        })
}

fn random_pool_addr() -> Ipv4Addr {
    let offset = rand::thread_rng().gen_range(0..=!FAKE_DNS_MASK);
    Ipv4Addr::from(FAKE_DNS_BASE | offset)
}

pub fn fake_dns_respond(ctx: &AnyCtx<Config>, pkt: &[u8]) -> anyhow::Result<Bytes> {
    let pkt = Packet::parse(pkt)?;
    tracing::trace!(pkt = debug(&pkt), "got DNS packet");
    let mut answers = vec![];
    for question in pkt.questions.iter() {
        if question.qtype == QTYPE::TYPE(simple_dns::TYPE::A) {
            answers.push(simple_dns::ResourceRecord::new(
                question.qname.clone(),
                simple_dns::CLASS::IN,
                1,
                simple_dns::rdata::RData::A(
                    fake_dns_allocate(ctx, &question.qname.to_string()).into(),
                ),
            ));
        }
    }
    let mut response = pkt.into_reply();
    response.answers = answers;
    Ok(response.build_bytes_vec_compressed()?.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::Config;
    use geph5_broker_protocol::ExitConstraint;

    fn test_ctx() -> AnyCtx<Config> {
        AnyCtx::new(Config {
            socks5_listen: None,
            http_proxy_listen: None,
            pac_listen: None,
            control_listen: None,
            control_listen_unix: None,
            control_listen_pipe: None,
            exit_constraint: ExitConstraint::Auto,
            allow_direct: false,
            cache: None,
            broker: None,
            tunneled_broker: None,
            broker_keys: None,
            port_forward: vec![],
            spoof_dns: true,
            passthrough_china: false,
            allow_lan: true,
            dry_run: true,
            credentials: Default::default(),
            sess_metadata: serde_json::Value::Null,
            task_limit: None,
        })
    }

    /// Every allocation must land in 198.18.0.0/15 — the whole point of the fix.
    fn assert_in_pool(ip: Ipv4Addr) {
        assert_eq!(
            u32::from(ip) & FAKE_DNS_MASK,
            FAKE_DNS_BASE,
            "{ip} is outside 198.18.0.0/15",
        );
    }

    #[test]
    fn allocations_are_in_pool() {
        let ctx = test_ctx();
        for i in 0..1000 {
            assert_in_pool(fake_dns_allocate(&ctx, &format!("host{i}.example.com")));
        }
    }

    #[test]
    fn allocation_is_stable_and_round_trips() {
        let ctx = test_ctx();
        let ip = fake_dns_allocate(&ctx, "example.com");
        assert_in_pool(ip);
        // Same name -> same IP.
        assert_eq!(ip, fake_dns_allocate(&ctx, "example.com"));
        // Fake IP -> original name.
        assert_eq!(
            fake_dns_backtranslate(&ctx, ip).as_deref(),
            Some("example.com")
        );
    }

    #[test]
    fn distinct_names_get_distinct_ips() {
        let ctx = test_ctx();
        let a = fake_dns_allocate(&ctx, "a.example.com");
        let b = fake_dns_allocate(&ctx, "b.example.com");
        assert_ne!(a, b);
        assert_eq!(fake_dns_backtranslate(&ctx, a).as_deref(), Some("a.example.com"));
        assert_eq!(fake_dns_backtranslate(&ctx, b).as_deref(), Some("b.example.com"));
    }

    #[test]
    fn unknown_ip_backtranslates_to_none() {
        let ctx = test_ctx();
        assert_eq!(fake_dns_backtranslate(&ctx, Ipv4Addr::new(198, 18, 0, 1)), None);
    }

    fn resolve_via_spoofer(ctx: &AnyCtx<Config>, name: &str) -> Ipv4Addr {
        let mut query = Packet::new_query(1234);
        query.questions.push(simple_dns::Question::new(
            simple_dns::Name::new(name).unwrap(),
            QTYPE::TYPE(simple_dns::TYPE::A),
            simple_dns::QCLASS::CLASS(simple_dns::CLASS::IN),
            false,
        ));
        let resp = fake_dns_respond(ctx, &query.build_bytes_vec().unwrap()).unwrap();
        let resp = Packet::parse(&resp).unwrap();
        match &resp.answers[0].rdata {
            simple_dns::rdata::RData::A(a) => Ipv4Addr::from(a.address),
            other => panic!("expected A record, got {other:?}"),
        }
    }

    /// Repro of the china-passthrough blackhole: under VPN mode the *system*
    /// resolver's only reachable upstream is the spoofer itself, so when the
    /// passthrough path in `open_conn` re-resolves the back-translated hostname
    /// with `tokio::net::lookup_host`, it gets a fake pool address back and
    /// "directly" dials a nonexistent destination.
    #[test]
    fn china_passthrough_re_resolution_is_poisoned() {
        let ctx = test_ctx();
        // 1. The app resolves baidu.com through the tun sentinel DNS -> fake IP.
        let fake_ip = resolve_via_spoofer(&ctx, "baidu.com");
        assert_in_pool(fake_ip);
        // 2. The app connects to the fake IP; the engine back-translates it.
        let host = fake_dns_backtranslate(&ctx, fake_ip).unwrap();
        assert_eq!(host, "baidu.com");
        // 3. The host is Chinese, so open_conn takes the direct-passthrough path.
        assert!(crate::china::is_chinese_host(&host));
        // 4. The passthrough re-resolves the name via the system resolver, whose
        // configured server under VPN mode is answered by this same spoofer:
        let direct_dial_target = resolve_via_spoofer(&ctx, &host);
        // The "direct" connection target is itself a fake address — a blackhole.
        assert_in_pool(direct_dial_target);
    }
}
