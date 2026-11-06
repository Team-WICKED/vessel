//! The node end to end: an in-process `vesseld` on a random loopback port
//! with a temporary data directory, driven only through `vessel-client`,
//! with testkit fakes standing in for the harnesses.
//!
//! Tests wait on records and replies, never on time; the timeout in
//! `support::within` only turns a hang into a failure.

#[cfg(test)]
mod across;
#[cfg(test)]
mod agents;
#[cfg(test)]
mod fleet;
#[cfg(test)]
mod isolation;
#[cfg(test)]
mod local;
#[cfg(test)]
mod moving;
#[cfg(test)]
mod profiles;
#[cfg(test)]
#[cfg(unix)]
mod remote;
#[cfg(test)]
mod secure;
#[cfg(test)]
mod structured;
#[cfg(test)]
mod support;
#[cfg(test)]
mod terminal;
#[cfg(test)]
mod transfers;
#[cfg(test)]
mod transfers_across;
