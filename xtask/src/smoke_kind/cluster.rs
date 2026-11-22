use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt as _, BufReader, Lines};
use tokio::process::{Child, ChildStdout, Command};

use super::manifests::{self, Credentials};
use super::{CLUSTER, CONTEXT, Result, require};

const NODE_DIGEST: &str = "sha256:a1ed56cfb0e7b93589bdf97c8cd566405a265939e3620fc4f5de89adff580ae5";
const NODE_REPO: &str = "kindest/node";

pub(super) struct Cluster<'a> {
    root: &'a Path,
    scratch: &'a Path,
    kubeconfig: PathBuf,
    image: String,
    node_ref: String,
    owned: Owned,
    foreground: Option<Foreground>,
    forward: Option<Forward>,
}

struct Foreground {
    child: Child,
    finish_on_interrupt: bool,
}

struct Forward {
    child: Child,
    output: Lines<BufReader<ChildStdout>>,
}

#[derive(Clone, Copy, Default)]
struct Owned {
    cluster: bool,
    node_image: bool,
    app_image: bool,
}

#[derive(Debug, PartialEq, Eq)]
enum CleanupStep {
    PortForward,
    Cluster,
    NodeImage,
    AppImage,
}

impl Owned {
    fn cleanup_order(self, has_forward: bool) -> impl Iterator<Item = CleanupStep> {
        [
            (has_forward, CleanupStep::PortForward),
            (self.cluster, CleanupStep::Cluster),
            (self.node_image, CleanupStep::NodeImage),
            (self.app_image, CleanupStep::AppImage),
        ]
        .into_iter()
        .filter_map(|(owned, step)| owned.then_some(step))
    }
}

impl<'a> Cluster<'a> {
    pub(super) fn new(root: &'a Path, scratch: &'a Path) -> Self {
        Self {
            root,
            scratch,
            kubeconfig: scratch.join("kubeconfig"),
            image: format!("hubdouble-smoke:{:016x}", rand::random::<u64>()),
            node_ref: format!("{NODE_REPO}@{NODE_DIGEST}"),
            owned: Owned::default(),
            foreground: None,
            forward: None,
        }
    }

    fn command(&self, program: &str) -> Command {
        let mut cmd = Command::new(program);
        cmd.current_dir(self.root)
            .kill_on_drop(true)
            .env("KIND_EXPERIMENTAL_PROVIDER", "docker")
            .env("KUBECONFIG", &self.kubeconfig)
            .env("TMPDIR", self.scratch);
        #[cfg(unix)]
        cmd.process_group(0);
        cmd
    }

    fn kube(&self) -> Command {
        let mut cmd = self.command("kubectl");
        cmd.arg("--kubeconfig")
            .arg(&self.kubeconfig)
            .args(["--context", CONTEXT])
            .arg("--cache-dir")
            .arg(self.scratch.join("kubectl-cache"));
        cmd
    }

    async fn execute(&mut self, mut cmd: Command, finish_on_interrupt: bool) -> Result<()> {
        let label = format!("{cmd:?}");
        let command = self.foreground.insert(Foreground {
            child: cmd.spawn()?,
            finish_on_interrupt,
        });
        let status = command.child.wait().await?;
        self.foreground = None;
        require(status.success(), &format!("{label} exited with {status}"))
    }

    async fn require_absent(&self) -> Result<()> {
        let output = self
            .command("kind")
            .args(["get", "clusters"])
            .output()
            .await?;
        require(
            output.status.success(),
            &format!(
                "Could not list kind clusters: {}",
                String::from_utf8_lossy(&output.stderr)
            ),
        )?;
        require(
            !String::from_utf8(output.stdout)?
                .lines()
                .any(|name| name == CLUSTER),
            "Cluster hubdouble-smoke already exists",
        )
    }

    async fn image_exists(&self, image: &str) -> Result<bool> {
        let output = self
            .command("docker")
            .args(["image", "inspect", image])
            .output()
            .await?;
        if output.status.success() {
            return Ok(true);
        }
        let error = String::from_utf8_lossy(&output.stderr);
        if error.contains("No such image") {
            Ok(false)
        } else {
            Err(format!("Could not inspect Docker image {image}: {error}").into())
        }
    }

    pub(super) async fn start(&mut self) -> Result<()> {
        self.require_absent().await?;
        let mut build = self.command("docker");
        build.args(["build", "-t", &self.image, "."]);
        self.owned.app_image = true;
        self.execute(build, false).await?;
        self.require_absent().await?;
        self.owned.node_image = !self.image_exists(&self.node_ref).await?;
        let mut create = self.command("kind");
        create
            .args(["create", "cluster", "--name", CLUSTER, "--image"])
            .arg(format!("{NODE_REPO}:v1.37.0@{NODE_DIGEST}"))
            .arg("--kubeconfig")
            .arg(&self.kubeconfig)
            .args(["--wait", "120s"]);
        self.owned.cluster = true;
        self.execute(create, true).await?;
        let mut load = self.command("kind");
        load.args(["load", "docker-image", &self.image, "--name", CLUSTER]);
        self.execute(load, false).await
    }

    pub(super) async fn deploy(
        &mut self,
        credentials: &Credentials,
        public_key: &str,
    ) -> Result<()> {
        let paths = manifests::paths(
            self.root,
            self.scratch,
            &self.image,
            credentials,
            public_key,
        )?;
        let mut apply = self.kube();
        apply.arg("apply");
        for path in &paths {
            apply.arg("-f").arg(path);
        }
        self.execute(apply, false).await?;
        let mut rollout = self.kube();
        rollout.args([
            "rollout",
            "status",
            "deployment/hubdouble",
            "--timeout=120s",
        ]);
        self.execute(rollout, false).await
    }

    pub(super) async fn forward(&mut self) -> Result<u16> {
        let mut child = self
            .kube()
            .args([
                "port-forward",
                "--address",
                "127.0.0.1",
                "service/hubdouble",
                ":8000",
            ])
            .stdout(Stdio::piped())
            .spawn()?;
        let stdout = child.stdout.take().ok_or("kubectl has no stdout pipe")?;
        let forward = self.forward.insert(Forward {
            child,
            output: BufReader::new(stdout).lines(),
        });
        let line = tokio::time::timeout(Duration::from_secs(30), forward.output.next_line())
            .await??
            .ok_or("kubectl port-forward exited before it opened a local port")?;
        forward_port(&line)
    }

    pub(super) async fn diagnose(&mut self) {
        let mut pods = self.kube();
        pods.args(["get", "pods"]);
        let _ = self.execute(pods, false).await;
        let mut logs = self.kube();
        logs.args(["logs", "deployment/hubdouble", "--tail=50"]);
        let _ = self.execute(logs, false).await;
    }

    pub(super) async fn cleanup(&mut self) -> Result<()> {
        let mut errors = Vec::new();
        if let Some(mut command) = self.foreground.take() {
            let result = if command.finish_on_interrupt {
                command.child.wait().await.map(|_| ())
            } else {
                stop(&mut command.child).await
            };
            if let Err(error) = result {
                errors.push(error.to_string());
            }
        }
        for step in self.owned.cleanup_order(self.forward.is_some()) {
            if let Err(error) = self.cleanup_step(step).await {
                errors.push(error.to_string());
            }
        }
        require(errors.is_empty(), &errors.join("\n"))
    }

    async fn cleanup_step(&mut self, step: CleanupStep) -> Result<()> {
        match step {
            CleanupStep::PortForward => {
                if let Some(mut forward) = self.forward.take() {
                    stop(&mut forward.child).await?;
                }
                Ok(())
            }
            CleanupStep::Cluster => {
                let mut delete = self.command("kind");
                delete
                    .args(["delete", "cluster", "--name", CLUSTER, "--kubeconfig"])
                    .arg(&self.kubeconfig);
                self.execute(delete, false).await
            }
            CleanupStep::NodeImage => self.remove_image(&self.node_ref).await,
            CleanupStep::AppImage => self.remove_image(&self.image).await,
        }
    }

    async fn remove_image(&self, image: &str) -> Result<()> {
        if self.image_exists(image).await? {
            let status = self
                .command("docker")
                .args(["image", "rm", image])
                .status()
                .await?;
            require(
                status.success(),
                &format!("Could not remove Docker image {image}: {status}"),
            )?;
        }
        Ok(())
    }
}

#[cfg(unix)]
async fn stop(child: &mut Child) -> std::io::Result<()> {
    use rustix::io::Errno;
    use rustix::process::{Pid, Signal, kill_process_group};

    let Some(group) = child
        .id()
        .and_then(|id| i32::try_from(id).ok())
        .and_then(Pid::from_raw)
    else {
        return child.wait().await.map(|_| ());
    };
    let signal = |sig| match kill_process_group(group, sig) {
        Ok(()) | Err(Errno::SRCH) => Ok(()),
        Err(error) => Err(error),
    };
    signal(Signal::TERM)?;
    if let Ok(exit) = tokio::time::timeout(Duration::from_secs(5), child.wait()).await {
        return exit.map(|_| ());
    }
    signal(Signal::KILL)?;
    child.wait().await.map(|_| ())
}

#[cfg(not(unix))]
async fn stop(child: &mut Child) -> std::io::Result<()> {
    if child.try_wait()?.is_none() {
        child.kill().await?;
    }
    Ok(())
}

fn forward_port(line: &str) -> Result<u16> {
    let port = line
        .strip_prefix("Forwarding from 127.0.0.1:")
        .and_then(|line| line.strip_suffix(" -> 8000"))
        .ok_or("kubectl port-forward did not report a loopback port for hubdouble")?;
    Ok(port.parse()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use CleanupStep::{AppImage, NodeImage, PortForward};

    #[test]
    fn cleanup_stops_the_forward_before_the_cluster_and_the_images() {
        let owned = Owned {
            cluster: true,
            node_image: true,
            app_image: true,
        };
        assert_eq!(
            owned.cleanup_order(true).collect::<Vec<_>>(),
            [PortForward, CleanupStep::Cluster, NodeImage, AppImage]
        );
    }

    #[test]
    fn cleanup_skips_resources_that_were_not_acquired() {
        let after_build_failure = Owned {
            app_image: true,
            ..Owned::default()
        };
        assert_eq!(
            after_build_failure.cleanup_order(false).collect::<Vec<_>>(),
            [AppImage]
        );
        let with_existing_node_image = Owned {
            cluster: true,
            ..after_build_failure
        };
        assert_eq!(
            with_existing_node_image
                .cleanup_order(false)
                .collect::<Vec<_>>(),
            [CleanupStep::Cluster, AppImage]
        );
    }

    #[test]
    fn kubectl_always_uses_the_private_kubeconfig_and_context() {
        let scratch = Path::new("/temporary/smoke");
        let cluster = Cluster::new(Path::new("."), scratch);
        let cmd = cluster.kube();
        assert_eq!(cmd.as_std().get_program(), "kubectl");
        assert_eq!(
            cmd.as_std().get_args().collect::<Vec<_>>(),
            [
                "--kubeconfig",
                "/temporary/smoke/kubeconfig",
                "--context",
                "kind-hubdouble-smoke",
                "--cache-dir",
                "/temporary/smoke/kubectl-cache",
            ],
        );
    }
}
