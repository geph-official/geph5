use bytes::Bytes;
use http_body_util::combinators::BoxBody;
use std::convert::Infallible;
pub use crate::tunneled_http::Connector;

pub type CtxClient = hyper_util::client::legacy::Client<Connector, BoxBody<Bytes, Infallible>>;
