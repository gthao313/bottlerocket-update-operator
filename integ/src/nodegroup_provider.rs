/*!
  ec2 provider helps launching bottlerocket nodes and connect to EKS cluster.
  Meanwhile, terminating all created ec2 instances when integration test is running 'clean' subcommand.
!*/

use aws_config::meta::region::RegionProviderChain;
use aws_sdk_ec2::model::{
    ArchitectureValues, InstanceType, LaunchTemplateTagSpecificationRequest,
    RequestLaunchTemplateData, ResourceType, Tag,
};
use aws_sdk_ec2::types::SdkError;
use aws_sdk_ec2::Region;
use aws_sdk_eks::model::{LaunchTemplateSpecification, NodegroupScalingConfig, NodegroupStatus};
use aws_sdk_iam::error::{GetInstanceProfileError, GetInstanceProfileErrorKind};
use aws_sdk_iam::output::GetInstanceProfileOutput;

use crate::eks_provider::ClusterInfo;
use crate::error::{IntoProviderError, ProviderError, ProviderResult};

use serde::{Deserialize, Serialize};
use std::fmt::Debug;
use std::process::Command;
use std::time::Duration;

/// The default number of instances to spin up.
const DEFAULT_INSTANCE_COUNT: i32 = 3;

/// The default resources to create nodegroup
const BRUPOP_INTERFACE_VERSION: &str = "2.0.0";
const IAM_INSTANCE_PROFILE_NAME: &str = "brupop-integ-test-eksNodeRole";
const INSTANCE_TAG_NAME: &str = "brupop";
const INSTANCE_TAG_VALUE: &str = "integration-test";
const LABEL_BRUPOP_INTERFACE_NAME: &str = "bottlerocket.aws/updater-interface-version";
const LAUNCH_TEMPLATE_NAME: &str = "brupop-integ-test";
const NODEGROUP_NAME: &str = "brupop-integ-test-nodegroup";
const EKS_WORKER_NODE_POLICY_ARN: &str = "arn:aws:iam::aws:policy/AmazonEKSWorkerNodePolicy";
const EKS_CNI_ARN: &str = "arn:aws:iam::aws:policy/AmazonEKS_CNI_Policy";
const EC2_CONTAINER_REGISTRY_ARN: &str =
    "arn:aws:iam::aws:policy/AmazonEC2ContainerRegistryReadOnly";
const SSM_MANAGED_INSTANCE_CORE_ARN: &str = "arn:aws:iam::aws:policy/AmazonSSMManagedInstanceCore";

// =^..^=   =^..^=   =^..^=   =^..^=   =^..^= Termination and Creation of NodeGroup  =^..^=   =^..^=   =^..^=   =^..^=   =^..^=

pub async fn create_nodegroup(
    cluster: ClusterInfo,
    ami_arch: &str,
    bottlerocket_version: &str,
) -> ProviderResult<()> {
    // Setup aws_sdk_config and clients.
    let region_provider = RegionProviderChain::first_try(Some(Region::new(cluster.region.clone())));
    let shared_config = aws_config::from_env().region(region_provider).load().await;
    let ec2_client = aws_sdk_ec2::Client::new(&shared_config);
    let ssm_client = aws_sdk_ssm::Client::new(&shared_config);
    let eks_client = aws_sdk_eks::Client::new(&shared_config);
    let iam_client = aws_sdk_iam::Client::new(&shared_config);

    // Prepare ami id
    //default eks_version to the version that matches cluster
    let eks_version = &cluster.version;
    let node_ami = find_ami_id(&ssm_client, ami_arch, bottlerocket_version, &eks_version).await?;

    // Prepare instance type
    let instance_type = instance_type(&ec2_client, &node_ami).await?;

    // create one time iam instance profile for nodegroup
    let iam_instance_profile_arn = create_iam_instance_profile(&iam_client).await?;

    // Mapping one time iam identity to eks cluster
    cluster_iam_identity_mapping(&cluster.name, &cluster.region, &iam_instance_profile_arn).await?;

    // Create nodegroup launch template
    let launch_template =
        create_launch_template(&ec2_client, &node_ami, &instance_type, &cluster.clone()).await?;

    // Create nodegroup on eks cluster
    eks_client
        .create_nodegroup()
        .launch_template(
            LaunchTemplateSpecification::builder()
                .id(&launch_template.launch_template_id)
                .version(&launch_template.latest_version_number.to_string())
                .build(),
        )
        .labels(LABEL_BRUPOP_INTERFACE_NAME, BRUPOP_INTERFACE_VERSION)
        .nodegroup_name(NODEGROUP_NAME.clone())
        .cluster_name(&cluster.name)
        .subnets(first_subnet_id(&cluster.private_subnet_ids)?)
        .node_role(&iam_instance_profile_arn)
        .scaling_config(
            NodegroupScalingConfig::builder()
                .desired_size(DEFAULT_INSTANCE_COUNT)
                .build(),
        )
        .send()
        .await
        .context("Failed to create nodegroup")?;

    // Ensure the nodegroup reach a active state.
    tokio::time::timeout(
        Duration::from_secs(180),
        wait_for_conforming_nodegroup(&eks_client, &cluster.name, "create"),
    )
    .await
    .context("Timed-out waiting for nodegroup to reach the `active` state.")??;

    Ok(())
}

pub async fn terminate_nodegroup(cluster: ClusterInfo) -> ProviderResult<()> {
    // Setup aws_sdk_config and clients.
    let region_provider = RegionProviderChain::first_try(Some(Region::new(cluster.region.clone())));
    let shared_config = aws_config::from_env().region(region_provider).load().await;
    let ec2_client = aws_sdk_ec2::Client::new(&shared_config);
    let eks_client = aws_sdk_eks::Client::new(&shared_config);
    let iam_client = aws_sdk_iam::Client::new(&shared_config);

    // Delete nodegroup from cluster
    eks_client
        .delete_nodegroup()
        .nodegroup_name(NODEGROUP_NAME.clone())
        .cluster_name(&cluster.name)
        .send()
        .await
        .context("Failed to delete nodegroup")?;

    // Ensure the instances reach a terminated state.
    tokio::time::timeout(
        Duration::from_secs(300),
        wait_for_conforming_nodegroup(&eks_client, &cluster.name, "delete"),
    )
    .await
    .context("Timed-out waiting for instances to be fully deleted")??;

    // Delete one time iam instance profile for nodegroup which created by integration test.
    delete_iam_instance_profile(&iam_client).await?;

    // Delete nodegroup launch template which created by integration test.
    delete_launch_template(&ec2_client).await?;

    Ok(())
}

// =^..^=   =^..^=   =^..^=   =^..^=   =^..^= Termination and Creation of Launch Template  =^..^=   =^..^=   =^..^=    =^..^=

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, Default)]
struct CreatedEc2LaunchTemplate {
    /// The ids of created template
    pub launch_template_id: String,

    /// The latest version of created template
    pub latest_version_number: i64,
}

async fn create_launch_template(
    ec2_client: &aws_sdk_ec2::Client,
    node_ami: &str,
    instance_type: &str,
    cluster: &ClusterInfo,
) -> ProviderResult<CreatedEc2LaunchTemplate> {
    let created_launch_template = ec2_client
        .create_launch_template()
        .launch_template_name(LAUNCH_TEMPLATE_NAME)
        .launch_template_data(
            RequestLaunchTemplateData::builder()
                .image_id(node_ami)
                .instance_type(InstanceType::from(instance_type))
                .user_data(userdata(
                    &cluster.endpoint,
                    &cluster.name,
                    &cluster.certificate,
                ))
                .tag_specifications(tag_specifications(&cluster.name))
                .build(),
        )
        .send()
        .await
        .context("Failed to create launch template")?
        .launch_template
        .context("Failed to get launch template")?;

    let created_template_id = created_launch_template
        .launch_template_id
        .context("Failed to get launch template id")?;
    let created_template_version = created_launch_template
        .latest_version_number
        .context("Failed to get launch template version")?;

    Ok(CreatedEc2LaunchTemplate {
        launch_template_id: created_template_id,
        latest_version_number: created_template_version,
    })
}

async fn delete_launch_template(ec2_client: &aws_sdk_ec2::Client) -> ProviderResult<()> {
    ec2_client
        .delete_launch_template()
        .launch_template_name(LAUNCH_TEMPLATE_NAME)
        .send()
        .await
        .context("Failed to delete launch template")?;

    Ok(())
}

// =^..^=   =^..^=   =^..^=   =^..^=   =^..^= Termination and Creation of IAM ROLE  =^..^=   =^..^=   =^..^=   =^..^=   =^..^=

async fn create_iam_instance_profile(iam_client: &aws_sdk_iam::Client) -> ProviderResult<String> {
    let get_instance_profile_result = iam_client
        .get_instance_profile()
        .instance_profile_name(IAM_INSTANCE_PROFILE_NAME)
        .send()
        .await;
    if exists(get_instance_profile_result) {
        instance_profile_arn(iam_client).await
    } else {
        iam_client
            .create_role()
            .role_name(IAM_INSTANCE_PROFILE_NAME)
            .assume_role_policy_document(eks_role_policy_document())
            .send()
            .await
            .context("Unable to create new role.")?;
        iam_client
            .attach_role_policy()
            .role_name(IAM_INSTANCE_PROFILE_NAME)
            .policy_arn(SSM_MANAGED_INSTANCE_CORE_ARN)
            .send()
            .await
            .context("Unable to attach AmazonSSM policy")?;
        iam_client
            .attach_role_policy()
            .role_name(IAM_INSTANCE_PROFILE_NAME)
            .policy_arn(EKS_WORKER_NODE_POLICY_ARN)
            .send()
            .await
            .context("Unable to attach AmazonEKSWorkerNode policy")?;
        iam_client
            .attach_role_policy()
            .role_name(IAM_INSTANCE_PROFILE_NAME)
            .policy_arn(EKS_CNI_ARN)
            .send()
            .await
            .context("Unable to attach AmazonEKS CNI policy")?;
        iam_client
            .attach_role_policy()
            .role_name(IAM_INSTANCE_PROFILE_NAME)
            .policy_arn(EC2_CONTAINER_REGISTRY_ARN)
            .send()
            .await
            .context("Unable to attach AmazonEC2ContainerRegistry policy")?;
        iam_client
            .create_instance_profile()
            .instance_profile_name(IAM_INSTANCE_PROFILE_NAME)
            .send()
            .await
            .context("Unable to create instance profile")?;
        iam_client
            .add_role_to_instance_profile()
            .instance_profile_name(IAM_INSTANCE_PROFILE_NAME)
            .role_name(IAM_INSTANCE_PROFILE_NAME)
            .send()
            .await
            .context("Unable to add role to instance profile")?;
        instance_profile_arn(iam_client).await
    }
}

async fn delete_iam_instance_profile(iam_client: &aws_sdk_iam::Client) -> ProviderResult<()> {
    iam_client
        .remove_role_from_instance_profile()
        .role_name(IAM_INSTANCE_PROFILE_NAME)
        .instance_profile_name(IAM_INSTANCE_PROFILE_NAME)
        .send()
        .await
        .context("Unable to remove roles from instance profile.")?;
    iam_client
        .detach_role_policy()
        .role_name(IAM_INSTANCE_PROFILE_NAME)
        .policy_arn(SSM_MANAGED_INSTANCE_CORE_ARN)
        .send()
        .await
        .context("Unable to detach AmazonSSM policy")?;
    iam_client
        .detach_role_policy()
        .role_name(IAM_INSTANCE_PROFILE_NAME)
        .policy_arn(EKS_WORKER_NODE_POLICY_ARN)
        .send()
        .await
        .context("Unable to detach AmazonEKSWorkerNode policy")?;
    iam_client
        .detach_role_policy()
        .role_name(IAM_INSTANCE_PROFILE_NAME)
        .policy_arn(EKS_CNI_ARN)
        .send()
        .await
        .context("Unable to detach AmazonEKS CNI policy")?;
    iam_client
        .detach_role_policy()
        .role_name(IAM_INSTANCE_PROFILE_NAME)
        .policy_arn(EC2_CONTAINER_REGISTRY_ARN)
        .send()
        .await
        .context("Unable to detach AmazonEC2ContainerRegistry policy")?;
    iam_client
        .delete_instance_profile()
        .instance_profile_name(IAM_INSTANCE_PROFILE_NAME)
        .send()
        .await
        .context("Unable to create instance profile")?;
    iam_client
        .delete_role()
        .role_name(IAM_INSTANCE_PROFILE_NAME)
        .send()
        .await
        .context("Unable to delete role.")?;

    Ok(())
}

// =^..^=   =^..^=   =^..^=   =^..^=  =^..^=  Related sub-functions of sources creation and termination   =^..^=   =^..^=   =^..^=   =^..^=   =^..^=

// Find the node ami id to use.
async fn find_ami_id(
    ssm_client: &aws_sdk_ssm::Client,
    arch: &str,
    br_version: &str,
    eks_version: &str,
) -> ProviderResult<String> {
    let parameter_name = format!(
        "/aws/service/bottlerocket/aws-k8s-{}/{}/{}/image_id",
        eks_version, arch, br_version
    );
    let ami_id = ssm_client
        .get_parameter()
        .name(parameter_name)
        .send()
        .await
        .context("Unable to get ami id")?
        .parameter
        .context("Unable to get ami id")?
        .value
        .context("ami id is missing")?;
    Ok(ami_id)
}

/// Determine the instance type to use. If provided use that one. Otherwise, for `x86_64` use `m5.large`
/// and for `aarch64` use `m6g.large`
async fn instance_type(ec2_client: &aws_sdk_ec2::Client, node_ami: &str) -> ProviderResult<String> {
    let arch = ec2_client
        .describe_images()
        .image_ids(node_ami)
        .send()
        .await
        .context("Unable to get ami architecture")?
        .images
        .context("Unable to get ami architecture")?
        .get(0)
        .context("Unable to get ami architecture")?
        .architecture
        .clone()
        .context("Ami has no architecture")?;

    Ok(match arch {
        ArchitectureValues::X8664 => "m5.large",
        ArchitectureValues::Arm64 => "m6g.large",
        _ => "m6g.large",
    }
    .to_string())
}

fn first_subnet_id(subnet_ids: &[String]) -> ProviderResult<String> {
    subnet_ids
        .get(0)
        .map(|id| id.to_string())
        .context("There are no private subnet ids")
}

fn tag_specifications(cluster_name: &str) -> LaunchTemplateTagSpecificationRequest {
    LaunchTemplateTagSpecificationRequest::builder()
        .resource_type(ResourceType::Instance)
        .tags(
            Tag::builder()
                .key("Name")
                .value(format!("{}_node", cluster_name))
                .build(),
        )
        .tags(
            Tag::builder()
                .key(format!("kubernetes.io/cluster/{}", cluster_name))
                .value("owned")
                .build(),
        )
        .tags(
            Tag::builder()
                .key(INSTANCE_TAG_NAME)
                .value(INSTANCE_TAG_VALUE)
                .build(),
        )
        .build()
}

fn userdata(endpoint: &str, cluster_name: &str, certificate: &str) -> String {
    base64::encode(format!(
        r#"[settings.updates]
            ignore-waves = true
                
            [settings.kubernetes]
            api-server = "{}"
            cluster-name = "{}"
            cluster-certificate = "{}"
        "#,
        endpoint, cluster_name, certificate
    ))
}

async fn wait_for_conforming_nodegroup(
    eks_client: &aws_sdk_eks::Client,
    cluster_name: &str,
    action: &str,
) -> ProviderResult<()> {
    loop {
        if !non_conforming_nodegroup(eks_client, cluster_name, action).await? {
            tokio::time::sleep(Duration::from_millis(1000)).await;
            continue;
        }
        return Ok(());
    }
}

async fn non_conforming_nodegroup(
    eks_client: &aws_sdk_eks::Client,
    cluster_name: &str,
    action: &str,
) -> ProviderResult<bool> {
    match action {
        "create" => {
            // let nodegroup_status = get_nodegroup_status(eks_client, cluster_name).await?;
            let nodegroup_status = eks_client
                .describe_nodegroup()
                .nodegroup_name(NODEGROUP_NAME)
                .cluster_name(cluster_name)
                .send()
                .await
                .context("Unable to describe nodegroup")?
                .nodegroup
                .context("Unable to extract nodegroup")?
                .status
                .context("Unable to extract nodegroup status")?;
            match nodegroup_status {
                NodegroupStatus::Active => Ok(true),
                _ => Ok(false),
            }
        }
        "delete" => confirm_nodegroup_exists(eks_client, cluster_name).await,
        _ => return Err(ProviderError::new_with_context("Invalid action input")),
    }
}

async fn confirm_nodegroup_exists(
    eks_client: &aws_sdk_eks::Client,
    cluster_name: &str,
) -> ProviderResult<bool> {
    let nodegroup = eks_client
        .describe_nodegroup()
        .nodegroup_name(NODEGROUP_NAME)
        .cluster_name(cluster_name)
        .send()
        .await;

    match nodegroup {
        Err(_resource_not_found_exception) => Ok(true),
        _ => Ok(false),
    }
}

fn exists(result: Result<GetInstanceProfileOutput, SdkError<GetInstanceProfileError>>) -> bool {
    if let Err(SdkError::ServiceError { err, raw: _ }) = result {
        if matches!(
            &err.kind,
            GetInstanceProfileErrorKind::NoSuchEntityException(_)
        ) {
            return false;
        }
    }
    true
}

async fn instance_profile_arn(iam_client: &aws_sdk_iam::Client) -> ProviderResult<String> {
    iam_client
        .get_instance_profile()
        .instance_profile_name(IAM_INSTANCE_PROFILE_NAME)
        .send()
        .await
        .context("Unable to get instance profile.")?
        .instance_profile()
        .and_then(|instance_profile| instance_profile.roles())
        .context("Instance profile does not contain roles.")?
        .get(0)
        .context("Instance profile does not contain roles.")?
        .arn
        .as_ref()
        .context("Role does not contain an arn.")
        .map(|arn| arn.to_string())
}

async fn cluster_iam_identity_mapping(
    cluster_name: &str,
    region: &str,
    arn: &str,
) -> ProviderResult<()> {
    Command::new("eksctl")
        .args([
            "create",
            "iamidentitymapping",
            "--cluster",
            cluster_name,
            "--region",
            region,
            "--arn",
            arn,
            "--group",
            "system:bootstrappers,system:nodes",
            "--username",
            "system:node:{{EC2PrivateDNSName}}",
        ])
        .output()
        .context("Unable to map iam identity.")?;

    Ok(())
}

fn eks_role_policy_document() -> String {
    r#"{
            "Version": "2012-10-17",
            "Statement": [
                {
                    "Effect": "Allow",
                    "Principal": {
                        "Service": "ec2.amazonaws.com"
                    },
                    "Action": "sts:AssumeRole"
                }
            ]
    }"#
    .to_string()
}
