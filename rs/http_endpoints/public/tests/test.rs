// Using a `pub mod` works around spurious dead code warnings; see
// https://users.rust-lang.org/t/invalid-dead-code-warning-for-submodule-in-integration-test/80259/2 and
// https://github.com/rust-lang/rust/issues/46379
pub mod common;

use crate::common::{
    basic_consensus_pool_cache, basic_registry_client, basic_state_manager_mock,
    create_conn_and_send_request, get_free_localhost_socket_addr, start_http_endpoint,
    wait_for_status_healthy,
};
use hyper::{client::conn::handshake, Body, Client, Method, Request, StatusCode};
use ic_agent::{
    agent::{http_transport::ReqwestHttpReplicaV2Transport, QueryBuilder, UpdateBuilder},
    agent_error::HttpErrorPayload,
    export::Principal,
    hash_tree::Label,
    identity::AnonymousIdentity,
    Agent, AgentError,
};
use ic_config::http_handler::Config;
use ic_interfaces_registry_mocks::MockRegistryClient;
use ic_pprof::Pprof;
use ic_protobuf::registry::crypto::v1::{
    AlgorithmId as AlgorithmIdProto, PublicKey as PublicKeyProto,
};
use ic_registry_keys::make_crypto_threshold_signing_pubkey_key;
use ic_test_utilities::{consensus::MockConsensusCache, mock_time, types::ids::subnet_test_id};
use ic_types::{
    batch::{BatchPayload, ValidationContext},
    consensus::{dkg::Dealings, Block, Payload, Rank},
    crypto::{threshold_sig::ThresholdSigPublicKey, CryptoHash, CryptoHashOf},
    messages::{Blob, HttpQueryResponse, HttpQueryResponseReply},
    Height, RegistryVersion,
};
use prost::Message;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use tokio::{
    net::TcpStream,
    runtime::Runtime,
    time::{sleep, Duration},
};
use tower::ServiceExt;

#[test]
fn test_healthy_behind() {
    let rt = Runtime::new().unwrap();
    let addr = get_free_localhost_socket_addr();
    let config = Config {
        listen_addr: addr,
        ..Default::default()
    };
    let certified_state_height = Height::from(1);
    let consensus_height = Height::from(certified_state_height.get() + 25);

    let mock_state_manager = basic_state_manager_mock();

    // We use this atomic to make sure that the health transistion is from healthy -> certified_state_behind
    let healthy = Arc::new(AtomicBool::new(false));
    let healthy_c = healthy.clone();
    let mut mock_consensus_cache = MockConsensusCache::new();
    mock_consensus_cache
        .expect_finalized_block()
        .returning(move || {
            // The last certified height seen in a block is used to determine if
            // replica is behind.
            let certified_height = if !healthy_c.load(Ordering::SeqCst) {
                certified_state_height
            } else {
                consensus_height
            };
            Block::new(
                CryptoHashOf::from(CryptoHash(Vec::new())),
                Payload::new(
                    ic_types::crypto::crypto_hash,
                    (
                        BatchPayload::default(),
                        Dealings::new_empty(Height::from(1)),
                        None,
                    )
                        .into(),
                ),
                Height::from(224),
                Rank(456),
                ValidationContext {
                    registry_version: RegistryVersion::from(99),
                    certified_height,
                    time: mock_time(),
                },
            )
        });

    let mut mock_registry_client = MockRegistryClient::new();
    mock_registry_client
        .expect_get_latest_version()
        .return_const(RegistryVersion::from(1));
    mock_registry_client
        .expect_get_value()
        .withf(move |key, version| {
            key == make_crypto_threshold_signing_pubkey_key(subnet_test_id(1)).as_str()
                && version == &RegistryVersion::from(1)
        })
        .return_const({
            let pk = PublicKeyProto {
                algorithm: AlgorithmIdProto::ThresBls12381 as i32,
                key_value: [42; ThresholdSigPublicKey::SIZE].to_vec(),
                version: 0,
                proof_data: None,
                timestamp: Some(42),
            };
            let mut v = Vec::new();
            pk.encode(&mut v).unwrap();
            Ok(Some(v))
        });

    start_http_endpoint(
        rt.handle().clone(),
        config,
        Arc::new(mock_state_manager),
        Arc::new(mock_consensus_cache),
        Arc::new(mock_registry_client),
        Arc::new(Pprof::default()),
    );

    let agent = Agent::builder()
        .with_transport(ReqwestHttpReplicaV2Transport::create(format!("http://{}", addr)).unwrap())
        .build()
        .unwrap();

    let status = rt.block_on(async {
        wait_for_status_healthy(&agent).await.unwrap();
        healthy.store(true, Ordering::SeqCst);
        agent.status().await.unwrap()
    });

    assert_eq!(
        status.replica_health_status,
        Some("certified_state_behind".to_string())
    );
}

// Check spec enforcement for read_state requests. https://internetcomputer.org/docs/current/references/ic-interface-spec#http-read-state
// Paths containing `.../canister_id/..` require the `canister_id` to be the same as the effective canister id
// specified through the url `/api/v2/canister/<effective_canister_id>/read_state`. Read state requests that request paths
// with different canister ids should be rejected.
#[test]
fn test_unathorized_controller() {
    let rt = Runtime::new().unwrap();
    let addr = get_free_localhost_socket_addr();
    let config = Config {
        listen_addr: addr,
        ..Default::default()
    };

    let mock_state_manager = basic_state_manager_mock();
    let mock_consensus_cache = basic_consensus_pool_cache();
    let mock_registry_client = basic_registry_client();

    start_http_endpoint(
        rt.handle().clone(),
        config,
        Arc::new(mock_state_manager),
        Arc::new(mock_consensus_cache),
        Arc::new(mock_registry_client),
        Arc::new(Pprof::default()),
    );

    let agent = Agent::builder()
        .with_transport(ReqwestHttpReplicaV2Transport::create(format!("http://{}", addr)).unwrap())
        .build()
        .unwrap();

    let canister1 = Principal::from_text("223xb-saaaa-aaaaf-arlqa-cai").unwrap();
    let canister2 = Principal::from_text("224lq-3aaaa-aaaaf-ase7a-cai").unwrap();
    let paths: Vec<Vec<Label>> = vec![vec![
        "canister".into(),
        canister2.into(),
        "metadata".into(),
        "time".into(),
    ]];

    let expected_error = AgentError::HttpError(HttpErrorPayload {
        status: 400,
        content_type: Some("text/plain".to_string()),
        content: format!(
            "Effective canister id in URL {} does not match requested canister id: {}.",
            canister1, canister2
        )
        .as_bytes()
        .to_vec(),
    });
    rt.block_on(async {
        wait_for_status_healthy(&agent).await.unwrap();
        assert_eq!(
            agent.read_state_raw(paths.clone(), canister1).await,
            Err(expected_error)
        );
    });
}

// Test that that http endpoint rejects queries with mismatch between canister id an effective canister id.
#[test]
fn test_unathorized_query() {
    let rt = Runtime::new().unwrap();
    let addr = get_free_localhost_socket_addr();
    let config = Config {
        listen_addr: addr,
        ..Default::default()
    };

    let mock_state_manager = basic_state_manager_mock();
    let mock_consensus_cache = basic_consensus_pool_cache();
    let mock_registry_client = basic_registry_client();

    let (_, _, mut query_handler) = start_http_endpoint(
        rt.handle().clone(),
        config,
        Arc::new(mock_state_manager),
        Arc::new(mock_consensus_cache),
        Arc::new(mock_registry_client),
        Arc::new(Pprof::default()),
    );

    let agent = Agent::builder()
        .with_transport(ReqwestHttpReplicaV2Transport::create(format!("http://{}", addr)).unwrap())
        .build()
        .unwrap();

    let canister1 = Principal::from_text("223xb-saaaa-aaaaf-arlqa-cai").unwrap();
    let canister2 = Principal::from_text("224lq-3aaaa-aaaaf-ase7a-cai").unwrap();

    // Query mock that returns empty Ok("success") response.
    rt.spawn(async move {
        loop {
            let (_, resp) = query_handler.next_request().await.unwrap();
            resp.send_response(HttpQueryResponse::Replied {
                reply: HttpQueryResponseReply {
                    arg: Blob("success".into()),
                },
            })
        }
    });

    // Query call tests.
    let mut query_tests = Vec::new();

    // Valid query call with canister_id = effective_canister_id
    let query = QueryBuilder::new(&agent, canister1, "test".to_string())
        .with_effective_canister_id(canister1)
        .with_arg(Vec::new())
        .sign()
        .unwrap();
    let expected_resp = "success".into();
    query_tests.push((query, Ok(expected_resp)));

    // Invalid query call with canister_id != effective_canister_id
    let query = QueryBuilder::new(&agent, canister1, "test".to_string())
        .with_effective_canister_id(canister2)
        .with_arg(Vec::new())
        .sign()
        .unwrap();
    let expected_resp = AgentError::HttpError(HttpErrorPayload {
        status: 400,
        content_type: Some("text/plain".to_string()),
        content: format!(
            "Specified CanisterId {} does not match effective canister id in URL {}",
            canister1, canister2
        )
        .as_bytes()
        .to_vec(),
    });
    query_tests.push((query, Err(expected_resp)));

    rt.block_on(async {
        wait_for_status_healthy(&agent).await.unwrap();
        for (query, expected_resp) in query_tests {
            assert_eq!(
                agent
                    .query_signed(query.effective_canister_id, query.signed_query)
                    .await,
                expected_resp
            );
        }
    });
}

// Test that that http endpoint rejects calls with mismatch between canister id an effective canister id.
#[test]
fn test_unathorized_call() {
    let rt = Runtime::new().unwrap();
    let addr = get_free_localhost_socket_addr();
    let config = Config {
        listen_addr: addr,
        ..Default::default()
    };

    let mock_state_manager = basic_state_manager_mock();
    let mock_consensus_cache = basic_consensus_pool_cache();
    let mock_registry_client = basic_registry_client();

    let (mut ingress_filter, mut ingress_sender, _) = start_http_endpoint(
        rt.handle().clone(),
        config,
        Arc::new(mock_state_manager),
        Arc::new(mock_consensus_cache),
        Arc::new(mock_registry_client),
        Arc::new(Pprof::default()),
    );

    let agent = Agent::builder()
        .with_identity(AnonymousIdentity)
        .with_transport(ReqwestHttpReplicaV2Transport::create(format!("http://{}", addr)).unwrap())
        .build()
        .unwrap();

    let canister1 = Principal::from_text("223xb-saaaa-aaaaf-arlqa-cai").unwrap();
    let canister2 = Principal::from_text("224lq-3aaaa-aaaaf-ase7a-cai").unwrap();

    // Ingress sender mock that returns empty Ok(()) response.
    rt.spawn(async move {
        loop {
            let (_, resp) = ingress_sender.next_request().await.unwrap();
            resp.send_response(Ok(()))
        }
    });

    // Ingress filter mock that returns empty Ok(()) response.
    rt.spawn(async move {
        loop {
            let (_, resp) = ingress_filter.next_request().await.unwrap();
            resp.send_response(Ok(()))
        }
    });

    // Query call tests.
    let mut update_tests = Vec::new();

    // Valid update call with canister_id = effective_canister_id
    let update = UpdateBuilder::new(&agent, canister1, "test".to_string())
        .with_effective_canister_id(canister1)
        .with_arg(Vec::new())
        .sign()
        .unwrap();
    update_tests.push((update.clone(), Ok(update.request_id)));

    // Invalid update call with canister_id != effective_canister_id
    let update = UpdateBuilder::new(&agent, canister1, "test".to_string())
        .with_effective_canister_id(canister2)
        .with_arg(Vec::new())
        .sign()
        .unwrap();
    let expected_resp = AgentError::HttpError(HttpErrorPayload {
        status: 400,
        content_type: Some("text/plain".to_string()),
        content: format!(
            "Specified CanisterId {} does not match effective canister id in URL {}",
            canister1, canister2
        )
        .as_bytes()
        .to_vec(),
    });
    update_tests.push((update, Err(expected_resp)));

    // Update call to mgmt canister with different effective canister id.
    let update = UpdateBuilder::new(&agent, Principal::management_canister(), "test".to_string())
        .with_effective_canister_id(canister2)
        .with_arg(Vec::new())
        .sign()
        .unwrap();
    update_tests.push((update.clone(), Ok(update.request_id)));

    // Update call to mgmt canister.
    let update = UpdateBuilder::new(&agent, Principal::management_canister(), "test".to_string())
        .with_effective_canister_id(Principal::management_canister())
        .with_arg(Vec::new())
        .sign()
        .unwrap();
    update_tests.push((update.clone(), Ok(update.request_id)));

    rt.block_on(async {
        wait_for_status_healthy(&agent).await.unwrap();
        for (update, expected_resp) in update_tests {
            assert_eq!(
                agent
                    .update_signed(update.effective_canister_id, update.signed_update)
                    .await,
                expected_resp
            );
        }
    });
}

/// Once we have reached the number of outstanding connection, new connections should be refused.
#[tokio::test]
async fn test_max_tcp_connections() {
    let rt_handle = tokio::runtime::Handle::current();
    let addr = get_free_localhost_socket_addr();
    let config = Config {
        listen_addr: addr,
        max_tcp_connections: 50,
        ..Default::default()
    };

    let mock_state_manager = basic_state_manager_mock();
    let mock_consensus_cache = basic_consensus_pool_cache();
    let mock_registry_client = basic_registry_client();

    // Start server
    start_http_endpoint(
        rt_handle.clone(),
        config.clone(),
        Arc::new(mock_state_manager),
        Arc::new(mock_consensus_cache),
        Arc::new(mock_registry_client),
        Arc::new(Pprof::default()),
    );

    // Create max connections and store to prevent connections from being closed
    let mut senders = vec![];
    for _i in 0..config.max_tcp_connections {
        let (request_sender, status_code) = create_conn_and_send_request(addr).await;
        senders.push(request_sender);
        assert!(status_code == StatusCode::OK);
    }

    // Expect additional connection to trigger error
    let target_stream = TcpStream::connect(addr)
        .await
        .expect("tcp connection to server address failed");
    let (_request_sender, connection) = handshake(target_stream)
        .await
        .expect("tcp client handshake failed");
    assert!(connection.await.err().unwrap().is_incomplete_message());
}

/// Once no bytes are read for the duration of 'connection_read_timeout_seconds', then
/// the connection is dropped.
#[tokio::test]
async fn test_connection_read_timeout() {
    let rt_handle = tokio::runtime::Handle::current();
    let addr = get_free_localhost_socket_addr();
    let config = Config {
        listen_addr: addr,
        max_tcp_connections: 50,
        connection_read_timeout_seconds: 2,
        ..Default::default()
    };

    let mock_state_manager = basic_state_manager_mock();
    let mock_consensus_cache = basic_consensus_pool_cache();
    let mock_registry_client = basic_registry_client();

    // Start server
    start_http_endpoint(
        rt_handle.clone(),
        config.clone(),
        Arc::new(mock_state_manager),
        Arc::new(mock_consensus_cache),
        Arc::new(mock_registry_client),
        Arc::new(Pprof::default()),
    );

    let (mut request_sender, status_code) = create_conn_and_send_request(addr).await;
    assert!(status_code == StatusCode::OK);

    sleep(Duration::from_secs(
        config.connection_read_timeout_seconds + 1,
    ))
    .await;
    assert!(request_sender.ready().await.err().unwrap().is_closed());
}

/// If the downstream service is stuck return 504.
#[test]
fn test_request_timeout() {
    let rt = Runtime::new().unwrap();
    let addr = get_free_localhost_socket_addr();
    let request_timeout_seconds = 2;
    let config = Config {
        listen_addr: addr,
        request_timeout_seconds,
        ..Default::default()
    };

    let mock_state_manager = basic_state_manager_mock();
    let mock_consensus_cache = basic_consensus_pool_cache();
    let mock_registry_client = basic_registry_client();

    let (_, _, mut query_handler) = start_http_endpoint(
        rt.handle().clone(),
        config,
        Arc::new(mock_state_manager),
        Arc::new(mock_consensus_cache),
        Arc::new(mock_registry_client),
        Arc::new(Pprof::default()),
    );

    let agent = Agent::builder()
        .with_transport(ReqwestHttpReplicaV2Transport::create(format!("http://{}", addr)).unwrap())
        .build()
        .unwrap();

    let canister = Principal::from_text("223xb-saaaa-aaaaf-arlqa-cai").unwrap();
    let query = QueryBuilder::new(&agent, canister, "test".to_string())
        .with_effective_canister_id(canister)
        .with_arg(Vec::new())
        .sign()
        .unwrap();

    rt.spawn(async move {
        loop {
            let (_, resp) = query_handler.next_request().await.unwrap();
            sleep(Duration::from_secs(request_timeout_seconds + 1)).await;
            resp.send_response(HttpQueryResponse::Replied {
                reply: HttpQueryResponseReply {
                    arg: Blob("success".into()),
                },
            })
        }
    });

    rt.block_on(async {
        loop {
            let resp = agent
                .query_signed(query.effective_canister_id, query.signed_query.clone())
                .await;
            if let Err(ic_agent::AgentError::HttpError(ref http_error)) = resp {
                match StatusCode::from_u16(http_error.status).unwrap() {
                    StatusCode::GATEWAY_TIMEOUT => break,
                    // the service may be unhealthy due to initialization, retry
                    StatusCode::SERVICE_UNAVAILABLE => sleep(Duration::from_millis(250)).await,
                    _ => panic!("Received unexpeceted response: {:?}", resp),
                }
            } else {
                panic!("Received unexpeceted response: {:?}", resp);
            }
        }
    });
}

/// Iff a http request body is greater than the configured limit, the endpoints responds with `413`.
#[test]
fn test_payload_too_large() {
    let rt = Runtime::new().unwrap();
    let addr = get_free_localhost_socket_addr();
    let config = Config {
        listen_addr: addr,
        max_request_size_bytes: 2048,
        ..Default::default()
    };

    let mock_state_manager = basic_state_manager_mock();
    let mock_consensus_cache = basic_consensus_pool_cache();
    let mock_registry_client = basic_registry_client();

    _ = start_http_endpoint(
        rt.handle().clone(),
        config.clone(),
        Arc::new(mock_state_manager),
        Arc::new(mock_consensus_cache),
        Arc::new(mock_registry_client),
        Arc::new(Pprof::default()),
    );

    let request = |body: Vec<u8>| {
        rt.block_on(async {
            let client = Client::new();

            let req = Request::builder()
                .method(Method::POST)
                .uri(format!(
                    "http://{}/api/v2/canister/{}/query",
                    addr, "223xb-saaaa-aaaaf-arlqa-cai"
                ))
                .header("Content-Type", "application/cbor")
                .body(Body::from(body))
                .expect("request builder");

            let response = client.request(req).await.unwrap();

            response.status()
        })
    };

    let mut body = vec![0; config.max_request_size_bytes.try_into().unwrap()];
    assert_ne!(StatusCode::PAYLOAD_TOO_LARGE, request(body.clone()));

    body.push(1);
    assert_eq!(StatusCode::PAYLOAD_TOO_LARGE, request(body.clone()));
}

/// Iff a http request body is slower to arrive than the configured limit, the endpoints responds with `408`.
#[test]
fn test_request_too_slow() {
    let rt = Runtime::new().unwrap();
    let addr = get_free_localhost_socket_addr();
    let config = Config {
        listen_addr: addr,
        max_request_receive_seconds: 1,
        ..Default::default()
    };

    let mock_state_manager = basic_state_manager_mock();
    let mock_consensus_cache = basic_consensus_pool_cache();
    let mock_registry_client = basic_registry_client();

    let _ = start_http_endpoint(
        rt.handle().clone(),
        config,
        Arc::new(mock_state_manager),
        Arc::new(mock_consensus_cache),
        Arc::new(mock_registry_client),
        Arc::new(Pprof::default()),
    );

    rt.block_on(async {
        let (mut sender, body) = Body::channel();

        assert!(sender
            .send_data(bytes::Bytes::from("hello world"))
            .await
            .is_ok());

        let client = Client::new();

        let req = Request::builder()
            .method(Method::POST)
            .uri(format!(
                "http://{}/api/v2/canister/{}/query",
                addr, "223xb-saaaa-aaaaf-arlqa-cai"
            ))
            .header("Content-Type", "application/cbor")
            .body(body)
            .expect("request builder");

        let response = client.request(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::REQUEST_TIMEOUT);
    })
}
