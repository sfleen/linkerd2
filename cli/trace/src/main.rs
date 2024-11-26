use anyhow::{Context as _, Result};
use futures::{StreamExt as _, TryStreamExt};
use k8s_api::{api::apps::v1::Deployment, ListParams, Pod};
use kube::{Api, Client};
use linkerd2_proxy_api::{
    destination::{
        destination_client::DestinationClient, DestinationProfile, GetDestination, WeightedAddr,
    },
    net::TcpAddress,
    outbound::{
        self, outbound_policies_client::OutboundPoliciesClient, proxy_protocol,
        traffic_spec::Target, OutboundPolicy, TrafficSpec,
    },
};
use linkerd_policy_controller_k8s_api::{self as k8s_api};
use protocol::{
    http::{create_http1_nodes, create_http2_nodes},
    opaque::create_opaque_nodes,
};
use std::{
    collections::HashMap,
    fmt::Display,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    sync::Arc,
    time::Duration,
};
use termtree::Tree;
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::TcpListener,
    select,
};
use tokio_stream::wrappers::TcpListenerStream;
use tonic::transport::{Channel, ClientTlsConfig};
use tracing::{debug, error, info, warn};

mod protocol;

const POLICY_PORT: u16 = 8090;
const DESTINATION_PORT: u16 = 8086;
const DESTINATION_DEPLOYMENT: &str = "linkerd-destination";
const DEFAULT_WORKLOAD: &str = "default:diagnostics";
const API_STREAM_TIMEOUT: Duration = Duration::from_millis(500);

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    warn!("EXPERIMENTAL: output may be innaccurate; use at your own risk");

    let k8s_client = Client::try_default().await?;

    let svc_name = "web-svc";
    let svc_namespace = "emojivoto";
    let svc_port = 80u16;
    // let svc_name = "backend-a-podinfo";
    // let svc_namespace = "test";
    // let svc_port = 9898u16;
    let svc: k8s_api::Service = Api::namespaced(k8s_client.clone(), svc_namespace)
        .get(svc_name)
        .await?;

    let linkerd_pod_api: Arc<Api<Pod>> = Arc::new(Api::namespaced(k8s_client.clone(), "linkerd"));
    let linkerd_deploy_api: Api<Deployment> = Api::namespaced(k8s_client.clone(), "linkerd");

    let destination_deployment: Deployment = linkerd_deploy_api.get(DESTINATION_DEPLOYMENT).await?;
    let destination_selector = &destination_deployment
        .spec
        .as_ref()
        .expect("destination must have spec")
        .selector;
    assert!(destination_selector.match_expressions.is_none());
    let destination_selector: String = destination_selector
        .match_labels
        .as_ref()
        .unwrap()
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join(",");

    let pods = linkerd_pod_api
        .list(&ListParams {
            label_selector: Some(destination_selector),
            ..Default::default()
        })
        .await?;
    let destination_pod = pods.items.first().unwrap();

    let (mut policy_client, destination_client) =
        setup_portforwards(linkerd_pod_api, destination_pod).await?;
    let policy = policy_client
        .get(TrafficSpec {
            source_workload: DEFAULT_WORKLOAD.to_string(),
            target: Some(Target::Authority(format!(
                "{}.{}.svc:{}",
                svc_name, svc_namespace, svc_port,
            ))),
        })
        .await?
        .into_inner();

    let svc_desc = ServiceDescription { svc, policy };

    info!(
        "\n{}",
        svc_desc
            .display(TraceCtx {
                k8s_client,
                dest_client: destination_client
            })
            .await?
    );

    Ok(())
}

async fn create_policy_client(addr: SocketAddr) -> Result<OutboundPoliciesClient<Channel>> {
    let channel = tonic::transport::Channel::from_shared(format!("http://{addr}"))?
        .tls_config(ClientTlsConfig::new())?
        .connect()
        .await?;

    Ok(OutboundPoliciesClient::new(channel))
}

async fn create_destination_client(addr: SocketAddr) -> Result<DestinationClient<Channel>> {
    let channel = tonic::transport::Channel::from_shared(format!("http://{addr}"))?
        .tls_config(ClientTlsConfig::new())?
        .connect()
        .await?;

    Ok(DestinationClient::new(channel))
}

async fn get_endpoints(
    client: &mut DestinationClient<Channel>,
    path: String,
) -> Result<(HashMap<SocketAddr, WeightedAddr>, Vec<DestinationProfile>)> {
    let req = GetDestination {
        scheme: "http:".to_string(),
        path: path.clone(),
        context_token: "diagnostics".to_string(),
    };

    let mut endpoint_stream = client.get(req.clone()).await?.into_inner();

    let mut updates = HashMap::new();
    loop {
        let res = select! {
            res = endpoint_stream.next() => res,
            _ = tokio::time::sleep(API_STREAM_TIMEOUT) => break,
        };
        let Some(res) = res else {
            break;
        };
        let Some(update) = res?.update else { continue };
        match update {
            linkerd2_proxy_api::destination::update::Update::Add(addrs) => addrs
                .addrs
                .into_iter()
                .filter(|addr| addr.addr.is_some())
                .for_each(|mut addr| {
                    for (label, value) in &addrs.metric_labels {
                        addr.metric_labels
                            .entry(label.clone())
                            .or_insert_with(|| value.clone());
                    }

                    let tcp_addr = addr.addr.clone().expect("only present addrs");
                    let socket_addr = to_socket_addr(tcp_addr);
                    updates.insert(socket_addr, addr);
                }),
            linkerd2_proxy_api::destination::update::Update::Remove(addrs) => {
                addrs
                    .addrs
                    .into_iter()
                    .map(to_socket_addr)
                    .for_each(|addr| {
                        updates.remove(&addr);
                    });
            }
            linkerd2_proxy_api::destination::update::Update::NoEndpoints(_) => {
                info!("No endpoints for {path}");
                updates.clear();
                break;
            }
        }
    }

    let mut profile_stream = client.get_profile(req).await?.into_inner();

    let mut profiles = Vec::new();
    loop {
        let res = select! {
            res = profile_stream.next() => res,
            _ = tokio::time::sleep(API_STREAM_TIMEOUT) => break,
        };
        let Some(res) = res else {
            break;
        };
        profiles.push(res?);
    }

    Ok((updates, profiles))
}

fn to_socket_addr(addr: TcpAddress) -> SocketAddr {
    let ip: IpAddr = match addr.ip.expect("must have IP").ip.expect("again") {
        linkerd2_proxy_api::net::ip_address::Ip::Ipv4(ip) => Ipv4Addr::from(ip).into(),
        linkerd2_proxy_api::net::ip_address::Ip::Ipv6(ip) => {
            Ipv6Addr::from(((ip.last as u128) << 64) | ip.first as u128).into()
        }
    };
    SocketAddr::new(ip, addr.port as u16)
}

async fn setup_portforwards(
    pods: Arc<Api<Pod>>,
    destination_pod: &Pod,
) -> Result<(OutboundPoliciesClient<Channel>, DestinationClient<Channel>)> {
    let policy_addr = spawn_forward_server(pods.clone(), destination_pod, POLICY_PORT).await?;
    let destination_addr = spawn_forward_server(pods, destination_pod, DESTINATION_PORT).await?;

    let policy_client = create_policy_client(policy_addr).await?;
    let destination_client = create_destination_client(destination_addr).await?;
    Ok((policy_client, destination_client))
}

async fn spawn_forward_server(
    pods: Arc<Api<Pod>>,
    destination_pod: &Pod,
    port: u16,
) -> Result<SocketAddr> {
    // 0 port tells listener to assign a random free port.
    let addr = SocketAddr::from(([127, 0, 0, 1], 0));
    let listener = TcpListener::bind(addr).await?;
    let addr = listener.local_addr()?;
    let name: Arc<str> = destination_pod
        .metadata
        .name
        .as_deref()
        .expect("Destination pod must have name")
        .into();
    tokio::spawn(async move {
        let server = TcpListenerStream::new(listener)
            .take_until(tokio::signal::ctrl_c())
            .try_for_each(move |client_conn| {
                let pods = pods.clone();
                let name = name.clone();
                async move {
                    if let Ok(peer_addr) = client_conn.peer_addr() {
                        debug!(%peer_addr, "new connection");
                    }
                    tokio::spawn(async move {
                        if let Err(e) = forward_connection(&pods, &name, port, client_conn).await {
                            error!(
                                error = e.as_ref() as &dyn std::error::Error,
                                "failed to forward connection"
                            );
                        }
                    });
                    // keep the server running
                    Ok(())
                }
            });
        if let Err(e) = server.await {
            error!(
                error = &e as &dyn std::error::Error,
                "Error in portforwarder"
            );
        }
    });
    Ok(addr)
}

async fn forward_connection(
    pods: &Api<Pod>,
    pod_name: &str,
    port: u16,
    mut client_conn: impl AsyncRead + AsyncWrite + Unpin,
) -> anyhow::Result<()> {
    let mut forwarder = pods.portforward(pod_name, &[port]).await?;
    let mut upstream_conn = forwarder
        .take_stream(port)
        .context("port not found in forwarder")?;
    tokio::io::copy_bidirectional(&mut client_conn, &mut upstream_conn).await?;
    drop(upstream_conn);
    forwarder.join().await?;
    info!("connection closed");
    Ok(())
}

#[derive(Clone)]
struct TraceCtx {
    pub k8s_client: Client,
    pub dest_client: DestinationClient<Channel>,
}

#[derive(Debug)]
struct ServiceDescription {
    svc: k8s_api::Service,
    policy: OutboundPolicy,
}

impl ServiceDescription {
    async fn display(self, ctx: TraceCtx) -> Result<TraceTree> {
        let mut root = TraceNode::node(format!(
            "trace for service {}.{}",
            self.svc.metadata.name.unwrap(),
            self.svc.metadata.namespace.unwrap()
        ));

        // root.push(TraceNode::label(
        //     "endpoint",
        //     format!(
        //         "{}:{}",
        //         self.svc
        //             .spec
        //             .unwrap()
        //             .cluster_ip
        //             .unwrap_or_else(|| "None".to_string()),
        //         port
        //     ),
        // ));

        match self.policy.protocol.unwrap().kind.unwrap() {
            proxy_protocol::Kind::Detect(detect) => {
                'outer: {
                    // check if this is the default route (no explicit routes)
                    if let (Some(opaq), Some(http1), Some(http2)) =
                        (&detect.opaque, &detect.http1, &detect.http2)
                    {
                        let opaq_dist = opaq
                            .routes
                            .iter()
                            .flat_map(|r| &r.rules)
                            .flat_map(|r| r.backends.as_ref().and_then(|d| d.kind.as_ref()))
                            .flat_map(|d| match d {
                                outbound::opaque_route::distribution::Kind::Empty(_) => vec![],
                                outbound::opaque_route::distribution::Kind::FirstAvailable(
                                    first_available,
                                ) => first_available
                                    .backends
                                    .iter()
                                    .flat_map(|i| i.backend.as_ref())
                                    .collect::<Vec<_>>(),
                                outbound::opaque_route::distribution::Kind::RandomAvailable(
                                    random_available,
                                ) => random_available
                                    .backends
                                    .iter()
                                    .flat_map(|i| {
                                        i.backend.as_ref().and_then(|b| b.backend.as_ref())
                                    })
                                    .collect::<Vec<_>>(),
                            })
                            .collect::<Vec<_>>();
                        let http1_dist = http1
                            .routes
                            .iter()
                            .flat_map(|r| &r.rules)
                            .flat_map(|r| r.backends.as_ref().and_then(|d| d.kind.as_ref()))
                            .flat_map(|d| match d {
                                outbound::http_route::distribution::Kind::Empty(_) => vec![],
                                outbound::http_route::distribution::Kind::FirstAvailable(
                                    first_available,
                                ) => first_available
                                    .backends
                                    .iter()
                                    .flat_map(|i| i.backend.as_ref())
                                    .collect::<Vec<_>>(),
                                outbound::http_route::distribution::Kind::RandomAvailable(
                                    random_available,
                                ) => random_available
                                    .backends
                                    .iter()
                                    .flat_map(|i| {
                                        i.backend.as_ref().and_then(|b| b.backend.as_ref())
                                    })
                                    .collect::<Vec<_>>(),
                            })
                            .collect::<Vec<_>>();
                        let http2_dist = http2
                            .routes
                            .iter()
                            .flat_map(|r| &r.rules)
                            .flat_map(|r| r.backends.as_ref().and_then(|d| d.kind.as_ref()))
                            .flat_map(|d| match d {
                                outbound::http_route::distribution::Kind::Empty(_) => vec![],
                                outbound::http_route::distribution::Kind::FirstAvailable(
                                    first_available,
                                ) => first_available
                                    .backends
                                    .iter()
                                    .flat_map(|i| i.backend.as_ref())
                                    .collect::<Vec<_>>(),
                                outbound::http_route::distribution::Kind::RandomAvailable(
                                    random_available,
                                ) => random_available
                                    .backends
                                    .iter()
                                    .flat_map(|i| {
                                        i.backend.as_ref().and_then(|b| b.backend.as_ref())
                                    })
                                    .collect::<Vec<_>>(),
                            })
                            .collect::<Vec<_>>();

                        if opaq_dist == http1_dist && http1_dist == http2_dist {
                            root.push(TraceNode::label(
                                "protocol",
                                "detect (opaque, HTTP/1, HTTP/2)",
                            ));
                            root.extend(
                                create_http1_nodes(ctx.clone(), detect.http1.unwrap()).await?,
                            );
                            break 'outer;
                        }
                    }

                    root.push(TraceNode::label("protocol", "detect"));
                    if let Some(opaq) = detect.opaque {
                        root.extend(create_opaque_nodes(ctx.clone(), opaq).await?);
                    }
                    if let Some(http1) = detect.http1 {
                        root.extend(create_http1_nodes(ctx.clone(), http1).await?);
                    }
                    if let Some(http2) = detect.http2 {
                        root.extend(create_http2_nodes(ctx, http2).await?);
                    }
                }
            }
            proxy_protocol::Kind::Opaque(opaque) => {
                root.extend(create_opaque_nodes(ctx, opaque).await?);
            }
            proxy_protocol::Kind::Http1(http1) => {
                root.extend(create_http1_nodes(ctx, http1).await?);
            }
            proxy_protocol::Kind::Http2(http2) => {
                root.extend(create_http2_nodes(ctx, http2).await?);
            }
            proxy_protocol::Kind::Grpc(_grpc) => todo!(),
            proxy_protocol::Kind::Tls(_tls) => todo!(),
        }

        Ok(root)
    }
}

type TraceTree = Tree<TraceNode>;

#[derive(Debug)]
enum TraceNode {
    Node(String),
    Label(String, String),
}

impl TraceNode {
    fn node(name: impl Into<String>) -> TraceTree {
        TraceTree::new(TraceNode::Node(name.into()))
    }

    fn label(key: impl Into<String>, value: impl Into<String>) -> TraceTree {
        TraceTree::new(TraceNode::Label(key.into(), value.into()))
    }

    fn prefix(&mut self, prefix: impl Into<String>) {
        let label = match self {
            TraceNode::Node(s) => s,
            TraceNode::Label(s, _) => s,
        };
        *label = format!("{} {}", prefix.into(), label);
    }
}

impl Display for TraceNode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TraceNode::Node(n) => write!(f, "{n}:"),
            TraceNode::Label(k, v) => write!(f, "{k}: {v}"),
        }
    }
}
