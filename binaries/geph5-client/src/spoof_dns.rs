use std::net::Ipv4Addr;

use anyctx::AnyCtx;
use bytes::Bytes;
use dashmap::DashMap;
use rand::Rng;
use simple_dns::{Packet, QTYPE};

use crate::{client::CtxField, Config};

static FAKE_DNS_FORWARD: CtxField<DashMap<String, Ipv4Addr>> = |_| DashMap::new();

static FAKE_DNS_BACKWARD: CtxField<DashMap<Ipv4Addr, String>> = |_| DashMap::new();

pub fn fake_dns_backtranslate(ctx: &AnyCtx<Config>, fake: Ipv4Addr) -> Option<String> {
    tracing::trace!(fake = debug(fake), "attempting to backtranslate");
    ctx.get(FAKE_DNS_BACKWARD)
        .get(&fake)
        .map(|entry| entry.clone())
}

pub fn fake_dns_allocate(ctx: &AnyCtx<Config>, dns_name: &str) -> Ipv4Addr {
    *ctx.get(FAKE_DNS_FORWARD)
        .entry(dns_name.to_string())
        .or_insert_with(|| {
            let base = u32::from_be_bytes([240, 0, 0, 0]);
            let mask = u32::from_be_bytes([240, 0, 0, 0]);
            let offset = rand::thread_rng().gen_range(0..=!mask);
            let ip_addr = base | offset;
            let ip_addr = Ipv4Addr::from(ip_addr);
            ctx.get(FAKE_DNS_BACKWARD)
                .insert(ip_addr, dns_name.to_string());
            tracing::debug!(
                from = debug(dns_name),
                to = debug(ip_addr),
                "created fake dns mapping",
            );
            ip_addr
        })
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
