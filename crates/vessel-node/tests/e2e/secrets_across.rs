//! `secret.copy` across a fleet (protocol 6, F3): a node sends its value
//! to another node's store as `secret.set` through the home, as its caller;
//! the target must have granted the home `admin`, and the refusals say how
//! to grant it. Values never appear in an error.

use vessel_node::fleet::DEFAULT_GRANT;
use vessel_proto::{ErrorCode, Scope, SecretName, SecretValue};
use vessel_rpc::CallError;

use crate::across::{Member, home, member_granting};
use crate::secure::Relay;
use crate::support::within;

const VALUE: &str = "s3cret-value-never-shown";

fn name(text: &str) -> SecretName {
    text.parse().unwrap()
}

/// The text of a refusal, checked to be `forbidden` and to hold no value.
fn forbidden(result: Result<(), CallError>) -> String {
    let Err(CallError::Remote(error)) = result else {
        panic!("expected a refusal, got {result:?}");
    };
    assert_eq!(error.code, ErrorCode::Forbidden, "{}", error.message);
    assert!(!error.message.contains(VALUE), "{}", error.message);
    error.message
}

/// The value `member`'s node keeps for `secret`, from its file.
fn stored(member: &Member, secret: &str) -> Option<String> {
    let path = member.fixture.data_dir().root().join("secrets.toml");
    let text = std::fs::read_to_string(path).ok()?;
    let document: toml_edit::DocumentMut = text.parse().unwrap();
    document.get(secret)?.as_str().map(ToOwned::to_owned)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_secret_is_copied_only_where_the_home_was_granted_admin() {
    within(async {
        let relay = Relay::start().await;
        let sources = tempfile::tempdir().unwrap();
        let home = home(&relay, sources.path()).await;
        let laptop = member_granting(
            &relay,
            &home,
            "laptop",
            vec![Scope::Sync, Scope::Relay],
            DEFAULT_GRANT.to_vec(),
        )
        .await;
        let desktop = member_granting(
            &relay,
            &home,
            "desktop",
            vec![Scope::Sync],
            vec![Scope::Read, Scope::Operate, Scope::Admin],
        )
        .await;
        let nas = member_granting(
            &relay,
            &home,
            "nas",
            vec![Scope::Sync],
            DEFAULT_GRANT.to_vec(),
        )
        .await;
        laptop
            .owner
            .secret_set(name("DOCS_TOKEN"), SecretValue::new(VALUE.to_owned()))
            .await
            .unwrap();

        // Member to member through the home: the desktop granted `admin`.
        laptop
            .owner
            .secret_copy(name("DOCS_TOKEN"), desktop.id())
            .await
            .unwrap();
        assert_eq!(stored(&desktop, "DOCS_TOKEN").as_deref(), Some(VALUE));

        // The nas granted the default, `read` and `operate`: refused, and
        // the refusal says how to grant `admin`.
        let refused = forbidden(laptop.owner.secret_copy(name("DOCS_TOKEN"), nas.id()).await);
        assert!(refused.contains("--grant read,operate,admin"), "{refused}");
        assert_eq!(stored(&nas, "DOCS_TOKEN"), None);

        // The home grants its members only `sync` and `relay`.
        let refused = forbidden(
            laptop
                .owner
                .secret_copy(name("DOCS_TOKEN"), home.id())
                .await,
        );
        assert!(
            refused.contains("vessel fleet secret set DOCS_TOKEN"),
            "{refused}"
        );
        assert_eq!(stored(&home, "DOCS_TOKEN"), None);

        // From the home straight to a member.
        home.owner
            .secret_set(name("API_KEY"), SecretValue::new(VALUE.to_owned()))
            .await
            .unwrap();
        home.owner
            .secret_copy(name("API_KEY"), desktop.id())
            .await
            .unwrap();
        assert_eq!(stored(&desktop, "API_KEY").as_deref(), Some(VALUE));
        relay.stop().await;
    })
    .await;
}
