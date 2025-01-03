use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum Command {
    Source(usize),
    Sink(usize),
}
