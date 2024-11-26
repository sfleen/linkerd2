use std::net::SocketAddr;

use crate::{get_endpoints, TraceCtx, TraceNode, TraceTree};
use anyhow::Result;
use indexmap::IndexMap;
use kube::Api;
use linkerd2_proxy_api::{
    destination::WeightedAddr,
    meta::{metadata, Metadata},
    outbound::{self, Backend},
};
use linkerd_policy_controller_k8s_api::{
    api::apps::v1::{DaemonSet, Deployment, ReplicaSet, StatefulSet},
    apimachinery::pkg::apis::meta::v1::OwnerReference,
    ObjectMeta, Pod, Service,
};

pub mod grpc;
pub mod http;
pub mod opaque;
pub mod tls;

fn node_from_meta(meta: Metadata, protocol: &str) -> TraceTree {
    match meta.kind.unwrap() {
        metadata::Kind::Default(name) => {
            TraceNode::node(format!("<default {protocol} route \"{name}\">"))
        }
        metadata::Kind::Resource(resource) => TraceNode::node(format!(
            "{}.{}/{} ({protocol})",
            resource.group, resource.kind, resource.name
        )),
    }
}

async fn create_backend_node(ctx: TraceCtx, backend: Backend) -> Result<TraceTree> {
    let mut route_backend_node = metadata_node(ctx.clone(), backend.metadata.unwrap()).await?;

    match backend.kind.unwrap() {
        outbound::backend::Kind::Forward(_weighted_addr) => {
            todo!()
        }
        outbound::backend::Kind::Balancer(balance) => {
            let outbound::backend::endpoint_discovery::Kind::Dst(dst) =
                balance.discovery.unwrap().kind.unwrap();
            let children = get_service_children(ctx, &dst.path).await?;
            route_backend_node.push(TraceNode::label("FQDN", dst.path));

            for child in children.deploy.into_values() {
                child.add_to_tree(&mut route_backend_node);
            }
            for child in children.sts.into_values() {
                child.add_to_tree(&mut route_backend_node);
            }
            for child in children.ds.into_values() {
                child.add_to_tree(&mut route_backend_node);
            }
        }
    }

    Ok(route_backend_node)
}

async fn get_service_children(
    mut ctx: TraceCtx,
    svc_fqdn: &str,
) -> anyhow::Result<ServiceChildren> {
    let (endpoints, _profiles) = get_endpoints(&mut ctx.dest_client, svc_fqdn.to_string()).await?;

    let mut children = ServiceChildren::default();

    for (addr, endpoint) in endpoints {
        let ep_ns = endpoint.metric_labels.get("namespace");
        let ep_pod = endpoint.metric_labels.get("pod");

        let Some(ep_ns) = ep_ns else { todo!() };
        let Some(ep_pod) = ep_pod else { todo!() };

        let pod_api = Api::<Pod>::namespaced(ctx.k8s_client.clone(), ep_ns);
        let rs_api = Api::<ReplicaSet>::namespaced(ctx.k8s_client.clone(), ep_ns);
        let deploy_api = Api::<Deployment>::namespaced(ctx.k8s_client.clone(), ep_ns);
        let sts_api = Api::<StatefulSet>::namespaced(ctx.k8s_client.clone(), ep_ns);
        let ds_api = Api::<DaemonSet>::namespaced(ctx.k8s_client.clone(), ep_ns);
        let pod = pod_api.get(ep_pod).await?;

        let pod_owner = get_owner_reference(&pod.metadata);
        let rs_name = match pod_owner.kind.as_str() {
            "ReplicaSet" => &pod_owner.name,
            "DaemonSet" => {
                let ds = ds_api.get(&pod_owner.name).await?;
                let ds = children
                    .ds
                    .entry(pod_owner.name.clone())
                    .or_insert_with(|| DaemonSetHierarchy {
                        daemonset: ds,
                        pods: IndexMap::default(),
                    });

                let pod = ds
                    .pods
                    .entry(ep_pod.clone())
                    .or_insert_with(|| PodHierarchy {
                        pod,
                        addrs: IndexMap::new(),
                    });

                pod.addrs.insert(addr, endpoint);
                continue;
            }
            owner => todo!("{owner}"),
        };
        let rs = rs_api.get(rs_name).await?;

        let rs_owner = get_owner_reference(&rs.metadata);
        let rs = match rs_owner.kind.as_str() {
            "Deployment" => {
                let deploy = deploy_api.get(&rs_owner.name).await?;
                let deploy = children
                    .deploy
                    .entry(rs_owner.name.clone())
                    .or_insert_with(|| DeploymentHierarchy {
                        deployment: deploy,
                        replicasets: IndexMap::new(),
                    });

                deploy
                    .replicasets
                    .entry(rs_name.to_string())
                    .or_insert_with(|| ReplicaSetHierarchy {
                        rs,
                        pods: IndexMap::new(),
                    })
            }
            "StatefulSet" => {
                let sts = sts_api.get(&rs_owner.name).await?;
                let sts = children
                    .sts
                    .entry(rs_owner.name.clone())
                    .or_insert_with(|| StatefulSetHierarchy {
                        statefulset: sts,
                        replicasets: IndexMap::new(),
                    });

                sts.replicasets
                    .entry(rs_name.clone())
                    .or_insert_with(|| ReplicaSetHierarchy {
                        rs,
                        pods: IndexMap::new(),
                    })
            }
            owner => todo!("{owner}"),
        };

        let pod = rs
            .pods
            .entry(ep_pod.clone())
            .or_insert_with(|| PodHierarchy {
                pod,
                addrs: IndexMap::new(),
            });

        pod.addrs.insert(addr, endpoint);
    }

    Ok(children)
}

fn get_owner_reference(meta: &ObjectMeta) -> &OwnerReference {
    let Some(owners) = &meta.owner_references else {
        todo!()
    };

    if owners.is_empty() {
        todo!();
    }
    assert!(owners.len() == 1);

    &owners[0]
}

async fn metadata_node(ctx: TraceCtx, meta: Metadata) -> Result<TraceTree> {
    match meta.kind.unwrap() {
        metadata::Kind::Default(_) => todo!(),
        metadata::Kind::Resource(r) => match (r.group.as_str(), r.kind.as_str()) {
            ("core", "Service") => {
                let mut node =
                    TraceNode::node(format!("svc {}.{}:{}", r.name, r.namespace, r.port));

                let svc_api = Api::<Service>::namespaced(ctx.k8s_client.clone(), &r.namespace);
                let svc = svc_api.get(&r.name).await?;
                let spec = svc.spec.unwrap();

                if let Some(cluster_ip) = spec.cluster_ip {
                    node.push(TraceNode::label(
                        "endpoint",
                        format!("{}:{}", cluster_ip, r.port),
                    ));
                } else {
                    todo!();
                }

                Ok(node)
            }
            _ => Ok(TraceNode::node(format!(
                "{}.{}/{}:{} -n {} ({})",
                r.group, r.kind, r.name, r.port, r.namespace, r.section
            ))),
        },
    }
}

#[derive(Default)]
struct ServiceChildren {
    deploy: IndexMap<String, DeploymentHierarchy>,
    sts: IndexMap<String, StatefulSetHierarchy>,
    ds: IndexMap<String, DaemonSetHierarchy>,
}

struct DeploymentHierarchy {
    deployment: Deployment,
    replicasets: IndexMap<String, ReplicaSetHierarchy>,
}

impl DeploymentHierarchy {
    fn add_to_tree(self, root: &mut TraceTree) {
        let mut node = TraceNode::node(format!(
            "deployment {}",
            self.deployment.metadata.name.unwrap()
        ));

        for rs in self.replicasets.into_values() {
            rs.add_to_tree(&mut node);
        }

        root.push(node);
    }
}

struct StatefulSetHierarchy {
    statefulset: StatefulSet,
    replicasets: IndexMap<String, ReplicaSetHierarchy>,
}

impl StatefulSetHierarchy {
    fn add_to_tree(self, root: &mut TraceTree) {
        let mut node = TraceNode::node(format!(
            "statefulset {}",
            self.statefulset.metadata.name.unwrap()
        ));

        for rs in self.replicasets.into_values() {
            rs.add_to_tree(&mut node);
        }

        root.push(node);
    }
}

struct DaemonSetHierarchy {
    daemonset: DaemonSet,
    pods: IndexMap<String, PodHierarchy>,
}

impl DaemonSetHierarchy {
    fn add_to_tree(self, root: &mut TraceTree) {
        let mut node = TraceNode::node(format!(
            "daemonset {}",
            self.daemonset.metadata.name.unwrap()
        ));

        for pod in self.pods.into_values() {
            pod.add_to_tree(&mut node);
        }

        root.push(node);
    }
}

struct ReplicaSetHierarchy {
    rs: ReplicaSet,
    pods: IndexMap<String, PodHierarchy>,
}

impl ReplicaSetHierarchy {
    fn add_to_tree(self, root: &mut TraceTree) {
        let status = self.rs.status.unwrap();
        let replicas = status.replicas;
        let available = status.available_replicas.unwrap_or(0);

        let mut node = TraceNode::node(format!(
            "replicaset {} ({available}/{replicas} replicas available)",
            self.rs.metadata.name.unwrap()
        ));

        for pod in self.pods.into_values() {
            pod.add_to_tree(&mut node);
        }

        root.push(node);
    }
}

struct PodHierarchy {
    pod: Pod,
    addrs: IndexMap<SocketAddr, WeightedAddr>,
}

impl PodHierarchy {
    fn add_to_tree(self, root: &mut TraceTree) {
        let container_statuses = self.pod.status.unwrap().container_statuses.unwrap();
        let (ready_containers, pending_containers): (Vec<_>, Vec<_>) =
            container_statuses.into_iter().partition(|c| c.ready);

        let mut node = TraceNode::node(format!(
            "pod {} ({}/{} containers ready)",
            self.pod.metadata.name.unwrap(),
            ready_containers.len(),
            ready_containers.len() + pending_containers.len(),
        ));
        if !ready_containers.is_empty() {
            let pod_names = ready_containers
                .into_iter()
                .map(|c| c.name)
                .collect::<Vec<_>>()
                .join(", ");
            node.push(TraceNode::label("ready containers", pod_names));
        }
        if !pending_containers.is_empty() {
            let pod_names = pending_containers
                .into_iter()
                .map(|c| c.name)
                .collect::<Vec<_>>()
                .join(", ");
            node.push(TraceNode::label("pending containers", pod_names));
        }

        for (addr, _weighted_addr) in self.addrs {
            node.push(TraceNode::label("endpoint", addr.to_string()));
        }

        root.push(node);
    }
}
