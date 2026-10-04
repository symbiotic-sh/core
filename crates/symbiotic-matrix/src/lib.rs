// matrix_sdk::Client's Send and Sync checks nest deeper than the default limit of 128 on rustc 1.101.
#![recursion_limit = "256"]

pub mod events;
pub mod intake;
pub mod registration;
pub mod transport;
