//! Saved-file language-server context, independent of editors and harnesses.
//!
//! This crate owns LSP framing, processes and document state. Authentication
//! and the decision about which roots a caller may read belong to the node.
//! No language-server request is an authority to read an arbitrary path.

pub mod framing;
pub mod position;
