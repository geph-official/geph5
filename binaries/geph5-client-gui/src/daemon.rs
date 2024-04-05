use egui::mutex::Mutex;
use once_cell::sync::Lazy;

pub static DAEMON: Lazy<Mutex<Option<geph5_client::Client>>> = Lazy::new(|| Mutex::new(None));
