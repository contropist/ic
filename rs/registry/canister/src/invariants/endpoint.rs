use crate::invariants::common::{
    get_node_records_from_snapshot, InvariantCheckError, RegistrySnapshot,
};

use std::{
    convert::TryFrom,
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    str,
};

use prost::alloc::collections::BTreeSet;

use ic_protobuf::registry::node::v1::{ConnectionEndpoint, Protocol};

#[cfg(target_arch = "wasm32")]
use dfn_core::println;

/// Node records are valid with connection endpoints containing
/// syntactically correct data ("ip_addr" field parses as an IP address,
/// "port" field is <= 65535):
///    * An Xnet endpoint entry exists
///    * A HTTP endpoint entry exists (either .http)
///    * IP address is not 0.0.0.0 ("unspecified" address)
///    * IP address is not 255.255.255.255 ("broadcast" address)
///    * We might want to ban others as well: must be global, not link-local
///    * IP address and ports are distinct (i.e., no two nodes share the same
///      ip:port pairs for anything, no node has the same ip:port for multiple
///      endpoints), i.e., all IP:port-pairs of all nodes are mutually exclusive
///      (this includes the prometheus-endpoints)
/// Strict check imposes stricter rules on IP addresses
pub(crate) fn check_endpoint_invariants(
    snapshot: &RegistrySnapshot,
    strict: bool,
) -> Result<(), InvariantCheckError> {
    let mut valid_endpoints = BTreeSet::<(IpAddr, u16)>::new();
    let node_records = get_node_records_from_snapshot(snapshot);
    let common_error_prefix = format!(
        "Invariant violation detected among {} node records",
        node_records.len()
    );
    for (node_id, node_record) in node_records {
        let error_prefix = format!("{common_error_prefix} (checking failed for node {node_id})");

        // P2P endpoints
        //    * For each of the flow endpoints that belong to one node, the identifier
        //      must be distinct (a node may have multiple flow endpoints). That is, the
        //      address-port-pair of different flow endpoints of the same node can be
        //      the same, but the flow identifier must be different.
        for endpoint in node_record.p2p_flow_endpoints {
            let connection_endpoint = match endpoint.endpoint {
                None => {
                    return Err(InvariantCheckError {
                        msg: format!("{error_prefix}: No connection endpoint specified"),
                        source: None,
                    })
                }
                Some(ep) => ep,
            };
            validate_endpoint(&connection_endpoint, false, strict)?;
        }

        // The Boolean indicates whether an unspecified address should be tolerated
        let mut endpoints_to_check = Vec::<(ConnectionEndpoint, bool)>::new();

        if node_record.xnet.is_none() {
            return Err(InvariantCheckError {
                msg: format!("{error_prefix}: No Xnet endpoint found for node"),
                source: None,
            });
        }
        endpoints_to_check.push((node_record.xnet.unwrap(), false));

        if node_record.http.is_none() {
            return Err(InvariantCheckError {
                msg: format!("{error_prefix}: No HTTP/Public API endpoint found"),
                source: None,
            });
        }
        endpoints_to_check.push((node_record.http.unwrap(), false));

        let mut new_valid_endpoints = BTreeSet::<(IpAddr, u16)>::new();

        // Validate all endpoints of this node (excluding p2p flow endpoints which are
        // validated separately)
        for (endpoint, tolerate_unspecified_ip) in endpoints_to_check {
            let valid_endpoint = validate_endpoint(&endpoint, tolerate_unspecified_ip, strict)?;
            // Multiple nodes may have unspecified addresses, so duplicates should be avioded only for specified endpoints
            if !valid_endpoint.0.is_unspecified() && !new_valid_endpoints.insert(valid_endpoint) {
                let error: Result<(), InvariantCheckError> = Err(InvariantCheckError {
                    msg: format!(
                        "{error_prefix}: Duplicate endpoint ({:?}, {:?}); previous endpoints: {new_valid_endpoints:?}",
                        &endpoint.ip_addr, &endpoint.port
                    ),
                    source: None,
                });
                // TODO: change to `return error;` after NNS1-2228 is closed.
                println!("WARNING: {error:?}");
            }
        }

        // Check that there are _some_ node endpoints
        if new_valid_endpoints.is_empty() {
            let error: Result<(), InvariantCheckError> = Err(InvariantCheckError {
                msg: format!("{error_prefix}: No endpoints to validate"),
                source: None,
            });
            // TODO: change to `return error;` after NNS1-2228 is closed.
            println!("WARNING: {error:?}");
        }

        // Check that there is no intersection with other nodes
        if !new_valid_endpoints.is_disjoint(&valid_endpoints) {
            let error: Result<(), InvariantCheckError> = Err(InvariantCheckError {
                msg: format!(
                    "{error_prefix}: Duplicate endpoints detected across nodes; new_valid_endpoints = {}",
                    new_valid_endpoints.iter().map(|x| if valid_endpoints.contains(x) {
                        format!("{x:?} (duplicate)")
                    } else {
                        format!("{x:?} (new)")
                    })
                    .collect::<Vec<String>>()
                    .join(", ")
                ),
                source: None,
            });
            // TODO: change to `return error;` after NNS1-2228 is closed.
            println!("WARNING: {error:?}");
        }

        // All is good -- add current endpoints to global set
        valid_endpoints.append(&mut new_valid_endpoints);
    }

    Ok(())
}

/// A helper function that validates invariants for a single endpoint
///    * IP address is valid (either v4 or v6, correct format)
///    * Port number is valid (<= 65535)
///    * IP address is not unspecified
///    * IP address is not broadcast
///    * IP address is not a multicast address
///
/// If `tolerate_unspecified_ip` is set, allow the IP to be unspecified, e.g., 0.0.0.0
///
/// If `strict` is set, also checks that:
///    * IPv4 address is not private, reserved, documentation address,
///      link-local, benchmarking
///    * IPv6 address is not link-local or unique-local unicast address
fn validate_endpoint(
    endpoint: &ConnectionEndpoint,
    tolerate_unspecified_ip: bool,
    strict: bool,
) -> Result<(IpAddr, u16), InvariantCheckError> {
    if endpoint.protocol() != Protocol::Http1
        && endpoint.protocol() != Protocol::Http1Tls13
        && endpoint.protocol() != Protocol::P2p1Tls13
    {
        return Err(InvariantCheckError {
            msg: format!(
                "Endpoint protocol is not supported: {:?}",
                endpoint.protocol
            ),
            source: None,
        });
    }

    let ip: IpAddr = endpoint
        .ip_addr
        .parse::<IpAddr>()
        .map_err(|e| InvariantCheckError {
            msg: format!("Failed to parse IP address: {:?}", endpoint.ip_addr),
            source: Some(Box::new(e)),
        })?;

    let port = u16::try_from(endpoint.port).map_err(|e| InvariantCheckError {
        msg: format!("Failed to parse port: {:?}", endpoint.port),
        source: Some(Box::new(e)),
    })?;

    if !tolerate_unspecified_ip && ip.is_unspecified() {
        return Err(InvariantCheckError {
            msg: format!("IP Address {:?} is unspecified", ip),
            source: None,
        });
    }

    if let IpAddr::V4(ipv4) = ip {
        if ipv4.is_broadcast() {
            return Err(InvariantCheckError {
                msg: format!("IP Address {:?} is a broadcast address", ip),
                source: None,
            });
        }

        if ipv4.is_multicast() {
            return Err(InvariantCheckError {
                msg: format!("IP Address {:?} is a multicast address", ip),
                source: None,
            });
        }
    } else if let IpAddr::V6(ipv6) = ip {
        let multicast_addr_and_mask = Ipv6Addr::new(0xff00, 0, 0, 0, 0, 0, 0, 0);
        if mask_ipv6(ipv6, multicast_addr_and_mask) == multicast_addr_and_mask {
            return Err(InvariantCheckError {
                msg: format!("IP Address {:?} is a multicast address", ip),
                source: None,
            });
        }
    }

    if strict {
        if ip.is_loopback() {
            return Err(InvariantCheckError {
                msg: format!("IP Address {:?} is the loopback address", ip),
                source: None,
            });
        }

        if let IpAddr::V4(ipv4) = ip {
            if ipv4.is_private() {
                return Err(InvariantCheckError {
                    msg: format!("IP Address {:?} is a private address", ip),
                    source: None,
                });
            }
            if ipv4.is_link_local() {
                return Err(InvariantCheckError {
                    msg: format!("IP Address {:?} is a link local address", ip),
                    source: None,
                });
            }
            for (addr, mask, res_type) in &IPV4_STRICT_CHECKS {
                if mask_ipv4(ipv4, *mask) == *addr {
                    return Err(InvariantCheckError {
                        msg: format!("IP Address {:?} is not allowed ({})", ip, res_type),
                        source: None,
                    });
                }
            }
        } else if let IpAddr::V6(ipv6) = ip {
            for (addr, mask, res_type) in &IPV6_STRICT_CHECKS {
                if mask_ipv6(ipv6, *mask) == *addr {
                    return Err(InvariantCheckError {
                        msg: format!("IP Address {:?} is not allowed ({})", ip, res_type),
                        source: None,
                    });
                }
            }
        }
    }

    Ok((ip, port))
}

const IPV4_STRICT_CHECKS: [(Ipv4Addr, Ipv4Addr, &str); 6] = [
    (
        Ipv4Addr::new(240, 0, 0, 0),
        Ipv4Addr::new(0xf0, 0, 0, 0),
        "RESERVED - IETF RFC 1112",
    ),
    (
        Ipv4Addr::new(192, 0, 0, 0),
        Ipv4Addr::new(255, 255, 255, 0),
        "IETF PROTOCOL ASSIGNMENT - IETF RFC 6890",
    ),
    (
        Ipv4Addr::new(198, 18, 0, 0),
        Ipv4Addr::new(255, 0xfe, 0, 0),
        "BENCHMARKING - IETF RFC 2544 errata 423",
    ),
    (
        Ipv4Addr::new(192, 0, 2, 0),
        Ipv4Addr::new(255, 255, 255, 0),
        "DOCUMENTATION - IETF RFC 5737 - TEST-NET-1",
    ),
    (
        Ipv4Addr::new(198, 51, 100, 0),
        Ipv4Addr::new(255, 255, 255, 0),
        "DOCUMENTATION - IETF RFC 5737 - TEST-NET-2",
    ),
    (
        Ipv4Addr::new(203, 0, 113, 0),
        Ipv4Addr::new(255, 255, 255, 0),
        "DOCUMENTATION - IETF RFC 5737 - TEST-NET-3",
    ),
];

const IPV6_STRICT_CHECKS: [(Ipv6Addr, Ipv6Addr, &str); 4] = [
    (
        Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 0),
        Ipv6Addr::new(0xffff, 0xc000, 0, 0, 0, 0, 0, 0),
        "UNICAST LINK LOCAL - IETF RFC 4291 sec. 2.4",
    ),
    (
        Ipv6Addr::new(0xfc00, 0, 0, 0, 0, 0, 0, 0),
        Ipv6Addr::new(0xfffe, 0, 0, 0, 0, 0, 0, 0),
        "UNICAST UNIQUE LOCAL - IETF RFC 4193",
    ),
    (
        Ipv6Addr::new(0xfec0, 0, 0, 0, 0, 0, 0, 0),
        Ipv6Addr::new(0xfffe, 0, 0, 0, 0, 0, 0, 0),
        "UNICAST SITE LOCAL - IETF RFC 4291 sec. 2.5.7",
    ),
    (
        Ipv6Addr::new(0x2001, 0x0db8, 0, 0, 0, 0, 0, 0),
        Ipv6Addr::new(0xffff, 0xffff, 0, 0, 0, 0, 0, 0),
        "IPv6 DOCUMENTATION - IETF RFC 3849",
    ),
];

fn mask_ipv4(addr: Ipv4Addr, mask: Ipv4Addr) -> Ipv4Addr {
    let octets: Vec<u8> = addr
        .octets()
        .iter()
        .zip(mask.octets().iter())
        .map(|(a, m)| a & m)
        .collect();

    Ipv4Addr::new(octets[0], octets[1], octets[2], octets[3])
}

fn mask_ipv6(addr: Ipv6Addr, mask: Ipv6Addr) -> Ipv6Addr {
    let segments: Vec<u16> = addr
        .segments()
        .iter()
        .zip(mask.segments().iter())
        .map(|(a, m)| a & m)
        .collect();

    Ipv6Addr::new(
        segments[0],
        segments[1],
        segments[2],
        segments[3],
        segments[4],
        segments[5],
        segments[6],
        segments[7],
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use ic_base_types::{NodeId, PrincipalId};
    use ic_nns_common::registry::encode_or_panic;
    use ic_protobuf::registry::node::v1::{FlowEndpoint, NodeRecord};
    use ic_registry_keys::make_node_record_key;

    #[test]
    fn test_validate_endpoint() {
        let loopback_ipv4_endpoint = ConnectionEndpoint {
            ip_addr: "127.0.0.1".to_string(),
            port: 8080,
            protocol: Protocol::Http1 as i32,
        };
        assert!(validate_endpoint(&loopback_ipv4_endpoint, false, true).is_err());

        let loopback_ipv6_endpoint = ConnectionEndpoint {
            ip_addr: "::1".to_string(),
            port: 8080,
            protocol: Protocol::Http1 as i32,
        };
        assert!(validate_endpoint(&loopback_ipv6_endpoint, false, true).is_err());

        let bad_port_endpoint = ConnectionEndpoint {
            ip_addr: "212.13.11.77".to_string(),
            port: 80802,
            protocol: Protocol::Http1 as i32,
        };
        assert!(validate_endpoint(&bad_port_endpoint, false, true).is_err());
        assert!(validate_endpoint(&bad_port_endpoint, false, false).is_err());

        let bad_ipv4_endpoint = ConnectionEndpoint {
            ip_addr: "280.13.11.77".to_string(),
            port: 8080,
            protocol: Protocol::Http1 as i32,
        };
        assert!(validate_endpoint(&bad_ipv4_endpoint, false, true).is_err());
        assert!(validate_endpoint(&bad_ipv4_endpoint, false, false).is_err());

        let bad_ipv6_endpoint = ConnectionEndpoint {
            ip_addr: "0fab:12345::".to_string(),
            port: 8080,
            protocol: Protocol::Http1 as i32,
        };
        assert!(validate_endpoint(&bad_ipv6_endpoint, false, true).is_err());
        assert!(validate_endpoint(&bad_ipv6_endpoint, false, false).is_err());

        let multicast_ipv4_endpoint = ConnectionEndpoint {
            ip_addr: "224.0.0.1".to_string(),
            port: 8080,
            protocol: Protocol::Http1 as i32,
        };
        assert!(validate_endpoint(&multicast_ipv4_endpoint, false, true).is_err());
        assert!(validate_endpoint(&multicast_ipv4_endpoint, false, false).is_err());

        let multicast_ipv6_endpoint = ConnectionEndpoint {
            ip_addr: "ff00:1:2::".to_string(),
            port: 8080,
            protocol: Protocol::Http1 as i32,
        };
        assert!(validate_endpoint(&multicast_ipv6_endpoint, false, true).is_err());
        assert!(validate_endpoint(&multicast_ipv6_endpoint, false, false).is_err());

        let private_ipv4_endpoint = ConnectionEndpoint {
            ip_addr: "192.168.0.1".to_string(),
            port: 8080,
            protocol: Protocol::Http1 as i32,
        };
        assert!(validate_endpoint(&private_ipv4_endpoint, false, true).is_err());
        assert!(validate_endpoint(&private_ipv4_endpoint, false, false).is_ok());

        let unique_ipv6_endpoint = ConnectionEndpoint {
            ip_addr: "fc00:1234::".to_string(),
            port: 8080,
            protocol: Protocol::Http1 as i32,
        };
        assert!(validate_endpoint(&unique_ipv6_endpoint, false, true).is_err());
        assert!(validate_endpoint(&unique_ipv6_endpoint, false, false).is_ok());
    }

    #[test]
    fn test_endpoints_invariants() {
        let mut snapshot = RegistrySnapshot::new();

        // Valid node
        let node_id = NodeId::from(PrincipalId::new_node_test_id(1));
        snapshot.insert(
            make_node_record_key(node_id).into_bytes(),
            encode_or_panic::<NodeRecord>(&NodeRecord {
                node_operator_id: vec![0],
                p2p_flow_endpoints: vec![FlowEndpoint {
                    endpoint: Some(ConnectionEndpoint {
                        ip_addr: "200.1.1.1".to_string(),
                        port: 8080,
                        protocol: Protocol::P2p1Tls13 as i32,
                    }),
                }],
                http: Some(ConnectionEndpoint {
                    ip_addr: "200.1.1.3".to_string(),
                    port: 9000,
                    protocol: Protocol::Http1 as i32,
                }),
                xnet: Some(ConnectionEndpoint {
                    ip_addr: "200.1.1.3".to_string(),
                    port: 9001,
                    protocol: Protocol::Http1 as i32,
                }),
                chip_id: vec![],
                hostos_version_id: None,
            }),
        );

        assert!(check_endpoint_invariants(&snapshot, true).is_ok());

        // Add a node with conflicting sockets
        let node_id = NodeId::from(PrincipalId::new_node_test_id(2));
        let key = make_node_record_key(node_id).into_bytes();
        snapshot.insert(
            key.clone(),
            encode_or_panic::<NodeRecord>(&NodeRecord {
                node_operator_id: vec![0],
                p2p_flow_endpoints: vec![FlowEndpoint {
                    endpoint: Some(ConnectionEndpoint {
                        ip_addr: "200.1.1.3".to_string(),
                        port: 8080,
                        protocol: Protocol::P2p1Tls13 as i32,
                    }),
                }],
                http: Some(ConnectionEndpoint {
                    ip_addr: "200.1.1.3".to_string(),
                    port: 9000,
                    protocol: Protocol::Http1 as i32,
                }),
                xnet: Some(ConnectionEndpoint {
                    ip_addr: "200.1.1.1".to_string(),
                    port: 9001,
                    protocol: Protocol::Http1 as i32,
                }),
                chip_id: vec![],
                hostos_version_id: None,
            }),
        );
        // TODO: change to `assert!(check_endpoint_invariants(&snapshot, true).is_err());` after NNS1-2228 is closed.
        assert!(check_endpoint_invariants(&snapshot, true).is_ok());

        snapshot.remove(&key);

        // Add a node with conflicting flow IDs
        let node_id = NodeId::from(PrincipalId::new_node_test_id(2));
        let key = make_node_record_key(node_id).into_bytes();
        snapshot.insert(
            key,
            encode_or_panic::<NodeRecord>(&NodeRecord {
                node_operator_id: vec![0],
                p2p_flow_endpoints: vec![FlowEndpoint {
                    endpoint: Some(ConnectionEndpoint {
                        ip_addr: "200.1.1.3".to_string(),
                        port: 8080,
                        protocol: Protocol::P2p1Tls13 as i32,
                    }),
                }],
                http: Some(ConnectionEndpoint {
                    ip_addr: "200.1.1.2".to_string(),
                    port: 9000,
                    protocol: Protocol::Http1 as i32,
                }),
                xnet: Some(ConnectionEndpoint {
                    ip_addr: "200.1.1.2".to_string(),
                    port: 9001,
                    protocol: Protocol::Http1 as i32,
                }),
                chip_id: vec![],
                hostos_version_id: None,
            }),
        );
        check_endpoint_invariants(&snapshot, true).unwrap();
    }

    #[test]
    fn test_mask_ip() {
        assert_eq!(
            mask_ipv4(
                Ipv4Addr::new(192, 168, 13, 241),
                Ipv4Addr::new(255, 255, 255, 0)
            ),
            Ipv4Addr::new(192, 168, 13, 0)
        );
        assert_eq!(
            mask_ipv4(
                Ipv4Addr::new(192, 168, 13, 241),
                Ipv4Addr::new(255, 255, 0, 0)
            ),
            Ipv4Addr::new(192, 168, 0, 0)
        );
        assert_eq!(
            mask_ipv4(
                Ipv4Addr::new(192, 168, 0xaa, 241),
                Ipv4Addr::new(255, 255, 0xf0, 0)
            ),
            Ipv4Addr::new(192, 168, 0xa0, 0)
        );
        assert_eq!(
            mask_ipv6(
                Ipv6Addr::new(0xabcd, 0xdef0, 0x1234, 0x5678, 0x9abc, 0, 0, 0x1234),
                Ipv6Addr::new(0xffff, 0xffff, 0xffff, 0xffff, 0, 0, 0, 0)
            ),
            Ipv6Addr::new(0xabcd, 0xdef0, 0x1234, 0x5678, 0, 0, 0, 0)
        );
        assert_eq!(
            mask_ipv6(
                Ipv6Addr::new(0xabcd, 0xdef0, 0x1234, 0x5678, 0x9abc, 0, 0, 0x1234),
                Ipv6Addr::new(0xffff, 0xffff, 0xffff, 0xff00, 0, 0, 0, 0)
            ),
            Ipv6Addr::new(0xabcd, 0xdef0, 0x1234, 0x5600, 0, 0, 0, 0)
        );
    }
}
