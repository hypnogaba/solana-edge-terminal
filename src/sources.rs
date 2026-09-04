//! What the terminal is reading from, and where each of those is configured.
//!
//! The feeds were only ever visible in the unit file. A viewer watching the
//! screen could not tell whether a quiet market meant a quiet market or a
//! source that never connected, and the operator had no way to answer "which
//! feed is this" without leaving the page.

use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct Source {
    /// The lane name the rest of the screen uses for this feed.
    pub name: String,
    pub kind: &'static str,
    /// Safe to show. Never a URL and never a key: the RPC endpoints carry
    /// api keys, so what is published is the name of the variable holding
    /// the endpoint, which is also the thing an operator needs to find it.
    pub detail: String,
    /// Where an operator changes this feed.
    pub set_by: String,
}

/// The shred feed, described from the flags that select it.
pub fn shred_source(iface: &str, group: std::net::Ipv4Addr, ports: &[u16]) -> Source {
    let ports: Vec<String> = ports.iter().map(|p| p.to_string()).collect();
    Source {
        name: "doublezero".to_string(),
        kind: "multicast shreds",
        detail: format!("{iface} · {group}:{}", ports.join(",")),
        set_by: "--iface --group --port".to_string(),
    }
}

/// One RPC lane, described from its `name=...` spec without publishing the
/// endpoint. An inline URL is called out, because a URL in argv is readable
/// by every user on the box, in `ps` and in `systemctl status`.
pub fn lane_source(spec: &str) -> Source {
    let (name, value) = spec.split_once('=').unwrap_or((spec, ""));
    let detail = match value.strip_prefix("env:") {
        Some(var) => format!("endpoint in ${var}"),
        None if value.is_empty() => "no endpoint set".to_string(),
        None => "endpoint inline in argv · move it to env:".to_string(),
    };
    Source {
        name: name.to_string(),
        kind: "rpc websocket",
        detail,
        set_by: format!("--lane {name}=env:VAR"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_lane_publishes_the_variable_name_and_never_the_endpoint() {
        // The whole point of env: is that the URL, which carries the key, is
        // not in argv. Printing it on the page would give that back.
        let source = lane_source("commercial=env:RPC_WS_COMMERCIAL");
        assert_eq!(source.name, "commercial");
        assert_eq!(source.detail, "endpoint in $RPC_WS_COMMERCIAL");
        assert!(!source.detail.contains("wss"));
    }

    #[test]
    fn an_inline_url_is_reported_as_a_problem_rather_than_printed() {
        let source = lane_source("public=wss://rpc.example.com/?api-key=abc123");
        assert!(!source.detail.contains("rpc.example.com"), "{}", source.detail);
        assert!(!source.detail.contains("abc123"), "{}", source.detail);
        assert!(source.detail.contains("move it to env:"));
    }

    #[test]
    fn the_shred_source_names_the_interface_and_the_group() {
        let source = shred_source("doublezero1", "233.84.178.1".parse().unwrap(), &[7733, 7734]);
        assert_eq!(source.detail, "doublezero1 · 233.84.178.1:7733,7734");
        assert!(source.set_by.contains("--group"));
    }

    #[test]
    fn a_lane_with_no_endpoint_says_so_rather_than_looking_configured() {
        assert_eq!(lane_source("bare").detail, "no endpoint set");
    }
}
