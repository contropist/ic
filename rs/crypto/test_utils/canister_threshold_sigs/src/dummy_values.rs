use crate::{
    create_params_for_dealers, mock_transcript, mock_unmasked_transcript_type, set_of_nodes,
};
use ic_types::crypto::canister_threshold_sig::idkg::{
    IDkgComplaint, IDkgDealing, IDkgOpening, IDkgTranscriptId, IDkgTranscriptOperation,
    InitialIDkgDealings, SignedIDkgDealing,
};
use ic_types::crypto::{BasicSig, BasicSigOf};
use ic_types::signature::BasicSignature;
use ic_types::{Height, NodeId, PrincipalId, SubnetId};
use std::collections::BTreeSet;

pub fn dummy_idkg_transcript_id_for_tests(id: u64) -> IDkgTranscriptId {
    let subnet = SubnetId::from(PrincipalId::new_subnet_test_id(314159));
    let height = Height::new(42);
    IDkgTranscriptId::new(subnet, id, height)
}

pub fn dummy_idkg_dealing_for_tests() -> IDkgDealing {
    IDkgDealing {
        transcript_id: IDkgTranscriptId::new(
            SubnetId::from(PrincipalId::new_subnet_test_id(1)),
            1,
            Height::new(1),
        ),
        internal_dealing_raw: vec![],
    }
}

pub fn dummy_dealings(
    transcript_id: IDkgTranscriptId,
    dealers: &BTreeSet<NodeId>,
) -> Vec<SignedIDkgDealing> {
    let mut dealings = Vec::new();
    for node_id in dealers {
        let signed_dealing = SignedIDkgDealing {
            content: IDkgDealing {
                transcript_id,
                internal_dealing_raw: format!("Dummy raw dealing for dealer {}", node_id)
                    .into_bytes(),
            },
            signature: BasicSignature {
                signature: BasicSigOf::new(BasicSig(vec![])),
                signer: *node_id,
            },
        };
        dealings.push(signed_dealing);
    }
    dealings
}

pub fn dummy_initial_idkg_dealing_for_tests() -> InitialIDkgDealings {
    let previous_receivers = set_of_nodes(&[35, 36, 37, 38]);
    let previous_transcript =
        mock_transcript(Some(previous_receivers), mock_unmasked_transcript_type());
    let dealers = set_of_nodes(&[35, 36, 38]);

    // For a Resharing Unmasked transcript, the dealer set should be a subset of the previous receiver set.
    assert!(dealers.is_subset(previous_transcript.receivers.get()));

    let params = create_params_for_dealers(
        &dealers,
        IDkgTranscriptOperation::ReshareOfUnmasked(previous_transcript),
    );
    let dealings = dummy_dealings(params.transcript_id(), &dealers);

    InitialIDkgDealings::new(params, dealings)
        .expect("Failed creating IDkgInitialDealings for testing")
}

pub fn dummy_idkg_opening_for_tests(complaint: &IDkgComplaint) -> IDkgOpening {
    IDkgOpening {
        transcript_id: complaint.transcript_id,
        dealer_id: complaint.dealer_id,
        internal_opening_raw: vec![],
    }
}
