#!/bin/bash
set -euo pipefail

NNS_TOOLS_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" &>/dev/null && pwd)
source "$NNS_TOOLS_DIR/lib/include.sh"

help() {
    print_green "
Usage: $0 <CANISTER_NAME> (<NNS_URL> <NEURON_ID>)
  CANISTER_NAME: Human readable canister name (from rs/nns/canister_ids.json)
  NNS_URL: The url to the subnet running the NNS in your testnet.
  NEURON_ID: The neuron used to submit proposals (should have following to immediately pass)

  NOTE: Both NNS_URL and NEURON_ID can be passed in or set as environment variables.
    Using \"source \$YOUR_WORKING_DIRECTORY/output_vars_nns_state_deployment.sh\" will give you the needed
    variables in your shell.

  This script will upgrade a canister on a given testnet with a given neuron id.  If that neuron does not have sufficient
  voting power to pass the proposal, the proposal will still have to be voted in.
  "
    exit 1
}

if [ $# -lt 1 ]; then
    help
fi

CANISTER_NAME=$1
NNS_URL=${2:-$NNS_URL}
NEURON_ID=${3:-$NEURON_ID}

ensure_variable_set NNS_URL || help
ensure_variable_set NEURON_ID || help

# Allow overriding PEM file, but default to shared identity
export PEM=${PEM:-$NNS_TOOLS_DIR/test_user.pem}

build_canister_and_propose_upgrade_pem "$NNS_URL" "$NEURON_ID" "$PEM" "$CANISTER_NAME"

# Wait for upgrade.
WASM_FILE="$(repo_root)/$(canister_bazel_artifact_path "${CANISTER_NAME}")"
for i in {1..20}; do
    echo "Testing if upgrade was successful..."
    if nns_canister_has_file_contents_installed $NNS_URL $CANISTER_NAME $WASM_FILE; then
        print_green "OK: Upgrade was successful."
        exit 0
    fi

    sleep 10
done

print_red "ERROR: Upgrade failed."
exit 1
