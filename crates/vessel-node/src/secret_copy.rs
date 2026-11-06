//! `secret.copy` on the wire: ask the target first, then send the value
//! ([`copy`]).
//!
//! [Decision Log]
//! - Purpose: a value must reach only a target that will keep it. A
//!   `secret.set` the target refuses (its grant to the home lacks `admin`)
//!   has already carried the value into that node's memory, and while a
//!   home could be misled about which node a member is (the id squatting
//!   the security review found, closed by binding each member's device to
//!   its node id and key), it could have reached an impostor.
//! - Choice: first a call that carries no value and needs the same scope,
//!   `secret.list` (names only), sent the same way, as the same caller, to
//!   the same node id; the value goes only if that passes. A target that
//!   would refuse the value refuses the question instead, and the refusal
//!   is the same `forbidden` with how to grant `admin`.
//! - Alternatives: a dedicated "may I" call (a protocol change for one
//!   check); `secret.set` with a blank value (it writes on the target);
//!   sealing the value to the target's key (a second Noise pattern, which
//!   protocol 6 decided against).
//! - Cost: one more round trip per copy. A grant withdrawn between the two
//!   calls still refuses the second one, after the value went: the window
//!   is that of a person revoking the home while a copy runs.

use std::future::Future;

use vessel_proto::{Call, ErrorCode, Forwarded, ListSecrets, Request as _, SetSecret, WireError};

/// What a copy came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Copied {
    /// The target stored it.
    Stored,
    /// The target refused for want of `admin`: nothing carrying the value
    /// was sent, unless the grant was withdrawn between the two calls.
    NotGranted,
}

/// Whether `error` is the target refusing for want of `admin` (the guard's
/// message names the scope a call needs).
fn wants_admin(error: &WireError) -> bool {
    error.code == ErrorCode::Forbidden && error.message.contains("`admin`")
}

/// The target's answer to `R`, or `internal` if it answered another call.
fn answer<R: vessel_proto::Request>(forwarded: Forwarded) -> Result<R::Response, WireError> {
    R::response_from(*forwarded.reply).map_err(|unexpected| {
        WireError::new(
            ErrorCode::Internal,
            format!("the target answered out of turn: {unexpected}"),
        )
    })
}

/// Send `set` to a target through `send` (the envelope, as the caller),
/// after asking it `secret.list` the same way: see the module's Decision
/// Log.
///
/// # Errors
/// Any other failure of either call.
pub(crate) async fn copy<F, Fut>(set: SetSecret, mut send: F) -> Result<Copied, WireError>
where
    F: FnMut(Call) -> Fut,
    Fut: Future<Output = Result<Forwarded, WireError>>,
{
    match send(ListSecrets.into_call()).await {
        Ok(forwarded) => drop(answer::<ListSecrets>(forwarded)?),
        Err(error) if wants_admin(&error) => return Ok(Copied::NotGranted),
        Err(error) => return Err(error),
    }
    match send(set.into_call()).await {
        Ok(forwarded) => answer::<SetSecret>(forwarded).map(|_ack| Copied::Stored),
        Err(error) if wants_admin(&error) => Ok(Copied::NotGranted),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use vessel_proto::{Ack, Reply, SecretValue};

    use super::*;

    const VALUE: &str = "a-value-that-must-not-travel";

    /// Every call a target was sent, as JSON (values included).
    type Calls = Arc<Mutex<Vec<String>>>;

    /// A target's answer, at once.
    type Answer = std::future::Ready<Result<Forwarded, WireError>>;

    fn set() -> SetSecret {
        SetSecret {
            name: "DOCS_TOKEN".parse().unwrap(),
            value: SecretValue::new(VALUE.to_owned()),
        }
    }

    /// A target that answers or refuses each call with `answer`, and the
    /// calls it was sent, as JSON (values included).
    fn target(
        answer: impl Fn(&Call) -> Result<Reply, WireError>,
    ) -> (impl FnMut(Call) -> Answer, Calls) {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let record = Arc::clone(&calls);
        let deliver = move |call: Call| {
            record
                .lock()
                .unwrap()
                .push(serde_json::to_string(&call).unwrap());
            std::future::ready(answer(&call).map(|reply| Forwarded {
                reply: Box::new(reply),
            }))
        };
        (deliver, calls)
    }

    fn refused(scope: &str) -> WireError {
        WireError::new(
            ErrorCode::Forbidden,
            format!("this call from another node may not call `x` (it needs the `{scope}` scope)"),
        )
    }

    #[tokio::test]
    async fn a_target_without_admin_never_gets_the_value() {
        let (deliver, calls) = target(|_call| Err(refused("admin")));
        assert_eq!(copy(set(), deliver).await.unwrap(), Copied::NotGranted);
        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 1, "only the question: {calls:?}");
        assert!(calls[0].contains("secret.list"), "{calls:?}");
        assert!(!calls[0].contains(VALUE));
    }

    #[tokio::test]
    async fn the_value_goes_once_the_question_passed() {
        let (deliver, calls) = target(|call| {
            Ok(if matches!(call, Call::ListSecrets(_)) {
                Reply::ListSecrets(Vec::new())
            } else {
                Reply::SetSecret(Ack)
            })
        });
        assert_eq!(copy(set(), deliver).await.unwrap(), Copied::Stored);
        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert!(!calls[0].contains(VALUE), "the question carries no value");
        assert!(calls[1].contains(VALUE));
    }

    #[tokio::test]
    async fn another_refusal_of_the_question_stops_the_copy() {
        let (deliver, calls) = target(|_call| Err(refused("relay")));
        let error = copy(set(), deliver).await.unwrap_err();
        assert_eq!(error.code, ErrorCode::Forbidden);
        assert_eq!(calls.lock().unwrap().len(), 1);
    }
}
