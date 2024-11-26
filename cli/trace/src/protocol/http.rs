use crate::{
    protocol::{create_backend_node, metadata_node},
    TraceCtx, TraceNode, TraceTree,
};
use anyhow::Result;
use linkerd2_proxy_api::{
    http_route::{self, Ratio},
    http_types::http_method::{self, Registered},
    outbound::{
        self,
        proxy_protocol::{Http1, Http2},
        HttpRoute,
    },
};

use super::node_from_meta;

pub async fn create_http1_nodes(ctx: TraceCtx, http1: Http1) -> Result<Vec<TraceTree>> {
    create_http_route_nodes(ctx, http1.routes, "HTTP/1").await
}

pub async fn create_http2_nodes(ctx: TraceCtx, http2: Http2) -> Result<Vec<TraceTree>> {
    create_http_route_nodes(ctx, http2.routes, "HTTP/2").await
}

async fn create_http_route_nodes(
    ctx: TraceCtx,
    routes: Vec<HttpRoute>,
    proto: &str,
) -> Result<Vec<TraceTree>> {
    let mut nodes = Vec::with_capacity(routes.len());

    for route in routes {
        let meta = route.metadata.unwrap();
        let mut route_node = node_from_meta(meta, proto);
        for (i, rule) in route.rules.into_iter().enumerate() {
            let mut rule_node = TraceNode::node(format!("<rule {i}>"));

            if !rule.matches.is_empty() {
                let mut match_node_children = Vec::new();
                for rule_match in rule.matches {
                    if !rule_match.headers.is_empty() {
                        let mut header_node = TraceNode::node("headers (all)");
                        for header in rule_match.headers {
                            header_node.push(TraceNode::label(
                                header.name,
                                header
                                    .value
                                    .map(|v| match v {
                                        http_route::header_match::Value::Exact(v) => {
                                            String::from_utf8_lossy(&v).into_owned()
                                        }
                                        http_route::header_match::Value::Regex(r) => {
                                            format!("Regex: {r}")
                                        }
                                    })
                                    .unwrap_or_else(|| "\"\"".into()),
                            ));
                        }
                        match_node_children.push(header_node);
                    }

                    if let Some(method) = rule_match.method {
                        let method = match method.r#type.unwrap() {
                            http_method::Type::Registered(i) => {
                                Registered::try_from(i)?.as_str_name().to_string()
                            }
                            http_method::Type::Unregistered(s) => s,
                        };
                        match_node_children.push(TraceNode::label("method", method));
                    }

                    if let Some(path) = rule_match.path {
                        let path_node = match path.kind.unwrap() {
                            http_route::path_match::Kind::Exact(s) => {
                                TraceNode::label("exact path", s)
                            }
                            http_route::path_match::Kind::Prefix(s) => {
                                TraceNode::label("path prefix", s)
                            }
                            http_route::path_match::Kind::Regex(s) => {
                                TraceNode::label("path regex", s)
                            }
                        };
                        match_node_children.push(path_node);
                    }

                    if !rule_match.query_params.is_empty() {
                        let mut query_param_node = TraceNode::node("query params (all)");
                        for query_param in rule_match.query_params {
                            let node = if let Some(value) = query_param.value {
                                let value = match value {
                                    http_route::query_param_match::Value::Exact(s) => s,
                                    http_route::query_param_match::Value::Regex(r) => {
                                        format!("regex: {r}")
                                    }
                                };
                                TraceNode::label(query_param.name, value)
                            } else {
                                TraceNode::node(query_param.name)
                            };
                            query_param_node.push(node);
                        }
                        match_node_children.push(query_param_node);
                    }
                }

                match match_node_children.len() {
                    0 => {}
                    1 => {
                        let mut node = match_node_children.remove(0);
                        node.root.prefix("matches");
                        rule_node.push(node);
                    }
                    _ => {
                        rule_node.push(
                            TraceNode::node("matches (any)").with_leaves(match_node_children),
                        );
                    }
                }
            }
            for filter in rule.filters {
                match filter.kind.unwrap() {
                    outbound::http_route::filter::Kind::FailureInjector(injector) => {
                        let ratio = injector.ratio.unwrap_or(Ratio {
                            numerator: 1,
                            denominator: 1,
                        });
                        let ratio = (((ratio.numerator as f32) / (ratio.denominator as f32))
                            * 100.0)
                            .round();
                        rule_node.push(TraceNode::label(
                            "failure injector",
                            format!(
                                "{}% of requests fail with code {} and status {}",
                                ratio, injector.status, injector.message
                            ),
                        ));
                    }
                    outbound::http_route::filter::Kind::RequestHeaderModifier(_req_modifier) => {
                        todo!()
                    }
                    outbound::http_route::filter::Kind::Redirect(_redirect) => {
                        todo!()
                    }
                    outbound::http_route::filter::Kind::ResponseHeaderModifier(_rsp_modifier) => {
                        todo!()
                    }
                }
            }
            let Some(backends) = rule.backends else {
                todo!()
            };

            let backend_node = match backends.kind.unwrap() {
                outbound::http_route::distribution::Kind::Empty(_) => {
                    TraceNode::label("backends", "<empty>")
                }
                outbound::http_route::distribution::Kind::FirstAvailable(first_available) => {
                    let mut backend_node = TraceNode::node("backends (first available)");

                    for route_backend in first_available.backends {
                        let backend = route_backend.backend.unwrap();
                        let route_backend_node = create_backend_node(ctx.clone(), backend).await?;

                        if !route_backend.filters.is_empty() {
                            for _filter in route_backend.filters {
                                todo!();
                            }
                        }

                        backend_node.push(route_backend_node);
                    }

                    backend_node
                }
                outbound::http_route::distribution::Kind::RandomAvailable(random) => {
                    let mut backend_node = TraceNode::node("backends (random)");

                    let total_weight = random.backends.iter().map(|b| b.weight).sum::<u32>() as f32;

                    for weighted_route_backend in random.backends {
                        let route_backend = weighted_route_backend.backend.unwrap();
                        let backend = route_backend.backend.unwrap();

                        let mut route_backend_node =
                            metadata_node(ctx.clone(), backend.metadata.unwrap()).await?;
                        route_backend_node.push(TraceNode::label(
                            "weight",
                            format!(
                                "{}%",
                                (weighted_route_backend.weight as f32 / total_weight).round()
                            ),
                        ));

                        let mut route_backend_node = match backend.kind.unwrap() {
                            outbound::backend::Kind::Forward(_weighted_addr) => {
                                todo!()
                            }
                            outbound::backend::Kind::Balancer(balance) => {
                                let outbound::backend::endpoint_discovery::Kind::Dst(dst) =
                                    balance.discovery.unwrap().kind.unwrap();
                                TraceNode::node(dst.path)
                            }
                        };
                        route_backend_node.push(TraceNode::label(
                            "weight",
                            weighted_route_backend.weight.to_string(),
                        ));
                        for _filter in route_backend.filters {
                            todo!();
                        }

                        backend_node.push(route_backend_node);
                    }

                    backend_node
                }
            };
            rule_node.push(backend_node);
            // rule.timeouts; // ignored
            // rule.retry; // ignored
            // rule.allow_l5d_request_headers; // ignored
            if !rule_node.leaves.is_empty() {
                route_node.push(rule_node);
            }
        }

        nodes.push(route_node);
    }
    Ok(nodes)
}
