use std::sync::Arc;

use hubdouble_git::CommitNode;
use hubdouble_gql::{Args, Ctx, GqlError, Obj, Registry, Resolved};
use hubdouble_model::{Conclusion, RunStatus, StatusState};
use hubdouble_store::Tx;
use serde::Serialize;
use serde_json::json;

use crate::view::{latest, latest_statuses, runs_on, statuses_on};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum RollupState {
    Success,
    Pending,
    Failure,
    Error,
}

#[derive(Debug, Serialize)]
pub struct Rollup {
    pub state: RollupState,
}

pub fn register(registry: &mut Registry) {
    registry.object::<Rollup>("StatusCheckRollup");
    registry.field("Commit", "statusCheckRollup", rollup);
    registry.field("StatusCheckRollup", "state", state);
}

/// The worst state of the latest check runs and the latest status per context, `None` when the
/// commit has neither, as GitHub answers `statusCheckRollup: null`.
#[must_use]
pub fn rollup_state(tx: &Tx, repo: i64, sha: &str) -> Option<RollupState> {
    let runs = latest(runs_on(tx, repo, sha)).into_iter().map(|run| {
        if run.status != RunStatus::Completed {
            RollupState::Pending
        } else if matches!(
            run.conclusion,
            Some(
                Conclusion::Failure
                    | Conclusion::TimedOut
                    | Conclusion::Cancelled
                    | Conclusion::ActionRequired
                    | Conclusion::StartupFailure
            )
        ) {
            RollupState::Failure
        } else {
            RollupState::Success
        }
    });
    let statuses = latest_statuses(&statuses_on(tx, repo, sha))
        .into_iter()
        .map(|status| match status.state {
            StatusState::Success => RollupState::Success,
            StatusState::Pending => RollupState::Pending,
            StatusState::Failure => RollupState::Failure,
            StatusState::Error => RollupState::Error,
        });
    runs.chain(statuses).max()
}

fn rollup(ctx: &Ctx<'_>, obj: &Obj, _: &Args) -> Result<Resolved, GqlError> {
    let commit = obj.get::<CommitNode>()?;
    Ok(
        match rollup_state(ctx.tx, commit.repo_id, &commit.id.to_string()) {
            None => Resolved::Null,
            Some(state) => Resolved::Object {
                type_name: "StatusCheckRollup",
                value: Arc::new(Rollup { state }),
            },
        },
    )
}

fn state(_: &Ctx<'_>, obj: &Obj, _: &Args) -> Result<Resolved, GqlError> {
    Ok(Resolved::Leaf(json!(obj.get::<Rollup>()?.state)))
}
