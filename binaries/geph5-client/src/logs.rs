use arc_writer::ArcWriter;
use once_cell::sync::Lazy;

pub static LOGS: Lazy<ArcWriter<Vec<u8>>> = Lazy::new(|| ArcWriter::new(vec![]));
