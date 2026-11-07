//! Pointing a harness at the node's model proxy (milestone 5, ADR 0015).
//!
//! [Decision Log]
//! - Purpose: a profile with a `model` route runs its harness against the
//!   node's proxy instead of the harness's vendor, pinned to the route's
//!   models, with the session's model token as its credential.
//! - Constraint: every harness is pointed elsewhere differently (Claude
//!   Code by environment variables, Codex by a custom provider in `-c`
//!   overrides), and that knowledge belongs to the harness's module (the
//!   harness rule): the descriptor's `model_proxy` renders it.
//! - Choice: [`ModelProxy`] says where and with what (the node fills it per
//!   session); a harness's renderer returns [`Routed`]: variables to set
//!   and remove, and arguments. `launch` applies them after the profile's
//!   own, so a profile cannot undo them; a profile that sets one of them is
//!   refused when `profiles.toml` is read, rather than silently overridden.
//! - Harnesses without a renderer (Gemini CLI, `OpenCode` in milestone 5)
//!   cannot have a routed profile.
//!
//! [Decision Log] (the security review of milestone 5, finding 6)
//! - Problem: a harness takes the last of two settings for one key, and a
//!   routed Codex session's own arguments (its profile's, its spec's) came
//!   after Vessel's: `-c model_providers.vessel.base_url=...` sent the
//!   session's prompts and token elsewhere while it was recorded as
//!   routed.
//! - Choice: each harness's [`Routing`] also names the arguments that
//!   would take a session off its route; a routed profile with one is
//!   refused when `profiles.toml` is read, and a routed session's own
//!   arguments are checked at launch. And the routing arguments come
//!   last, after the profile's and the session's (before a `--`, after
//!   which arguments are text), so nothing missed by the check can come
//!   after them. For Codex: `-c`/`--config` setting `model`,
//!   `model_provider(s)`, `profile(s)` or `oss_provider`, and `-m`/
//!   `--model`, `--oss`, `--local-provider`, `-p`/`--profile`, in every
//!   spelling clap accepts. For Claude Code: `--settings`, which can set
//!   the gateway variables itself.
//! - Not covered: what the harness reads from its own home (an account's
//!   settings file, an administrator's managed configuration); those are
//!   the account's, as the session's home is.

use std::fmt;

use vessel_proto::{HarnessKind, ModelId};

use crate::harnesses::descriptor;

/// Where a routed session's harness sends its model calls, and with what.
#[derive(Clone, PartialEq, Eq)]
pub struct ModelProxy {
    /// The node's proxy, `http://127.0.0.1:<port>`, without a path.
    pub base_url: String,
    /// The session's model token. A secret: `Debug` never shows it.
    pub token: String,
    /// The route's model, the one the harness is pinned to.
    pub model: ModelId,
    /// The route's model for the harness's small, fast calls; the model
    /// itself when the route names none.
    pub small_model: Option<ModelId>,
}

impl ModelProxy {
    /// The model for small, fast calls.
    #[must_use]
    pub fn small(&self) -> &ModelId {
        self.small_model.as_ref().unwrap_or(&self.model)
    }
}

impl fmt::Debug for ModelProxy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ModelProxy")
            .field("base_url", &self.base_url)
            .field("token", &"<redacted>")
            .field("model", &self.model)
            .field("small_model", &self.small_model)
            .finish()
    }
}

/// What routing changes in one harness's launch.
#[derive(Default)]
pub(crate) struct Routed {
    /// Variables set, after the profile's.
    pub(crate) env: Vec<(&'static str, String)>,
    /// Variables removed (inherited ones the route must not lose to).
    pub(crate) unset: &'static [&'static str],
    /// Arguments, in terminal mode before the profile's and the session's,
    /// in structured mode to the ACP program.
    pub(crate) args: Vec<String>,
}

/// How one harness is routed.
#[derive(Debug)]
pub(crate) struct Routing {
    /// Its variables and arguments for a session.
    pub(crate) render: fn(&ModelProxy) -> Routed,
    /// The first of a routed session's own arguments (its profile's or
    /// its spec's) that would take it off its route, if any.
    pub(crate) overriding: fn(&[String]) -> Option<String>,
}

/// The first argument of `args` that would take a routed session of
/// `harness` off its route.
pub(crate) fn overriding(harness: HarnessKind, args: &[String]) -> Option<String> {
    descriptor(harness)
        .model_proxy
        .as_ref()
        .and_then(|routing| (routing.overriding)(args))
}

/// Whether `harness` can have a routed profile.
#[must_use]
pub fn routable(harness: HarnessKind) -> bool {
    descriptor(harness).model_proxy.is_some()
}

/// The variables routing sets or removes for `harness`: a profile of that
/// harness with a route may not set them itself. Empty when the harness
/// cannot be routed.
pub(crate) fn routed_variables(harness: HarnessKind) -> Vec<&'static str> {
    let Some(routing) = &descriptor(harness).model_proxy else {
        return Vec::new();
    };
    let routed = (routing.render)(&ModelProxy {
        base_url: String::new(),
        token: String::new(),
        model: ModelId::new(""),
        small_model: None,
    });
    routed
        .env
        .iter()
        .map(|(name, _)| *name)
        .chain(routed.unset.iter().copied())
        .collect()
}
