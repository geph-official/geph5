use std::collections::{BTreeMap, VecDeque};

use egui::mutex::RwLock;
use once_cell::sync::Lazy;

use tracing::Level;
use tracing_subscriber::Layer;

#[derive(Clone, Debug)]
pub struct LogLine {
    pub level: Level,

    pub fields: BTreeMap<String, String>,
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
        let line = LogLine {
            level: *event.metadata().level(),
            fields: event
                .fields()
                .map(|field| (field.name().to_string(), field.to_string()))
                .collect(),
        };
        let mut logs = LOGS.write();
        logs.push_back(line);
        if logs.len() > 1000 {
            logs.pop_front();
        }
    }
}
