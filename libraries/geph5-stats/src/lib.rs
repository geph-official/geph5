use std::{
    collections::BTreeMap,
    net::{SocketAddr, UdpSocket},
    sync::Mutex,
};

use serde::{Deserialize, Serialize};

/// A single stat datapoint with statsd-style semantics and named tags.
///
/// This is the wire type used both for local DogStatsD emission and for
/// shipping batches from bridges/exits to the broker over RPC.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct StatEvent {
    pub name: String,
    /// BTreeMap so that serialization is deterministic, which Mac authentication requires.
    pub tags: BTreeMap<String, String>,
    pub value: f64,
    pub kind: StatKind,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum StatKind {
    /// Summed over the flush window (statsd `|c`).
    Counter,
    /// Last value wins (statsd `|g`).
    Gauge,
    /// Timing in milliseconds; the statsd server derives count/mean/percentiles (statsd `|ms`).
    TimerMs,
}

impl StatEvent {
    pub fn counter(name: &str, tags: &[(&str, &str)], value: f64) -> Self {
        Self::new(name, tags, value, StatKind::Counter)
    }

    pub fn gauge(name: &str, tags: &[(&str, &str)], value: f64) -> Self {
        Self::new(name, tags, value, StatKind::Gauge)
    }

    pub fn timer_ms(name: &str, tags: &[(&str, &str)], value: f64) -> Self {
        Self::new(name, tags, value, StatKind::TimerMs)
    }

    fn new(name: &str, tags: &[(&str, &str)], value: f64, kind: StatKind) -> Self {
        Self {
            name: sanitize(name),
            tags: tags
                .iter()
                .map(|(k, v)| (sanitize(k), sanitize(v)))
                .collect(),
            value,
            kind,
        }
    }

    /// Encodes as a single DogStatsD line, e.g. `bridge_bytes:1048576|c|#pool:hk,country:CN`.
    pub fn dogstatsd_line(&self) -> String {
        let kind = match self.kind {
            StatKind::Counter => "c",
            StatKind::Gauge => "g",
            StatKind::TimerMs => "ms",
        };
        let mut line = format!("{}:{}|{}", sanitize(&self.name), self.value, kind);
        if !self.tags.is_empty() {
            line.push_str("|#");
            let mut first = true;
            for (k, v) in &self.tags {
                if !first {
                    line.push(',');
                }
                first = false;
                line.push_str(&sanitize(k));
                line.push(':');
                line.push_str(&sanitize(v));
            }
        }
        line
    }
}

/// Replaces characters that have meaning in the statsd line protocol.
fn sanitize(s: &str) -> String {
    s.replace([':', '|', '#', ',', '\n', '@', '='], "_")
}

/// Maximum payload of a single statsd datagram. Conservative for loopback.
const MAX_DATAGRAM: usize = 1400;

/// Fire-and-forget DogStatsD emitter over UDP. Send failures are silently dropped,
/// matching statsd semantics: stats must never take down or slow the caller.
pub struct StatsdUdpSink {
    sock: UdpSocket,
    dest: SocketAddr,
}

impl StatsdUdpSink {
    pub fn new(dest: SocketAddr) -> std::io::Result<Self> {
        let bind: SocketAddr = if dest.is_ipv4() {
            "0.0.0.0:0".parse().unwrap()
        } else {
            "[::]:0".parse().unwrap()
        };
        let sock = UdpSocket::bind(bind)?;
        sock.set_nonblocking(true)?;
        Ok(Self { sock, dest })
    }

    pub fn send_one(&self, event: &StatEvent) {
        let _ = self
            .sock
            .send_to(event.dogstatsd_line().as_bytes(), self.dest);
    }

    /// Sends events packed into as few datagrams as possible (newline-separated).
    pub fn send_many<'a>(&self, events: impl IntoIterator<Item = &'a StatEvent>) {
        let mut buf = String::new();
        for event in events {
            let line = event.dogstatsd_line();
            if !buf.is_empty() && buf.len() + 1 + line.len() > MAX_DATAGRAM {
                let _ = self.sock.send_to(buf.as_bytes(), self.dest);
                buf.clear();
            }
            if !buf.is_empty() {
                buf.push('\n');
            }
            buf.push_str(&line);
        }
        if !buf.is_empty() {
            let _ = self.sock.send_to(buf.as_bytes(), self.dest);
        }
    }
}

/// Accumulates stats locally so that semi-trusted nodes (bridges, exits) can ship
/// them to the broker in periodic batches: counters with identical name+tags are
/// summed, gauges keep the last value, and timers are kept as individual events.
#[derive(Default)]
pub struct StatBatcher {
    inner: Mutex<BatcherInner>,
}

#[derive(Default)]
struct BatcherInner {
    counters: BTreeMap<(String, BTreeMap<String, String>), f64>,
    gauges: BTreeMap<(String, BTreeMap<String, String>), f64>,
    timers: Vec<StatEvent>,
}

impl StatBatcher {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&self, event: StatEvent) {
        let mut inner = self.inner.lock().unwrap();
        match event.kind {
            StatKind::Counter => {
                *inner
                    .counters
                    .entry((event.name, event.tags))
                    .or_insert(0.0) += event.value;
            }
            StatKind::Gauge => {
                inner.gauges.insert((event.name, event.tags), event.value);
            }
            StatKind::TimerMs => inner.timers.push(event),
        }
    }

    pub fn counter(&self, name: &str, tags: &[(&str, &str)], value: f64) {
        self.push(StatEvent::counter(name, tags, value));
    }

    pub fn gauge(&self, name: &str, tags: &[(&str, &str)], value: f64) {
        self.push(StatEvent::gauge(name, tags, value));
    }

    pub fn timer_ms(&self, name: &str, tags: &[(&str, &str)], value: f64) {
        self.push(StatEvent::timer_ms(name, tags, value));
    }

    /// Takes all accumulated events, leaving the batcher empty.
    pub fn drain(&self) -> Vec<StatEvent> {
        let mut inner = self.inner.lock().unwrap();
        let counters = std::mem::take(&mut inner.counters);
        let gauges = std::mem::take(&mut inner.gauges);
        let timers = std::mem::take(&mut inner.timers);
        counters
            .into_iter()
            .map(|((name, tags), value)| StatEvent {
                name,
                tags,
                value,
                kind: StatKind::Counter,
            })
            .chain(gauges.into_iter().map(|((name, tags), value)| StatEvent {
                name,
                tags,
                value,
                kind: StatKind::Gauge,
            }))
            .chain(timers)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dogstatsd_line_formats_counter_with_tags() {
        let event = StatEvent::counter(
            "bridge_bytes",
            &[("pool", "hk"), ("country", "CN")],
            1048576.0,
        );
        assert_eq!(
            event.dogstatsd_line(),
            "bridge_bytes:1048576|c|#country:CN,pool:hk"
        );
    }

    #[test]
    fn dogstatsd_line_formats_untagged_gauge() {
        let event = StatEvent::gauge("plus", &[], 17.0);
        assert_eq!(event.dogstatsd_line(), "plus:17|g");
    }

    #[test]
    fn dogstatsd_line_formats_timer() {
        let event = StatEvent::timer_ms("broker_rpc_calls", &[("method", "get_exits")], 12.5);
        assert_eq!(
            event.dogstatsd_line(),
            "broker_rpc_calls:12.5|ms|#method:get_exits"
        );
    }

    #[test]
    fn sanitize_strips_protocol_characters() {
        let event = StatEvent::gauge("we|ird:name", &[("ta#g", "va,lue")], 1.0);
        assert_eq!(event.dogstatsd_line(), "we_ird_name:1|g|#ta_g:va_lue");
    }

    #[test]
    fn batcher_sums_counters_and_overwrites_gauges() {
        let batcher = StatBatcher::new();
        batcher.counter("bytes", &[("pool", "a")], 10.0);
        batcher.counter("bytes", &[("pool", "a")], 5.0);
        batcher.counter("bytes", &[("pool", "b")], 1.0);
        batcher.gauge("load", &[], 0.5);
        batcher.gauge("load", &[], 0.7);

        let mut drained = batcher.drain();
        drained.sort_by(|a, b| (&a.name, &a.tags).cmp(&(&b.name, &b.tags)));
        assert_eq!(drained.len(), 3);
        assert_eq!(drained[0].value, 15.0);
        assert_eq!(drained[1].value, 1.0);
        assert_eq!(drained[2].value, 0.7);
        assert!(batcher.drain().is_empty());
    }

    #[test]
    fn batcher_keeps_individual_timers() {
        let batcher = StatBatcher::new();
        batcher.timer_ms("lat", &[], 1.0);
        batcher.timer_ms("lat", &[], 2.0);
        assert_eq!(batcher.drain().len(), 2);
    }

    #[test]
    fn send_many_packs_multiple_lines() {
        let receiver = UdpSocket::bind("127.0.0.1:0").unwrap();
        let sink = StatsdUdpSink::new(receiver.local_addr().unwrap()).unwrap();
        sink.send_many(&[
            StatEvent::gauge("a", &[], 1.0),
            StatEvent::gauge("b", &[], 2.0),
        ]);
        let mut buf = [0u8; 2048];
        let (n, _) = receiver.recv_from(&mut buf).unwrap();
        assert_eq!(std::str::from_utf8(&buf[..n]).unwrap(), "a:1|g\nb:2|g");
    }
}
