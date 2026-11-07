//! Fronts: the APIs harnesses speak. Each turns its API's request into the
//! neutral form ([`crate::ir`]) and writes the neutral stream back in its
//! API's events, ending each answer exactly once.

pub mod anthropic;
