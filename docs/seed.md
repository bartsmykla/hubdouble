# Seed reference

Set `HUBDOUBLE_SEED` to the path of a YAML or JSON file. Use a list of objects. Set the required `kind` string on each object to one of the names below.

hubdouble applies entries in kind order. Within a kind, it uses file order, except that a repository precedes its forks. If you omit an optional `id`, hubdouble allocates it above the largest ID in the seed. An RFC 3339 timestamp is a string such as `2026-01-01T00:00:00Z`.

The YAML examples below form one seed file in the order shown. Set `HUBDOUBLE_APP_PUBLIC_KEY` to a PEM public key before you use the examples.

## Templates

Use `{{env "NAME"}}` in a string value to insert a nonempty environment variable. A missing or empty variable causes the seed load to fail. The error identifies the variable and the entry.

Use `{{sha "owner/repo" "ref"}}` in a repository's `files` or `commits[].files` content. The loader checks the branch before the tag for a short ref. Use a full ref such as `refs/tags/v1.0.0` to select one directly. The loader peels an annotated tag and inserts the target commit SHA. The ref must exist before the file that uses it.

## `user`

| Field | Type | Required | Default |
| --- | --- | --- | --- |
| `id` | Integer | No | Allocated ID |
| `login` | String | Yes | None |
| `type` | `User` or `Bot` | No | `User` |
| `name`, `email`, `company`, `blog`, `location`, `bio`, `twitter_username` | String | No | None |
| `site_admin` | Boolean | No | `false` |
| `verified_email` | Boolean | No | `true` |
| `tokens` | List of token objects | No | `[]` |
| `tokens[].token` | String | No | `ghp_` plus 36 random hex digits |
| `tokens[].scopes` | List of strings | No | `repo`, `workflow`, `read:org`, and `user` |
| `tokens[].expires_at` | RFC 3339 timestamp | No | No expiry |

```yaml
- kind: user
  login: alice
  email: alice@example.com
- kind: user
  login: sam
```

## `org`

| Field | Type | Required | Default |
| --- | --- | --- | --- |
| `id` | Integer | No | Allocated ID |
| `login` | String | Yes | None |
| `name`, `email`, `description`, `company`, `blog`, `location`, `twitter_username` | String | No | None |
| `sso_enforced` | Boolean | No | `false` |
| `members` | List of member objects | No | `[]` |
| `members[].login` | String | Yes | None |
| `members[].role` | `member` or `admin` | No | `member` |

```yaml
- kind: org
  login: workshop
  members:
    - login: alice
      role: admin
```

## `repository`

| Field | Type | Required | Default |
| --- | --- | --- | --- |
| `id` | Integer | No | Allocated ID |
| `owner` | String | Yes | None |
| `name` | String | Yes | None |
| `description`, `homepage`, `language` | String | No | None |
| `topics` | List of strings | No | `[]` |
| `visibility` | `public`, `private`, or `internal` | No | `private` if `private: true`, `public` otherwise |
| `private` | Boolean | No | None. `visibility` has priority |
| `fork_of` | `owner/repo` string | No | None |
| `default_branch` | String | No | The parent's default branch for a fork, `main` otherwise |
| `archived`, `is_template`, `has_discussions`, `allow_auto_merge`, `delete_branch_on_merge` | Boolean | No | `false` |
| `has_issues`, `has_projects`, `has_wiki`, `allow_merge_commit`, `allow_squash_merge`, `allow_rebase_merge` | Boolean | No | `true` |
| `squash_merge_commit_title` | `PR_TITLE` or `COMMIT_OR_PR_TITLE` | No | `COMMIT_OR_PR_TITLE` |
| `squash_merge_commit_message` | `PR_BODY`, `COMMIT_MESSAGES` or `BLANK` | No | `COMMIT_MESSAGES` |
| `collaborators` | List of collaborator objects | No | `[]` |
| `collaborators[].login` | String | Yes | None |
| `collaborators[].permission` | `read`, `triage`, `write`, `maintain`, or `admin` | No | `write` |
| `files` | Map from file paths to strings | No | `{}` |
| `from_dir` | Directory path string | No | None |
| `branches` | List of branch names | No | `[]` |
| `tags` | List of tag objects | No | `[]` |
| `commits` | List of commit objects | No | `[]` |
| `sender` | User login string | No | The repository owner |

The loader resolves a relative `from_dir` from the directory of the seed file. Entries in `files` replace files at the same path from `from_dir`. The loader copies the parent's refs first for a fork. The top-level `tags` attach to the initial commit, and `branches` start at the default branch before the loader applies `commits`.

| Commit field | Type | Required | Default |
| --- | --- | --- | --- |
| `branch` | String | No | Default branch |
| `message` | String | No | `Update` |
| `files` | Map from paths to strings or `null` | No | `{}`. Set a path to `null` to delete it |
| `tags` | List of tag objects | No | `[]` |
| `author` | Object | No | `hubdouble <seed@hubdouble.local>` at `2026-01-01T00:00:00Z` |
| `author.name` | String | Yes, if `author` is present | None |
| `author.email` | String | Yes, if `author` is present | None |
| `author.date` | RFC 3339 timestamp | No | `2026-01-01T00:00:00Z` |

| Tag field | Type | Required | Default |
| --- | --- | --- | --- |
| `name` | String | Yes | None |
| `message` | String | No | None. Set a message to create an annotated tag |

The loader creates a lightweight tag if you omit the message.

```yaml
- kind: repository
  owner: workshop
  name: demo
  files:
    README.md: "# Demo\n"
  tags:
    - name: v1.0.0
      message: Version 1.0.0
  commits:
    - branch: add-guide
      author:
        name: Alice
        email: alice@example.com
      files:
        guide.md: "# Guide\n"
```

## `app`

The ID `15368` and the slug `github-actions` belong to the built-in GitHub Actions App. A seed or admin entry that uses either fails with `422`. An allocated App ID is never `15368`.

| Field | Type | Required | Default |
| --- | --- | --- | --- |
| `id` | Integer | No | Allocated ID |
| `slug` | String | Yes | None |
| `name` | String | No | App slug |
| `owner` | Account login string | Yes | None |
| `bot_id` | Integer | No | Allocated ID for `<slug>[bot]` |
| `description` | String | No | None |
| `external_url` | String | No | `https://github.com/apps/<slug>` |
| `client_id` | String | No | `Iv23` plus the App ID as 16 lowercase hex digits |
| `client_secret` | String | No | 40 random hex digits |
| `public_key` | PEM string | Yes | None |
| `private_key` | PEM string | No | None |
| `webhook` | Object | No | No webhook |
| `webhook.url` | URL string | Yes, if `webhook` is present | None |
| `webhook.secret` | String | No | None |
| `webhook.active` | Boolean | No | `true` |
| `permissions` | Map from permission names to `read`, `write`, or `admin` | No | `{}` |
| `events` | List of event names | No | `[]` |
| `device_flow` | Boolean | No | `false` |

```yaml
- kind: app
  slug: workshop-app
  owner: workshop
  public_key: '{{env "HUBDOUBLE_APP_PUBLIC_KEY"}}'
  permissions:
    contents: read
```

## `installation`

| Field | Type | Required | Default |
| --- | --- | --- | --- |
| `id` | Integer | No | Allocated ID |
| `app` | Integer App ID or string App slug | Yes | None |
| `account` | Account login string | Yes | None |
| `repository_selection` | `all` or `selected` | No | `all` for an empty repository list, `selected` otherwise |
| `repositories` | List of repository names or `owner/repo` strings | No | `[]` |
| `permissions` | Map from permission names to `read`, `write`, or `admin` | No | The App's permissions |
| `events` | List of event names | No | The App's events |
| `suspended` | Boolean | No | `false` |

Use `selected` to give an installation access to named repositories. The repositories must belong to the account. Each installation permission must be within the App's grant.

```yaml
- kind: installation
  app: workshop-app
  account: workshop
  repositories: [demo]
```

## `token`

| Field | Type | Required | Default |
| --- | --- | --- | --- |
| `user` | User login string | Yes | None |
| `token` | String | No | `ghp_` plus 36 random hex digits |
| `scopes` | List of strings | No | `repo`, `workflow`, `read:org`, and `user` |
| `expires_at` | RFC 3339 timestamp | No | No expiry |

```yaml
- kind: token
  user: alice
  scopes: [repo]
```

## `oauth`

| Field | Type | Required | Default |
| --- | --- | --- | --- |
| `default_user` | User login string | Yes | None |

```yaml
- kind: oauth
  default_user: alice
```

## `org_membership`

| Field | Type | Required | Default |
| --- | --- | --- | --- |
| `org` | Organization login string | Yes | None |
| `login` | User login string | Yes | None |
| `role` | `admin` or `member` | No | Current role, or `member` for a new membership |
| `state` | `active` or `pending` | No | Current state, or `active` for a new membership |
| `public` | Boolean | No | Current visibility, or `false` for a new membership |
| `sender` | User login string | No | The first active org admin, or the org if it has no active admin |

```yaml
- kind: org_membership
  org: workshop
  login: sam
```

## `team`

| Field | Type | Required | Default |
| --- | --- | --- | --- |
| `id` | Integer | No | Allocated ID |
| `org` (alias `organization`) | Organization login string | Yes | None |
| `name` | String | Yes | None |
| `slug` | String | No | Lowercase words from `name`, with hyphens between them |
| `parent` | Team slug string | No | No parent |
| `privacy` | `closed` or `secret` | No | `closed` |
| `description` | String | No | None |
| `notifications_enabled` | Boolean | No | `true` |
| `sender` | User login string | No | The first active org admin, or the org if it has no active admin |
| `members` | List of member objects | No | `[]` |
| `members[].login` | User login string | Yes | None |
| `members[].role` | `member` or `maintainer` | No | `member` |
| `repositories` | List of repository objects | No | `[]` |
| `repositories[].repository` (alias `name`) | Repository name or `owner/repo` string | Yes | None |
| `repositories[].permission` | `read`, `triage`, `write`, `maintain`, or `admin` | No | `read` |

A team member must be an active member of the org. Put a parent team before its children. Secret teams cannot have parents or children.

```yaml
- kind: team
  org: workshop
  name: Reviewers
  members:
    - login: sam
  repositories:
    - repository: demo
```

## `pull_request`

| Field | Type | Required | Default |
| --- | --- | --- | --- |
| `id` | Integer | No | Allocated ID |
| `repository` (alias `repo`) | `owner/repo` string | Yes | None |
| `user` (alias `author`) | User login string | Yes | None |
| `title` | String | Yes | None |
| `head` | Branch name or `owner:branch` string | Yes | None |
| `head_repo` | Repository name or `owner/repo` string | No | Inferred from `head` and the repository |
| `base` | Branch name string | Yes | None |
| `body` | String | No | None |
| `draft` | Boolean | No | `false` |
| `maintainer_can_modify` | Boolean | No | `true` |

```yaml
- kind: pull_request
  repository: workshop/demo
  user: alice
  title: Add guide
  head: add-guide
  base: main
```

A seeded pull request has its mergeability and test merge computed when it loads. One that the API opens, reopens, moves to another base, or gets a new head answers `mergeable: null` until reads complete the computation, as the [`mergeability`](#mergeability) kind sets.

## `mergeability`

Sets how many reads of a pull request in a repository answer `mergeable: null` after its mergeability goes stale. A read is `GET /repos/{owner}/{repo}/pulls/{pull_number}` or a GraphQL read of `PullRequest.mergeable` or `mergeStateStatus`. The read after the last of them answers the computed value.

| Field | Type | Required | Default |
| --- | --- | --- | --- |
| `repository` (alias `repo`) | `owner/repo` string | Yes | None |
| `reads` | Integer, `0` or more | Yes | `2` for a repository without this entry. `0` computes the test merge in the change itself, so no read answers `null` |

```yaml
- kind: mergeability
  repository: workshop/demo
  reads: 0
```

## `review`

| Field | Type | Required | Default |
| --- | --- | --- | --- |
| `id` | Integer | No | Allocated ID |
| `repository` | `owner/repo` string | Yes | None |
| `pull_number` | Integer | Yes | None |
| `author` | User login string | Yes | None |
| `body` | String | With `event: COMMENT` or `event: REQUEST_CHANGES` | Empty string |
| `event` | `APPROVE`, `REQUEST_CHANGES`, or `COMMENT` | No | None. The review stays pending |
| `commit_id` | Commit SHA string | No | Pull request head SHA |

```yaml
- kind: review
  id: 100
  repository: workshop/demo
  pull_number: 1
  author: sam
  event: COMMENT
  body: Please add a short example.
```

## `review_thread`

| Field | Type | Required | Default |
| --- | --- | --- | --- |
| `id` | Integer | No | Allocated ID |
| `review_id` | Integer | Yes | None |
| `comment` | Object | Yes | None |
| `comment.id` | Integer | No | Allocated ID |
| `comment.body` | String | Yes | None |
| `comment.path` | File path string | Yes | None |
| `comment.subject_type` | `file` or `line` | No | `line` |
| `comment.line` | Integer | For a line comment without `position` | None |
| `comment.position` | Integer | For a line comment without `line` | None |
| `comment.side` | `LEFT` or `RIGHT` | No | `RIGHT` for `line`, the side from the diff for `position` |
| `comment.start_line` | Integer | No | None |
| `comment.start_side` | `LEFT` or `RIGHT` | No | Same as `side` if `start_line` is present |

```yaml
- kind: review_thread
  review_id: 100
  comment:
    body: Add a short example to the guide.
    path: guide.md
    subject_type: file
```

## `issue_link`

Links a pull request to an issue by hand, as the issue's Development sidebar does on GitHub. The issue and the pull request are in the same repository. GraphQL `Issue.closedByPullRequestsReferences` lists the pull request unless `excludeUserLinked` is `true`.

| Field | Type | Required | Default |
| --- | --- | --- | --- |
| `repository` | `owner/repo` string | Yes | None |
| `issue_number` | Integer | Yes | None |
| `pull_number` | Integer | Yes | None |

```yaml
- kind: issue_link
  repository: workshop/demo
  issue_number: 2
  pull_number: 1
```

## `check_run`

| Field | Type | Required | Default |
| --- | --- | --- | --- |
| `id` | Integer | No | Allocated ID |
| `repository` | `owner/repo` string | Yes | None |
| `app` | App slug string | Yes | None |
| `name` | String | Yes | None |
| `head_sha` | Commit SHA or Git ref string | Yes | None |
| `details_url` | URL string | No | The App's external URL |
| `external_id` | String | No | Empty string |
| `status` | `queued`, `in_progress`, `completed`, `waiting`, `requested`, or `pending` | No | `queued`, or `completed` with a conclusion |
| `conclusion` | `action_required`, `cancelled`, `failure`, `neutral`, `success`, `skipped`, `stale`, `timed_out`, or `startup_failure` | With `status: completed` | None |
| `started_at` | RFC 3339 timestamp | No | Server time at seed load |
| `completed_at` | RFC 3339 timestamp | No | Server time at seed load for a completed run |
| `output` | Object | No | Empty output |
| `output.title`, `output.summary` | String | Yes, if `output` is present | None |
| `output.text` | String | No | None |
| `output.annotations` | List of up to 50 annotation objects | No | `[]` |
| `output.annotations[].path` | File path string | Yes | None |
| `output.annotations[].start_line`, `output.annotations[].end_line` | Integer | Yes | None |
| `output.annotations[].start_column`, `output.annotations[].end_column` | Integer | No | None |
| `output.annotations[].annotation_level` | `notice`, `warning`, or `failure` | Yes | None |
| `output.annotations[].message` | String | Yes | None |
| `output.annotations[].title`, `output.annotations[].raw_details` | String | No | None |
| `actions` | List of up to three action objects | No | `[]` |
| `actions[].label`, `actions[].identifier` | String of up to 20 characters | Yes | None |
| `actions[].description` | String of up to 40 characters | Yes | None |

```yaml
- kind: check_run
  repository: workshop/demo
  app: workshop-app
  name: Check docs
  head_sha: main
  conclusion: success
```

## `status`

| Field | Type | Required | Default |
| --- | --- | --- | --- |
| `id` | Integer | No | Allocated ID |
| `repository` | `owner/repo` string | Yes | None |
| `sha` | Commit SHA or Git ref string | Yes | None |
| `creator` | User or bot login string | Yes | None |
| `state` | `error`, `failure`, `pending`, or `success` | Yes | None |
| `target_url` | URL string | No | None |
| `description` | String | No | None |
| `context` | String | No | `default` |

```yaml
- kind: status
  repository: workshop/demo
  sha: main
  creator: alice
  state: success
```

## `actions`

This policy applies to the server.

| Field | Type | Required | Default |
| --- | --- | --- | --- |
| `auto_complete` | `none`, `success`, or `failure` | No | `none` |
| `oidc_signing_key` | PEM RSA private key with at least 2048 bits | No | An RSA key that the server generates on first use |

With `none`, a dispatch creates a queued run. Use `POST /_hubdouble/actions/runs/{id}` with the admin token to change the run state.

Use `POST /_hubdouble/actions/oidc/token` with the admin token to mint a token for a run. The discovery path is `/_actions/.well-known/openid-configuration`. The JSON Web Key Set (JWKS) path is `/_actions/.well-known/jwks`. A runner uses its bearer token at `/_actions/token`. If you set `HUBDOUBLE_OIDC_ISSUER`, use a URL that ends in `/_actions`.

```yaml
- kind: actions
  auto_complete: none
```

## `actions_secret` and `actions_variable`

| Field | Type | Required | Default |
| --- | --- | --- | --- |
| `repository` | `owner/repo` string | Exactly one of `repository` and `organization` | None |
| `organization` | Organization login string | Exactly one of `repository` and `organization` | None |
| `name` | String | Yes | None |
| `value` | String for a variable, `null` for a secret | For a variable | None |
| `visibility` | `all`, `private`, or `selected` | No | `private` for an organization, none for a repository |
| `repositories` | List of `owner/repo` strings | No | `[]` |

Use letters, digits, and underscores in a name. Start it with a letter or underscore. The server stores the name in uppercase. A secret has metadata only.

Set `visibility` only for organization entries. Set `repositories` only with `visibility: selected`. Each selected repository must belong to the organization.

```yaml
- kind: actions_secret
  repository: workshop/demo
  name: NPM_TOKEN
```

```yaml
- kind: actions_variable
  repository: workshop/demo
  name: AGENT_TRIGGER_PHRASE
  value: /review
```

## `actions_runner`

Add a self-hosted runner to an organization. Use an installation token with `organization_self_hosted_runners: read` to read the runner by ID.

| Field | Type | Required |
| --- | --- | --- |
| `organization` | Organization login string | Yes |
| `id` | Integer | Yes |
| `name`, `os`, `status` | String | Yes |
| `busy` | Boolean | Yes |
| `labels` | List of label objects | Yes |
| `runner_group_id` | Integer | No |
| `ephemeral` | Boolean | No |
| `version` | String or null | No |

A label has a string `name`, an optional integer `id`, and an optional `type` of `read-only` or `custom`.

```yaml
- kind: actions_runner
  organization: workshop
  id: 7011
  name: local-runner
  os: Linux
  status: online
  busy: true
  runner_group_id: 1
  ephemeral: true
  labels:
    - name: self-hosted
      type: read-only
```

## `release`

| Field | Type | Required | Default |
| --- | --- | --- | --- |
| `id` | Integer | No | Allocated ID |
| `repository` | `owner/repo` string | Yes | None |
| `tag_name` | String | Yes | None |
| `target_commitish` | Git ref or commit SHA string | No | Default branch |
| `name`, `body` | String | No | None |
| `draft`, `prerelease` | Boolean | No | `false` |
| `author` | Account login string | No | Repository owner |
| `assets` | List of asset objects | No | `[]` |
| `assets[].id` | Integer | No | Allocated ID |
| `assets[].name` | String | Yes | None |
| `assets[].label` | String | No | None |
| `assets[].content_type` | Media type string | No | `application/octet-stream` |
| `assets[].content` | String | No | Empty string |

For a release with `draft: false`, the loader creates the tag at `target_commitish` if the tag does not exist.

```yaml
- kind: release
  repository: workshop/demo
  tag_name: v1.0.0
  author: alice
```

## `environment`

| Field | Type | Required | Default |
| --- | --- | --- | --- |
| `id` | Integer | No | Allocated ID |
| `repository` | `owner/repo` string | Yes | None |
| `name` | String, unique in the repository without case | Yes | None |

```yaml
- kind: environment
  repository: workshop/demo
  name: production
```

## `ruleset`

| Field | Type | Required | Default |
| --- | --- | --- | --- |
| `id` | Integer | No | Allocated ID |
| `repository` | `owner/repo` string | Exactly one of `repository` and `org` | None |
| `org` | Organization login string | Exactly one of `repository` and `org` | None |
| `name` | String | Yes | None |
| `target` | `branch`, `tag`, or `push` | No | `branch` |
| `enforcement` | `disabled`, `active`, or `evaluate` | No | `active` |
| `bypass_actors` | List of bypass actor objects | No | `[]` |
| `bypass_actors[].actor_id` | Integer | No | None |
| `bypass_actors[].actor_type` | `Integration`, `OrganizationAdmin`, `RepositoryRole`, `Team`, or `DeployKey` | Yes | None |
| `bypass_actors[].bypass_mode` | `always`, `pull_request`, or `exempt` | No | `always` |
| `conditions` | JSON value | No | None |
| `rules` | List of rule objects | No | `[]` |
| `rules[].type` | String | Yes | None |
| `rules[].parameters` | JSON value | No | None |

```yaml
- kind: ruleset
  repository: workshop/demo
  name: Protect main
  conditions:
    ref_name:
      include: ["~DEFAULT_BRANCH"]
      exclude: []
  rules:
    - type: pull_request
```

## Kubernetes example

Make a copy of the [Secret template](../deploy/kubernetes/secret.yaml). Set the PEM public key for App `1000001`. Set the webhook secret to the value that the client uses. Set the user token to `ghp_` followed by 36 hex digits. Set the admin token to a random value.

Apply your Secret and the other manifests in the same namespace.

```sh
kubectl apply -f /path/to/your/secret.yaml
kubectl apply -f deploy/kubernetes/configmap.yaml -f deploy/kubernetes/deployment.yaml -f deploy/kubernetes/service.yaml
```

Keep the [ConfigMap](../deploy/kubernetes/configmap.yaml) in sync with [the example seed](../examples/client.yaml).
