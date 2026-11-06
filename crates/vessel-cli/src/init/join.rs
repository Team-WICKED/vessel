//! `--join`: membership of a fleet (ADR 0012), through the same pairing as
//! `vessel fleet join` (`vessel_node::fleet::join`). The link is read from
//! standard input, never the command line; the home it names is shown and
//! confirmed first (`--yes` confirms). A member, or a home, has nothing to
//! join, and no link is read then.

use vessel_node::fleet::{self, FleetFiles, Role};

use super::{Consent, Init, Outcome};
use crate::error::CliError;
use crate::pair;

pub(super) async fn run(init: &mut Init) -> Result<(), CliError> {
    let files = FleetFiles::new(&init.data_dir);
    let status = {
        let files = files.clone();
        tokio::task::spawn_blocking(move || fleet::status(&files)).await??
    };
    match status.role {
        Role::Member { home, .. } => {
            let detail = match &status.state.grant {
                None => format!("a member of {home}"),
                Some(refused) => format!(
                    "a member of {home}, but its home has not taken this node's grant: \
                     {refused}"
                ),
            };
            init.report(Outcome::Already, "fleet", &detail).await?;
            return Ok(());
        }
        Role::Home { .. } => {
            init.report(
                Outcome::Skipped,
                "fleet",
                "this node is a fleet's home; it joins none",
            )
            .await?;
            return Ok(());
        }
        Role::None => {}
    }
    let yes = matches!(init.options.consent, Consent::Yes | Consent::YesAndInstall);
    let lines = init.answers.lines()?;
    let grant = fleet::DEFAULT_GRANT.to_vec();
    let question = crate::fleet::join_question(&grant);
    let offer = match pair::offer_from_lines(lines, yes, "vessel fleet invite", &question).await {
        Ok(offer) => offer,
        Err(CliError::NotPaired) => {
            init.report(Outcome::Skipped, "fleet", "not joined (you said no)")
                .await?;
            return Ok(());
        }
        Err(error) => return Err(error),
    };
    let joined = fleet::join(&files, &offer, &crate::fleet::node_name(), grant).await?;
    let detail = format!(
        "joined the fleet of {} ({}) via {}",
        joined.home.name, joined.home.id, joined.route
    );
    init.report(Outcome::Done, "fleet", &detail).await?;
    if let Some(clash) = crate::fleet::named_like_home(&joined.home.name) {
        init.todo.push(clash.trim_end().to_owned());
    }
    if !init.options.service {
        init.todo.push(
            "the node follows the fleet once it runs: `vessel node start`, or `vessel init \
             --service`"
                .to_owned(),
        );
    }
    Ok(())
}
