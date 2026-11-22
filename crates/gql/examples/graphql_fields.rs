use std::collections::BTreeMap;
use std::process::ExitCode;

const INVENTORY: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/inventory");

const TYPE_OWNERS: &[(&str, &[&str])] = &[
    ("P0-B", &["Node", "PageInfo"]),
    ("T1", &["App", "Bot"]),
    ("T2", &["User", "Actor"]),
    (
        "T3",
        &[
            "Organization",
            "Team",
            "RepositoryCollaboratorConnection",
            "RepositoryCollaboratorEdge",
        ],
    ),
    (
        "T4",
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
        "T5",
        &[
            "PullRequest",
            "PullRequestCommit",
            "MarkPullRequestReadyForReviewPayload",
        ],
    ),
    (
        "T6",
        &[
            "PullRequestReview",
            "PullRequestReviewThread",
            "PullRequestReviewComment",
            "AddPullRequestReviewThreadPayload",
            "ResolveReviewThreadPayload",
            "UnresolveReviewThreadPayload",
        ],
    ),
    ("T7", &["Issue", "IssueComment", "Comment"]),
    ("T8", &["StatusCheckRollup"]),
    ("T10", &["Repository"]),
    (
        "T11",
        &["SearchResultItemConnection", "SearchResultItemEdge"],
    ),
];

const FIELD_OWNERS: &[(&str, &str, &str)] = &[
    ("Query", "node", "P0-B"),
    ("Query", "nodes", "P0-B"),
    ("Query", "viewer", "T2"),
    ("Query", "organization", "T3"),
    ("Query", "repository", "T10"),
    ("Query", "search", "T11"),
    ("Repository", "collaborators", "T3"),
    ("Repository", "defaultBranchRef", "T4"),
    ("Repository", "object", "T4"),
    ("Repository", "ref", "T4"),
    ("Repository", "refs", "T4"),
    ("Repository", "pullRequest", "T5"),
    ("Repository", "issue", "T7"),
    ("Ref", "associatedPullRequests", "T5"),
    ("PullRequest", "reviewThreads", "T6"),
    ("PullRequest", "reviews", "T6"),
    ("Commit", "statusCheckRollup", "T8"),
    ("Mutation", "createCommitOnBranch", "T4"),
    ("Mutation", "updateRefs", "T4"),
    ("Mutation", "markPullRequestReadyForReview", "T5"),
    ("Mutation", "addPullRequestReviewThread", "T6"),
    ("Mutation", "resolveReviewThread", "T6"),
    ("Mutation", "unresolveReviewThread", "T6"),
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
    (type_name.ends_with("Connection") || type_name.ends_with("Edge")).then_some("P0-B")
}

/// Regenerates `docs/inventory/graphql-fields.tsv`, keeping every owner already assigned.
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
