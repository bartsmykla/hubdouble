# Configuration

Set the server variables before you start `hubdouble serve`. A server variable that is empty or contains only whitespace uses its default.

| Variable | Default | Effect |
| --- | --- | --- |
| `HUBDOUBLE_ADDR` | `0.0.0.0:8000` | Set the listen address as `:port` or an IP socket address. `:port` binds all IPv4 interfaces. |
| `HUBDOUBLE_BASE_URL` | `http://localhost:8000` | Set the advertised API URL and the base for pages served by hubdouble. This value does not change the listen address. |
| `HUBDOUBLE_WEB_URL` | The value of `HUBDOUBLE_BASE_URL` | Set the base for `html_url` and other links to GitHub resources. |
| `HUBDOUBLE_OIDC_ISSUER` | `HUBDOUBLE_BASE_URL` plus `/_actions` | Set the discovery issuer, the JWKS URL base, and the `iss` token claim. The URL must end in `/_actions`. hubdouble serves the OIDC routes under `/_actions`. |
| `HUBDOUBLE_ADMIN_TOKEN` | `hubdouble-admin` | Set the credential for the [admin API](admin-api.md). |
| `HUBDOUBLE_SEED` | Unset | Load a YAML or JSON [seed file](seed.md) at startup. With no seed file the store starts empty. |
| `HUBDOUBLE_WORKERS` | Tokio's default worker count | Set the number of Tokio runtime workers. Use a positive integer. |

Use an `http://` or `https://` URL for the base, web, and issuer URLs. The server removes trailing slashes. Use a separate admin token if other processes can access the server. For a loopback-only Docker port, follow the [Docker quick start](../README.md#docker-quick-start).

## client v5 seed

The [example seed](../examples/client-v5.yaml) reads these values from the environment at startup. They have no server defaults. If a value is absent or empty, this seed fails to load.

| Variable | Effect |
| --- | --- |
| `HUBDOUBLE_APP_PUBLIC_KEY` | Set the PEM public key for App `1000001`. The server verifies App JWTs with this key. |
| `HUBDOUBLE_APP_WEBHOOK_URL` | Set the App webhook target. Use an `http://` target to receive deliveries. |
| `HUBDOUBLE_APP_WEBHOOK_SECRET` | Set the secret for the webhook payload signature. |
| `HUBDOUBLE_USER_TOKEN` | Set the personal access token for `local-user`. |

The [Kubernetes Deployment](../deploy/kubernetes/deployment.yaml) advertises `http://hubdouble:8000` as the API base and `https://github.com` as the web URL. The [Secret template](../deploy/kubernetes/secret.yaml) supplies `http://client/webhook` as the webhook target. The smoke task supplies test credentials when it deploys these manifests.

The conformance fixture supplies `HUBDOUBLE_TEST_WEBHOOK_URL` for its temporary webhook receiver. This variable has no server default.
