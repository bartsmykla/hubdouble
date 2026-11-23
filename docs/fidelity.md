# GitHub API fidelity

hubdouble implements the GitHub operations in the [client inventory](inventory/rest-routes.txt). A known REST operation without a handler returns `501`. An unknown REST path returns `404`. See the [implementation plan](PLAN.md#47-fidelity-rules-every-track-follows) for the compatibility contract.

## Differences that clients can see

| Area | hubdouble behavior |
| --- | --- |
| Webhook transport | The server accepts an `https://` webhook URL, but the delivery attempt fails. The receiver gets no request. The attempt log has `status_code: 0`. Use an `http://` receiver for local tests. |
| Webhook queue limit | When 10 000 events or 32 MiB of payload wait for a delivery attempt or are in one, a new event gets no attempt. Its delivery record has `status_code: 0` and the status `Queue full`. Redeliver it when the receiver is back. If more than 8 MiB of such events wait to be logged, the oldest of them are logged without their request, as the webhook delivery retention row says. |
| Webhook response body | The delivery record keeps only the first 4 KiB of the receiver's answer body in `response.payload`. hubdouble does not read the rest. GitHub documents no limit. |
| Webhook delivery retention | GitHub keeps deliveries for 3 days and then removes them. hubdouble keeps the request and response of the newest deliveries of all Apps up to 8 MiB in total. An older delivery stays in `GET /app/hook/deliveries` with all its fields. A read of it with `GET /app/hook/deliveries/{id}` and a redelivery of it return `404`, as for a delivery that GitHub no longer keeps. |
| Release asset download | A download with `Accept: application/octet-stream` answers `302` to an unguessable `/_blobs/` URL on hubdouble, after the access check. GitHub redirects to a signed URL that expires after a few minutes. The hubdouble URL does not expire. |
| Actions job log download | After the access check, hubdouble redirects to an unguessable `/_blobs/` URL with no time limit. GitHub's URL expires after one minute. |
| Tree deletion of an absent path | GitHub's docs say that `POST /repos/{owner}/{repo}/git/trees` returns an error when an entry with `sha: null` deletes a file that does not exist, but they give no status or message. hubdouble answers `422` with a validation error on `tree.sha`. |
| Git tree depth | GitHub recommends a maximum of 50 directory levels. hubdouble enforces the limit to protect recursive tree operations. The Git Trees API returns `422` for a deeper tree. |
| Webhook ID | A webhook payload or delivery record with a `hook_id` uses the App ID as that value. The `X-GitHub-Hook-ID` header also uses the App ID. |
| Admin writes | Most seed kinds have no direct update or delete route. An unsupported admin method and path return `404`. See the [admin route list](admin-api.md). |
| Repository renames | The old owner and name return `404` through REST, with no `301` redirect. GraphQL accepts the schema's `followRenames` argument but has no rename lookup. An old name returns a null repository with `NOT_FOUND`, regardless of that argument. |
| Repository permissions | Standard REST repository responses omit `permissions` for anonymous callers, App JWTs, and installation tokens. User and personal tokens can receive the viewer's `permissions`. The team repository media type reports the team's permissions. |
| OAuth client authentication | `POST /login/oauth/access_token` reads `client_id` and, for grants that require it, `client_secret` from the body. An HTTP Basic header has no effect. Basic credentials alone return the OAuth error `incorrect_client_credentials`. |
| Private team repositories | A classic personal token with `admin:org` but without `repo` gets `404` when it checks access to a private team repository. |
| Search types | Only GraphQL issue search, which also returns pull requests, has a handler. `ISSUE`, `ISSUE_ADVANCED`, and `ISSUE_HYBRID` use the same search. Other GraphQL search types return `HUBDOUBLE_NOT_IMPLEMENTED`. REST search operations return `501`. |
| Negated search qualifiers | A qualifier such as `-author:octocat` acts as a text term. It does not exclude matches. |
| Search in comments | Text search checks titles and bodies. `in:comments` alone with a text term finds no matches. If you also specify `title` or `body`, the search uses those fields. |
| Search order | `sort:created` and `sort:updated` support ascending and descending order. Sorts by comments, reactions, or interactions have no effect. The default order is creation time, newest first. |
| Pull request state order | `Issue.closedByPullRequestsReferences` accepts `orderByState`. The argument has no effect. The [GitHub schema](https://docs.github.com/en/graphql/reference/issues#issue) gives no sort direction, and client omits the argument. |

GitHub documents redirect responses for the [repository endpoint](https://docs.github.com/en/rest/repos/repos#get-a-repository) and more qualifiers for [issue and pull request search](https://docs.github.com/en/search-github/searching-on-github/searching-issues-and-pull-requests).

## REST, IDs, and commits

- Errors use `message`, `documentation_url`, and a string `status`. A validation error can include `errors` entries with `resource`, `field`, and `code`.
- Page-based lists accept `page` and `per_page` (default 30, maximum 100). Responses include `Link` when there are other pages. Operations with cursors, such as App deliveries, use cursor pagination. An empty list is `[]`.
- Successful API GET responses use a weak `ETag` derived from the media type and response body. A match with `If-None-Match` returns `304`.
- API responses include GitHub request, media type, version, and rate-limit headers. Rate-limit counters are fixed and the remaining count stays above zero unless a fault overrides it. User tokens can receive `X-OAuth-Scopes`.
- The API accepts `X-GitHub-Api-Version` values `2022-11-28` and `2026-03-10`. It defaults to `2022-11-28` and returns `400` for other versions.
- New object IDs use a server-wide signed 64-bit sequence. An explicit admin ID at or below that sequence returns `422`. Current GraphQL node IDs have a type prefix and an opaque payload. For a resolved legacy base64 ID, GraphQL returns a `DEPRECATION` warning in `extensions` with `next_global_id`.
- Timestamps use RFC 3339 in UTC with second precision. Reads of an unchanged Git ref return the same target.
- The App delivery log retains the last 50,000 attempts per App.
- Merge commits and commits from `createCommitOnBranch` use committer `GitHub <noreply@github.com>` and the local web-flow key. Installation-token Git data commits without an author, committer, or signature use the same key. They report `verification.verified: true` and reason `valid`. Client-signed Git data commits report `unknown_key`. Other unsigned commits report `unsigned`.

## GraphQL

- The validation exception in the plan allows fields with the same response name to differ in nullability on mutually exclusive concrete types. The `originalCommentEdit` inventory document uses this for `pullRequest` on `IssueComment` and `PullRequestReviewComment`.
- A null from a failed non-null field propagates to the nearest nullable parent. GraphQL errors give a `type`, and execution errors identify the field with a `path` and source `locations`.
- Connections require `first` or `last`, with a maximum of 100. An unimplemented field returns `HUBDOUBLE_NOT_IMPLEMENTED` on its path. A repository the caller cannot see returns null with `NOT_FOUND` for that alias.

## Test controls

Use [admin faults](admin-api.md#state-request-log-and-faults) to substitute a response, such as a secondary rate limit, `502`, or an `X-GitHub-SSO` header. The rule controls the match, status, body, headers, delay, and count.

Workflow dispatch creates a run and its jobs. The double simulates run state and can issue a run token. It does not execute workflow jobs. Use the Actions admin routes or the seed's `auto_complete` option to control run state.
