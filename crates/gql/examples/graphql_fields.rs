use std::collections::BTreeMap;
use std::process::ExitCode;

const INVENTORY: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/inventory");

const TYPE_OWNERS: &[(&str, &[&str])] = &[
    ("gql", &["Node", "PageInfo"]),
    ("api-apps", &["App", "Bot"]),
    ("api-users", &["User", "Actor"]),
    (
        "api-orgs",
        &[
            "Organization",
            "Team",
            "RepositoryCollaboratorConnection",
            "RepositoryCollaboratorEdge",
        ],
    ),
    (
        "api-gitdata",
        &[
            "Ref",
            "Commit",
            "Tree",
            "TreeEntry",
            "Blob",
            "Tag",
            "GitObject",
            "GitActor",
            "GitSignature",
            "GpgSignature",
            "SshSignature",
            "SmimeSignature",
            "UnknownSignature",
            "CreateCommitOnBranchPayload",
            "UpdateRefsPayload",
        ],
    ),
    (
        "api-pulls",
        &[
            "PullRequest",
            "PullRequestCommit",
            "MarkPullRequestReadyForReviewPayload",
        ],
    ),
    (
        "api-reviews",
        &[
            "PullRequestReview",
            "PullRequestReviewThread",
            "PullRequestReviewComment",
            "AddPullRequestReviewThreadPayload",
            "ResolveReviewThreadPayload",
            "UnresolveReviewThreadPayload",
        ],
    ),
    ("api-issues", &["Issue", "IssueComment", "Comment"]),
    ("api-checks", &["StatusCheckRollup"]),
    ("api-repos", &["Repository"]),
    (
        "api-search",
        &["SearchResultItemConnection", "SearchResultItemEdge"],
    ),
];

const FIELD_OWNERS: &[(&str, &str, &str)] = &[
    ("Query", "node", "gql"),
    ("Query", "nodes", "gql"),
    ("Query", "viewer", "api-users"),
    ("Query", "organization", "api-orgs"),
    ("Query", "repository", "api-repos"),
    ("Query", "search", "api-search"),
    ("Repository", "collaborators", "api-orgs"),
    ("Repository", "defaultBranchRef", "api-gitdata"),
    ("Repository", "object", "api-gitdata"),
    ("Repository", "ref", "api-gitdata"),
    ("Repository", "refs", "api-gitdata"),
    ("Repository", "pullRequest", "api-pulls"),
    ("Repository", "issue", "api-issues"),
    ("Ref", "associatedPullRequests", "api-pulls"),
    ("PullRequest", "closingIssuesReferences", "api-issues"),
    ("PullRequest", "reviewThreads", "api-reviews"),
    ("PullRequest", "reviews", "api-reviews"),
    ("Commit", "statusCheckRollup", "api-checks"),
    ("Mutation", "createCommitOnBranch", "api-gitdata"),
    ("Mutation", "updateRefs", "api-gitdata"),
    ("Mutation", "markPullRequestReadyForReview", "api-pulls"),
    ("Mutation", "addPullRequestReviewThread", "api-reviews"),
    ("Mutation", "resolveReviewThread", "api-reviews"),
    ("Mutation", "unresolveReviewThread", "api-reviews"),
];

fn default_owner(type_name: &str, field: &str) -> Option<&'static str> {
    if let Some((_, _, owner)) = FIELD_OWNERS
        .iter()
        .find(|(t, f, _)| *t == type_name && *f == field)
    {
        return Some(owner);
    }
    if let Some((owner, _)) = TYPE_OWNERS.iter().find(|(_, ts)| ts.contains(&type_name)) {
        return Some(owner);
    }
    (type_name.ends_with("Connection") || type_name.ends_with("Edge")).then_some("gql")
}

fn main() -> ExitCode {
    let schema = match hubdouble_gql::Schema::load() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };
    let out = format!("{INVENTORY}/graphql-fields.tsv");
    let assigned: BTreeMap<String, String> = std::fs::read_to_string(&out)
        .unwrap_or_default()
        .lines()
        .skip(1)
        .filter_map(|l| {
            let mut cols = l.split('\t');
            Some((cols.next()?.to_owned(), cols.next()?.to_owned()))
        })
        .filter(|(_, owner)| !owner.is_empty())
        .collect();
    let mut rows: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut paths: Vec<_> = std::fs::read_dir(format!("{INVENTORY}/graphql"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .collect();
    paths.sort();
    for path in paths {
        let name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_owned();
        let query = std::fs::read_to_string(&path).unwrap_or_default();
        match schema.selected_fields(&query) {
            Ok(fields) => {
                for (t, f) in fields {
                    rows.entry(format!("{t}.{f}"))
                        .or_default()
                        .push(name.clone());
                }
            }
            Err(e) => {
                eprintln!("{name}: {e:?}");
                return ExitCode::FAILURE;
            }
        }
    }
    let tsv: String = std::iter::once("field\towner\tdocuments\n".to_owned())
        .chain(rows.iter().map(|(field, docs)| {
            let owner = assigned.get(field).map(String::as_str).or_else(|| {
                let (t, f) = field.split_once('.')?;
                default_owner(t, f)
            });
            format!(
                "{field}\t{}\t{}\n",
                owner.unwrap_or_default(),
                docs.join(",")
            )
        }))
        .collect();
    if let Err(e) = std::fs::write(&out, tsv) {
        eprintln!("{out}: {e}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}
