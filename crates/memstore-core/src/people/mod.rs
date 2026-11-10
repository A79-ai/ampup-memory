//! People: orgs, teams and members ([`directory`]), and the CLI that signs them in and points them at
//! their memory ([`client`], [`cli`]); a team pilot from a csv, before there is an IdP ([`team`]).

pub use memstore_client::client;
pub mod directory;
pub mod installations;
pub mod team;
