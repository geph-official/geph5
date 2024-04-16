use std::collections::{BTreeMap, VecDeque};

use chrono::Utc;
use egui::mutex::RwLock;
use once_cell::sync::Lazy;

use smol_str::{SmolStr, ToSmolStr};
use tracing::Level;
use tracing_subscriber::Layer;

#[derive(Clone, Debug)]
pub struct LogLine {
    pub timestamp: chrono::DateTime<Utc>,
    pub level: Level,
    pub target: SmolStr,

    pub fields: BTreeMap<SmolStr, String>,
}

pub static LOGS: Lazy<RwLock<VecDeque<LogLine>>> = Lazy::new(|| RwLock::new(VecDeque::new()));

pub struct LogLayer;

impl<S> Layer<S> for LogLayer
where
    S: tracing::Subscriber,
{
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let mut fields = BTreeMap::new();
        let mut visitor = MapVisitor(&mut fields);

        event.record(&mut visitor);

        let line = LogLine {
            timestamp: chrono::Utc::now(),
            level: *event.metadata().level(),
            target: event.metadata().target().to_smolstr(),
            fields,
        };
        let mut logs = LOGS.write();
        logs.push_back(line);
        if logs.len() > 1000 {
            logs.pop_front();
        }
    }
}

struct MapVisitor<'a>(&'a mut BTreeMap<SmolStr, String>);

impl<'a> tracing::field::Visit for MapVisitor<'a> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.0
            .insert(field.name().to_smolstr(), format!("{:?}", value));
    }
}
