/*!
  updater helps running brupop on existing EKS cluster and clean up all
  resources once completing integration test
!*/

use lazy_static::lazy_static;
use snafu::{ensure, ResultExt};
use std::process::Command;

const BRUPOP_NAMESPACE: &str = "brupop-bottlerocket-aws";

lazy_static! {
    static ref BRUPOP_CLUSTER_ROLES: Vec<&'static str> = {
        let mut m = Vec::new();
        m.push("brupop-apiserver-role");
        m.push("brupop-agent-role");
        m.push("brupop-controller-role");
        m
    };
}

lazy_static! {
    static ref BRUPOP_CLUSTER_ROLE_BINDINGS: Vec<&'static str> = {
        let mut m = Vec::new();
        m.push("brupop-apiserver-role-binding");
        m.push("brupop-agent-role-binding");
        m.push("brupop-controller-role-binding");
        m
    };
}

// installing brupop on EKS cluster
pub async fn run_brupop(kube_config_path: &str) -> UpdaterResult<()> {
    let brupop_resource_status = Command::new("kubectl")
        .args([
            "apply",
            "-f",
            "yamlgen/deploy/bottlerocket-update-operator.yaml",
            "--kubeconfig",
            kube_config_path,
        ])
        .status()
        .context(update_error::BrupopProcess)?;

    ensure!(brupop_resource_status.success(), update_error::BrupopRun);

    Ok(())
}

// destroy all brupop resources which were created when integration test installed brupop
pub async fn delete_brupop_cluster_resources(kube_config_path: &str) -> UpdaterResult<()> {
    // delete namespaces brupop-bottlerocket-aws. This can clean all resources under this namespace like daemonsets.apps
    let namespace_deletion_status = Command::new("kubectl")
        .args([
            "delete",
            "namespaces",
            BRUPOP_NAMESPACE,
            "--kubeconfig",
            kube_config_path,
        ])
        .status()
        .context(update_error::BrupopCleanUp {
            cluster_resource: "namespaces",
        })?;
    ensure!(namespace_deletion_status.success(), update_error::BrupopRun);

    // delete clusterrolebinding.rbac.authorization.k8s.io
    for cluster_role_binding in BRUPOP_CLUSTER_ROLE_BINDINGS.iter() {
        let clusterrolebinding_deletion_status = Command::new("kubectl")
            .args([
                "delete",
                "clusterrolebinding.rbac.authorization.k8s.io",
                cluster_role_binding,
                "--kubeconfig",
                kube_config_path,
            ])
            .status()
            .context(update_error::BrupopCleanUp {
                cluster_resource: "clusterrolebinding.rbac.authorization.k8s.io",
            })?;
        ensure!(
            clusterrolebinding_deletion_status.success(),
            update_error::BrupopRun
        );
    }

    // delete clusterrole.rbac.authorization.k8s.io
    for cluster_role in BRUPOP_CLUSTER_ROLES.iter() {
        let clusterrole_deletion_status = Command::new("kubectl")
            .args([
                "delete",
                "clusterrole.rbac.authorization.k8s.io",
                cluster_role,
                "--kubeconfig",
                kube_config_path,
            ])
            .status()
            .context(update_error::BrupopCleanUp {
                cluster_resource: "clusterrole.rbac.authorization.k8s.io",
            })?;
        ensure!(
            clusterrole_deletion_status.success(),
            update_error::BrupopRun
        );
    }

    Ok(())
}

/// The result type returned by instance create and termiante operations.
type UpdaterResult<T> = std::result::Result<T, update_error::Error>;

pub mod update_error {
    use snafu::Snafu;

    #[derive(Debug, Snafu)]
    #[snafu(visibility = "pub")]
    pub enum Error {
        #[snafu(display("Failed to install brupop: {}", source))]
        BrupopProcess { source: std::io::Error },

        #[snafu(display("Failed to run brupop test"))]
        BrupopRun,

        #[snafu(display("Failed to deleted resource {}: {}", cluster_resource, source))]
        BrupopCleanUp {
            cluster_resource: String,
            source: std::io::Error,
        },
        #[snafu(display("Unable to convert kubeconfig path to string path"))]
        ConvertPathToStr {},
    }
}
