#![deny(unsafe_code)]
//! buzz-seat-clerk: headless delivery clerk for one Buzz seat.

pub mod badge;
pub mod config;
pub mod connection;
pub mod discovery;
pub mod error;
pub mod lane;
pub mod mailbox;
pub mod read_state;
pub mod subscription;
pub mod wake;
