use std::collections::BTreeMap;

use once_cell::sync::Lazy;
use smol_str::SmolStr;

use crate::prefs::pref_read;

static L10N_TABLE: Lazy<BTreeMap<SmolStr, BTreeMap<SmolStr, SmolStr>>> = Lazy::new(|| {
    let csv = include_bytes!("l10n.csv");
    let mut rdr = csv::Reader::from_reader(&csv[..]);
    let mut toret = BTreeMap::new();
    for result in rdr.deserialize() {
        let mut record: BTreeMap<SmolStr, SmolStr> = result.unwrap();
        let label = record.remove("label").unwrap();
        toret.insert(label, record);
    }
    toret
});

pub fn l10n(label: &str) -> &str {
    &L10N_TABLE[label][&pref_read("lang").unwrap()]
}
