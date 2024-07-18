use std::sync::{Arc, Mutex, MutexGuard};

#[derive(Clone)]
pub struct ArcWriter<W: std::io::Write> {
    logs: Arc<Mutex<W>>,
}

impl<W: std::io::Write> ArcWriter<W> {
    pub fn new(w: W) -> Self {
        Self {
            logs: Arc::new(Mutex::new(w)),
        }
    }

    pub fn lock(&self) -> MutexGuard<'_, W> {
        self.logs.lock().unwrap()
    }
}

impl<W: std::io::Write> std::io::Write for ArcWriter<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.logs.lock().unwrap().write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.logs.lock().unwrap().flush()
    }
}

impl<W: std::io::Write> std::io::Write for &ArcWriter<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.logs.lock().unwrap().write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.logs.lock().unwrap().flush()
    }
}
