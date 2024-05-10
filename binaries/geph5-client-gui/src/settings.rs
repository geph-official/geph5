use geph5_broker_protocol::Credential;
use geph5_client::{Config, ExitConstraint};
use isocountry::CountryCode;

use once_cell::sync::Lazy;
use smol_str::{SmolStr, ToSmolStr};

use crate::store_cell::StoreCell;

pub fn get_config() -> anyhow::Result<Config> {
    let yaml: serde_yaml::Value = serde_yaml::from_str(include_str!("settings_default.yaml"))?;
    let json: serde_json::Value = serde_json::to_value(&yaml)?;
    let mut cfg: Config = serde_json::from_value(json)?;
    cfg.credentials = Credential::LegacyUsernamePassword {
        username: USERNAME.get(),
        password: PASSWORD.get(),
    };
    cfg.exit_constraint = match (SELECTED_COUNTRY.get(), SELECTED_CITY.get()) {
        (Some(country), Some(city)) => ExitConstraint::CountryCity(country, city),
        (Some(country), None) => ExitConstraint::Country(country),
        _ => ExitConstraint::Auto,
    };
    Ok(cfg)
}

pub static USERNAME: Lazy<StoreCell<String>> =
    Lazy::new(|| StoreCell::new_persistent("username", || "".to_string()));

pub static PASSWORD: Lazy<StoreCell<String>> =
    Lazy::new(|| StoreCell::new_persistent("password", || "".to_string()));

pub static LANG_CODE: Lazy<StoreCell<SmolStr>> =
    Lazy::new(|| StoreCell::new_persistent("lang_code", || "en".to_smolstr()));

pub static PROXY_AUTOCONF: Lazy<StoreCell<bool>> =
    Lazy::new(|| StoreCell::new_persistent("proxy_autoconf", || false));

pub static SELECTED_COUNTRY: Lazy<StoreCell<Option<CountryCode>>> =
    Lazy::new(|| StoreCell::new_persistent("selected_country", || None));

pub static SELECTED_CITY: Lazy<StoreCell<Option<String>>> =
    Lazy::new(|| StoreCell::new_persistent("selected_city", || None));
