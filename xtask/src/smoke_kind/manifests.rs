use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use super::Result;

pub(super) struct Credentials {
    pub admin: String,
    pub user: String,
    pub webhook: String,
}

pub(super) fn paths(
    root: &Path,
    scratch: &Path,
    image: &str,
    credentials: &Credentials,
    public_key: &str,
) -> Result<[PathBuf; 4]> {
    let dir = root.join("deploy/kubernetes");
    let deployment = scratch.join("deployment.json");
    let secret = scratch.join("secret.json");
    std::fs::write(
        &deployment,
        serde_json::to_vec(&rewrite_deployment(
            &std::fs::read_to_string(dir.join("deployment.yaml"))?,
            image,
        )?)?,
    )?;
    std::fs::write(
        &secret,
        serde_json::to_vec(&rewrite_secret(
            &std::fs::read_to_string(dir.join("secret.yaml"))?,
            credentials,
            public_key,
        )?)?,
    )?;
    Ok([
        dir.join("configmap.yaml"),
        dir.join("service.yaml"),
        secret,
        deployment,
    ])
}

fn rewrite_deployment(yaml: &str, image: &str) -> Result<Value> {
    let mut doc: Value = serde_saphyr::from_str(yaml)?;
    let containers = doc
        .pointer_mut("/spec/template/spec/containers")
        .and_then(Value::as_array_mut)
        .ok_or("Deployment has no container list")?;
    let container = containers
        .iter_mut()
        .find(|value| value["name"] == "hubdouble")
        .ok_or("Deployment has no hubdouble container")?;
    container["image"] = json!(image);
    container["imagePullPolicy"] = json!("Never");
    Ok(doc)
}

fn rewrite_secret(yaml: &str, credentials: &Credentials, public_key: &str) -> Result<Value> {
    let mut doc: Value = serde_saphyr::from_str(yaml)?;
    let values = doc
        .get_mut("stringData")
        .and_then(Value::as_object_mut)
        .ok_or("Secret has no stringData")?;
    for (name, value) in [
        ("HUBDOUBLE_ADMIN_TOKEN", credentials.admin.as_str()),
        ("HUBDOUBLE_APP_PUBLIC_KEY", public_key),
        ("HUBDOUBLE_APP_WEBHOOK_SECRET", credentials.webhook.as_str()),
        ("HUBDOUBLE_USER_TOKEN", credentials.user.as_str()),
    ] {
        values.insert(name.to_owned(), json!(value));
    }
    Ok(doc)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deployment_uses_the_loaded_image_and_keeps_the_seed_volume() {
        let yaml = include_str!("../../../deploy/kubernetes/deployment.yaml");
        let mut actual = rewrite_deployment(yaml, "hubdouble-smoke:local").unwrap();
        let original: Value = serde_saphyr::from_str(yaml).unwrap();
        let container = actual
            .pointer_mut("/spec/template/spec/containers/0")
            .unwrap();
        assert_eq!(container["image"], "hubdouble-smoke:local");
        assert_eq!(container["imagePullPolicy"], "Never");
        container["image"] = json!("ghcr.io/bartsmykla/hubdouble:latest");
        container.as_object_mut().unwrap().remove("imagePullPolicy");
        assert_eq!(actual, original);
    }

    #[test]
    fn secret_uses_ephemeral_credentials_and_keeps_the_webhook_target() {
        let yaml = include_str!("../../../deploy/kubernetes/secret.yaml");
        let credentials = Credentials {
            admin: "test-admin".into(),
            user: "test-user".into(),
            webhook: "test-webhook".into(),
        };
        let key = "public\nkey\n";
        let doc = rewrite_secret(yaml, &credentials, key).unwrap();
        assert_eq!(doc["metadata"]["name"], "hubdouble-secrets");
        assert_eq!(
            doc["stringData"],
            json!({
                "HUBDOUBLE_ADMIN_TOKEN": "test-admin",
                "HUBDOUBLE_APP_PUBLIC_KEY": key,
                "HUBDOUBLE_APP_WEBHOOK_URL": "http://client/webhook",
                "HUBDOUBLE_APP_WEBHOOK_SECRET": "test-webhook",
                "HUBDOUBLE_USER_TOKEN": "test-user",
            }),
        );
    }
}
