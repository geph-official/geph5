use async_trait::async_trait;
use geph5_broker_protocol::{
    BridgeDescriptor, BrokerProtocol, ExitDescriptor, ExitList, GenericError, Mac, RouteDescriptor,
    Signed, DOMAIN_EXIT_DESCRIPTOR,
};

use crate::{
    database::{insert_exit, ExitRow},
    CONFIG_FILE,
};

struct BrokerImpl {}

#[async_trait]
impl BrokerProtocol for BrokerImpl {
    async fn get_exits(&self) -> Result<Signed<ExitList>, GenericError> {
        // Implement your logic here
        unimplemented!();
    }

    async fn get_routes(&self, exit: String) -> Result<RouteDescriptor, GenericError> {
        // Implement your logic here
        unimplemented!();
    }

    async fn put_exit(&self, descriptor: Mac<Signed<ExitDescriptor>>) -> Result<(), GenericError> {
        let descriptor = descriptor
            .verify(blake3::hash(CONFIG_FILE.wait().bridge_token.as_bytes()).as_bytes())?;
        let pubkey = descriptor.pubkey;
        let descriptor = descriptor.verify(DOMAIN_EXIT_DESCRIPTOR, |_| true)?;
        let exit = ExitRow {
            pubkey: pubkey.to_bytes(),
            c2e_listen: descriptor.c2e_listen.to_string(),
            b2e_listen: descriptor.b2e_listen.to_string(),
            country: descriptor.country.to_string(),
            city: descriptor.city.clone(),
            load: descriptor.load,
            expiry: descriptor.expiry,
        };
        insert_exit(&exit).await?;
        Ok(())
    }

    async fn put_bridge(&self, descriptor: Mac<BridgeDescriptor>) -> Result<(), GenericError> {
        todo!()
    }
}
