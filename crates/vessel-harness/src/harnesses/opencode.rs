//! The `OpenCode` CLI (`opencode`).
//!
//! Speaks ACP natively through its `acp` subcommand, which serves ACP on
//! stdio. That subcommand has no model flag; the model is chosen over ACP
//! (see the crate docs).

use super::{Descriptor, Structured};

pub(super) const DESCRIPTOR: Descriptor = Descriptor {
    binary: "opencode",
    structured: Structured::Native { args: &["acp"] },
};
