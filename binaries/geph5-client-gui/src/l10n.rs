use std::collections::BTreeMap;

use isocountry::CountryCode;
use once_cell::sync::Lazy;
use smol_str::SmolStr;

use crate::settings::LANG_CODE;

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

pub fn l10n(label: &str) -> &'static str {
    if let Some(inner) = L10N_TABLE.get(label) {
        if let Some(inner) = inner.get(&LANG_CODE.get()) {
            return inner;
        }
    }
    "(unk)"
}

pub fn l10n_country(country: CountryCode) -> &'static str {
    l10n(&format!("country_{}", country.alpha2().to_lowercase()))
}
