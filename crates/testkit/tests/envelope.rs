use hubdouble_render::{OrganizationSimple, Repository, SimpleUser, Urls};
use hubdouble_store::{Accounts, Repos};
use hubdouble_testkit::TestServer;
use hubdouble_testkit::fixture::{ALICE_ID, ORG_ID, PUBLIC_REPO_ID};
use hubdouble_testkit::validate::webhook_payload;
use serde_json::json;

#[tokio::test]
async fn the_shared_webhook_envelope_passes_the_octokit_schema() {
    // Given the fixture's org repository and alice
    let server = TestServer::builder().fixture().start().await;
    let services = server.server.services().clone();
    // When the envelope hooks adds to every event is rendered for a star event
    let payload = services
        .store
        .view(|tx| {
            let urls = Urls::new(tx.config(), "");
            let accounts = tx.table::<Accounts>();
            let repo = tx.table::<Repos>().get(PUBLIC_REPO_ID).unwrap();
            let org = accounts.get(ORG_ID).unwrap();
            Ok(json!({
                "action": "created",
                "starred_at": "2026-01-01T00:00:00Z",
                "repository": Repository::new(&urls, repo, org),
                "organization": OrganizationSimple::new(&urls, org),
                "sender": SimpleUser::new(&urls, accounts.get(ALICE_ID).unwrap()),
            }))
        })
        .unwrap();
    // Then octokit's schema accepts it
    assert_eq!(webhook_payload("star", Some("created"), &payload), Ok(()));
}
