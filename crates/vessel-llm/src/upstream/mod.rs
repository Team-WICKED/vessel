//! Upstreams: the APIs providers speak. Each writes the neutral request in
//! its API's terms and reads its API's stream into neutral events, ending
//! each answer exactly once ([`crate::ir::Event::Stop`] or
//! [`crate::ir::Event::Failed`]).

pub mod chat;
