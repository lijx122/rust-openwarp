//! `mcp.rs` 单元测试。
//!
//! 覆盖 P0-3 prompt cache 优化:`build_mcp_tool_defs` 必须**字典序稳定**,
//! 跨请求同一 `MCPContext` 调多次产出 byte-equal 的 tools 列表,否则
//! Anthropic 会判定 tools 字段改动 → 全部缓存层失效。
//!
//! 注:`rmcp::model::Tool` 与 `rmcp::model::Resource`(= `Annotated<RawResource>`)
//! 来自上游 vendor crate,这里只用其公开构造路径(`Tool::new` / `RawResource::new`)。

use prost_types::{value::Kind as ProstKind, Struct, Value};
use rmcp::model::{AnnotateAble, RawResource, Tool};
use serde_json::json;
use std::sync::Arc;
use warp_multi_agent_api as api;

use crate::ai::agent::{MCPContext, MCPServer};

use super::{
    build_mcp_tool_defs, function_name, parse_mcp_tool_call, serialize_outgoing_call,
    serialize_outgoing_read_resource,
};

/// 构造一个 `rmcp::model::Tool`,带最小输入 schema。
fn mk_tool(name: &'static str, desc: &'static str) -> Tool {
    let schema: serde_json::Map<String, serde_json::Value> = json!({
        "type": "object",
        "properties": {
            "x": { "type": "string" }
        }
    })
    .as_object()
    .unwrap()
    .clone();
    Tool::new(name, desc, Arc::new(schema))
}

/// 构造 MCPServer。tools 顺序与 resources 顺序按入参原样保留(模拟上游
/// 在 HashMap iterate 顺序下可能传入的乱序输入)。
fn mk_server(
    id: &str,
    name: &str,
    tools: Vec<Tool>,
    resources: Vec<rmcp::model::Resource>,
) -> MCPServer {
    MCPServer {
        id: id.to_owned(),
        name: name.to_owned(),
        description: String::new(),
        resources,
        tools,
    }
}

fn mk_resource(uri: &str, name: &str) -> rmcp::model::Resource {
    RawResource::new(uri, name).no_annotation()
}

fn mk_args_struct() -> Struct {
    Struct {
        fields: [("x".to_string(), Value {
            kind: Some(ProstKind::StringValue("y".to_string())),
        })]
        .into_iter()
        .collect(),
    }
}

#[test]
fn build_mcp_tool_defs_is_stable_across_calls() {
    let ctx = MCPContext {
        #[allow(deprecated)]
        resources: vec![],
        #[allow(deprecated)]
        tools: vec![],
        servers: vec![
            mk_server(
                "id-b",
                "server-b",
                vec![mk_tool("zeta", "z"), mk_tool("alpha", "a")],
                vec![],
            ),
            mk_server(
                "id-a",
                "server-a",
                vec![mk_tool("beta", "b"), mk_tool("gamma", "g")],
                vec![],
            ),
        ],
    };
    let r1 = build_mcp_tool_defs(&ctx);
    let r2 = build_mcp_tool_defs(&ctx);
    assert_eq!(r1, r2, "build_mcp_tool_defs 必须确定性产出");
}

#[test]
fn build_mcp_tool_defs_outputs_lexicographic_order() {
    let ctx = MCPContext {
        #[allow(deprecated)]
        resources: vec![],
        #[allow(deprecated)]
        tools: vec![],
        servers: vec![
            mk_server(
                "id-b",
                "server-b",
                vec![mk_tool("zeta", "z"), mk_tool("alpha", "a")],
                vec![],
            ),
            mk_server(
                "id-a",
                "server-a",
                vec![mk_tool("beta", "b"), mk_tool("gamma", "g")],
                vec![],
            ),
        ],
    };
    let out = build_mcp_tool_defs(&ctx);
    let names: Vec<&str> = out.iter().map(|(n, _, _)| n.as_str()).collect();
    let expected = [
        function_name(&mk_server("id-a", "server-a", vec![], vec![]), "beta"),
        function_name(&mk_server("id-a", "server-a", vec![], vec![]), "gamma"),
        function_name(&mk_server("id-b", "server-b", vec![], vec![]), "alpha"),
        function_name(&mk_server("id-b", "server-b", vec![], vec![]), "zeta"),
    ];
    assert_eq!(
        names,
        expected.iter().map(|s| s.as_str()).collect::<Vec<_>>()
    );
}

#[test]
fn build_mcp_tool_defs_invariant_under_servers_permutation() {
    let server_a = mk_server(
        "id-a",
        "server-a",
        vec![mk_tool("beta", "b"), mk_tool("gamma", "g")],
        vec![],
    );
    let server_b = mk_server(
        "id-b",
        "server-b",
        vec![mk_tool("zeta", "z"), mk_tool("alpha", "a")],
        vec![],
    );
    let ctx1 = MCPContext {
        #[allow(deprecated)]
        resources: vec![],
        #[allow(deprecated)]
        tools: vec![],
        servers: vec![server_a.clone(), server_b.clone()],
    };
    let ctx2 = MCPContext {
        #[allow(deprecated)]
        resources: vec![],
        #[allow(deprecated)]
        tools: vec![],
        servers: vec![server_b, server_a],
    };
    assert_eq!(build_mcp_tool_defs(&ctx1), build_mcp_tool_defs(&ctx2));
}

#[test]
fn read_resource_description_is_stable_and_sorted() {
    let ctx1 = MCPContext {
        #[allow(deprecated)]
        resources: vec![],
        #[allow(deprecated)]
        tools: vec![],
        servers: vec![mk_server(
            "id-a",
            "srv",
            vec![mk_tool("t", "")],
            vec![
                mk_resource("file:///z.txt", "Z"),
                mk_resource("file:///a.txt", "A"),
            ],
        )],
    };
    let ctx2 = MCPContext {
        #[allow(deprecated)]
        resources: vec![],
        #[allow(deprecated)]
        tools: vec![],
        servers: vec![mk_server(
            "id-a",
            "srv",
            vec![mk_tool("t", "")],
            vec![
                mk_resource("file:///a.txt", "A"),
                mk_resource("file:///z.txt", "Z"),
            ],
        )],
    };
    let r1 = build_mcp_tool_defs(&ctx1);
    let r2 = build_mcp_tool_defs(&ctx2);
    assert_eq!(r1, r2, "read_resource 描述必须 byte-equal");

    let last = r1.last().expect("应至少含 read_resource");
    assert_eq!(last.0, "mcp_read_resource");
    let pos_a = last.1.find("a.txt").expect("应含 a.txt");
    let pos_z = last.1.find("z.txt").expect("应含 z.txt");
    assert!(pos_a < pos_z, "available_uris 必须按字典序排");
}

#[test]
fn function_name_uses_server_id_to_avoid_name_collisions() {
    let first = mk_server("srv-1", "dup/name", vec![], vec![]);
    let second = mk_server("srv-2", "dup:name", vec![], vec![]);

    assert_eq!(function_name(&first, "tool"), "mcp__srv-1__tool");
    assert_eq!(function_name(&second, "tool"), "mcp__srv-2__tool");
    assert_ne!(function_name(&first, "tool"), function_name(&second, "tool"));
}

#[test]
fn parse_mcp_tool_call_matches_server_by_id() {
    let ctx = MCPContext {
        #[allow(deprecated)]
        resources: vec![],
        #[allow(deprecated)]
        tools: vec![],
        servers: vec![
            mk_server("srv-1", "dup/name", vec![mk_tool("tool", "")], vec![]),
            mk_server("srv-2", "dup:name", vec![mk_tool("tool", "")], vec![]),
        ],
    };

    let tool = parse_mcp_tool_call("mcp__srv-2__tool", r#"{"x":"y"}"#, Some(&ctx)).unwrap();
    let api::message::tool_call::Tool::CallMcpTool(call) = tool else {
        panic!("expected CallMcpTool");
    };
    assert_eq!(call.server_id, "srv-2");
    assert_eq!(call.name, "tool");
}

#[test]
fn serialize_outgoing_call_roundtrips_server_id() {
    let ctx = MCPContext {
        #[allow(deprecated)]
        resources: vec![],
        #[allow(deprecated)]
        tools: vec![],
        servers: vec![mk_server("srv-2", "dup:name", vec![mk_tool("tool", "")], vec![])],
    };
    let call = api::message::tool_call::CallMcpTool {
        name: "tool".to_string(),
        args: Some(mk_args_struct()),
        server_id: "srv-2".to_string(),
    };

    let (name, args_json) = serialize_outgoing_call(&call, Some(&ctx));
    assert_eq!(name, "mcp__srv-2__tool");

    let parsed = parse_mcp_tool_call(&name, &args_json, Some(&ctx)).unwrap();
    let api::message::tool_call::Tool::CallMcpTool(parsed_call) = parsed else {
        panic!("expected CallMcpTool");
    };
    assert_eq!(parsed_call.server_id, "srv-2");
    assert_eq!(parsed_call.name, "tool");
}

#[test]
fn serialize_outgoing_read_resource_uses_server_id_field() {
    let ctx = MCPContext {
        #[allow(deprecated)]
        resources: vec![],
        #[allow(deprecated)]
        tools: vec![],
        servers: vec![mk_server(
            "srv-2",
            "dup:name",
            vec![],
            vec![mk_resource("file:///shared.txt", "shared")],
        )],
    };
    let read = api::message::tool_call::ReadMcpResource {
        uri: "file:///shared.txt".to_string(),
        server_id: "srv-2".to_string(),
    };

    let (name, args_json) = serialize_outgoing_read_resource(&read, Some(&ctx));
    assert_eq!(name, "mcp_read_resource");
    assert!(args_json.contains(r#""server_id":"srv-2""#), "got: {args_json}");
}

#[test]
fn parse_read_resource_prefers_server_id_and_keeps_legacy_server_name() {
    let ctx = MCPContext {
        #[allow(deprecated)]
        resources: vec![],
        #[allow(deprecated)]
        tools: vec![],
        servers: vec![
            mk_server(
                "srv-1",
                "dup/name",
                vec![],
                vec![mk_resource("file:///shared.txt", "shared")],
            ),
            mk_server(
                "srv-2",
                "dup:name",
                vec![],
                vec![mk_resource("file:///shared.txt", "shared")],
            ),
        ],
    };

    let by_id = parse_mcp_tool_call(
        "mcp_read_resource",
        r#"{"uri":"file:///shared.txt","server_id":"srv-2"}"#,
        Some(&ctx),
    )
    .unwrap();
    let api::message::tool_call::Tool::ReadMcpResource(by_id) = by_id else {
        panic!("expected ReadMcpResource");
    };
    assert_eq!(by_id.server_id, "srv-2");

    let legacy = parse_mcp_tool_call(
        "mcp_read_resource",
        r#"{"uri":"file:///shared.txt","server":"dup:name"}"#,
        Some(&ctx),
    )
    .unwrap();
    let api::message::tool_call::Tool::ReadMcpResource(legacy) = legacy else {
        panic!("expected ReadMcpResource");
    };
    assert_eq!(legacy.server_id, "srv-2");
}
