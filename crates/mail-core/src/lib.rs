//! Core of the Carteiro mail client: secure transports, IMAP/POP3/SMTP
//! clients, local storage, conversation threading and the rules engine.
//! Nothing in here depends on the UI so it can be tested headless.

pub mod account;
pub mod b64;
pub mod backup;
pub mod caldav;
pub mod carddav;
pub mod calendar;
pub mod contacts;
pub mod dedup;
pub mod doctext;
pub mod error;
pub mod graph;
pub mod http;
pub mod ical;
pub mod imap;
pub mod invites;
pub mod imap_proto;
pub mod message;
pub mod net;
pub mod oauth;
pub mod ops;
pub mod pop3;
pub mod recur;
pub mod presets;
pub mod rules;
pub mod safety;
pub mod settings_io;
pub mod scan;
pub mod search;
pub mod smtp;
pub mod store;
pub mod sync;
pub mod threading;
pub mod tracking;
pub mod tz;
pub mod update;
pub mod xml;

pub use account::{Account, AuthMethod, Credential, Protocol};
pub use error::{Error, Result};
pub use net::{Endpoint, Security};
