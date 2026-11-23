# hubdouble

hubdouble is a local GitHub API double for end-to-end tests of GitHub Apps. It serves the REST and GraphQL operations that its first client uses. It also provides App tokens, OAuth, webhooks, and Actions workflow runs and OpenID Connect (OIDC) tokens. It keeps state in memory.

## Docker quick start

Start Docker. Run these commands from the repository root.

```sh
docker build -t hubdouble:quickstart .
docker run --rm -p 127.0.0.1:8000:8000 hubdouble:quickstart
```

Check readiness from a second terminal. The result is `ok`.

```sh
curl --fail --silent --show-error http://localhost:8000/readyz
```

Press Ctrl+C in the first terminal to stop the container. The default server starts with an empty store. See the [seed reference](docs/seed.md) to supply test data.

## kind quick start

Install mise, Rust, Docker, kind, and kubectl. Start Docker. Allow access to the image registries and Rust crate registry for the first build.

```sh
mise run smoke:kind
```

The task builds a local image and deploys the [client v5 seed](examples/client-v5.yaml) in a temporary kind cluster. It checks App authentication, the seeded pull request, a review thread, a check run, and a workflow dispatch. It verifies the runner OIDC token with the public key from the JSON Web Key Set (JWKS).

The task uses a separate kubeconfig and the `kind-hubdouble-smoke` context. If the cluster already exists, the task stops. The task removes its cluster and image after the checks, even if a check fails.

## Commands

| Command | Result |
| --- | --- |
| `mise run build` | Build the release binary at `target/release/hubdouble`. |
| `hubdouble --help` | Show the command syntax. |
| `hubdouble` or `hubdouble serve` | Start the server. |
| `hubdouble coverage` | Report the number and share of registered REST operations by category. |
| `hubdouble coverage --inventory docs/inventory/rest-routes.txt` | Check the REST inventory and its GraphQL endpoint. Return a nonzero exit code if a route is missing. |
| `mise run check` | Check formatting, Clippy, dependency policy, tests, and doc tests. |
| `mise run inventory -- --client <path> [--names <file>]` | Write `docs/inventory/` from the client checkout at `<path>`. The optional names file holds `from==>to` lines. The task replaces each literal `from` with `to`, in file order, in every generated text before it writes or compares. Use `mise run inventory:check` with the same arguments to fail on a difference. |

Run the `hubdouble` commands from your PATH or use `target/release/hubdouble` from the repository root.

## Reference

- [Configuration](docs/configuration.md)
- [Admin API](docs/admin-api.md)
- [Seed file format](docs/seed.md)
- [GitHub API fidelity](docs/fidelity.md)
- [Design and work plan](docs/PLAN.md)

## License

MIT. The files under `spec/` keep their own licenses.
