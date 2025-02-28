use crate::driver::test_env_api::*;
use crate::util::*;
use anyhow::bail;
use candid::Principal;
use ic_base_types::PrincipalId;
use reqwest::Url;
use slog::{debug, info, Logger};
use std::time::Duration;

pub(crate) fn store_message(url: &Url, effective_canister_id: PrincipalId, msg: &str) -> Principal {
    block_on(async {
        let agent = assert_create_agent(url.as_str()).await;
        let mcan = MessageCanister::new(&agent, effective_canister_id).await;
        // send an update call to it
        mcan.store_msg(msg.to_string()).await;
        mcan.canister_id()
    })
}

pub(crate) fn store_message_with_retries(
    url: &Url,
    effective_canister_id: PrincipalId,
    msg: &str,
    log: &Logger,
) -> Principal {
    block_on(async {
        let agent = assert_create_agent(url.as_str()).await;
        let mcan = MessageCanister::new_with_retries(
            &agent,
            effective_canister_id,
            log,
            secs(300),
            secs(10),
        )
        .await;
        // send an update call to it
        mcan.store_msg(msg.to_string()).await;
        mcan.canister_id()
    })
}

/// Try to store the given message within the next 30 seconds, return true if successful
pub(crate) fn can_store_msg(log: &Logger, url: &Url, canister_id: Principal, msg: &str) -> bool {
    block_on(async {
        match create_agent(url.as_str()).await {
            Ok(agent) => {
                debug!(log, "Try to get canister reference");
                let mcan = MessageCanister::from_canister_id(&agent, canister_id);
                debug!(log, "Success, will try to write next");
                matches!(
                    tokio::time::timeout(
                        Duration::from_secs(30),
                        mcan.try_store_msg(msg.to_string()),
                    )
                    .await,
                    Ok(Ok(_))
                )
            }
            Err(e) => {
                debug!(log, "Could not create agent: {:?}", e,);
                false
            }
        }
    })
}

/// Try to store the given message. Retry for 300 seconds or until update was unsuccessful
pub(crate) fn cannot_store_msg(log: Logger, url: &Url, canister_id: Principal, msg: &str) -> bool {
    retry(log.clone(), secs(300), secs(10), || {
        if can_store_msg(&log, url, canister_id, msg) {
            bail!("Message could still be stored.")
        } else {
            Ok(())
        }
    })
    .is_ok()
}

pub(crate) fn can_read_msg(log: &Logger, url: &Url, canister_id: Principal, msg: &str) -> bool {
    block_on(can_read_msg_impl(log, url, canister_id, msg, 0))
}

pub(crate) fn can_read_msg_with_retries(
    log: &Logger,
    url: &Url,
    canister_id: Principal,
    msg: &str,
    retries: usize,
) -> bool {
    block_on(can_read_msg_impl(log, url, canister_id, msg, retries))
}

async fn can_read_msg_impl(
    log: &Logger,
    url: &Url,
    canister_id: Principal,
    msg: &str,
    retries: usize,
) -> bool {
    for i in 0..retries + 1 {
        debug!(log, "Try to create agent for node {:?}...", url.as_str());
        match create_agent(url.as_str()).await {
            Ok(agent) => {
                debug!(log, "Try to get canister reference");
                let mcan = MessageCanister::from_canister_id(&agent, canister_id);
                debug!(log, "Success, will try to read next");
                if mcan.try_read_msg().await == Ok(Some(msg.to_string())) {
                    return true;
                } else {
                    info!(
                        log,
                        "Could not read expected message, will retry {:?} times",
                        retries - i
                    );
                }
            }
            Err(e) => {
                debug!(
                    log,
                    "Could not create agent: {:?}, will retry {:?} times",
                    e,
                    retries - i
                );
            }
        };
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    }
    false
}

pub(crate) fn get_cert_time(
    url: &url::Url,
    effective_canister_id: PrincipalId,
) -> Result<u64, String> {
    use ic_agent::lookup_value;
    block_on(async {
        let path = vec!["time".into()];
        let paths = vec![path.clone()];
        let agent = assert_create_agent(url.as_str()).await;
        match agent
            .read_state_raw(paths.clone(), effective_canister_id.into())
            .await
        {
            Ok(cert) => match lookup_value(&cert, path.clone()) {
                Ok(mut t) => Ok(leb128::read::unsigned(&mut t).unwrap()),
                Err(err) => Err(err.to_string()),
            },
            Err(err) => Err(err.to_string()),
        }
    })
}

pub(crate) fn cert_state_makes_progress_with_retries(
    url: &url::Url,
    effective_canister_id: PrincipalId,
    logger: &slog::Logger,
    timeout: Duration,
    backoff: Duration,
) {
    let mut current_timestamp: Option<u64> = None;
    retry(logger.clone(), timeout, backoff, || {
        info!(logger, "Performing read_state request...");
        let next_timestamp = {
            let timestamp = get_cert_time(url, effective_canister_id);
            if let Err(err) = timestamp {
                bail!("Cannot perform read_state request: {}", err);
            };
            timestamp.ok()
        };
        // Set initial timestamp, if not yet set.
        if current_timestamp.is_none() {
            info!(logger, "Initial timestamp recorded!");
            current_timestamp = next_timestamp;
            bail!("Timestamp hasn't advanced yet!");
        } else if next_timestamp > current_timestamp {
            info!(logger, "Timestamp advanced!");
            Ok(())
        } else {
            bail!("Timestamp hasn't advanced yet!");
        }
    })
    .expect("System should make progress!");
}

pub(crate) fn cert_state_makes_no_progress_with_retries(
    url: &url::Url,
    effective_canister_id: PrincipalId,
    logger: &slog::Logger,
    timeout: Duration,
    backoff: Duration,
) {
    let mut current_timestamp: Option<u64> = None;
    retry(logger.clone(), timeout, backoff, || {
        info!(logger, "Performing read_state request...");
        let next_timestamp = {
            let timestamp = get_cert_time(url, effective_canister_id);
            if timestamp.is_err() {
                return Ok(());
            };
            timestamp.ok()
        };
        if current_timestamp.is_none() {
            info!(logger, "Initial timestamp recorded!");
            current_timestamp = next_timestamp;
            bail!("No timestamp to compare with!");
        } else if next_timestamp > current_timestamp {
            info!(logger, "Current timestamp recorded!");
            current_timestamp = next_timestamp;
            bail!("Timestamp advanced!");
        } else {
            info!(logger, "Timestamp hasn't advanced!");
            Ok(())
        }
    })
    .expect("System shouldn't make progress!");
}

pub fn install_nns_with_customizations_and_check_progress(
    topology: TopologySnapshot,
    canister_wasm_strategy: NnsCanisterWasmStrategy,
    customizations: NnsCustomizations,
) {
    let logger = topology.test_env().logger();
    // Perfrom IC checks prior to canister installation.
    // 1. Check that all subnet nodes are healthy.
    topology.subnets().for_each(|subnet| {
        subnet.nodes().for_each(|node| {
            node.await_status_is_healthy()
                .expect("Node's status endpoint didn't report healthy")
        })
    });

    // 2. Check that all unassigned nodes (if any) are healthy.
    topology.unassigned_nodes().for_each(|node| {
        node.await_can_login_as_admin_via_ssh()
            .expect("Timeout while waiting for all unassigned nodes to be healthy");
    });
    info!(logger, "IC is healthy and ready.");

    topology
        .root_subnet()
        .nodes()
        .next()
        .unwrap()
        .install_nns_canisters_with_customizations(canister_wasm_strategy, customizations)
        .expect("NNS canisters not installed");
    info!(logger, "NNS canisters are installed.");

    topology.subnets().for_each(|subnet| {
        if subnet.subnet_id != topology.root_subnet_id() {
            subnet.nodes().for_each(|node| {
                // make sure the node is participating in a subnet
                cert_state_makes_progress_with_retries(
                    &node.get_public_url(),
                    node.effective_canister_id(),
                    &logger,
                    secs(600),
                    secs(10),
                );
            });
        }
    });
}

pub(crate) fn install_nns_and_check_progress(topology: TopologySnapshot) {
    install_nns_with_customizations_and_check_progress(
        topology,
        NnsCanisterWasmStrategy::TakeBuiltFromSources,
        NnsCustomizations::default(),
    );
}
