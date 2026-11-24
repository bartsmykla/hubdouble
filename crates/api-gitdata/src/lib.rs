pub mod branch_rules;
pub mod codeowners;
mod common;
mod contents;
mod data;
mod graphql;
mod refs;
mod repos;
mod tree_edit;
mod tree_view;

use hubdouble_admin::Registrar;
use hubdouble_git::{
    BlobNode, CommitNode, RefNode, SignatureNode, TagNode, TreeEntryNode, TreeNode,
};
use hubdouble_model::{Grant, Requirement, Scope};

pub use common::{account_by_email, decode_base64, text_reply, wrapped_base64};
pub use refs::{denied_workflow, workflow_refusal};

pub(crate) const READ_CONTENTS: Requirement = Requirement {
    any_of: &[Grant::read("contents")],
    scope: Scope::Repo,
    scopes: &[],
};

pub(crate) const WRITE_CONTENTS: Requirement = Requirement {
    any_of: &[Grant::write("contents")],
    scope: Scope::Repo,
    scopes: &[],
};

pub(crate) const WRITE_WORKFLOWS: Requirement = Requirement {
    any_of: &[Grant::write("workflows")],
    scope: Scope::Repo,
    scopes: &["workflow"],
};

/// Registers what T4 owns: the git data, contents, commits, compare, merge and branch
/// operations, and the GraphQL git fields and mutations.
pub fn register(r: &mut Registrar<'_>) {
    let rest = &mut *r.rest;
    rest.handle("git/create-blob", WRITE_CONTENTS, data::create_blob);
    rest.handle("git/get-blob", READ_CONTENTS, data::get_blob);
    rest.handle("git/create-tree", WRITE_CONTENTS, data::create_tree);
    rest.handle("git/get-tree", READ_CONTENTS, data::get_tree);
    rest.handle("git/create-commit", WRITE_CONTENTS, data::create_git_commit);
    rest.handle("git/get-commit", READ_CONTENTS, data::get_git_commit);
    rest.handle("git/get-ref", READ_CONTENTS, refs::get_ref);
    rest.handle(
        "git/list-matching-refs",
        READ_CONTENTS,
        refs::list_matching_refs,
    );
    rest.handle("git/create-ref", WRITE_CONTENTS, refs::create_ref);
    rest.handle("git/update-ref", WRITE_CONTENTS, refs::update_ref);
    rest.handle("git/delete-ref", WRITE_CONTENTS, refs::delete_ref);
    rest.handle("git/get-tag", READ_CONTENTS, data::get_tag);
    rest.handle("repos/get-content", READ_CONTENTS, contents::get_content);
    rest.handle("repos/list-commits", READ_CONTENTS, repos::list_commits);
    rest.handle("repos/get-commit", READ_CONTENTS, repos::get_commit);
    rest.handle("repos/compare-commits", READ_CONTENTS, repos::compare);
    rest.handle("repos/merge", WRITE_CONTENTS, repos::merge);
    rest.handle("repos/get-branch", READ_CONTENTS, repos::get_branch);

    let gql = &mut *r.gql;
    gql.object_resolved::<RefNode>("Ref");
    gql.object_resolved::<CommitNode>("Commit");
    gql.object_resolved::<TreeNode>("Tree");
    gql.object_resolved::<TreeEntryNode>("TreeEntry");
    gql.object_resolved::<BlobNode>("Blob");
    gql.object_resolved::<TagNode>("Tag");
    gql.object_resolved::<SignatureNode>("GpgSignature");
    gql.object_resolved::<graphql::CommitPayload>("CreateCommitOnBranchPayload");
    gql.object_resolved::<graphql::RefsPayload>("UpdateRefsPayload");
    gql.field("Repository", "object", graphql::repository_object);
    gql.field("Repository", "ref", graphql::repository_ref);
    gql.field("Repository", "refs", graphql::repository_refs);
    gql.field(
        "Repository",
        "defaultBranchRef",
        graphql::repository_default_branch_ref,
    );
    gql.field("Ref", "name", graphql::ref_name);
    gql.field("Ref", "target", graphql::ref_target);
    gql.field("Commit", "oid", graphql::commit_oid);
    gql.field("Commit", "tree", graphql::commit_tree);
    gql.field("Commit", "signature", graphql::commit_signature);
    gql.field("Tree", "oid", graphql::tree_oid);
    gql.field("Tree", "entries", graphql::tree_entries);
    gql.field("Blob", "oid", graphql::blob_oid);
    gql.field("Blob", "byteSize", graphql::blob_byte_size);
    gql.field("Blob", "text", graphql::blob_text);
    gql.field("Blob", "isTruncated", graphql::blob_is_truncated);
    gql.field("Tag", "oid", graphql::tag_oid);
    gql.field("Tag", "target", graphql::tag_target);
    gql.field("TreeEntry", "name", graphql::tree_entry_name);
    gql.field("TreeEntry", "mode", graphql::tree_entry_mode);
    gql.field("GpgSignature", "state", graphql::signature_state);
    gql.field(
        "CreateCommitOnBranchPayload",
        "commit",
        graphql::payload_commit,
    );
    gql.field(
        "CreateCommitOnBranchPayload",
        "clientMutationId",
        graphql::commit_payload_client_mutation_id,
    );
    gql.field(
        "UpdateRefsPayload",
        "clientMutationId",
        graphql::payload_client_mutation_id,
    );
    gql.mutation("createCommitOnBranch", graphql::create_commit_on_branch);
    gql.mutation("updateRefs", graphql::update_refs);
}
