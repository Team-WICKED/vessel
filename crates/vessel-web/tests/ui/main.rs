//! `vessel ui`'s server over real loopback sockets: what a browser on
//! this machine gets, and what a page elsewhere, a rebinding site or a
//! client without the token does not (ADR 0017).

#[cfg(test)]
mod server;
