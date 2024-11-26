use crate::{protocol::create_backend_node, TraceCtx, TraceNode, TraceTree};
use anyhow::Result;
use linkerd2_proxy_api::outbound::{self, proxy_protocol::Opaque};

use super::node_from_meta;

pub async fn create_opaque_nodes(ctx: TraceCtx, opaq: Opaque) -> Result<Vec<TraceTree>> {
    let mut nodes = Vec::with_capacity(opaq.routes.len());
    for route in opaq.routes {
        let meta = route.metadata.unwrap();
        let mut route_node = node_from_meta(meta, "opaque");
        for (i, rule) in route.rules.into_iter().enumerate() {
            let mut rule_node = TraceNode::node(format!("<rule {i}>"));
            let backend_node = match rule.backends.unwrap().kind.unwrap() {
                outbound::opaque_route::distribution::Kind::Empty(_) => {
                    TraceNode::label("backends", "<empty>")
                }
                outbound::opaque_route::distribution::Kind::FirstAvailable(first_available) => {
                    let mut backend_node = TraceNode::node("backends (first available)");

                    for route_backend in first_available.backends {
                        let backend = route_backend.backend.unwrap();
                        let route_backend_node = create_backend_node(ctx.clone(), backend).await?;

                        backend_node.push(route_backend_node);
                    }

                    backend_node
                }
                outbound::opaque_route::distribution::Kind::RandomAvailable(_random_available) => {
                    todo!()
                }
            };
            rule_node.push(backend_node);
            route_node.push(rule_node);
        }
        nodes.push(route_node);
    }
    Ok(nodes)
}
