//! Utilities for non-interactive Distributed Key Generation (NI-DKG), and
//! for testing distributed key generation and threshold signing.

use ic_crypto_internal_types::NodeIndex;
use ic_crypto_temp_crypto::{NodeKeysToGenerate, TempCryptoComponent};
use ic_interfaces::crypto::{KeyManager, NiDkgAlgorithm, ThresholdSigner};
use ic_registry_client_fake::FakeRegistryClient;
use ic_registry_keys::make_crypto_node_key;
use ic_registry_proto_data_provider::ProtoRegistryDataProvider;
use ic_types::consensus::get_faults_tolerated;
use ic_types::crypto::threshold_sig::ni_dkg::config::{NiDkgConfig, NiDkgConfigData};
use ic_types::crypto::threshold_sig::ni_dkg::{
    DkgId, NiDkgDealing, NiDkgId, NiDkgTag, NiDkgTargetId, NiDkgTargetSubnet, NiDkgTranscript,
};
use ic_types::crypto::{KeyPurpose, Signable, ThresholdSigShareOf};
use ic_types::{Height, NodeId, NumberOfNodes, PrincipalId, RegistryVersion, SubnetId};
use rand::prelude::*;
use std::cmp;
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::convert::TryFrom;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;

mod initial_config;

pub use initial_config::{initial_dkg_transcript, InitialNiDkgConfig};

pub fn create_transcript(
    ni_dkg_config: &NiDkgConfig,
    crypto_components: &BTreeMap<NodeId, TempCryptoComponent>,
    dealings: &BTreeMap<NodeId, NiDkgDealing>,
    node_id: NodeId,
) -> NiDkgTranscript {
    crypto_for(node_id, crypto_components)
        .create_transcript(ni_dkg_config, dealings)
        .unwrap_or_else(|error| {
            panic!("failed to create transcript for {:?}: {:?}", node_id, error)
        })
}

pub fn load_transcript(
    transcript: &NiDkgTranscript,
    crypto_components: &BTreeMap<NodeId, TempCryptoComponent>,
    node_id: NodeId,
) {
    crypto_for(node_id, crypto_components)
        .load_transcript(transcript)
        .unwrap_or_else(|error| panic!("failed to load transcript for {:?}: {:?}", node_id, error));
}

/// Load transcript on each node (if resharing), create all dealings, and build
/// transcript from those dealings.
pub fn run_ni_dkg_and_create_single_transcript(
    ni_dkg_config: &NiDkgConfig,
    crypto_components: &BTreeMap<NodeId, TempCryptoComponent>,
) -> NiDkgTranscript {
    let dealings =
        load_resharing_transcript_if_needed_and_create_dealings(ni_dkg_config, crypto_components);
    let transcript_creator = ni_dkg_config.dealers().get().iter().next().unwrap();
    create_transcript(
        ni_dkg_config,
        crypto_components,
        &dealings,
        *transcript_creator,
    )
}

pub fn load_resharing_transcript_if_needed_and_create_dealings(
    ni_dkg_config: &NiDkgConfig,
    crypto_components: &BTreeMap<NodeId, TempCryptoComponent>,
) -> BTreeMap<NodeId, NiDkgDealing> {
    ni_dkg_config
        .dealers()
        .get()
        .iter()
        .map(|node| {
            let dealing = load_resharing_transcript_and_create_dealing(
                ni_dkg_config,
                crypto_components,
                *node,
            );
            (*node, dealing)
        })
        .collect()
}

pub fn create_dealings(
    ni_dkg_config: &NiDkgConfig,
    crypto_components: &BTreeMap<NodeId, TempCryptoComponent>,
) -> BTreeMap<NodeId, NiDkgDealing> {
    ni_dkg_config
        .dealers()
        .get()
        .iter()
        .map(|node| {
            let dealing = create_dealing(ni_dkg_config, crypto_components, *node);
            (*node, dealing)
        })
        .collect()
}

pub fn create_dealing(
    ni_dkg_config: &NiDkgConfig,
    crypto_components: &BTreeMap<NodeId, TempCryptoComponent>,
    node_id: NodeId,
) -> NiDkgDealing {
    crypto_for(node_id, crypto_components)
        .create_dealing(ni_dkg_config)
        .unwrap_or_else(|error| {
            panic!(
                "failed to create NI-DKG dealing for {:?}: {:?}",
                node_id, error
            )
        })
}

pub fn load_resharing_transcript_and_create_dealing(
    ni_dkg_config: &NiDkgConfig,
    crypto_components: &BTreeMap<NodeId, TempCryptoComponent>,
    node_id: NodeId,
) -> NiDkgDealing {
    if let Some(resharing_transcript) = ni_dkg_config.resharing_transcript() {
        load_transcript(resharing_transcript, crypto_components, node_id);
    }

    create_dealing(ni_dkg_config, crypto_components, node_id)
}

pub fn verify_dealing(
    ni_dkg_config: &NiDkgConfig,
    crypto_components: &BTreeMap<NodeId, TempCryptoComponent>,
    dealer_node_id: NodeId,
    verifier_node_id: NodeId,
    dealing: &NiDkgDealing,
) {
    crypto_for(verifier_node_id, crypto_components)
        .verify_dealing(ni_dkg_config, dealer_node_id, dealing)
        .unwrap_or_else(|error| {
            panic!(
                "failed to verify NI-DKG dealing by {:?} for {:?}: {:?}",
                dealer_node_id, verifier_node_id, error
            )
        });
}

pub fn retain_only_active_keys(
    crypto_components: &BTreeMap<NodeId, TempCryptoComponent>,
    node_id: NodeId,
    retained_transcripts: HashSet<NiDkgTranscript>,
) {
    crypto_for(node_id, crypto_components)
        .retain_only_active_keys(retained_transcripts)
        .unwrap_or_else(|error| {
            panic!(
                "failed to retain active keys for {:?}: {:?}",
                node_id, error
            )
        });
}

pub fn sign_threshold_for_each<H: Signable>(
    signers: &[NodeId],
    msg: &H,
    dkg_id: NiDkgId,
    crypto_components: &BTreeMap<NodeId, TempCryptoComponent>,
) -> BTreeMap<NodeId, ThresholdSigShareOf<H>> {
    signers
        .iter()
        .map(|signer| {
            let sig_share = crypto_for(*signer, crypto_components)
                .sign_threshold(msg, DkgId::NiDkgId(dkg_id))
                .unwrap_or_else(|_| panic!("signing by node {:?} failed", signer));
            (*signer, sig_share)
        })
        .collect()
}

pub struct RandomNiDkgConfigBuilder {
    subnet_size: Option<usize>,
    dealer_count: Option<usize>,
    dkg_tag: Option<NiDkgTag>,
    registry_version: Option<RegistryVersion>,
}

impl RandomNiDkgConfigBuilder {
    pub fn subnet_size(mut self, subnet_size: usize) -> Self {
        self.subnet_size = Some(subnet_size);
        self
    }

    pub fn dealer_count(mut self, dealer_count: usize) -> Self {
        self.dealer_count = Some(dealer_count);
        self
    }

    pub fn dkg_tag(mut self, dkg_tag: NiDkgTag) -> Self {
        self.dkg_tag = Some(dkg_tag);
        self
    }

    pub fn registry_version(mut self, registry_version: RegistryVersion) -> Self {
        self.registry_version = Some(registry_version);
        self
    }

    pub fn build(self) -> RandomNiDkgConfig {
        let subnet_size = self.subnet_size.expect("must specify a subnet size");

        let dkg_tag = self.dkg_tag.unwrap_or_else(|| {
            let rng = &mut thread_rng();
            match rng.gen::<bool>() {
                true => NiDkgTag::LowThreshold,
                false => NiDkgTag::HighThreshold,
            }
        });

        // The registry version is used as DKG epoch and an epoch is u32. Because of
        // this, the maximum registry version we choose is u32::MAX, decreased by a
        // margin that allows for increasing it again sufficiently during tests.
        let registry_version = self.registry_version.unwrap_or_else(|| {
            let rng = &mut thread_rng();
            RegistryVersion::new(rng.gen_range(1..u32::MAX - 10_000) as u64)
        });

        let dealer_count = self.dealer_count.unwrap_or_else(|| {
            let rng = &mut thread_rng();
            let threshold = dkg_tag.threshold_for_subnet_of_size(subnet_size);
            let required_dealer_count = threshold;
            let dealer_surplus = rng.gen_range(0..3);
            required_dealer_count + dealer_surplus
        });

        RandomNiDkgConfig::new(subnet_size, dkg_tag, registry_version, dealer_count)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct RandomNiDkgConfig(NiDkgConfig);

impl RandomNiDkgConfig {
    const ONE_REGISTRY_VERSION: RegistryVersion = RegistryVersion::new(1);

    pub fn get(&self) -> &NiDkgConfig {
        &self.0
    }

    pub fn into_config(self) -> NiDkgConfig {
        self.0
    }

    pub fn builder() -> RandomNiDkgConfigBuilder {
        RandomNiDkgConfigBuilder {
            subnet_size: None,
            dealer_count: None,
            dkg_tag: None,
            registry_version: None,
        }
    }

    /// Creates a random NI-DKG config for `dkg_tag` satisfying all
    /// invariants.
    pub fn new(
        subnet_size: usize,
        dkg_tag: NiDkgTag,
        registry_version: RegistryVersion,
        num_of_dealers: usize,
    ) -> Self {
        assert!(subnet_size > 0, "subnet must not be empty");
        let rng = &mut thread_rng();

        let receivers = Self::random_node_ids(subnet_size);
        let threshold = dkg_tag.threshold_for_subnet_of_size(subnet_size);
        let dealers = {
            assert!(
                num_of_dealers >= threshold,
                "dealers must be at least the threshold"
            );
            // Exclude receivers from being dealers because initial DKG is done by NNS for
            // another (remote) subnet, which means the dealers and receivers are disjoint.
            Self::random_node_ids_excluding(&receivers, num_of_dealers)
        };

        let config_data = NiDkgConfigData {
            dkg_id: NiDkgId {
                start_block_height: Height::new(random()),
                dealer_subnet: SubnetId::from(PrincipalId::new_subnet_test_id(random())),
                dkg_tag,
                // The first DKG is always done by NNS for another (remote) subnet
                target_subnet: NiDkgTargetSubnet::Remote(NiDkgTargetId::new(random())),
            },
            max_corrupt_dealers: Self::number_of_nodes_from_usize(rng.gen_range(0..dealers.len())),
            dealers,
            max_corrupt_receivers: {
                Self::number_of_nodes_from_usize(get_faults_tolerated(subnet_size))
            },
            receivers,
            threshold: Self::number_of_nodes_from_usize(threshold),
            registry_version,
            resharing_transcript: None,
        };
        Self(NiDkgConfig::new(config_data).expect("invariant violated"))
    }

    /// Create a random NI-DKG config that is a peer for another
    /// configuration with the opposite threshold requirements so it
    /// is possible to perform tests where a single subnet has both
    /// high and low threshold transcripts
    pub fn new_with_inverted_threshold(&self) -> Self {
        let rng = &mut thread_rng();

        let dkg_tag = match self.0.dkg_id().dkg_tag {
            NiDkgTag::LowThreshold => NiDkgTag::HighThreshold,
            NiDkgTag::HighThreshold => NiDkgTag::LowThreshold,
        };

        let subnet_size = self.0.receivers().get().len();
        let receivers = self.0.receivers().get().clone();
        let threshold = dkg_tag.threshold_for_subnet_of_size(subnet_size);
        let dealers = {
            let required_dealer_count = threshold;
            let dealer_surplus = rng.gen_range(0..3);
            // Exclude receivers from being dealers because initial DKG is done by NNS for
            // another (remote) subnet, which means the dealers and receivers are disjoint.
            Self::random_node_ids_excluding(&receivers, required_dealer_count + dealer_surplus)
        };

        let config_data = NiDkgConfigData {
            dkg_id: NiDkgId {
                start_block_height: self.0.dkg_id().start_block_height,
                dealer_subnet: self.0.dkg_id().dealer_subnet,
                dkg_tag,
                target_subnet: self.0.dkg_id().target_subnet,
            },
            max_corrupt_dealers: Self::number_of_nodes_from_usize(rng.gen_range(0..dealers.len())),
            dealers,
            max_corrupt_receivers: {
                Self::number_of_nodes_from_usize(get_faults_tolerated(subnet_size))
            },
            receivers,
            threshold: Self::number_of_nodes_from_usize(threshold),
            registry_version: self.0.registry_version() + Self::ONE_REGISTRY_VERSION,
            resharing_transcript: None,
        };

        Self(NiDkgConfig::new(config_data).expect("invariant violated"))
    }

    /// Reshares the config into a new random NI-DKG config with the given
    /// `transcript`.
    ///
    /// The subnet size is changed dynamically based on subnet_size_change
    /// with minimum of 1 and maximum of `max_subnet_size`. If new nodes are
    /// added as part of the resizing, the registry version is increased by
    /// 1.
    pub fn reshare(
        transcript: NiDkgTranscript,
        subnet_size_change: std::ops::RangeInclusive<isize>,
        max_subnet_size: usize,
    ) -> Self {
        let rng = &mut thread_rng();

        // let max_corrupt_dealers = self.0.max_corrupt_receivers();
        let max_corrupt_dealers = Self::number_of_nodes_from_usize(get_faults_tolerated(
            transcript.committee.get().len(),
        ));
        let dealers = {
            let lower_bound_u32 = cmp::max(
                max_corrupt_dealers.get() + 1, // Ensures #dealers > max_corrupt_dealers
                transcript.threshold.get().get(), // Ensures #dealers >= resharing threshold
            );
            let lower_bound = usize::try_from(lower_bound_u32).expect("conversion error");
            let dealer_count = rng.gen_range(lower_bound..=transcript.committee.get().len());
            let dealers_vec = transcript
                .committee
                .get()
                .iter()
                .copied()
                .choose_multiple(rng, dealer_count);
            dealers_vec.into_iter().collect()
        };
        let new_subnet_size = {
            let transcript_committee_len_isize =
                isize::try_from(transcript.committee.get().len()).expect("conversion error");

            let change_in_subnet_size =
                rng.gen_range(*subnet_size_change.start()..=*subnet_size_change.end());

            let new_subnet_size_isize =
                cmp::max(1, transcript_committee_len_isize + change_in_subnet_size);
            let new_subnet_size = usize::try_from(new_subnet_size_isize).expect("conversion error");
            cmp::min(new_subnet_size, max_subnet_size)
        };
        let mut registry_version = transcript.registry_version;
        let receivers = {
            if new_subnet_size <= transcript.committee.get().len() {
                // Keep as many receivers as needed from the existing ones
                let receivers_vec = transcript
                    .committee
                    .get()
                    .iter()
                    .copied()
                    .choose_multiple(rng, new_subnet_size);
                receivers_vec.into_iter().collect()
            } else {
                // Keep all existing receivers and add new ones as needed
                let committee = transcript.committee.get();
                let additional_receivers_count = new_subnet_size - committee.len();
                let additional_receivers =
                    Self::random_node_ids_excluding(committee, additional_receivers_count);
                let receivers = committee.union(&additional_receivers).copied().collect();
                // Adding of nodes means that new nodes will be added to the registry
                // which in turn means that the registry version needs to be bumped up
                registry_version += Self::ONE_REGISTRY_VERSION;
                receivers
            }
        };
        let dkg_tag = transcript.dkg_id.dkg_tag;
        let config_data = NiDkgConfigData {
            dkg_id: NiDkgId {
                start_block_height: Height::new(transcript.dkg_id.start_block_height.get() + 1),
                // Theoretically the subnet ID should change on the _first_ DKG in the new
                // subnet, but this is not important: relevant is only that
                // the NiDkgId is different, which is already achieved by
                // increasing the start_block_height.
                dealer_subnet: transcript.dkg_id.dealer_subnet,
                dkg_tag,
                target_subnet: NiDkgTargetSubnet::Local,
            },
            max_corrupt_dealers,
            dealers,
            max_corrupt_receivers: {
                Self::number_of_nodes_from_usize(get_faults_tolerated(new_subnet_size))
            },
            receivers,
            threshold: Self::number_of_nodes_from_usize(
                dkg_tag.threshold_for_subnet_of_size(new_subnet_size),
            ),
            registry_version,
            resharing_transcript: Some(transcript),
        };
        Self(NiDkgConfig::new(config_data).expect("invariant violated"))
    }

    pub fn receiver_ids(&self) -> HashSet<NodeId> {
        self.get().receivers().get().iter().cloned().collect()
    }

    pub fn random_receiver_id(&self) -> NodeId {
        let rng = &mut thread_rng();
        let receiver_vec: Vec<_> = self.receiver_ids().into_iter().collect();
        *receiver_vec.choose(rng).expect("nodes empty")
    }

    pub fn dealer_ids(&self) -> HashSet<NodeId> {
        self.get().dealers().get().iter().cloned().collect()
    }

    pub fn random_dealer_id(&self) -> NodeId {
        let rng = &mut thread_rng();
        let dealer_vec: Vec<_> = self.dealer_ids().into_iter().collect();
        *dealer_vec.choose(rng).expect("nodes empty")
    }

    pub fn all_node_ids(&self) -> HashSet<NodeId> {
        self.receiver_ids()
            .union(&self.dealer_ids())
            .cloned()
            .collect()
    }

    fn random_node_ids(n: usize) -> BTreeSet<NodeId> {
        let rng = &mut thread_rng();
        let mut node_ids = BTreeSet::new();
        while node_ids.len() < n {
            node_ids.insert(Self::node_id(rng.gen()));
        }
        node_ids
    }

    fn random_node_ids_excluding(exclusions: &BTreeSet<NodeId>, n: usize) -> BTreeSet<NodeId> {
        let rng = &mut thread_rng();
        let mut node_ids = BTreeSet::new();
        while node_ids.len() < n {
            let candidate = Self::node_id(rng.gen());
            if !exclusions.contains(&candidate) {
                node_ids.insert(candidate);
            }
        }
        assert!(node_ids.is_disjoint(exclusions));
        node_ids
    }

    fn node_id(id: u64) -> NodeId {
        NodeId::from(PrincipalId::new_node_test_id(id))
    }

    fn number_of_nodes_from_usize(count: usize) -> NumberOfNodes {
        let count = NodeIndex::try_from(count).expect("node index overflow");
        NumberOfNodes::from(count)
    }
}

pub struct NiDkgTestEnvironment {
    pub crypto_components: BTreeMap<NodeId, TempCryptoComponent>,
    pub registry_data: Arc<ProtoRegistryDataProvider>,
    pub registry: Arc<FakeRegistryClient>,
}

impl NiDkgTestEnvironment {
    /// Creates a new empty test environment.
    pub fn new() -> Self {
        let registry_data = Arc::new(ProtoRegistryDataProvider::new());
        let registry = Arc::new(FakeRegistryClient::new(Arc::clone(&registry_data) as Arc<_>));
        Self {
            crypto_components: BTreeMap::new(),
            registry_data,
            registry,
        }
    }

    /// Creates a new test environment appropriate for the given config.
    pub fn new_for_config(config: &NiDkgConfig) -> Self {
        let mut env = Self::new();
        env.update_for_config(config);
        env
    }

    /// Ensures that all node IDs appearing in the given `ni_dkg_config`
    /// have (1) a crypto component and (2) a DKG dealing encryption
    /// public key in the registry. If registry entries need to be
    /// added, they are added for the config's registry version.
    ///
    /// Additionally, for all node IDs that no longer appear in the
    /// `ni_dkg_config`, the crypto components are removed.
    pub fn update_for_config(&mut self, ni_dkg_config: &NiDkgConfig) {
        let new_node_ids = self.added_nodes(ni_dkg_config);
        for node_id in new_node_ids {
            self.add_crypto_component_and_registry_entry(ni_dkg_config, node_id);
        }
        self.registry.update_to_latest_version();
        self.cleanup_unused_nodes(ni_dkg_config);
    }

    /// Serializes this environment to disk.
    ///
    /// The resulting directory structure can be loaded back into
    /// a new `NiDkgTestEnvironment` using the `new_from_dir` function.
    pub fn save_to_dir(&self, toplevel_path: &Path) {
        for (node_id, crypto_component) in self.crypto_components.iter() {
            crypto_component.copy_crypto_root_to(&toplevel_path.join(node_id.to_string()));
        }

        self.registry_data
            .write_to_file(toplevel_path.join("registry_data.pb"));
    }

    /// Deserializes a new `NiDkgTestEnvironment` from disk.
    ///
    /// Note that this only works if the environment was originally serialized
    /// using `save_to_dir`.
    pub fn new_from_dir(toplevel_path: &Path) -> Self {
        fn node_ids_from_dir_names(toplevel_path: &Path) -> BTreeMap<NodeId, PathBuf> {
            std::fs::read_dir(toplevel_path)
                .expect("crypto_root directory doesn't exist")
                .into_iter()
                .map(|e| e.unwrap().path())
                .filter(|e| e.is_dir())
                .map(|p| {
                    (
                        NodeId::from(
                            PrincipalId::from_str(p.file_name().unwrap().to_str().unwrap())
                                .expect("directory name is not a node ID"),
                        ),
                        p,
                    )
                })
                .collect()
        }

        let registry_data = Arc::new(ProtoRegistryDataProvider::load_from_file(
            toplevel_path.join("registry_data.pb"),
        ));
        let registry = Arc::new(FakeRegistryClient::new(Arc::clone(&registry_data) as Arc<_>));

        let mut ret = NiDkgTestEnvironment {
            crypto_components: BTreeMap::new(),
            registry_data,
            registry,
        };
        for (node_id, crypto_root) in node_ids_from_dir_names(toplevel_path) {
            let crypto_component = TempCryptoComponent::builder()
                .with_temp_dir_source(crypto_root)
                .with_registry(Arc::clone(&ret.registry) as Arc<_>)
                .with_node_id(node_id)
                .build();
            ret.crypto_components.insert(node_id, crypto_component);
        }

        ret.registry.update_to_latest_version();

        ret
    }

    /// Determines the config's node IDs that are not in the environment
    fn added_nodes(&self, ni_dkg_config: &NiDkgConfig) -> Vec<NodeId> {
        Self::dealers_and_receivers(ni_dkg_config)
            .into_iter()
            .filter(|node_id| !self.crypto_components.contains_key(node_id))
            .collect()
    }

    /// Adds a crypto component and a registry entry for a node
    fn add_crypto_component_and_registry_entry(
        &mut self,
        ni_dkg_config: &NiDkgConfig,
        node_id: NodeId,
    ) {
        // Insert TempCryptoComponent
        let temp_crypto = TempCryptoComponent::builder()
            .with_registry(Arc::clone(&self.registry) as Arc<_>)
            .with_node_id(node_id)
            .with_keys(NodeKeysToGenerate::only_dkg_dealing_encryption_key())
            .build();
        let dkg_dealing_encryption_pubkey = temp_crypto
            .current_node_public_keys()
            .expect("Failed to retrieve node public keys")
            .dkg_dealing_encryption_public_key
            .expect("missing dkg_dealing_encryption_pk");
        self.crypto_components.insert(node_id, temp_crypto);

        // Insert DKG dealing encryption public key into registry
        self.registry_data
            .add(
                &make_crypto_node_key(node_id, KeyPurpose::DkgDealingEncryption),
                ni_dkg_config.registry_version(),
                Some(dkg_dealing_encryption_pubkey),
            )
            .expect("failed to add DKG dealing encryption key to registry");
    }

    /// Cleans up nodes whose IDs are no longer in use
    fn cleanup_unused_nodes(&mut self, ni_dkg_config: &NiDkgConfig) {
        let dealers_and_receivers = Self::dealers_and_receivers(ni_dkg_config);
        let unused_node_ids: Vec<NodeId> = self
            .crypto_components
            .keys()
            .copied()
            .filter(|node_id| !dealers_and_receivers.contains(node_id))
            .collect();
        for node_id in unused_node_ids {
            self.crypto_components.remove(&node_id);
        }
    }

    fn dealers_and_receivers(config: &NiDkgConfig) -> Vec<NodeId> {
        let dealer_set: BTreeSet<_> = config.dealers().get().iter().copied().collect();
        let receiver_set: BTreeSet<_> = config.receivers().get().iter().copied().collect();
        dealer_set.union(&receiver_set).copied().collect()
    }
}

impl Default for NiDkgTestEnvironment {
    fn default() -> Self {
        NiDkgTestEnvironment::new()
    }
}

fn crypto_for<T>(node_id: NodeId, crypto_components: &BTreeMap<NodeId, T>) -> &T {
    crypto_components
        .get(&node_id)
        .unwrap_or_else(|| panic!("missing crypto component for {:?}", node_id))
}
