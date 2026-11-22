# Admin API

Send these requests to the hubdouble API base URL. Add `Authorization: Bearer <admin token>` to every request. The `token` authorization scheme also works. See [Configuration](configuration.md) for the token.

Send JSON for a request with a body. The tables list the success status and response. A `204` response has an empty body. An error response contains `message`, `documentation_url`, and a string `status`. It can also contain `errors`. A missing or invalid admin credential returns `401`. An unknown route or resource returns `404`. Malformed JSON returns `400`, and a body that fails validation usually returns `422`.

## Seed objects

Use `POST /_hubdouble/{kind}` with one JSON object. Use the fields for that kind from the [seed reference](seed.md). Omit the `kind` field because the path supplies it. The response is `201 {}`.

| Path suffix after `/_hubdouble/` | Request body |
| --- | --- |
| `user`, `org`, `repository` | [User](seed.md#user), [organization](seed.md#org), or [repository](seed.md#repository) fields |
| `app`, `installation`, `token`, `oauth` | [App](seed.md#app), [installation](seed.md#installation), [token](seed.md#token), or [OAuth](seed.md#oauth) fields |
| `org_membership`, `team` | [Membership](seed.md#org_membership) or [team](seed.md#team) fields |
| `pull_request`, `review`, `review_thread` | [Pull request](seed.md#pull_request), [review](seed.md#review), or [thread](seed.md#review_thread) fields |
| `check_run`, `status` | [Check run](seed.md#check_run) or [commit status](seed.md#status) fields |
| `actions`, `actions_secret`, `actions_variable` | [Actions policy](seed.md#actions) or [secret and variable](seed.md#actions_secret-and-actions_variable) fields |
| `release`, `ruleset` | [Release](seed.md#release) or [ruleset](seed.md#ruleset) fields |

An explicit ID must exceed the current ID sequence. Replace startup environment templates with their values in admin requests. Some creates emit webhook events. Startup seed application emits none.

## State, request log, and faults

| Method and path | Request | Response |
| --- | --- | --- |
| `POST /_hubdouble/reset` | No body | `204`. Restores the startup snapshot and clears the request log. |
| `POST /_hubdouble/hooks/drain` | No body | `204` after the webhook queue drains. |
| `GET /_hubdouble/log` | Optional unsigned integer `after` in the query | `200` with `{"entries":[...],"cursor":number}`. Return entries with a cursor greater than `after`. |
| `GET /_hubdouble/log/wait` | Optional `after` and repeated `match` query parameters | `200` with the first matching entry. Hold the request open if no entry matches. |
| `POST /_hubdouble/faults` | A fault object, described below | `201` with `{"id":number}`. |
| `GET /_hubdouble/faults` | No body | `200` with an array of fault objects, each with its `id` and remaining `count`. |
| `DELETE /_hubdouble/faults` | No body | `204`. Removes all fault rules. |

The log keeps the last 50,000 records. Each has a `cursor` and a `kind`:

- `"request"`: an API request, with `method`, `path`, `operation_id`, `actor` (login or null), and `status`. The wait route looks for every `match` string in the space-separated method, path, operation ID, status, and actor.
- `"event"`: an event that got a delivery ID for one App, with `app_id`, `delivery_id`, `event`, and `action` (or null). The wait route matches against `event`, then `event.action`, the App ID and the delivery ID, for example `event issues.opened`.
- `"delivery"`: a finished attempt that the delivery list now shows, with `app_id`, `delivery_id`, `event`, `action`, and `status_code` (`0` when no answer came). The wait route matches against `delivery`, then `event.action`, the status code, the App ID and the delivery ID, for example `delivery issues.opened 200`. Omit `after` to start at the oldest available entry. Both routes return `410` for a cursor older than the window. A reset also returns `410` to a wait request that is open across the reset.

A fault requires `status` from 100 through 599. Use optional `method` (case insensitive), `path` (a pattern with `*`), or `operation` (a REST operation ID or GraphQL operation name). All supplied match fields must match. Optional fields are `body` (raw string or JSON), `headers` (object of string values), `delay_ms` (default 0), and `count` (omit for no limit). The first applicable rule replaces the normal response. A positive count decreases after each match and the rule expires at zero. A rule with an initial count of zero never matches.

## Users, Apps, and installations

| Method and path | Request body | Response |
| --- | --- | --- |
| `GET /_hubdouble/user/{login}` | None | `200` with `id`, `node_id`, `login`, `type`, and `tokens`. Each token has `id`, `token`, `kind`, `scopes`, `expires_at`, and `revoked`. |
| `GET /_hubdouble/app/{slug}` | None | `200` with `id`, `slug`, `node_id`, `client_id`, `client_secret`, `public_key`, `private_key`, `webhook`, and `bot` (object with `id` and `login`, or null). |
| `PATCH /_hubdouble/app/{slug}` | Optional `name`, `permissions` map, `events` list, and `webhook` object. The webhook requires `url` and accepts `secret` and `active` (default true). | `200` with `{"id":number}`. |
| `DELETE /_hubdouble/app/{slug}` | None | `204`. Removes the App and revokes its tokens. Returns `409` if the App still has installations. |
| `DELETE /_hubdouble/app/{slug}/authorizations/{login}` | None | `204`. Revokes the user's App user and refresh tokens and emits the authorization event. |
| `GET /_hubdouble/installation/{id}` | None | `200` with the GitHub installation object, including the App, target account, selection, permissions, events, and suspension fields. |
| `PATCH /_hubdouble/installation/{id}` | Optional `suspended` boolean, `repository_selection` (`all` or `selected`), `repositories` list of repository names, and `permissions` map. | `200` with the updated installation object. A repository list without an explicit selection implies `selected`. |
| `DELETE /_hubdouble/installation/{id}` | None | `204`. Removes the installation and revokes its tokens. |
| `PATCH /_hubdouble/installation-target/{login}` | Required new `login` | `200` with `id` and the new `login`. |
| `POST /_hubdouble/oauth/device/{user_code}` | Optional `login` and `approve` (default true). If you omit `login`, approval uses the configured default OAuth user. | `204`. Approves or denies the device request. |

The App routes accept a numeric App ID in place of the slug. App and user reads can return credentials.

## Repositories, organizations, and teams

`permission` accepts `read`, `triage`, `write`, `maintain`, or `admin`. `sender` names the account to attribute to the change or event.

| Method and path | Request body | Response |
| --- | --- | --- |
| `PATCH /_hubdouble/repos/{owner}/{repo}` | Optional `sender`, `name`, `owner`, `description`, `homepage`, `default_branch`, `topics`, `archived`, `visibility`, and `private`. | `200` with the updated repository object. |
| `DELETE /_hubdouble/repos/{owner}/{repo}` | Optional `sender` | `204`. Removes the repository. |
| `PATCH /_hubdouble/releases/{release_id}` | Optional `sender`, `tag_name`, `target_commitish`, `name`, `body`, `draft`, and `prerelease`. | `200` with the updated release object. |
| `GET /_hubdouble/org/{org}/members/{login}` | None | `200` with the membership object, including `url`, `organization_url`, `organization`, `user`, `role`, and `state`. |
| `PUT /_hubdouble/org/{org}/members/{login}` | Optional `sender`, `role` (`member` or `admin`), `state` (`active` or `pending`), and `public` (boolean). A new membership defaults to `member`, `active`, and private. | `204`. Creates or updates the membership. |
| `DELETE /_hubdouble/org/{org}/members/{login}` | Optional `sender` | `204`. Removes the membership. |
| `PUT /_hubdouble/repository/{owner}/{repo}/collaborators/{login}` | Required `permission` and optional `sender` | `204`. Sets the collaborator's permission. |
| `DELETE /_hubdouble/repository/{owner}/{repo}/collaborators/{login}` | Optional `sender` | `204`. Removes the collaborator. |
| `GET /_hubdouble/team/{org}/{slug}` | None | `200` with the stored team, including `id`, `nodeId`, `orgId`, `name`, `slug`, `description`, `privacy`, `notificationsEnabled`, `parentId`, `members`, `repos`, `createdAt`, and `updatedAt`. |
| `PATCH /_hubdouble/team/{org}/{slug}` | Optional `name`, `slug`, `description`, `parent` (team slug), `privacy` (`closed` or `secret`), `notifications_enabled`, and `sender`. Set `description` or `parent` to null to clear it. | `204`. Updates the team. |
| `DELETE /_hubdouble/team/{org}/{slug}` | Optional `sender` | `204`. Removes the team and unsets it as the parent of its direct children. |
| `PUT /_hubdouble/team/{org}/{slug}/members/{login}` | Optional `role` (`member` by default, or `maintainer`) and `sender` | `204`. Sets the team member's role. |
| `DELETE /_hubdouble/team/{org}/{slug}/members/{login}` | Optional `sender` | `204`. Removes the team member. |
| `PUT /_hubdouble/team/{org}/{slug}/repos/{owner}/{repo}` | Required `permission` and optional `sender` | `204`. Sets the team's repository permission. |
| `DELETE /_hubdouble/team/{org}/{slug}/repos/{owner}/{repo}` | Optional `sender` | `204`. Removes the team's repository permission. |

## Checks and Actions

| Method and path | Request body | Response |
| --- | --- | --- |
| `POST /_hubdouble/check-runs/{id}/requested-action` | Required `identifier` and `sender` (login) | `204`. Emits the requested action event for that check run. |
| `GET /_hubdouble/actions` | None | `200` with `auto_complete` (`none`, `success`, or `failure`). |
| `POST /_hubdouble/actions/runs/{id}` | Optional `status`, `conclusion`, and `jobs` (job update objects). See the fields below. | `200` with the updated workflow run object. |
| `POST /_hubdouble/actions/oidc/token` | Required numeric `run_id`. Optional numeric `job_id`, string `audience`, and `claims` object. | `200` with `token` (JWT), `request_token` (runner credential), and `request_url` (the runner endpoint). Use the runner credential as a bearer token at that URL. |

Each job update requires numeric `id`. Optional fields are `status`, `conclusion`, `steps`, `logs` (text), `labels` (string array), `runner_id`, and `runner_name`. Each step requires `number`, `name`, and `status`. It can also have `conclusion`, `started_at`, and `completed_at`. A completed run or job requires a conclusion.

An OIDC claim override must name a claim that the token already has. Use integers for time claims and strings for the other claims. A supplied `job_id` must belong to the current run attempt.

Each path below supports `GET` and `DELETE`. Both methods take no body. `GET` returns `200` with `name`, `created_at`, and `updated_at`. Variables also return `value`. `DELETE` removes the named entry and returns `204`.

| Paths |
| --- |
| `/_hubdouble/actions/repos/{owner}/{repo}/secrets/{name}` |
| `/_hubdouble/actions/repos/{owner}/{repo}/variables/{name}` |
| `/_hubdouble/actions/orgs/{owner}/secrets/{name}` |
| `/_hubdouble/actions/orgs/{owner}/variables/{name}` |
