use egui::mutex::RwLock;
use serde::{de::DeserializeOwned, Serialize};
use smol_str::ToSmolStr;

use crate::prefs::{pref_read, pref_write};

pub struct StoreCell<T: Clone> {
    inner: RwLock<T>,
    on_set: Box<dyn Fn(&T) + Send + Sync + 'static>,
}

impl<T: Clone> StoreCell<T> {
    pub fn new(default_val: T) -> Self {
        Self {
            inner: RwLock::new(default_val),
            on_set: Box::new(|_| {}),
        }
    }

    pub fn set(&self, val: T) -> T {
        let mut inner = self.inner.write();
        (self.on_set)(&val);
        std::mem::replace(&mut inner, val)
    }

    pub fn get(&self) -> T {
        self.inner.read().clone()
    }

    pub fn modify<U>(&self, clos: impl FnOnce(&mut T) -> U) -> U {
        let mut inner = self.inner.write();
        let u = clos(&mut inner);
        (self.on_set)(&inner);
        u
    }
}

impl<T: Clone + Serialize + DeserializeOwned> StoreCell<T> {
    pub fn new_persistent(key: &str, default_val: impl FnOnce() -> T) -> Self {
        let val = pref_read(key)
            .ok()
            .and_then(|res| serde_json::from_str(&res).ok());
        let key = key.to_smolstr();
        Self {
            inner: RwLock::new(val.unwrap_or_else(default_val)),
            on_set: Box::new(move |val| {
                let _ = pref_write(&key, &serde_json::to_string(val).unwrap());
            }),
        }
    }
}
