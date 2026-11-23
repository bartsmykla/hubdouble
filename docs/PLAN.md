# hubdouble plan

hubdouble is a local stand-in for GitHub: one small Rust binary that serves the GitHub REST API, the GraphQL API, App webhooks, OAuth sign-in and the Actions OIDC issuer closely enough that a GitHub App can run end to end against it. The first client is `octo-org/client`, called the client below. Everything the client calls must work. Everything else in GitHub's published API is routed and answers `501` until a later change implements it.

This plan is written for a swarm of agents. Phase 0 builds the frame and freezes the contracts in section 6. Phase 1 runs about a dozen agents in parallel, one per track, each in its own worktree and branch, each owning a disjoint set of crates and files.

hubdouble is written in Rust. Rounds 1 to 12 of the council review approved the behaviour (sections 1, 2 and 4) on a Go draft; from round 13 the plan is the Rust one, and sections 3, 5, 6, 7, 8, 9, 10 and 11 were rewritten for it. The client is still a Go program that calls GitHub through go-github v93, so the client behaviour the plan cites (escaping, headers, how a call reads an answer) is go-github's.

## 1. Goals

- **G1 Coverage of the client.** Every REST route in [docs/inventory/rest-routes.txt](inventory/rest-routes.txt), every GraphQL document in [docs/inventory/graphql/](inventory/graphql/), every webhook event in section 4.3, every response variant in section 4.8, the OAuth web, device and SSO flows, App JWT and installation tokens, and the Actions OIDC issuer work as GitHub does. A conformance suite (section 8) proves it, and CI fails when an inventory item does not pass.
- **G2 Fidelity.** Status codes, error bodies, headers, pagination, JSON shapes (including explicit `null`s), node IDs, permission checks and webhook payloads match GitHub. Responses are validated in tests against GitHub's OpenAPI description, GraphQL documents against GitHub's schema with GitHub's observed validation rules (section 4.7), and webhook payloads against `@octokit/webhooks` schemas.
- **G3 Room to grow to all of GitHub.** The full OpenAPI operation table and the full GraphQL schema are built in from day one. Adding an operation is one handler and one registration line, and `hubdouble coverage` reports the share implemented.
- **G4 Light and fast.** One static binary, no git binary, no database, pure Rust apart from the allocator (section 3). Targets in section 9.
- **G5 Scriptable.** A seed file and an admin API create every object a test needs without a UI. Tests wait on events (a drained delivery queue), never on time.
- **G6 Runs in Kubernetes as-is.** Plain HTTP on port 8000 behind a Service, configured by environment variables.

### 1.1 Hard requirements from the client's local end-to-end setup

The client runs against the hubdouble image on a local Kubernetes cluster (OrbStack first, also kind, k3d and minikube). These are acceptance criteria next to G1:

| # | Requirement | Where |
| --- | --- | --- |
| E1 | REST at `/` and `/api/v3/`, GraphQL at `/graphql` and `/api/graphql`, OAuth web, device and SSO flows, all on one HTTP port | 4.1, 4.4 |
| E2 | App JWT (RS256, `iss` the App ID or client ID) checked against a public key the seed supplies; the private key may stay with the client only | 4.2, 4.6 |
| E3 | `POST /app/installations/{id}/access_tokens` honours, enforces and returns `repositories` and `permissions`; `DELETE /installation/token` revokes; `GET /app`, `/app/installations`, `/repos/{o}/{r}/installation`, `/orgs/{o}/installation` | 4.2, T1 |
| E4 | App webhook URL and secret from the seed; every event on an installed repository delivered there, signed, with GitHub's headers and the `installation` object; `GET /app/hook/deliveries` and redelivery | 4.3, T1 |
| E5 | dispatched workflows become runs the client can read; OIDC discovery and JWKS; an admin call that mints a token for a given run | 4.5, T9 |
| E6 | declarative seed from a mounted file, and an admin call that resets the state to the seed | 4.6 |
| E7 | a script acts as any seeded user (opens a PR, comments `/agent review`, reviews) through the normal API with that user's personal token, with no browser | 4.6 |
| E8 | one static multi-arch image, plain HTTP, no TLS, no external database, `/healthz` and `/readyz`, ready in under a second, low memory, published to `ghcr.io/bartsmykla/hubdouble` and buildable with `docker build` | 4.1, 9, T12 |
| E9 | `POST .../actions/workflows/{id}/dispatches` accepts `return_run_details: true` and then answers `200` with `workflow_run_id`, `run_url` and `html_url` (the client's `internal/dispatch/dispatch.go:562`); without it, `204` | 4.5, T9 |
| E10 | the seed may set the ID of any user, org, repository, App and installation, so it can use the real IDs the client pins (octo-org is 962416 in the client's `helm/client/values.yaml` `firstPartyOwners`); the ID sequence starts above the largest seeded ID | 4.6, 6.1 |

### 1.2 Prerequisite outside this plan

The client hard-codes `https://api.github.com` (`internal/ghapp/ghapp.go:44`), `https://github.com` (`internal/dispatch/accounts.go:48`) and the Actions issuer (`internal/oidc/oidc.go:36`, checked at `:105`). The client's local end-to-end work (branch `feat-funny-local-agent-e2e`) adds `CLIENT_GITHUB_DOUBLE` (`http://hubdouble:8000` in the local cluster) and the Actions issuer override, refused outside `CLIENT_ENVIRONMENT=Local`. That change is owned by the client's session, not by this plan. hubdouble's own conformance suite does not depend on it: it drives hubdouble with the requests go-github v93 makes, pointed at the test server (section 8). Only the phase 2 Kubernetes smoke test with the real client service waits for it. The same change decides whether the client's checks that a link starts with `https://github.com/` follow the double; the client's session decided to keep those checks, so the example sets `HUBDOUBLE_WEB_URL=https://github.com`. The same client change adds `mise run e2e:up` and the local smoke that phase 2's real-service step runs; that step waits for this change to merge. Section 4.10 is the contract those tasks are written against.

## 2. Non-goals for v1

- A web UI. The only pages are the OAuth authorize, SSO and device pages, and each can be skipped with query parameters.
- Git over HTTP or SSH (clone, fetch, push). The client does not use it. The object store in section 6.4 can serve it later (section 11).
- Squash and rebase merges (`PUT /pulls/{n}/merge` with those methods answers `422` naming the limit) and line-level (diff3) content merges. The client merges branches through `POST /repos/{o}/{r}/merges` (`Repositories.Merge`, `internal/gitwrite/tree.go:327`); PRs become merged as section 6.4 says.
- Persistence across restarts. State lives in memory; reset goes back to the seed.
- REST search. The client searches only through GraphQL.
- GitHub Enterprise admin APIs, billing, Copilot, Packages, Pages, Codespaces, Projects v2, Discussions.
- Running Actions workflows. hubdouble records dispatches and lets a test or a fake runner drive a run's lifecycle.
- Multi-node operation.
- Rate limit enforcement beyond static headers and fault injection (section 4.7).

## 3. Decisions

Every crate is pinned to its latest stable version when P0-A adds it, and kept current.

| Area | Decision | Why |
| --- | --- | --- |
| Language | Rust, latest stable at start (pinned in `rust-toolchain.toml`), edition 2024, one Cargo workspace (section 5) | static binary, small memory, no garbage collector pauses, data races ruled out by the compiler |
| Runtime and HTTP | `tokio` (multi-thread runtime, worker count from `HUBDOUBLE_WORKERS`, default the CPU count) and `hyper` 1 with `hyper-util` (HTTP/1.1 and h2c), no web framework; a router generated from the OpenAPI operation table (section 6.2) | a framework router cannot hold GitHub's table: `GET .../issues/comments/{comment_id}` and `GET .../issues/{issue_number}/events` both match `.../issues/comments/events`, and GitHub has parameters that hold slashes (`git/ref/{ref}`, `contents/{path}`, `branches/{branch}`, `compare/{basehead}`) |
| Handlers | REST handlers and GraphQL resolvers are plain synchronous functions that run inside one store transaction; only admin long polls, drain, `/_blobs` streaming and webhook delivery are `async` | a request's reads see one consistent state, and no lock is held across an `.await` |
| State | in memory, one `parking_lot::RwLock`, typed tables with indexes, entities held as `Arc` and replaced on write, an undo log for rollback | fastest and simplest for one local process; readers keep a snapshot without a copy |
| Git | `gix-object` and `gix-hash` to encode, decode and hash objects, hubdouble's own in-memory object store and ref tables, `imara-diff` for patches, hubdouble's own tree-level three-way merge | no git binary and no repository on disk; gitoxide's object crates are maintained and pure Rust |
| Commit signing | `pgp` (rPGP) with an Ed25519 web-flow key | GitHub signs web-flow commits with OpenPGP |
| REST spec | `github/rest-api-description`, pinned tag, OpenAPI 3.0 `api.github.com` bundle | source of the operation table and of response schemas for tests |
| GraphQL | `apollo-compiler` to parse GitHub's `schema.docs.graphql` (pinned) and to parse and validate documents, with GitHub's exceptions (section 4.7) applied by diagnostic kind and narrowed to the exact case (a `ConflictingFieldType` is dropped only when the two fields sit in distinct concrete types and differ only in nullability; every other type conflict stays an error; the validator has no per-rule switch, so `gql` runs full validation, removes only those diagnostics, and wraps the document with `Valid::assume_valid` when nothing else remains); hubdouble's own small executor with a resolver registry | `async-graphql` and `juniper` need a Rust type for every schema type, unworkable on GitHub's whole schema; a registry turns an unimplemented field into a per-field error |
| JSON | `serde` (with its `rc` feature, so `Arc` entities serialize) and `serde_json`, hand-written render structs per resource; `Option<T>` serializes as `null`, and a field is skipped only where the schema marks it optional | GitHub sends `null` for required nullable fields; a `Vec` always renders `[]`; tests catch drift |
| Crypto | RustCrypto (`rsa`, `sha1`, `sha2`, `hmac`) and `jsonwebtoken` on its RustCrypto backend | App JWT, OIDC tokens and webhook signatures without C code |
| Seed format | `serde-saphyr` for YAML, `serde_json` for JSON | `serde_yaml` is archived |
| Allocator | `mimalloc` | musl's allocator is slow under threads; the only C code in the binary |
| Test tools | `cargo nextest`, `jsonschema` for responses (OpenAPI 3.0 schemas converted to JSON Schema in the test kit) and webhook payloads (`@octokit/webhooks` schemas pinned), `reqwest` with rustls as the HTTP client of the conformance suite, `criterion` for benchmarks | named once so every agent uses the same |
| Lint | `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings` with the workspace lint table (section 7), `cargo deny check` (licenses, advisories, duplicates) | every advisory fails the build; the one allowed exception is RUSTSEC-2023-0071 in `rsa`, if the pinned version still carries it, ignored by ID in `deny.toml` with its reason: it is a timing side channel, and hubdouble signs only with its own test keys in a local double |
| Config | environment variables only | Kubernetes friendly |
| Tasks | `mise` for tools, tasks in `tasks.toml` (`mise run check`, `test`, `lint`, `build`, `bench`, `gen`) | the habits of the user's other projects |
| Image | static musl binary on the newest `gcr.io/distroless/static` Debian image with the `nonroot` tag, pinned by digest, multi-arch, `ghcr.io/bartsmykla/hubdouble` | smallest image with CA roots and a non-root user |
| License | MIT | |

## 4. Behaviour

### 4.1 Process and URLs

One listener, `HUBDOUBLE_ADDR` (default `:8000`). `HUBDOUBLE_BASE_URL` (default `http://localhost:8000`) is the origin written into every URL in a response or payload, except the resource links below. `HUBDOUBLE_WEB_URL` (default the base URL) is the origin of `html_url` and other links to resources only. Pages hubdouble serves itself always use `HUBDOUBLE_BASE_URL`: `/login/oauth/*`, `/login/device` and the device flow's `verification_uri`, `/orgs/{org}/sso` and the `url=` in `X-GitHub-SSO`, so a person signing in locally never lands on real GitHub (the client shows `verification_uri` to people, `internal/dispatch/accounts.go:91-109`). Set `HUBDOUBLE_WEB_URL` to `https://github.com` when a client checks that links are GitHub's, as the client's draft notice does (`internal/report/surfaces.go:152`). API URLs in answers and payloads (`url`, `issue_url`, `pull_request.url`, `repository.url` and the like) always use `HUBDOUBLE_BASE_URL` with the API prefix the request came in on, because clients compare them with their API base (the client's `internal/dispatch/request_original.go:199`). `HUBDOUBLE_OIDC_ISSUER` overrides the issuer, which is `HUBDOUBLE_BASE_URL + "/_actions"` by default. An override may change the origin only: its path must end in `/_actions`, where hubdouble serves discovery and JWKS, and any other path fails at start.

| Path | Serves |
| --- | --- |
| `/` and `/api/v3/` | REST API |
| `/graphql`, `/api/graphql` and `/api/v3/graphql` | GraphQL (go-github resolves `NewRequest("graphql")` against its base URL, so a base of `/api/v3/` sends it to `/api/v3/graphql`) |
| `/login/oauth/authorize`, `/login/oauth/access_token`, `/login/device/code`, `/login/device`, `/orgs/{org}/sso` | OAuth and SSO |
| `/_actions/.well-known/openid-configuration`, `/_actions/.well-known/jwks`, `/_actions/token` | Actions OIDC issuer |
| `/_blobs/{id}` | redirect targets for job logs and release assets, with `HEAD` and `Range` |
| `/healthz`, `/readyz` | liveness and readiness (GitHub has no route there) |
| `/_hubdouble/` | admin API, metrics |

### 4.2 Authentication and authorization

| Kind | How it is made | Rules |
| --- | --- | --- |
| App JWT | client signs RS256 with the App's private key | `iss` is the App ID or client ID; refused when `exp` is past, when `exp` is more than 10 minutes after the server's now, or when `iat` is more than 60 s after now ([GitHub](https://docs.github.com/en/apps/creating-github-apps/authenticating-with-a-github-app/generating-a-json-web-token-jwt-for-a-github-app)) |
| Installation token `ghs_` | `POST /app/installations/{id}/access_tokens` | 1 h expiry; `repositories`, `repository_ids` and `permissions` narrow it and are returned; asking beyond the installation's grant is `422`; `DELETE /installation/token` revokes it |
| User token `ghu_`, refresh `ghr_` | OAuth web or device flow | 8 h expiry, refresh 6 months, refresh rotates both |
| Personal token `ghp_` | seed or admin API | a script acting as a user |
| Admin token | `HUBDOUBLE_ADMIN_TOKEN` (default `hubdouble-admin`) | admin API only |

One choke point decides every REST request: the middleware resolves the actor, then checks the operation's `Requirement` (section 6.2) against the actor's grant and the target repository. A repository the actor cannot see is `404`. A missing permission is `403` with `X-Accepted-GitHub-Permissions`. An implemented operation without a `Requirement` fails a test that walks the registry, so access is denied by default.

GraphQL has the same check at one place too: every resolver reaches a repository, org or node only through `gql::Ctx::load` (P0-B, backed by P0-D), which applies the actor's grant per object. A hidden repository in one alias answers `null` with `NOT_FOUND` for that alias while the other aliases answer; `node` and `nodes` answer `null` for hidden objects; search drops them. A mutation checks write access on its target before any change inside its `Update`, so a denied mutation changes nothing. A phase 0 test with test-only resolvers covers a visible and a hidden alias in one document and a denied mutation that leaves a ref as it was; T4 and T10 repeat it with the real `updateRefs` and `repository`.

### 4.3 Webhooks

Each App has one webhook (URL, secret, active flag), set in the seed, at App creation or later through the admin API. Every event on an account or repository where the App is installed, and that the App subscribes to, is delivered; `installation`, `installation_repositories`, `installation_target` and `github_app_authorization` are delivered whatever the subscription list holds, as on GitHub (the client reads their repository lists, `internal/webhook/resweep_asks.go:39`). Each delivery has `X-GitHub-Event`, `X-GitHub-Delivery` (a lowercase UUID, as GitHub sends; the client's `internal/store/store.go:27` accepts only `[0-9A-Za-z-]`, at most 64 bytes), `X-GitHub-Hook-ID`, `X-GitHub-Hook-Installation-Target-Type`, `X-GitHub-Hook-Installation-Target-ID`, `X-Hub-Signature-256`, `X-Hub-Signature`, `Content-Type: application/json` and `User-Agent: GitHub-Hookshot/hubdouble`. Payloads carry `installation`, `repository`, `organization` and `sender` as GitHub does, and every field GitHub's payload has; the octokit schemas check the shape, and the conformance case per event asserts the fields the client reads (its `internal/webhook/payload.go`), among them `push` `ref`, `before`, `after`, `created`, `deleted`, `forced` and `commits[].added|removed|modified`; `pull_request_review_thread` `thread.comments[].pull_request_review_id`; `check_run.requested_action` `requested_action.identifier` with `check_run.external_id`, `name` and `head_sha`; `release.tag_name`; `workflow_run` `path`, `head_sha`, `head_branch`, `event`, `run_attempt`, `html_url`, `name`, `conclusion` and `pull_requests[].number`. A `ping` goes out when an App's webhook is set.

Deliveries go through a bounded queue (capacity 10 000) and a small worker pool, in commit order. `Update` appends its events to an outbox with a commit sequence number while it holds the lock (an append, never a wait); one publisher task moves the outbox to the worker queue strictly in sequence order, so two concurrent writers cannot reorder each other. The outbox and the worker queue together hold at most 10 000 events and 32 MiB of payload (`Options::queue_capacity` and `Options::queue_bytes`), and the store checks both at commit under the lock, so neither a writer nor the publisher waits for space. A payload is held as its serialized JSON body, made once in `emit`. An event past either bound is not dropped: it gets its delivery ID in order and is logged at once as a failed delivery with `status_code: 0`, the status `Queue full` and no attempt, so a redelivery can send it later. The one exception: while such records wait in the outbox, an App keeps at most `log_capacity` of them, and the oldest past that is removed before it gets a delivery ID, because that many newer records of the same App would push it out of the App's delivery log ring before anyone could read it. Reason: with parsed payloads and no bound, a run of 16 writers against a receiver that answers in 2 s grew RSS by 356 to 993 MiB over 24 000 events; 32 MiB (about 3 400 `issue_comment` payloads) plus the idle RSS (under 40 MB) stays well inside the 128 Mi memory limit of the k8s deployment. The request and response (headers and bodies) of the deliveries of every App share one more budget of 8 MiB (`Options::log_bytes`, the `LOG_BYTES` default), counted from the start of an attempt, through the wait behind a slower lower ID, to the delivery log, `Queue full` records included; past it the oldest deliveries lose their request and response and keep every other field, so the list and the client's walk see them unchanged, while `GET /app/hook/deliveries/{id}` and a redelivery of such a delivery answer `404`, as for a delivery past GitHub's 3-day retention (`docs/fidelity.md`). Measured with `mise run bench`, each case in its own process: the slow case grows RSS by about 70 MiB, and a delivery log filled with 50 000 deliveries of 10 KB payloads by about 64 MiB, so idle RSS plus either stays inside 128 Mi; the bench gates each growth at 128 Mi minus the 40 MB idle target. A test runs two concurrent writers and checks delivery order. Delivery IDs are assigned at enqueue, so two attempts that finish in reverse order still list by ID, and a test reads both through the client's walk (`internal/reconcile/reconcile.go:673`). Each attempt has GitHub's 10 s timeout. Each attempt is logged with an `int64` `id` (go-github v93 `HookDelivery.ID`; the client walks down until `id <= floor`, `internal/reconcile/reconcile.go:674`) and the UUID as `guid`, plus `delivered_at`, `redelivery`, `duration`, `status`, `status_code`, `event`, `action`, `installation_id` and `repository_id`, for `GET /app/hook/deliveries` (newest first, cursor pagination with `cursor` in the `Link` header, `status=failure` filter, `per_page`), `GET /app/hook/deliveries/{id}` (request and response) and `POST /app/hook/deliveries/{id}/attempts`. The list shows an attempt only after every attempt with a lower ID for that App has finished, so a scan between two out-of-order completions never sees a later ID first and saves it as its cursor (the client's `internal/reconcile/reconcile.go:740-762`); a test scans between the two completions. A redelivery keeps the original GUID, gets a new delivery ID and has `redelivery: true`. There is no automatic retry. `POST /_hubdouble/hooks/drain` returns once every event committed before the call has left the outbox and the publisher, the worker queue is empty, no attempt is in flight and every attempt is logged. Deliveries carry the state epoch they were committed in. Reset, under the store lock, clears the outbox and the worker queue and moves to a new epoch, then releases the lock and returns only after attempts already in flight have finished (it never waits while it holds the lock, since a finishing attempt writes its log entry); an attempt from an older epoch adds no delivery log entry, and no attempt from an older epoch starts after reset returns. A test resets while attempts are queued and in flight and checks that no request reaches the receiver and no log entry appears after reset returns.

Events the client consumes (its `internal/metrics/metrics.go` `knownEvents`, plus the events its handlers branch on), with one owning track each:

| Event and actions | Caused by | Owner |
| --- | --- | --- |
| `ping` | webhook set | P0-E |
| `push`, `create` | every ref change (section 6.4) | P0-C via the ref subscriber |
| `pull_request` (opened, edited, closed, reopened, synchronize, ready_for_review, converted_to_draft, review_requested, review_request_removed, labeled, unlabeled) | PR changes; synchronize from the ref subscriber | T5 |
| `pull_request_review` (submitted, edited, dismissed) | reviews | T6 |
| `pull_request_review_comment` (created, edited, deleted) | review comments | T6 |
| `pull_request_review_thread` (resolved, unresolved) | `resolveReviewThread`, `unresolveReviewThread` | T6 |
| `issues` (opened, edited, closed, reopened, labeled, assigned) | issues; the client does not consume it today, so it does not block done | T7 |
| `issue_comment` (created, edited, deleted) | comments on issues and PRs; on a PR, `issue.pull_request` holds `url`, `html_url`, `diff_url` and `patch_url`, which the client reads to tell a PR from an issue (`internal/webhook/payload.go:264`), and a conformance case checks it for both | T7 |
| `check_run` (created, completed, rerequested, requested_action), `check_suite` (requested, completed, rerequested) | checks; a suite per App on each pushed head | T8 |
| `status` | commit status | T8 |
| `workflow_run` (requested, in_progress, completed; the client records a run only on `completed`, `internal/webhook/task_facts.go:56`), and `workflow_job` (not consumed by the client today, does not block done) | run and job lifecycle (section 4.5); `completed` whenever a run reaches a conclusion, by the admin API, auto-complete or cancel | T9 |
| `release` (published, edited) | release create and edit through seed or admin API | T10 |
| `repository` (created, edited, renamed, archived, transferred, privatized, publicized, deleted) | repository changes; rename, transfer and visibility through the admin API | T10 |
| `installation` (created, deleted, suspend, unsuspend, new_permissions_accepted), `installation_repositories` (added, removed), `installation_target` (renamed), `github_app_authorization` (revoked) | admin API | T1 |
| `organization` (member_added, member_removed, member_invited; `membership.user.id`), `membership` (added, removed; `member.id`), `member` (added, removed, edited; `member.id`), `team` (created, deleted, edited, added_to_repository, removed_from_repository) | admin API calls for org members, invitations, team membership and team repositories (the client's `internal/webhook/handler.go:2419-2440`) | T3 |

`POST /_hubdouble/events` injects an arbitrary event for one App, for what a test cannot cause through the API (for example `check_run.requested_action`).

### 4.4 OAuth and SSO

- Web flow: `/login/oauth/authorize` with `client_id`, `redirect_uri`, `state`, `code_challenge` and `code_challenge_method=S256` answers `302` to the supplied `redirect_uri`, on whatever origin it names, with `code` and the unchanged `state` added to its query (the client sends its own callback and refuses one without the original `state`, `internal/dispatch/accounts.go:62`, `internal/staff/auth.go:223`). The signed-in user is the `login` query parameter, else the `hubdouble_login` cookie, else the seed's default user.
- `/orgs/{org}/sso?return_to=...` records SSO for the user and that org and redirects to `return_to` (same origin only). An org can be seeded as SSO-enforced: a user token without SSO for it gets `403` with `X-GitHub-SSO: required; url=...` on that org's resources, `GET /orgs/{org}/repos` included (the client probes there, `internal/dispatch/accounts.go:229`).
- `/login/oauth/access_token` checks per grant what GitHub checks: the code grant needs `client_id`, `client_secret`, `code`, the `redirect_uri` the code was made for and the PKCE verifier; the device grant needs only `client_id`, `device_code` and `grant_type` (what the client sends, `internal/dispatch/accounts.go:118`); the refresh grant needs `client_id`, `client_secret` and `refresh_token`. It answers form-encoded by default and JSON with `Accept: application/json`. Errors answer `200` with an `error` field (`bad_verification_code`, `incorrect_client_credentials`, `redirect_uri_mismatch`, `bad_refresh_token`, `unsupported_grant_type`, and `unverified_user_email` for a seeded user marked unverified), as GitHub does.
- Device flow: `/login/device/code` returns `device_code`, `user_code`, `verification_uri`, `expires_in` and `interval`, or `device_flow_disabled` when the App has it off. Polling (`grant_type=urn:ietf:params:oauth:grant-type:device_code`) answers `authorization_pending`, `slow_down` with a new `interval` 5 s longer, `expired_token`, `access_denied`, `incorrect_device_code`. A test approves with `POST /_hubdouble/oauth/device/{user_code}` (`login`, `approve`).
- Refresh: `grant_type=refresh_token` rotates both tokens.
- A conformance case sets `HUBDOUBLE_WEB_URL` to another origin and asserts that `verification_uri`, the authorize page and the SSO header still use the base origin, and that the authorize answer redirects to a `redirect_uri` on a third origin with `code` and the same `state`.

### 4.5 Actions

- Workflows are the files under `.github/workflows/` on the default branch, with stable IDs per path.
- The `{workflow_id}` path segment takes the numeric ID or the file name (`agent.yml`), for dispatches, runs and the workflow itself; the client uses the file name (`CreateWorkflowDispatchEventByFileName`, `ListWorkflowRunsByFileName`).
- `POST .../dispatches` checks the ref and inputs against the workflow's `workflow_dispatch` block (an undeclared input, a missing required input or an unknown ref is `422`, as on GitHub; for undeclared inputs the message is GitHub's `Unexpected inputs provided: ["a", "b"]`, naming every undeclared input in double quotes, sorted by name, and with no other text that could hold an input name (the client matches each name as a substring of the whole message, `internal/dispatch/dispatch.go:759`); a workflow without a `workflow_dispatch` trigger answers `422` with GitHub's `Workflow does not have 'workflow_dispatch' trigger`, which the client matches, `internal/dispatch/dispatch.go:739`; because the client retries without inputs only when that message names all it sent, `internal/dispatch/dispatch.go:717`; a `422` creates no run and emits nothing), and otherwise creates exactly one run (`queued`) with one job per job key, emits `workflow_run.requested` and `workflow_job.queued`, and answers `204`, or `200` with `workflow_run_id`, `run_url` and `html_url` when the body has `return_run_details: true`.
- `POST /_hubdouble/actions/runs/{id}` moves a run (`status`, `conclusion`, per-job steps and logs) and emits the matching events. A seed policy `actions.auto_complete: success|failure|none` completes runs at once.
- Runs, attempts, jobs, cancel, rerun-failed-jobs (a new attempt), repo and org secrets (metadata only) and variables follow GitHub's shapes. Job logs answer `302` to `/_blobs/{id}`.
- OIDC: discovery and JWKS under the issuer. `POST /_hubdouble/actions/oidc/token` mints a token for a run with `iss`, `sub`, `aud`, `exp`, `nbf`, `iat`, `jti`, `repository`, `repository_id`, `repository_owner`, `repository_owner_id`, `repository_visibility`, `ref`, `ref_type`, `sha`, `workflow`, `workflow_ref`, `workflow_sha`, `job_workflow_ref`, `job_workflow_sha`, `run_id`, `run_number`, `run_attempt`, `check_run_id`, `actor`, `actor_id`, `event_name`, `environment`, `runner_environment`, each overridable in the request. `exp`, `nbf` and `iat` are JSON numbers; every other claim, IDs, `run_number` and `run_attempt` included, is a string, as in [GitHub's example token](https://docs.github.com/en/actions/concepts/security/openid-connect#understanding-the-oidc-token). `job_workflow_ref` and `job_workflow_sha` default to the reusable workflow the seed names for the repository's caller (for v5, `octo-org/runner/.github/workflows/agent.yml@<the 40-hex commit the caller pins>`, which a release tag peels to), so the client's v5 release check (`internal/oidc/oidc.go:250`) accepts the token; hubdouble's conformance checks those claims, and the real-service proof waits for the runner step the client's session adds later. Tokens are RS256 with a `kid` from the JWKS. Discovery is at `<issuer>/.well-known/openid-configuration` and its `jwks_uri` is on the same origin (the client's `internal/jwks/jwks.go:303` requires that). Each run also has a request token, and `GET /_actions/token?audience=...` with it answers like `ACTIONS_ID_TOKEN_REQUEST_URL`, so `@actions/core.getIDToken` works unchanged in a fake runner.

### 4.6 Seed and admin API

The seed (`HUBDOUBLE_SEED`, YAML or JSON) is a list of the objects the admin API takes, applied at start in the kind registration order: P0-F's kinds first (users, orgs, repositories, Apps, installations, tokens), then each track's kinds in the `register_all` order of section 5 (teams after repositories, so a team can name its repositories), and in file order within a kind, except that a `fork_of` parent is applied before its fork. The list below groups the objects, it is not the order:

- users (ID, login, name, email, type User or Bot, personal tokens given or generated, each with classic OAuth `scopes`, default `repo`, `workflow`, `read:org` and `user`, so a seeded token reaches the user's private repositories as a real one would)
- orgs (ID, members and roles, SSO enforced), teams (ID, parent, privacy, members and roles, repositories and permission)
- repositories (ID, owner, name, visibility, `fork_of`, default branch, files as a path to content map or `from_dir`, extra branches, annotated and lightweight tags, collaborators, rulesets, releases with assets)
- Apps (ID, slug, name, owner, client ID and secret, a public key alone or a private key given or generated, webhook URL and secret, permissions, events, device flow on or off)
- installations (ID, App, account, repository selection and list, permissions, suspended)
- Actions policy, OIDC signing key (given or generated), default OAuth user

A repository's history is an ordered `commits` list: each entry names a branch, the file changes, a message, and optional tags (annotated or lightweight) on the commit it makes, and each entry's parent is the branch's previous commit, so a branch can hold commits on top of a tagged commit. File contents may hold `{{sha "<owner>/<repo>" "<ref>"}}`, which the loader replaces with the commit that ref resolves to (a tag peels to its commit); the ref must come from an entry applied earlier, else the seed fails to load with the entry's position. This is how `releases.json` and the caller's pin name a tag's commit without a hand-computed SHA. Any string value in the seed may hold `{{env "NAME"}}`, which the loader replaces with that process environment variable; an unset or empty variable fails the load with its name and the entry's position. Per-cluster values (keys, webhook URL and secret, tokens) come in this way, so a seed file holds no secret.

Seeded commits and tags are deterministic: author, committer and tagger default to a fixed seed identity and the fixed time `2026-01-01T00:00:00Z` unless the seed gives them, and files are written in sorted path order, so the same seed file gives the same SHAs on every start and every reset. A seed test loads the example seed on two fresh servers and checks that each tag peels to the same commit and that the target caller's pinned SHA is that commit.

`GET /_hubdouble/log` (a ring buffer of the last 50 000 entries; a cursor older than the window answers `410`) lists every API request (method, path, operation ID, actor, status) and every emitted event and delivery since a cursor, and `GET /_hubdouble/log/wait?match=...&after=<cursor>` holds the request open until a matching entry exists (or the caller's own timeout ends it), so a smoke script waits on "installations listed", "reaction added" or "workflow run created" without sleeping. Applying the seed at start and on reset is silent: it emits no events and no deliveries. Creates and changes through the admin API emit events as section 4.3 says. Reset restores the table snapshot taken right after the seed load, so every seeded object, with a given ID or not, comes back with the same ID and number (the seeded PR stays `1`), but every generated sequence (object IDs, delivery IDs, run IDs, issue and PR numbers, log cursors) keeps counting up from where it was, because a running client keeps IDs and cursors across a reset (its delivery walk stops at the first ID at or below its stored floor, `internal/reconcile/reconcile.go:711-746`, and it binds run IDs to dispatches, `internal/dispatch/dispatch.go:648`). Reset empties the request log, and a `/_hubdouble/log` cursor from before the reset answers `410`, including a `/_hubdouble/log/wait` request held open across the reset. Reset keeps the App delivery log (a ring buffer of the last 50 000 attempts per App), as GitHub keeps deliveries, so a running client resumes its saved page cursor and its walk still reaches an ID at or below its stored floor without recording a source gap (`internal/reconcile/reconcile.go:714`, `:754-761`). A `GET /app/hook/deliveries` `cursor` hubdouble never issued, or one that points past the ring buffer, answers `422`, because the client clears a saved delivery cursor only on `400` or `422` (`internal/reconcile/reconcile.go:725`, `:858-861`). hubdouble's own tests get repeatable IDs from a new server per test, not from reset. Reset keeps every unexpired issued token whose installation or user is in the seed, because the client's installation client hands the first `401` back to its caller before it mints again (`internal/ghapp/ghapp.go:126`). The admin API under `/_hubdouble/` (admin token) creates, reads, updates and deletes each of these, reads generated keys and tokens, resets the state to the seed (`POST /_hubdouble/reset`), drains webhooks, injects events and faults, approves device codes and drives Actions runs. Each track registers the admin routes and seed kinds for the objects it owns through the contract in section 6.6.

### 4.7 Fidelity rules every track follows

- Errors: `{"message", "documentation_url", "status"}`, plus `errors[]` with `resource`, `field`, `code` on `422`.
- Pagination: `per_page` (default 30, max 100) and `page`, with a `Link` header; cursor pagination where GitHub uses it.
- An empty list renders as `[]`, never `null` (the client refuses a `null` matching-refs answer, `internal/dispatch/release_catalog.go:205`); the render layer marshals nil slices as empty arrays, and a test lists matching refs on a repository with no match.
- Conditional requests: every `200` `GET` has a weak `ETag` from the hash of the media type and the body, and a matching `If-None-Match` answers `304` (the client's `internal/dispatch/conditional.go` sends it, keyed by URL and `Accept`).
- Headers: `X-GitHub-Request-Id`, `X-GitHub-Media-Type`, `X-RateLimit-*` with realistic static values and `X-RateLimit-Remaining` never `0` unless a fault sets it (go-github skips calls while it is `0`), `X-OAuth-Scopes` for user tokens, `X-GitHub-Api-Version-Selected`. `X-GitHub-Api-Version` accepts the versions GitHub publishes (`2022-11-28` and `2026-03-10` today; go-github v93 sends either); a request without the header is served as `2022-11-28`, and any other value is `400`.
- IDs: one global `int64` sequence. The seed loader reads every given ID before it applies any kind and starts the sequence above the largest; an admin create with a given ID that is taken or at or below the sequence answers `422`. Node IDs use GitHub's current format, a type prefix and an opaque payload (`PR_kw...`). Legacy base64 IDs are accepted, and the answer adds GitHub's `DEPRECATION` warning with `next_global_id` in `extensions`.
- Timestamps in RFC 3339 UTC with second precision.
- Reads of an unchanged ref answer the same commit every time.
- The delivery log is a ring buffer of the last 50 000 attempts per App.
- Commits GitHub writes itself (merges, `createCommitOnBranch`) have committer `GitHub <noreply@github.com>`, a signature from hubdouble's web-flow key, and `verification.verified: true`, reason `valid`. A commit an installation makes through the git data API with no `author`, `committer` or `signature` is signed the same way and reports `valid`, as GitHub signs a bot's API commit (the client's `internal/gitwrite/tree.go` `signedOn` refuses anything else). A client-signed commit from the git data API reports `unknown_key`; any other unsigned one `unsigned`.
- GraphQL validation follows GitHub's observed behaviour, which is the spec's rules with listed exceptions, each confirmed against real GitHub. The one known today: fields of the same name in different concrete types may differ in nullability (`PullRequest` on `IssueComment` and `PullRequest!` on `PullRequestReviewComment` in the client's `originalCommentEdit`; GitHub accepted that document on 2026-11-22).
- GraphQL execution follows the spec: a `null` from a failing non-null field propagates to the nearest nullable parent, and the error carries `type` (`NOT_FOUND`, `FORBIDDEN`, ...), `path` and `locations`. Connections require `first` or `last` (max 100) with GitHub's message. An unimplemented field is an error on its path only, `type: "HUBDOUBLE_NOT_IMPLEMENTED"`. A repository the actor cannot see answers `null` with a `NOT_FOUND` error for that alias only.
- Faults: `POST /_hubdouble/faults` with a method and path pattern (or GraphQL operation name), status, body, headers, delay and count, for a secondary rate limit, a `502` or `X-GitHub-SSO`.

### 4.8 Response variants the client depends on

| Call | Variant | Owner |
| --- | --- | --- |
| `GET .../actions/jobs/{id}/logs` | `302` to `/_blobs/{id}`; the target answers `HEAD` with `Content-Length` and `GET` with `Range` as `206` (the client's `internal/dispatch/evidence.go:190-240`) | T9 |
| `GET .../releases/assets/{id}` | JSON by default; `Accept: application/octet-stream` answers `302` to the bytes (`internal/dispatch/assets.go:181`) | T10 |
| `GET .../git/blobs/{sha}` | JSON by default; `Accept: application/vnd.github.raw` or `application/vnd.github.v3.raw` (go-github's `GetBlobRaw`, the client's `internal/dispatch/autofix_paths.go:184`) answers the raw bytes | T4 |
| `POST` and `PATCH .../check-runs` | `actions[]` with `label`, `description`, `identifier` stored, so a requested action names one of them (the REST answer has no actions member, as on GitHub) (`internal/dispatch/checks.go:172`) | T8 |
| `GET .../commits/{ref}` | JSON by default; `Accept: application/vnd.github.v3.sha` answers the 40-hex SHA as text, and `304` when `If-None-Match` holds that SHA (go-github `GetCommitSHA1`, the client's `internal/dispatch/review_runtime.go:17`, `internal/reviewadmission/runtime.go:126`) | T4 |
| `GET .../contents/{path}` | file, directory listing, and `application/vnd.github.raw`; the root path | T4 |
| `DELETE .../pulls/{n}/requested_reviewers` | JSON body with `reviewers` and `team_reviewers`; `422` without `reviewers` (`internal/dispatch/dispatch.go:944-955`) | T6 |
| `POST .../merges` | `201` with the merge commit, `204` when the base already holds the head, `409` on conflict, `404` for a missing base or head | T4 |
| `POST .../actions/workflows/{id}/dispatches` | E9 | T9 |
| `POST /login/oauth/access_token` | `200` with `error` on failure | T2 |
| `GET .../rules/branches/{branch}` | a JSON array of rules with `type` and `parameters`, `page` and `per_page` with a `Link` header (`internal/dispatch/guarded.go:121`, `internal/versioning/github.go:245`) | T10 |
| `GET .../branches/{branch}` | `protected` set from rulesets and protection; a missing branch is `404` with GitHub's message `Branch not found` (`internal/dispatch/guarded.go:104-114`) | T4 |
| `GET .../rulesets/{id}` | includes `current_user_can_bypass` for the actor (`internal/versioning/github.go:333`) | T10 |
| `POST` and `PATCH .../check-runs` | `output.annotations` stored, up to 50 per call as GitHub allows (`internal/dispatch/checks.go:159`) | T8 |
| `GET .../commits/{ref}/status` | `state` always set (`pending` with no statuses) (`internal/dispatch/head.go:81`) | T8 |
| `GET .../actions/runs` and `.../workflows/{id}/runs` | `total_count`; each run has `path`, `check_suite_id`, `run_attempt`, `status` (`internal/dispatch/ci.go:200`), `run_started_at` and `updated_at` (also in the `workflow_run` webhook, `internal/webhook/autofix.go:95`) | T9 |

### 4.9 The client v5 example seed

`examples/client-v5.yaml` (owned by T12) serves the client's v5 line only: no `client.yml` or `agent-v5.yml` callers.

- `octo-org/app`, private, holds only `main` with `.github/releases.json` set to `{"releases": []}` and no `agent/` tags. The versions measure reads the source catalog from it before it writes any row and fails when the repository is missing (the client's `internal/versioning/measure.go:444`, `internal/versioning/versions.go:361-369`, `internal/dispatch/release_catalog.go:58-77`). A missing file also reads as empty, but the seed keeps the file because production has it.
- `octo-org/runner` holds annotated v5 release tags named `v5...` (for example `v5.0.0`, never `agent/v5...`, which the runtime catalog refuses, `internal/dispatch/releases.go:60-71`) that GraphQL `refs` peel to commits (`internal/dispatch/release_catalog.go:272`). Each tagged commit holds `.github/workflows/agent.yml` and the capability file `.github/runtime-capabilities.json` (`internal/reviewadmission/runtime.go:24`), copied from a real `octo-org/runner` release so it passes the client's completeness check (`:185-190`): `version: 1` and a `capabilities.core_review` entry holding exactly the stage, source, candidate and result versions the client expects (`completeReviewCapability`, `:294-309`), with no unknown members; `deadline` and `answers` are optional there (an absent `deadline` only makes the review unbounded), so the seed has them only where the copied real release tag has them, and the check accepts both. Native review reads it at the pinned commit (`:180`), and an incomplete file makes admission refuse the job, so `xtask inventory` reads those version constants from the client checkout and fails when the seed's file differs. `main` is a later commit on top of the tags that adds `.github/releases.json`, listing each tag with `{{sha}}`, because the client reads `releases.json` only at `main` (`internal/dispatch/release_catalog.go:83`) and a file cannot name the commit that holds it. The entry the caller pins has `tag` equal to the caller's comment version and `commit` equal to that tag's peeled SHA, so the measure reads the target as pinned (`internal/versioning/versions.go:737-762`).
- `octo-org/e2e`, private, is the target, with default branch `main`, the ref the client dispatches on (`internal/webhook/payload.go:324`). Its `.github/workflows/agent.yml` is the v5 example caller (`octo-org/runner` `examples/consumer-client.yaml`): `on: workflow_dispatch: {}`, which declares no inputs, and `jobs.agent.uses: octo-org/runner/.github/workflows/agent.yml@<40-hex SHA> # v<tag version>`, the SHA written as `{{sha "octo-org/runner" "<tag>"}}`, so `octo-org/runner` is listed before the target. The version comment stays because the client reads the major version from it (`internal/versioning/pinrefs.go:243`), and native admission refuses a tag ref. The client's first dispatch with inputs gets the `Unexpected inputs` `422`, and its retry without inputs starts the run the smoke test checks; that run's `job_workflow_ref` is `octo-org/runner/.github/workflows/agent.yml@<that SHA>`. The target has a seeded open PR whose compare, files and commits show changed files and commit authors who are seeded users, and the seeded user has write permission on it.
- One App with the fixed numeric ID `1000001` (the client's chart leaves `app.id` empty, so the smoke deploy sets this value) and slug `agent-local`, so its bot account is `agent-local[bot]`, subscribed to `issue_comment` only: a `pull_request.opened` delivery would start a second review. A later smoke test of run completion adds `workflow_run` and `check_run`, which the client's task facts need (`internal/webhook/task_facts.go:52-70`).
- One installation on org `octo-org` (ID 962416) with repository selection `selected` covering the three repositories, listed by `/app/installations`, `/installation/repositories` and the per-repository reads, with the union of the permissions the client mints: `actions: write` (the dispatch token, `internal/dispatch/dispatch.go:1135`), `actions_variables: read`, `checks: write`, `contents: write`, `issues: write`, `members: read`, `metadata: read`, `pull_requests: write`, `statuses: write` and `workflows: write` (also `internal/versioning/projects.go:24-27`, `internal/dispatch/grants.go:120-123`). A token request for a permission the installation lacks answers `422`.
- `octo-org/public-app` is absent (the public mirror exists only from GA) and answers a plain `404`.

Seed tests: the example seed loads on two fresh servers with the same SHAs (section 4.6); the pinned SHA is among the tags of the verified runtime catalog that native admission reads, whose tags come from `releases.json` on `main` (`internal/dispatch/observed_releases.go:359-373`) (`internal/reviewadmission/runtime.go:259-271`, `cmd/client/main.go:488`): `releases.json` at a steady `main`, read between two `heads/main` reads, plus matching refs `tags/v` and the peel, together (`internal/dispatch/release_catalog.go:121-189`); each `releases.json` entry holds only the `Release` JSON fields, and the test parses the file rejecting unknown fields as the client does (`internal/dispatch/releases.go:142`); the reusable workflow keeps the basename `agent` (`internal/reviewadmission/runtime.go:177`); and `xtask inventory` extracts every permission map (`map[string]string` floors included) and `github.InstallationPermissions` literal from the client and fails when the seed's installation does not cover one, with a test that adds a fake permission and checks that the tool fails.

### 4.10 The client's local end-to-end contract

The client's local end-to-end tasks are written against this; changing any item is a breaking change that goes to the client's session first.

- Image: `docker build .` at the repository root makes the working image. The client's `mise run e2e:up` builds it as `hubdouble:local` from `AGENT_E2E_HUBDOUBLE_DIR` and loads it into OrbStack, Docker Desktop or kind; nothing pulls from a registry.
- Seed: `examples/client-v5.yaml` (section 4.9), mounted at `/seed/client-v5.yaml`, with `HUBDOUBLE_SEED=/seed/client-v5.yaml`. `HUBDOUBLE_SEED` is a file path.
- Environment the example seed reads with `{{env}}`: `HUBDOUBLE_APP_PUBLIC_KEY` (PEM public key of App `1000001`; the client holds the private key), `HUBDOUBLE_APP_WEBHOOK_URL` (`http://client/webhook`), `HUBDOUBLE_APP_WEBHOOK_SECRET`, and `HUBDOUBLE_USER_TOKEN` (the personal token, `ghp_` plus 36 hex, of the seeded user with write on `octo-org/e2e`). `HUBDOUBLE_ADMIN_TOKEN` is set to a random value.
- Process: `HUBDOUBLE_ADDR=:8000`, `HUBDOUBLE_BASE_URL=http://hubdouble:8000`, `HUBDOUBLE_WEB_URL=https://github.com`; the Service is `hubdouble:8000`.
- The seeded open PR on the target is number `1`.
- Calls the smoke makes, each covered by a conformance case with the same credentials: `POST /_hubdouble/hooks/drain` with `Authorization: Bearer <admin token>`, answering `204` once drained; `GET /app/hook/deliveries` and `GET /app/hook/deliveries/{id}` with the App JWT, where the single delivery's `request.payload` is the delivered JSON object, so `request.payload.comment.id` is the comment's ID; `POST /repos/octo-org/e2e/issues/1/comments` and `GET /repos/octo-org/e2e/issues/comments/{id}/reactions` with the user token, where the smoke expects the `eyes` reaction the client adds as the App (`content: "eyes"`, `user.type: "Bot"`, `user.login` equal to `<slug>[bot]` for the `slug` that `GET /app` with the App JWT answers, so the App seed kind's bot account and `GET /app` agree); `GET /repos/octo-org/e2e/actions/workflows/agent.yml/runs` with the user token.
- The smoke does not use `/_hubdouble/log/wait`, so its match syntax is not part of this contract.
- Routes the smoke calls that the client itself does not (`GET .../issues/comments/{id}/reactions`) are in `docs/inventory/rest-routes.txt` too, so the coverage gate holds them.

## 5. Repository layout

One Cargo workspace. Each crate has one owner, so parallel agents never edit the same crate, and a change recompiles only the crates that depend on it. Crate names are `hubdouble-<dir>`; the dependency order runs down the list, and no crate depends on one below it except through the registries of section 6.

```text
crates/model/             frozen entities, IDs, node ID codec, Event, Grant, Requirement, Scope, ApiError, Clock, Config (parsed environment)
crates/store/             Store, Tx, TxMut, undo log, table registry, outbox
crates/render/            frozen REST render structs and their webhook payload forms
crates/hooks/             emit, recipients, outbox publisher, delivery queue and workers, delivery log, signing
crates/git/               object store, refs, ref subscribers, compare, three-way merge, signing
crates/auth/              token kinds, JWT checks, grants, the authorization choke point
crates/spec/              generated operation table (ops_gen.rs), embedded GraphQL schema
crates/rest/              Registry, Op, Ctx, Reply, pagination, Link header
crates/gql/               executor, resolver registry, connections, node and nodes, errors, validation rules
crates/admin/             admin API frame, seed loader, per-kind registration
crates/server/            environment parsing into model::Config, listener, router, middleware chain, error writer, ETag, faults, /_blobs
crates/api-<track>/       one crate per phase 1 track: REST handlers, GraphQL resolvers, side tables, events, admin and seed kinds
crates/api/               register_all: calls each track crate's register in a fixed order
crates/hubdouble/         the binary: serve, coverage, signal handling
crates/testkit/           in-process server, go-github request builders, GraphQL client, webhook receiver, validators, fixture seed
crates/conformance/       client inventory suite (tests only)
xtask/                    gen (operation table and router from the pinned OpenAPI bundle), inventory (reads a client checkout)
spec/                     pinned OpenAPI bundle, GraphQL schema, octokit webhook schemas (test data, licenses kept)
examples/                 seeds, Kubernetes manifest
docs/                     this plan, inventory, user docs
```

Track crate names: `api-apps` (T1), `api-users` (T2), `api-orgs` (T3), `api-gitdata` (T4), `api-pulls` (T5), `api-reviews` (T6), `api-issues` (T7), `api-checks` (T8), `api-actions` (T9), `api-repos` (T10), `api-search` (T11). T12 owns `examples/`, `deploy/` and the release workflow, no crate.

## 6. Contracts frozen in phase 0

A phase 1 agent that needs a change to a contract stops and asks the coordinator, who changes it on `main` for everyone.

### 6.1 Store

```rust
impl Store {
    pub fn view<R>(&self, f: impl FnOnce(&Tx) -> Result<R, ApiError>) -> Result<R, ApiError>;      // shared lock, read only
    pub fn update<R>(&self, f: impl FnOnce(&mut TxMut<'_>) -> Result<R, ApiError>) -> Result<R, ApiError>; // exclusive lock
    pub fn update_quiet<R>(&self, f: impl FnOnce(&mut TxMut<'_>) -> Result<R, ApiError>) -> Result<R, ApiError>; // as update, but the outbox buffer is dropped on commit: the seed load and reset only
    pub fn snapshot(&self) -> Snapshot;           // clones every registered table under the lock (entities are Arcs, so the copy is shallow); taken once after the seed load
    pub fn restore(&self, s: &Snapshot);          // reset: each table's Table::restore(&mut self, seeded: &Self) decides what comes back
}
pub trait Table: Any + Send + Sync + Clone {     // not object safe: the store keeps tables in a TypeId map with typed accessors
    fn restore(&mut self, seeded: &Self) { *self = seeded.clone(); } // default: back to the seeded state
}
pub struct Tx { /* every table, Arc<Config> */ }  // the state inside the lock; only & methods; config() gives the URLs to code that renders outside a request
pub struct TxMut<'a> { /* &'a mut Tx (private), undo log, outbox buffer, id sequence */ } // Deref<Target = Tx>, no DerefMut
impl TxMut<'_> {
    pub fn next_id(&mut self) -> i64;
    pub fn table_mut<T: Table>(&mut self) -> TableMut<'_, T>; // every insert, replace and remove records its undo
    pub fn push_outbox(&mut self, entry: OutboxEntry);        // buffered; reaches the outbox only on commit
}
impl Tx { pub fn table<T: Table>(&self) -> &T; pub fn config(&self) -> &Config; }
pub struct StoreBuilder;                          // register_table::<T>() for every table, then build_with(config) (build() uses Config::default, for tests); no table is added after start
pub struct OutboxEntry { pub app_id: i64, pub installation_id: Option<i64>, pub repo_id: Option<i64>, pub event: &'static str, pub action: Option<&'static str>, pub payload: serde_json::Value, pub redelivery_of: Option<String> }
```

A table overrides `restore` only where section 4.6 keeps state across a reset: the token table keeps every unexpired token of a seeded installation or user, the delivery table keeps the App delivery log, and the counter table (the global ID sequence, delivery IDs, run IDs, each repository's issue and PR number, and the request log cursor) keeps its current values, so every generated sequence keeps counting up. A track's side table uses the default unless 4.6 says otherwise.

`update` commits when the closure returns `Ok`. An `Err` or a panic inside the closure replays the undo log in reverse (every table, side table and ref update) through a drop guard, so a failed or panicking `update` leaves no change and no outbox entry. Outbox entries are handed to the publisher after the lock is released. `Arc` entities are replaced, never mutated in place, so a reader that cloned an `Arc` keeps a stable snapshot.

Frozen entities in `hubdouble-model`, with their fields, in phase 0: `Account` (users, bots and orgs in one ID space), `Membership`, `Team`, `Repo`, `App`, `Installation`, `Token`, `Issue` (issues and PRs share number space and table; PR fields in `Issue.pull`), `IssueComment`, `Review`, `ReviewComment`, `ReviewThread`, `Reaction`, `Label`, `CheckSuite`, `CheckRun`, `Status`, `Release`, `ReleaseAsset`, `WorkflowRun`, `WorkflowJob`, `Ruleset`, `Delivery`. Their tables, with the indexes every track needs (by ID, by repository, by number, by node ID), are in `hubdouble-store`. A track adds fields only through side tables it defines in its own crate, keyed by entity ID and registered on the `StoreBuilder` by its `register` function, never by editing a frozen struct. `Clock` (a trait with `now()`, a system clock and a manual test clock) is in `hubdouble-model`, and every time-dependent rule reads it, so no test waits for real time.

### 6.2 REST router and registry

`cargo xtask gen` builds a segment tree from every OpenAPI path and writes it as static tables into `crates/spec/src/ops_gen.rs`, which is committed, so a normal build reads no OpenAPI file; CI fails when a fresh generation differs. The router splits the escaped path on `/` first and decodes each segment after, so an encoded slash never separates segments: `branches/feature%2Fprotection` is the branch `feature/protection`, not the protection route of `feature` (`GetBranch` and `GetRulesForBranch` send `%2F`, the client's `internal/dispatch/guarded.go:97`); a slash-holding parameter joins its decoded segments with `/`. A literal segment beats a parameter at the same depth. The parameters GitHub allows to hold slashes (`ref`, `path`, `branch`, `basehead`, `tag`) match one or more segments with backtracking, so a route with a literal suffix wins: `commits/{ref}/check-runs`, `/check-suites`, `/status`, `/statuses` and `commits/{commit_sha}/pulls` beat `commits/{ref}`, and `branches/{branch}/protection` beats `branches/{branch}`. A test registers the whole table and checks these pairs and `issues/comments/{comment_id}` against `issues/{issue_number}/events`, plus go-github v93's real escaping for `Repositories.GetContents` and `Git.GetRef` next to the `%2F` branch case. Those escaped paths are literal test data copied from what go-github v93 sends (it escapes each path segment with `url.PathEscape` and keeps `/` between segments), with the go-github source line cited next to each.

```rust
pub struct Grant { pub permission: &'static str, pub access: Access } // GitHub App permission ("contents", "pull_requests", ...)
pub enum Access { Read, Write, Admin }
pub enum Scope { Repo, Org, App /* JWT */, User, Installation, Public }
pub struct Requirement {
    pub any_of: &'static [Grant],         // any one suffices, as GitHub documents per operation; empty for public data
    pub scope: Scope,
    pub scopes: &'static [&'static str],  // classic OAuth scopes
}
pub type Handler = fn(&mut Ctx<'_>) -> Result<Reply, ApiError>;
impl Registry {
    pub fn handle(&mut self, operation_id: &str, req: Requirement, h: Handler); // panics on an unknown or already registered operation
    pub fn extra(&mut self, method: Method, path: &str, operation_id: &'static str, req: Requirement, h: Handler); // routes outside the OpenAPI bundle
    pub fn extra_authenticating(/* as extra */); // the handler checks its own bearer (the Actions runtime token); it runs as Actor::Anonymous
    pub fn extra_writing(/* as extra */); // a GET that changes state (/orgs/{org}/sso records SSO); the server runs it in Store::update
}
```

`operation_id` is GitHub's (`pulls/create`). `extra` registers the routes GitHub serves outside its REST description (`/login/oauth/*`, `/login/device/*`, `/orgs/{org}/sso`) under hubdouble's own operation IDs (`oauth/authorize`), so the choke point, the request log and `hubdouble coverage --inventory` treat them like every other route. GraphQL is not a REST route: `hubdouble-server`, which depends on `gql` and holds every frozen registry, serves `POST /graphql` and its aliases itself, resolves the actor through the same `auth` code, logs the request as operation `graphql`, counts it in coverage, and calls `gql::execute(&gql::Registry, &Store, &Git, &Actor, request)`, which runs a query in one `view` and each mutation in its own `update`, in order. Some operations accept alternative permissions: creating an issue comment takes `issues:write` or `pull_requests:write` ([GitHub](https://docs.github.com/en/rest/issues/comments?apiVersion=2026-03-10#create-an-issue-comment)), and the client relies on the second (`internal/dispatch/assets.go:37`); the tests use a token with each accepted grant and one with neither. `Ctx` gives the path and query parameters, the body (decoded on demand into a `serde` type, a decode error answering GitHub's `400` or `422`), the actor, the resolved repository, the request headers, the `&Git` handle (usable next to the `TxMut`, because the object store is outside the store lock), a `TxMut` for a write operation or a `Tx` for a read (from the HTTP method: `GET` and `HEAD` read, every other method writes), the clock, the base URLs, and `Reply` builders: `json`, `page` (with the `Link` header), `redirect`, `raw`, `empty` and `status`. The `Ctx` lives inside the store transaction, so a handler never locks.

### 6.3 GraphQL registry

```rust
pub enum Resolved {                                   // what a resolver returns
    Null,
    Leaf(serde_json::Value),                          // scalars and enums
    Object { type_name: &'static str, value: Arc<dyn Any + Send + Sync> },
    List(Vec<Resolved>),
}
pub type Resolver = fn(&Ctx<'_>, &Obj, &Args) -> Result<Resolved, GqlError>;     // Obj::get::<T>() downcasts the parent
pub type MutationResolver = fn(&mut MutCtx<'_>, &Args) -> Result<Resolved, GqlError>; // runs inside one update
impl Registry {
    pub fn object<T: Serialize + Send + Sync + 'static>(&mut self, type_name: &'static str); // owner only
    pub fn field(&mut self, type_name: &str, field_name: &str, r: Resolver);               // panics on a second registration
    pub fn mutation(&mut self, name: &str, r: MutationResolver);
    pub fn node(&mut self, prefix: &'static str, type_name: &'static str, load: fn(&Ctx<'_>, i64) -> Result<Resolved, GqlError>);
}
pub fn connection<T>(type_name: &'static str, items: &[T], args: &Args, cursor: impl Fn(&T) -> String, node: impl Fn(&T) -> Resolved) -> Result<Resolved, GqlError>;
// type_name is the schema's connection type ("PullRequestConnection"); slices items by first, last, after and before, and returns an owned
// gql::Conn object under that name (edges with cursors and nodes, pageInfo, totalCount), an empty page included
```

A field without a resolver falls back to the field of the same name in the parent's `serde` form (`#[serde(rename_all = "camelCase")]` on the Rust type registered with `object`), serialized once per parent object and only when a query selects such a field, else a `HUBDOUBLE_NOT_IMPLEMENTED` error on its path. `Ctx` holds the `Tx`, the `&Git` handle, the actor and `Ctx::load`; `MutCtx` holds the `TxMut` and the same `&Git` at once (`Git` is a separate shared value outside the store lock, so a shared borrow of it and the exclusive borrow of the `TxMut` coexist), with the same `load` (the per-object grant check of section 4.2, backed by P0-D's choke point). Each GraphQL type has one owner, and only the owner calls `object` and `node` for it. Each field has one owner too: P0-B generates `docs/inventory/graphql-fields.tsv` with every `Type.field` the inventory documents select, the coordinator assigns one track to each row in phase 0, `Registry::field` panics on a second registration, and a phase 0 test fails on a row without an owner. A field's owner is the type's owner unless the table says otherwise (for example `Repository.ref` is T4's and `Repository.pullRequest` is T5's). Type owners:

| Owner | Types | Rust value behind each object type |
| --- | --- | --- |
| P0-B | `Query.node`, `Query.nodes`, `PageInfo`, connection and edge types other than T3's and T11's, `Node` | `gql::Conn`, `gql::Edge`, both owned (`'static`) |
| T1 | `App`, `Bot` | `Arc<model::App>`, `Arc<model::Account>` |
| T2 | `User`, `Query.viewer`, `Actor` | `Arc<model::Account>` |
| T3 | `Organization`, `Team`, `RepositoryCollaboratorConnection` and edge (the edge has its own `permission` field) | `Arc<model::Account>`, `Arc<model::Team>`, T3's own |
| T4 | `Ref`, `Commit`, `Tree`, `TreeEntry`, `Blob`, `Tag`, `GitSignature` types, `createCommitOnBranch`, `updateRefs` | `git::RefNode`, `git::CommitNode`, `git::TreeNode`, `git::TreeEntryNode`, `git::BlobNode`, `git::TagNode`, `git::Signature`, each holding its repository ID |
| T5 | `PullRequest`, `markPullRequestReadyForReview`, `Ref.associatedPullRequests` | `Arc<model::Issue>` with `pull` set |
| T6 | `PullRequestReview`, `PullRequestReviewThread`, `PullRequestReviewComment`, `addPullRequestReviewThread`, `resolveReviewThread`, `unresolveReviewThread` | `Arc<model::Review>`, `Arc<model::ReviewThread>`, `Arc<model::ReviewComment>` |
| T7 | `Issue`, `IssueComment`, `Comment` interface, `Issue.closedByPullRequestsReferences` | `Arc<model::Issue>`, `Arc<model::IssueComment>` |
| T8 | `StatusCheckRollup`, `Commit.statusCheckRollup` | T8's own |
| T10 | `Repository` | `Arc<model::Repo>` |
| T11 | `Query.search`, `SearchResultItemConnection` | T11's own |

The Rust value of every type another track can reach is frozen in this table. For an entity row, the type is registered as `object::<model::Issue>` (never `Arc<model::Issue>`), and `Resolved::Object` holds the entity's own `Arc<model::Issue>` coerced to `Arc<dyn Any>`, so a `PullRequest` that T6 or T11 builds passes T5's check; and P0-A writes the `git::*Node` structs in the git contract. "Own" is allowed only for a type that no other track's resolver builds or reads. An interface or union resolves to the value of its concrete type. `Registry::object` records each type's `TypeId`, and the executor checks every `Resolved::Object` against it, reading the value's own type with `(*value).type_id()` (a bare `value.type_id()` on the `Arc` gives the `Arc`'s type): a mismatch panics in a debug or test build and is an `INTERNAL` error on the field's path in a release build, so a track's own tests catch a wrong value before phase 2.

### 6.4 Git

```rust
impl Git {                                         // one per server; objects shared per fork network, refs per repository
    pub fn resolve_ref(&self, tx: &Tx, repo: i64, name: &str) -> Result<ObjectId, GitError>;
    pub fn refs(&self, tx: &Tx, repo: i64, prefix: &str) -> Vec<Ref>;
    pub fn update_ref(&self, tx: &mut TxMut<'_>, repo: i64, sender: i64, name: &str, old: Option<ObjectId>, new: Option<ObjectId>, force: bool) -> Result<(), GitError>; // sender: the acting account, the push event's sender and pusher
    pub fn object(&self, tx: &Tx, repo: i64, id: ObjectId) -> Result<Object, GitError>;
    pub fn expression(&self, tx: &Tx, repo: i64, expr: &str) -> Result<Object, GitError>; // "main:docs/a.md", "HEAD:", an oid
    pub fn write_blob / write_tree / write_commit / write_tag(&self, tx: &Tx, repo: i64, ...) -> Result<ObjectId, GitError>;
    pub fn commit(&self, tx: &mut TxMut<'_>, repo: i64, sender: i64, branch: &str, parent: Option<ObjectId>, changes: &[FileChange], message: &str, author: &Signature, committer: &Signature, sign: bool) -> Result<ObjectId, GitError>;
    pub fn sign(&self, commit: &mut Commit) -> Result<(), GitError>; // web-flow signature, for a commit written with write_commit
    // RefNode { repo_id, name (full), prefix, target }: GraphQL Ref.name is name without prefix, the refPrefix of a refs listing or refs/heads/, refs/tags/, refs/
    pub fn compare(&self, tx: &Tx, repo: i64, base: ObjectId, head: ObjectId) -> Result<Comparison, GitError>; // merge base, ahead, behind, commits, files with patches
    pub fn merge(&self, tx: &mut TxMut<'_>, repo: i64, sender: i64, base: &str, head: &str, msg: &str) -> Result<MergeResult, GitError>; // GitError::NothingToMerge, GitError::Conflict; author: the sender, committer: web-flow
    pub fn mergeable(&self, tx: &Tx, repo: i64, base: ObjectId, head: ObjectId) -> Result<bool, GitError>;
}
impl GitBuilder { pub fn on_ref_update(&mut self, f: fn(&mut TxMut<'_>, &Git, &RefChange) -> Result<(), ApiError>); } // at start only
```

The object store is content-addressed and append-only, outside the store lock (a sharded map of `Arc<[u8]>` per fork network); a rolled-back `update` leaves only unreachable objects whose IDs no answer ever gave. Refs live in a store table that P0-C defines and registers on the `StoreBuilder`, so ref changes roll back with everything else. Every `Git` call takes the `Tx` so it can find a repository's fork network through `Repo.fork_of`; inside an `update` or a ref subscriber, pass the `TxMut` itself (`&*tx` through `Deref`), never a second lock. Every ref change goes through `update_ref`. Subscribers run inside the same transaction: P0-C emits `push` and `create`; T5 subscribes for `pull_request.synchronize`, PR head oids and `mergeable`, and, as GitHub does, marks an open PR merged (REST `state: closed` with `merged`, `merged_at`, `merge_commit_sha`; GraphQL `state: MERGED`; `pull_request.closed` with `merged: true`) when an update makes its head reachable from its base; T8 subscribes to create a `check_suite` per App on each pushed head. `PUT /pulls/{n}/merge` with method `merge` (T5, on `merge`) is the other way to merge, for scripts. A fork and its parent share one object store, as a GitHub fork network does, so `object`, `expression` and `compare` on the parent find a commit pushed only to the fork, and refs stay per repository; a pull request from a fork reads its head from the fork's ref (the client compares a fork head through the base repository, `internal/dispatch/review_comparison.go:40`, `internal/router/router.go:851`). A conformance case pushes a commit only to a fork, opens a PR on the parent, and compares it through the parent with the head given both as the fork ref and as the 40-hex head SHA (native review compares SHAs, the client's `internal/reviewadmission/prepare.go:32`). Merges are per path: a path changed on only one side takes that side, a path changed on both sides to different content is a conflict (`ponytail`: line-level merge later if a client needs it).

### 6.5 Webhooks and renderers

```rust
pub struct Event {                                 // in hubdouble-model
    pub name: &'static str,
    pub action: Option<&'static str>,
    pub repo_id: Option<i64>,
    pub org_id: Option<i64>,
    pub sender_id: i64,
    pub app_id: Option<i64>,          // set with installation_id for installation-family events: only that App gets it, for that installation
    pub installation_id: Option<i64>,
    pub payload: serde_json::Map<String, serde_json::Value>, // built from hubdouble-render structs; hooks adds installation, repository, organization, sender
}
pub fn emit(tx: &mut TxMut<'_>, e: Event);         // in hubdouble-hooks; every track and P0-C emit through it; URLs from Urls::new(tx.config(), "")
pub fn ping(tx: &mut TxMut<'_>, app_id: i64);
pub fn redeliver(tx: &mut TxMut<'_>, app_id: i64, delivery_id: i64) -> Result<(), HooksError>; // same GUID, new delivery ID
impl Handle { pub async fn drain(&self); pub async fn reset(&self, s: &Snapshot); } // admin reset calls Handle::reset, never Store::restore directly
```

`emit` renders the payload and resolves the recipients (the App and installation when set, otherwise every App installed on the account or repository that subscribes) inside the transaction and pushes one `OutboxEntry` per recipient with `TxMut::push_outbox`, so an `installation.deleted` or `installation_repositories.removed` event carries the installation and repositories as they were before the change, and a rolled-back `update` delivers nothing. A test installs two Apps on one account, deletes one installation, and checks that only that App receives the event, with the deleted installation's data.

`hubdouble-render` holds the frozen REST shapes for every entity in 6.1, each written once and used both in REST answers and in webhook payloads. A track adds render structs only for its own new resources, in its own crate.

### 6.6 Admin, seed and track registration

```rust
pub type KindApply = fn(&mut SeedCtx<'_>, serde_json::Value) -> Result<(), ApiError>; // SeedCtx: TxMut, Git, clock, env lookup
pub struct Services { pub store: Store, pub git: Git, pub hooks: hooks::Handle, pub clock: Arc<dyn Clock>, pub config: Arc<model::Config> } // built by the server after the registries freeze
pub struct AdminRequest { pub request: Request<Incoming>, pub params: Params, pub services: Arc<Services> }
pub type AdminResponse = hyper::Response<http_body_util::Full<bytes::Bytes>>;
pub type AdminHandler = Arc<dyn Fn(AdminRequest) -> Pin<Box<dyn Future<Output = AdminResponse> + Send>> + Send + Sync>;
impl Admin {
    pub fn kind(&mut self, name: &'static str, apply: KindApply);    // seed kind and POST /_hubdouble/{name}
    pub fn route(&mut self, method: Method, path: &str, h: AdminHandler); // extra admin routes under /_hubdouble/
}
pub struct Registrar<'a> {                          // in hubdouble-admin; handed to each track crate's `pub fn register(r: &mut Registrar)`
    pub store: &'a mut StoreBuilder, pub rest: &'a mut rest::Registry, pub gql: &'a mut gql::Registry,
    pub git: &'a mut GitBuilder, pub admin: &'a mut Admin,
}
```

Each track registers the tables, operations, GraphQL types, fields and mutations, ref subscribers, kinds and admin routes for what it owns in its one `register` function; `hubdouble-api::register_all` calls them in the fixed order of section 5, after P0-F's own kinds; kinds apply in that registration order, as section 4.6 states. No crate holds a global registry, a `static` with interior mutability or a `lazy` value a test changes: every registry is built at start and frozen behind an `Arc`, so each test builds its own server.

## 7. Work plan

### Phase 0: the frame (blocks phase 1)

First P0-A writes every contract in section 6 as compiling Rust stubs (bodies that return a `NotImplemented` error or `todo!()` only where no test reaches them) and merges them (hours, not days). Then P0-B to P0-F run in parallel against the stubs. Phase 0 ends with one exit test: a seeded App and repo, a branch push through `update_ref`, a test-only ref subscriber standing in for T5's that records the move, the visible and hidden alias GraphQL check from 4.2 and the denied mutation, both through test-only resolvers on a test registry (so P0-B registers no field T4 or T10 own), and a signed `push` delivery logged and drained, all green on `main`.

| Task | Owns | Delivers |
| --- | --- | --- |
| P0-A frame | the workspace, `xtask gen`, `model`, `store`, `render`, `spec`, `rest`, `server`, `api`, `hubdouble`, one empty `api-<track>` crate per track, and the stub `contract.rs` of `gql`, `git`, `auth`, `hooks` and `admin` | `Cargo.toml` workspace with the lint table, `rust-toolchain.toml`, `mise.toml`, `tasks.toml`, `deny.toml`, `rustfmt.toml`, CI, Dockerfile, license, `AGENTS.md`; contract stubs; generated router and full table with `501` fallback; middleware, error writer, ETag, `/_blobs`; store with undo log and frozen entities; render structs for every frozen entity; `hubdouble coverage` |
| P0-B GraphQL engine | `crates/gql`, the embedded schema in `crates/spec` | first a spike, reported to the coordinator before anything builds on it: `apollo-compiler` reports the field-merge conflict of 4.7 as a diagnostic it can narrow by kind, naming both fields' parent types through its public API (else P0-B re-checks the two selections itself at the diagnostic's locations), and the parsed GitHub schema fits the memory and start targets of section 9; then load and validate with GitHub's rule set, executor (fragments, aliases, variables, `@include`, `@skip`, interfaces, unions, non-null propagation), registry, connections, `node`/`nodes`, legacy ID warning, serial mutations, benchmarks |
| P0-C git | `crates/git` | section 6.4 on the in-memory object store, ref subscribers, `push` and `create`, web-flow signing and verification states, compare with patches, three-way merge proven on divergent branches and a conflict, first parent, `204` case, benchmarks |
| P0-D auth | `crates/auth` | all token kinds, JWT rules, installation narrowing, the choke point, the registry walk test, SSO enforcement hook |
| P0-E webhooks | `crates/hooks` | `emit` and recipients, outbox publisher, bounded queue, workers, signing, headers, delivery log, redelivery, drain, `ping` |
| P0-F admin, seed and testkit | `crates/admin`, `crates/testkit` | admin frame, seed loader with given IDs, kinds for accounts, orgs, repos with files, Apps (each with its bot account `<slug>[bot]`, `type: Bot`, which App-made comments and reactions name), installations, tokens; the post-seed `Store::snapshot` and reset through `Store::restore`; reset; faults; log and wait; testkit server, clients, receiver, validators, fixture seed, and test-only GraphQL parent resolvers (`Query.repository`, `Repository.pullRequest`, `Repository.issue`, `Repository.object`), each added to a test registry only when a test asks for it, so a track that owns one of those fields registers its own and opts out of the stand-in (a test registry is built from the phase 0 frame, the track under test and the stand-ins it opts into, never from `register_all`, so it still builds once every track has merged), so each track tests the fields it owns without another track's resolvers |

### Phase 1: tracks (parallel, 12 agents)

Each track owns its `crates/api-<track>/` crate (section 5) with its side tables and its unit tests, which use only the crates above it in section 5 (a track crate never takes `testkit` as a dev-dependency, because `testkit` depends on every track through `api`), and its tests through the whole server, with `testkit`, in `crates/conformance/tests/<track>.rs`, and implements its REST operations, GraphQL types or fields, events (section 4.3), response variants (section 4.8), admin and seed kinds, at the fidelity of section 4.7.

| Track | REST operations (from the inventory) | GraphQL documents its fields serve |
| --- | --- | --- |
| T1 Apps and installations | `/app`, `/apps/{slug}`, `/app/installations`, access tokens, `DELETE /installation/token`, `/installation/repositories`, `/orgs/{org}/installation`, `/repos/{o}/{r}/installation`, hook deliveries list, get, redeliver | none |
| T2 OAuth and users | `/login/oauth/*`, `/login/device/*`, `/orgs/{org}/sso`, `/user`, `/users/{username}`, `/user/{account_id}` | none |
| T3 Orgs and teams | `/orgs/{org}/members/{u}`, `/orgs/{org}/teams`, team by slug, team memberships, team repo check, `collaborators/{u}/permission` | `teams`, `access` |
| T4 Git data and contents | git blobs, trees, commits, tags, refs, matching-refs, `contents/{path}`, `commits`, `commits/{ref}`, `compare/{basehead}`, `merges`, `branches/{branch}` | `createCommit`, `head`, `updateRefs`, `workflows`, `defaultHead`, `peelTags`, `tags`, `releasesProbe`, `filesAt`, `holdsFiles`, `versionRef`, `versionFiles` |
| T5 Pull requests | pulls list, create, get, update, `files`, `commits`, `commits/{sha}/pulls`; also `merge` (method `merge` only) for scripts, not counted in the client gate | `ready`, `pullOfNode` (PR part), `proposalOpen`, `implementPull` (PR fields) |
| T6 Reviews and threads | reviews list, create, get, delete pending, submit; review comments list (PR and repo-wide), create (line, file, reply), get, reactions; requested reviewers get, add, remove | `fileThread`, `resolveThread`, `threads`, `threadOpenings`, `contextThreads`, `reviewBodies`, `planThreads`, `implementThreadComments`, `pullOfNode` (thread part), `implementPull` (threads) |
| T7 Issues and comments | issues list, create, get, update, `events`, labels (repo and issue), issue comments (issue and repo-wide) create, get, update, delete, reactions | `lastEdits`, `originalCommentEdit`, `implementIssue` |
| T8 Checks and statuses | check runs create, get, update, by ref; check suites by ref; statuses create and by ref; combined status | rollup part of `proposals` |
| T9 Actions and OIDC | workflow runs, dispatches (by ID or file name), runs, attempts, jobs, job logs, cancel, rerun-failed-jobs, repo secrets and variables, organization secrets and variables | none |
| T10 Repositories, rules, releases | `repos/{o}/{r}`, `repositories/{id}`, `orgs/{org}/repos`, `activity`, `codeowners/errors`, `rulesets/{id}`, `rules/branches/{branch}`, releases list, by tag, assets | `Repository` part of every document |
| T11 Search | none | `proposals` (search part): `search(type: ISSUE)` with `repo:`, `org:`, `is:pr`, `is:issue`, `is:open`, `is:closed`, `is:merged`, `is:draft`, `head:`, `base:`, `author:`, `label:`, `in:title`, `in:body`, `sort:` |
| T12 Kubernetes and examples | none | none; delivers the Kubernetes manifest, image publishing, seed reference doc, and `examples/client-v5.yaml` as section 4.9 specifies, with its seed tests |

A track that needs another track's data in tests writes it through the store and git contracts, never through the other track's handlers, so no track waits for another. A track proves its own fields with documents that select only fields it owns (plus P0-B's); the full inventory documents, which cross tracks, gate in phase 2.

### Phase 2: integration (one agent and the coordinator)

- Every conformance case green, `hubdouble coverage --inventory` at 100 %.
- Benchmarks against section 9, including parallel writes with one slow webhook receiver to measure write latency, queue depth and outbox memory; profile and fix.
- `xtask inventory` against the client's current `main`; close any new gap.
- Kubernetes smoke test in kind with `examples/client-v5.yaml`: App token exchange, a PR, a review thread, a check run, a dispatched workflow with an OIDC token. With the section 1.2 prerequisite merged, the same test runs the real client service: a script comments `/agent review` on a PR, and the client dispatches exactly one run of `agent.yml`. The client skips dispatch while `CLIENT_DRY_RUN` is on (its default, `internal/config/config.go:252`), while the target project is off the control plane (`internal/webhook/handler.go:1248`) and while the comment path is not armed for it (`internal/webhook/handler.go:1577`), so the real-service step does not copy those steps: it runs the client's own `mise run e2e:up` and local smoke against the section 4.10 contract, owned by the client's local end-to-end session. That task deploys with `app.dryRun=false`, `app.admitWorkRequests=true` and run settings seeded with the engine `opencode2` (with admission off, its chart default, or another engine, the client writes a legacy record instead of an intake row, `helm/client/values.yaml:82`, `internal/requestintake/ordinary_record.go:96`), and `CLIENT_VERSIONS_ENGINE=true`, so the versions measure lists the installations from hubdouble, reads the target's `.github/workflows/agent.yml` pin and comment, and puts the repository on the line, which arms the comment path (the client's `internal/repoconfig/set.go:331`). It waits for the measure row, posts `/agent review`, waits until hubdouble's deliveries API records the client's `status_code`, and checks for the intake row, posting once more if the first comment came before the repository was on the line. It ends at the client's core-review job row, found by the intake's native job ID (a native core-review job has no task, the client's `internal/jobs/review.go:314`, so a lookup by task ID never finds it), on its `reviewer` instance (the core-review playbook's dispatch stage, the client's `internal/playbook/core_review.go:14`, copied to the instance ID at `internal/jobs/state.go:23`; `review` is the job's native kind, not a stage): the attempt's `dispatched_run_id` equals the one run hubdouble created for `agent.yml`, and `unbound` is `0` (an absent field, because the client omits it at zero, `internal/jobs/jobs.go:229`). Nothing runs the dispatched workflow in this step (no runner, no `octo-org/runner` actions), so no run OIDC token reaches the client; that check comes with the runner, the OIDC issuer override and a model stub, whose contract the client's session sends later. hubdouble supplies the seed, the App ID `1000001` and those delivery fields.
- User docs: README, configuration, seed reference, admin API reference, fidelity notes.

### How the swarm runs

- One coordinator (the session that owns this plan) starts each agent with a self-contained brief: this plan, the task row, owned paths, the inventory items, and the definition of done.
- Each agent works in its own worktree on its own branch (`p0-b-gql`, `t5-pulls`), commits signed with sign-off, runs `mise run check`, and pushes the branch. The coordinator merges branches into `main` in dependency order, re-runs CI, and removes each worktree and its `target/` once merged.
- Agents do not edit files they do not own. A contract change goes to the coordinator. The files every track appears in, `crates/api/Cargo.toml` and `register_all`, already hold each track's line, written by P0-A, so no track edits them.
- Each worktree has its own `target/` (Cargo's default; no shared or redirected target or cache directory). The dev profile uses `debug = "line-tables-only"` and the test profile builds dependencies with `opt-level = 1`, which keeps each `target/` small and the tests fast; the coordinator runs at most six agents at a time and checks free disk space before it starts each wave.
- Models: phase 0 on `claude-opus-5-5` (provider `claudeAgent`, effort `high`); phase 1 tracks split between `claude-opus-5-5` and `gpt-6.2-sol` (provider `codex`, reasoning effort `high`).

### Engineering rules every agent follows

P0-A writes these into `AGENTS.md`; the workspace lint table and CI enforce what a tool can.

- Lints: `unsafe_code = "forbid"`, Clippy `pedantic` as warnings with `-D warnings` in CI, `unwrap_used` and `expect_used` denied outside tests, `print_stdout` and `dbg_macro` denied; a lint is allowed only on the item that needs it, with `#[expect(..., reason = "...")]`.
- Errors: `thiserror` enums per crate in libraries, `ApiError` at the HTTP edge; no `anyhow` in library crates.
- No code comments beyond `///` doc comments on public items where they add a contract the signature does not show, and `Given`, `When`, `Then` markers in tests. Reasoning goes in commit messages.
- No global mutable state: no `static` with interior mutability, no `OnceLock` or `LazyLock` a test can change; a fixed `Regex` or table may be a `LazyLock` that nothing writes. Timeouts, limits, clocks and environment lookups are fields or parameters, the environment read through a `Fn(&str) -> Option<String>`.
- Tests never wait for real time: the manual `Clock` for wall time, and `#[tokio::test(start_paused = true)]` with `tokio::time` for timers, deadlines and pauses. A test waits on the event it needs (a channel, a drained queue), never on a duration. A test server binds its listener on port 0 before it serves, and an in-process `hyper` service is used where the code takes a connector.
- Tests are independent and run in parallel under `cargo nextest`: no order between tests, no shared fixture a test changes, temporary files only under `tempfile::TempDir`. An expensive shared fixture is a `LazyLock` that no test changes.
- A test asserts behaviour (status, error variant, fields and values), never the wording of a message a person reads. Expected values come from GitHub's behaviour and this plan, never from the code under test.
- Performance: parse an input once and pass the parsed form, avoid per-request allocations on hot paths, and benchmark each hot path with `criterion`.

### Definition of done for a task

- Every listed operation registered with a `Requirement`; every listed GraphQL type, field and mutation registered; every owned event emitted from every cause.
- Tests per operation: success, `404` for an unseen repository, `403` for a missing permission, `422` where the operation validates input, and each response variant.
- Every query parameter and body option the client passes is honoured and tested against records that match only some of it: among them recursive trees (`internal/dispatch/autofix_paths.go:103`), check runs by `check_name`, `app_id` and `filter` (`internal/dispatch/checks.go:88`), workflow runs by `event`, `created`, `status`, `branch` and `head_sha` (`internal/dispatch/work.go:415`), and `per_page` with `page`. The conformance case uses the same options as the client call it mirrors. Response bodies validated against the OpenAPI schema, payloads against the octokit schema, the track's inventory documents executed in conformance.
- `mise run check` green: `cargo fmt --check`, Clippy clean, `cargo deny check`, `cargo nextest run` and doc tests pass, no sleeps or real-time waits in tests, benchmarks for any hot path the track adds.

## 8. Conformance suite

`crates/conformance` runs against an in-process server, one fresh server per test, seeded with a fixed fixture: two orgs (one SSO-enforced), users, teams with a parent, a public and a private repository with workflows, templates, tags and releases, an App with a narrowed installation, and a proposal PR on a `prompts/proposal-` branch that only the `refs` part of `proposals` finds, not its `search` part.

- REST: one case per inventory route and per variant in section 4.8, made with the exact request go-github v93 or the client's raw call sends (method, escaped path, query, `Accept`, `X-GitHub-Api-Version` and body, built by the test kit's request builders and cited to the go-github or the client source line), asserting status, schema validity and the fields the client reads. go-github itself drives hubdouble in the phase 2 smoke, where the real client runs.
- Merging: a PR merged through `POST /merges` into its base and through `PUT .../merge` reports `merged`, `mergedAt`, `mergeCommit` and GraphQL `state: MERGED` and sends `pull_request.closed` with `merged: true`.
- Dispatch retry, with a repository-scoped installation token holding `actions: write` as the client mints it: a dispatch with inputs to the v5 caller gets the `Unexpected inputs` `422` naming each sent input and creates no run, and the retry with empty inputs and `return_run_details: true` answers `200` with `workflow_run_id`, `run_url` and `html_url`; the repository then holds exactly one run.
- Reset keeps sequences: read the delivery list's newest ID, reset, cause a failed delivery, and the walk from the old newest ID finds it and still reaches an ID at or below the old floor; a delivery page cursor saved before the reset still pages on; an unknown cursor answers `422`; a run created after the reset has an ID above every run ID from before it.
- Actions: a dispatched run moved to a conclusion sends `workflow_run.completed` with the fields in 4.3.
- Proposals: `proposals` and `proposalOpen` assert `state`, `reviewDecision` (`APPROVED`, `REVIEW_REQUIRED`) and `statusCheckRollup.state` (`SUCCESS`, `PENDING`) against seeded reviews and checks.
- GraphQL: each document in `docs/inventory/graphql/` validated with GitHub's rule set and executed with realistic variables, asserting no errors and the values the client reads. Run-time built documents are kept as rendered samples (`filesAt`, `holdsFiles`, `versionRef`, `versionFiles`, `peelTags`, `access`, `implementIssue`) and also run with one alias and with 100 aliases, the most that the client sends (`internal/dispatch/repository_access.go:131`). `access` runs with one visible and one hidden repository.
- Webhooks: a scenario per event in section 4.3 asserting delivery, headers, signature and payload schema; redelivery keeps the GUID.
- Flows: App JWT to installation token to write; OAuth web with PKCE; SSO; device flow including `slow_down`; refresh; an OIDC token verified with a stock OIDC verifier for every claim in section 4.5.

`xtask inventory` keeps the inventory honest: it parses every Go package of a client checkout with `tree-sitter-go` (not only `internal/dispatch`; `internal/proposals` and `internal/versioning` send documents too), maps every go-github method call to its operation through the `//meta:operation` comments in the go-github source at the version the client's `go.mod` pins (fetched once from the Go module proxy into `target/`), fails on any call on a go-github service it cannot map to one operation (the parser has no type information, so it never skips one silently; a method with two `//meta:operation` routes, such as `Issues.ListComments` for one issue or for the whole repository, is resolved from its arguments, with a test for each route), finds raw request paths and every `graphql` call site, and flags query text built with `fmt.Sprintf`, `+` or `strings.Builder`, so a new run-time document gets a rendered sample.

## 9. Performance targets

Measured by `mise run bench` on one CPU, gated in CI against `main` with a 20 % regression budget.

| Measure | Target |
| --- | --- |
| Image size | under 20 MB |
| Start to ready, fixture seed | under 200 ms |
| Idle memory | under 40 MB RSS, the parsed GraphQL schema included |
| REST `GET` in process | p50 under 100 µs, p99 under 1 ms |
| Inventory GraphQL document | p99 under 3 ms |
| Mixed inventory load, reads and writes | over 5 000 requests per second |
| Webhook enqueue | under 20 µs per event |

Measured on the release profile (`lto = "fat"`, `codegen-units = 1`, `panic = "unwind"`, and the server wraps each handler call in `catch_unwind`, so a panicking handler rolls back and answers `500` without dropping the connection or ending the process). If parsing GitHub's GraphQL schema at start breaks the start target, `xtask gen` precomputes what the executor needs at build time.

## 10. Risks

| Risk | Answer |
| --- | --- |
| Executor gaps against GitHub's semantics | conformance runs real client documents; the executor's tests run the GraphQL spec's examples |
| Validation differs from GitHub's | the exception list in 4.7, each entry confirmed with a real GitHub call |
| Render structs drift from GitHub | every response validated against the pinned OpenAPI schema |
| One global lock | fine for one local client set; `ponytail` note with the upgrade path (a lock per repository) if a benchmark shows contention |
| Swarm merge conflicts | disjoint ownership, frozen contracts, stubs merged first |
| Spec updates change shapes | specs pinned; a bump is its own change with conformance re-run |

## 11. Growing to all of GitHub

- Every operation already routes and reports `501` with its operation ID, and `hubdouble coverage` lists what is left by category.
- A new operation is a handler, a `Requirement`, render structs and tests in the matching track crate.
- `xtask gen` can later generate render structs from the OpenAPI schemas.
- Next candidates, each a separate change: git smart HTTP, persistence, PR merge methods and line-level merges, repository and organization webhooks, REST search, GraphQL introspection, and a record mode that runs the conformance suite against real GitHub with a sandbox org.

## Council review

Rounds: 16

| Delegate | Round 1 | Round 2 | Round 3 | Round 4 | Round 5 | Round 6 | Round 7 | Round 8 | Round 9 | Round 10 | Round 11 | Round 12 | Round 13 | Round 14 | Round 15 | Round 16 |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| Claude Opus 5.5 | 6/10 | 7/10 | 9/10 | 9/10 | 9/10 | 9/10 | 9/10 | 9/10 | 9/10 | 9/10 | 9/10 | 9/10 | 8/10 | 8/10 | 9/10 | 9/10 |
| GPT-6.2 Sol | 5/10 | 7/10 | 8/10 | 8/10 | 8/10 | 8/10 | 8/10 | 9/10 | 8/10 | 9/10 | 8/10 | 9/10 | 8/10 | 9/10 | 8/10 | 9/10 |
| Grok 4.7 | 6/10 | 7/10 | 8/10 | 9/10 | 9/10 | 8/10 | 9/10 | 8/10 | 9/10 | 8/10 | 9/10 | 9/10 | 9/10 | 9/10 | 9/10 | 9/10 |

Rounds 1 to 12 reviewed a Go draft. Rounds 13 to 16 reviewed this Rust plan, and all three approved round 16.
