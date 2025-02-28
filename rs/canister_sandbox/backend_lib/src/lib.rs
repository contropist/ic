mod dts;
pub mod logging;
pub mod sandbox_manager;
pub mod sandbox_server;

use ic_canister_sandbox_common::{
    child_process_initialization, controller_client_stub, protocol, rpc,
    transport::{self, SocketReaderConfig},
};
use ic_config::embedders::Config as EmbeddersConfig;
use ic_logger::new_replica_logger_from_config;
use std::sync::Arc;

pub use ic_canister_sandbox_common::{RUN_AS_CANISTER_SANDBOX_FLAG, RUN_AS_SANDBOX_LAUNCHER_FLAG};

/// The `main()` of the canister sandbox binary. This function is called from
/// binaries such as `ic-replay` and `drun` to run as a canister sandbox.
///
/// It sets up for operation and then hands over control to the
/// RPC management system.
///
/// Sandbox processes are spawned by the replica passing in a control
/// file descriptor as file descriptor number 3 (in addition to
/// stdin/stdout/stderr). This descriptor is a unix domain socket
/// used for RPC. The RPCs are bidirectional: The sandbox process
/// receives execution and management instructions from the controller
/// process, and it calls for system call and execution state change
/// operations into the controller.
pub fn canister_sandbox_main() {
    let socket = child_process_initialization();
    let mut embedder_config_arg = None;

    let mut args = std::env::args();
    while let Some(arg) = args.next() {
        if arg.as_str() == "--embedder-config" {
            let config_arg = args.next().expect("Missing embedder config.");
            embedder_config_arg = Some(
                serde_json::from_str(config_arg.as_str())
                    .expect("Could not parse the argument, invalid embedder config value."),
            )
        }
    }
    let embedder_config = embedder_config_arg
        .expect("Error from the sandbox process due to unknown embedder config.");

    // Currently Wasmtime uses the default rayon thread-pool with a thread per core.
    // In production this results in 64 threads. This MR reduces the default
    // thread pool size to 10 in the sandbox process because
    // benchmarks show that 10 is the sweet spot.
    rayon::ThreadPoolBuilder::new()
        .num_threads(EmbeddersConfig::default().num_rayon_compilation_threads)
        .build_global()
        .unwrap();

    run_canister_sandbox(socket, embedder_config);
}

/// Runs the canister sandbox service in the calling thread. The service
/// will use the given unix domain socket as its only means of
/// communication. It expects execution IPC commands to passed as
/// inputs on this communication channel, and will communicate
/// completions as well as auxiliary requests back on this channel.
pub fn run_canister_sandbox(
    socket: std::os::unix::net::UnixStream,
    embedder_config: EmbeddersConfig,
) {
    // TODO(RUN-204): Get the logger config from the replica instead of
    // hardcoding the parameters.
    let logger_config = ic_config::logger::Config {
        target: ic_config::logger::LogTarget::Stderr,
        level: slog::Level::Warning,
        ..Default::default()
    };
    let (log, _log_guard) = new_replica_logger_from_config(&logger_config);

    let socket = Arc::new(socket);

    let out_stream =
        transport::UnixStreamMuxWriter::<protocol::transport::SandboxToController>::new(
            Arc::clone(&socket),
        );

    let request_out_stream = out_stream.make_sink::<protocol::ctlsvc::Request>();
    let reply_out_stream = out_stream.make_sink::<protocol::sbxsvc::Reply>();

    // Construct RPC channel client to controller.
    let reply_handler = Arc::new(rpc::ReplyManager::<protocol::ctlsvc::Reply>::new());
    let controller = Arc::new(controller_client_stub::ControllerClientStub::new(Arc::new(
        rpc::Channel::new(request_out_stream, reply_handler.clone()),
    )));

    // Construct RPC server for the  service offered by this binary,
    // namely access to the sandboxed canister runner functions.
    let svc = Arc::new(sandbox_server::SandboxServer::new(
        sandbox_manager::SandboxManager::new(controller, embedder_config, log),
    ));

    // Wrap it all up to handle frames received on socket -- either
    // replies to our outgoing requests, or incoming requests to the
    // RPC service offered by this binary.
    let frame_handler = transport::Demux::<_, _, protocol::transport::ControllerToSandbox>::new(
        Arc::new(rpc::ServerStub::new(svc, reply_out_stream)),
        reply_handler,
    );

    // It is fine if we fail to spawn this thread. Used for fault
    // injection only.
    let inject_failure = std::env::var("SANDBOX_TESTING_ON_MALICIOUS_SHUTDOWN_MANUAL").is_ok();
    if inject_failure {
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(10));
            std::process::exit(1);
        });
    }

    // Run RPC operations on the stream socket.
    transport::socket_read_messages::<_, _>(
        move |message| {
            frame_handler.handle(message);
        },
        socket,
        SocketReaderConfig::for_sandbox(),
    );
}
