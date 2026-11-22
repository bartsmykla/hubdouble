# hubdouble

A GitHub API double: one static binary that serves GitHub's REST API, GraphQL API, webhooks, OAuth and Actions from an in-memory store, for local end-to-end tests of GitHub Apps.

Work in progress. `docs/PLAN.md` describes the design and the work plan.

## Run

```sh
mise run build
HUBDOUBLE_ADDR=:8000 target/release/hubdouble
```

Or with Docker:

```sh
docker build -t hubdouble .
docker run --rm -p 8000:8000 hubdouble
```

`hubdouble coverage` lists the share of GitHub's REST operations that hubdouble implements. `hubdouble coverage --inventory docs/inventory/rest-routes.txt` also lists each route of the inventory that is not implemented yet.

## Develop

```sh
mise install
mise run check
```

## License

MIT. The files under `spec/` keep their own licenses.
