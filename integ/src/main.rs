use log::info;
use snafu::{OptionExt, ResultExt};
use std::env::temp_dir;
use std::fs;
use std::process;
use structopt::StructOpt;

use integ::eks_provider::{get_cluster_info, write_kubeconfig};
use integ::error::ProviderError;
use integ::nodegroup_provider::{create_nodegroup, terminate_nodegroup};
use integ::updater::{delete_brupop_cluster_resources, run_brupop};

type Result<T> = std::result::Result<T, error::Error>;

/// The default path for kubeconfig file
const DEFAULT_KUBECONFIG_FILE_NAME: &str = "kubeconfig.yaml";

/// The default region for the cluster.
const DEFAULT_REGION: &str = "us-west-2";
const CLUSTER_NAME: &str = "brupop-integration-test";

//The default values for AMI ID
const AMI_ARCH: &str = "x86_64";

#[tokio::main]
async fn main() {
    env_logger::init();

    if let Err(e) = run().await {
        eprintln!("{}", e);
        process::exit(1);
    }
}

#[derive(StructOpt, Debug)]
pub(crate) struct Arguments {
    #[structopt(global = true, long = "--cluster-name", default_value = CLUSTER_NAME)]
    cluster_name: String,

    #[structopt(global = true, long = "--region", default_value = DEFAULT_REGION)]
    region: String,

    #[structopt(
        global = true,
        long = "--kube-config-path",
        default_value = DEFAULT_KUBECONFIG_FILE_NAME
    )]
    kube_config_path: String,

    #[structopt(subcommand)]
    subcommand: SubCommand,
}

#[derive(StructOpt, Debug)]
enum SubCommand {
    IntegrationTest(IntegrationTestArgs),
    Clean,
}

// Stores user-supplied arguments for the 'integration-test' subcommand.
#[derive(StructOpt, Debug)]
pub struct IntegrationTestArgs {
    #[structopt(long = "--bottlerocket-version")]
    bottlerocket_version: String,

    #[structopt(long = "--arch", default_value = AMI_ARCH)]
    ami_arch: String,
}

async fn generate_kubeconfig(arguments: &Arguments) -> Result<String> {
    // default kube config path is /temp/{CLUSTER_NAME}-{REGION}/kubeconfig.yaml
    let kube_config_path = generate_kubeconfig_file_path(&arguments).await?;

    // decode and write kubeconfig
    info!("decoding and writing kubeconfig ...");

    write_kubeconfig(
        &arguments.cluster_name,
        &arguments.region,
        &kube_config_path,
    )
    .context(error::WriteKubeconfig)?;
    info!(
        "kubeconfig has been written and store at {:?}",
        &kube_config_path
    );

    Ok(kube_config_path)
}

async fn generate_kubeconfig_file_path(arguments: &Arguments) -> Result<String> {
    let unique_kube_config_temp_dir = get_kube_config_temp_dir_path(&arguments)?;

    fs::create_dir_all(&unique_kube_config_temp_dir).context(error::CreateDir)?;

    let kube_config_path = format!(
        "{}/{}",
        &unique_kube_config_temp_dir, DEFAULT_KUBECONFIG_FILE_NAME
    );

    Ok(kube_config_path)
}

fn get_kube_config_temp_dir_path(arguments: &Arguments) -> Result<String> {
    let unique_tmp_dir_name = format!("{}-{}", arguments.cluster_name, arguments.region);
    let unique_kube_config_temp_dir = format!(
        "{}/{}",
        temp_dir().to_str().context(error::FindTmpDir)?,
        unique_tmp_dir_name
    );

    Ok(unique_kube_config_temp_dir)
}

async fn run() -> Result<()> {
    // Parse and store the args passed to the program
    let args = Arguments::from_args();

    let cluster_info = get_cluster_info(&args.cluster_name, &args.region)
        .await
        .context(error::GetClusterInfo)?;

    match &args.subcommand {
        SubCommand::IntegrationTest(integ_test_args) => {
            // generate kubeconfig if no input value for argument `kube_config_path`
            let kube_config_path: String = match args.kube_config_path.as_str() {
                DEFAULT_KUBECONFIG_FILE_NAME => generate_kubeconfig(&args).await?,
                res => res.to_string(),
            };

            // create instances via nodegroup and add nodes to eks cluster
            info!("Creating EC2 instances via nodegroup ...");
            create_nodegroup(
                cluster_info,
                &integ_test_args.ami_arch,
                &integ_test_args.bottlerocket_version,
            )
            .await
            .context(error::CreateNodeGroup)?;
            info!("EC2 instances/nodegroup have been created");

            // install brupop on EKS cluster
            info!("Running brupop on existing EKS cluster ...");
            run_brupop(&kube_config_path)
                .await
                .context(error::RunBrupop)?;
        }
        SubCommand::Clean => {
            // generate kubeconfig path if no input value for argument `kube_config_path`
            let kube_config_path: String = match args.kube_config_path.as_str() {
                DEFAULT_KUBECONFIG_FILE_NAME => generate_kubeconfig_file_path(&args).await?,
                res => res.to_string(),
            };

            // terminate nodegroup which created by integration test.
            info!("Terminating nodegroup ...");
            terminate_nodegroup(cluster_info)
                .await
                .context(error::TerminateNodeGroup)?;

            // clean up all brupop resources like namespace, deployment on brupop test
            info!("Deleting all brupop cluster resources which created by integration test ...");
            delete_brupop_cluster_resources(&kube_config_path)
                .await
                .context(error::DeleteClusterResources)?;

            // delete tmp directory and kubeconfig.yaml if no input value for argument `kube_config_path`
            if &args.kube_config_path == DEFAULT_KUBECONFIG_FILE_NAME {
                info!("Deleting tmp directory and kubeconfig.yaml ...");
                fs::remove_dir_all(get_kube_config_temp_dir_path(&args)?)
                    .context(error::DeleteTmpDir)?;
            }
        }
    }
    Ok({})
}

mod error {
    use crate::ProviderError;
    use integ::updater::update_error;
    use snafu::Snafu;

    #[derive(Debug, Snafu)]
    #[snafu(visibility = "pub(super)")]
    pub(super) enum Error {
        #[snafu(display("Failed to get eks cluster info: {}", source))]
        GetClusterInfo { source: ProviderError },

        #[snafu(display("Unable to create directory for storing kubeconfig file: {}", source))]
        CreateDir { source: std::io::Error },

        #[snafu(display("Failed to create node group: {}", source))]
        CreateNodeGroup { source: ProviderError },

        #[snafu(display("Failed to install brupop on eks cluster: {}", source))]
        RunBrupop { source: update_error::Error },

        #[snafu(display("Failed to terminate node group: {}", source))]
        TerminateNodeGroup { source: ProviderError },

        #[snafu(display("Failed to delete created eks cluster resources: {}", source))]
        DeleteClusterResources { source: update_error::Error },

        #[snafu(display("Failed to delete tmp directory and kubeconfig.yaml: {}", source))]
        DeleteTmpDir { source: std::io::Error },

        #[snafu(display("Unable to find temp directory"))]
        FindTmpDir {},

        #[snafu(display("Failed to write content to kubeconfig: {}", source))]
        WriteKubeconfig { source: ProviderError },

        #[snafu(display("Logger setup error: {}", source))]
        Logger { source: log::SetLoggerError },
    }
}
