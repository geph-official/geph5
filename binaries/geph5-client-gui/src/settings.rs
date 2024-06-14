use std::net::{Ipv4Addr, SocketAddrV4};

use geph5_broker_protocol::Credential;
use geph5_client::{BrokerSource, Config, ExitConstraint};
use isocountry::CountryCode;

use once_cell::sync::Lazy;
use smol_str::{SmolStr, ToSmolStr};

use crate::store_cell::StoreCell;

pub static DEFAULT_SETTINGS: Lazy<serde_yaml::Value> =
    Lazy::new(|| serde_yaml::from_str(include_str!("settings_default.yaml")).unwrap());

pub fn get_config() -> anyhow::Result<Config> {
    let yaml: serde_yaml::Value = DEFAULT_SETTINGS.to_owned();
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
    if let Some(custom_broker) = CUSTOM_BROKER.get() {
        cfg.broker = Some(custom_broker);
    }
    cfg.socks5_listen = Some(std::net::SocketAddr::V4(SocketAddrV4::new(
        Ipv4Addr::new(127, 0, 0, 1),
        SOCKS5_PORT.get(),
    )));
    cfg.http_proxy_listen = Some(std::net::SocketAddr::V4(SocketAddrV4::new(
        Ipv4Addr::new(127, 0, 0, 1),
        HTTP_PROXY_PORT.get(),
    )));
    cfg.vpn = VPN_MODE.get();
    Ok(cfg)
}

pub static USERNAME: Lazy<StoreCell<String>> =
    Lazy::new(|| StoreCell::new_persistent("username", || "".to_string()));

pub static PASSWORD: Lazy<StoreCell<String>> =
    Lazy::new(|| StoreCell::new_persistent("password", || "".to_string()));

pub static LANG_CODE: Lazy<StoreCell<SmolStr>> =
    Lazy::new(|| StoreCell::new_persistent("lang_code", || "en".to_smolstr()));

pub static PROXY_AUTOCONF: Lazy<StoreCell<bool>> =
    Lazy::new(|| StoreCell::new_persistent("proxy_autoconff", || true));

pub static SELECTED_COUNTRY: Lazy<StoreCell<Option<CountryCode>>> =
    Lazy::new(|| StoreCell::new_persistent("selected_country", || None));

pub static SELECTED_CITY: Lazy<StoreCell<Option<String>>> =
    Lazy::new(|| StoreCell::new_persistent("selected_city", || None));

pub static CUSTOM_BROKER: Lazy<StoreCell<Option<BrokerSource>>> =
    Lazy::new(|| StoreCell::new_persistent("custom_broker_1", || None));

pub static SOCKS5_PORT: Lazy<StoreCell<u16>> =
    Lazy::new(|| StoreCell::new_persistent("socks5_port", || 9999));

pub static HTTP_PROXY_PORT: Lazy<StoreCell<u16>> =
    Lazy::new(|| StoreCell::new_persistent("http_proxy_port", || 19999));

pub static VPN_MODE: Lazy<StoreCell<bool>> =
    Lazy::new(|| StoreCell::new_persistent("vpn_mode", || false));
