use crate::{
    canister_http::CanisterHttpPayloadValidationError,
    ingress_manager::IngressPayloadValidationError, messaging::XNetPayloadValidationError,
    self_validating_payload::SelfValidatingPayloadValidationError, validation::ValidationResult,
};
use ic_base_types::NumBytes;
use ic_types::{
    batch::ValidationContext, consensus::BlockPayload, crypto::CryptoHashOf, Height, Time,
};

/// Collection of all possible validation errors that may occur during
/// validation of a batch payload.
#[derive(Debug)]
pub enum BatchPayloadValidationError {
    Ingress(IngressPayloadValidationError),
    XNet(XNetPayloadValidationError),
    Bitcoin(SelfValidatingPayloadValidationError),
    CanisterHttp(CanisterHttpPayloadValidationError),
}

impl BatchPayloadValidationError {
    pub fn is_transient(&self) -> bool {
        match self {
            Self::Ingress(e) => e.is_transient(),
            Self::XNet(e) => e.is_transient(),
            Self::Bitcoin(e) => e.is_transient(),
            Self::CanisterHttp(e) => e.is_transient(),
        }
    }
}

/// A list of [`PastPayload`] will be passed to invocation of
///  [`BatchPayloadBuilder::build_payload`].
///
/// The purpose is to allow the payload builders to deduplicate
/// messages that they have already included in prior.
pub struct PastPayload<'a> {
    /// Height of the payload
    pub height: Height,
    /// Timestamp of the past payload
    pub time: Time,
    /// The hash of the block, in which this payload is included.
    ///
    /// This can be used to differenciate between multiple blocks of the same
    /// height, e.g. when the payload builder wants to maintain an internal cache
    /// of past payloads.
    pub block_hash: CryptoHashOf<BlockPayload>,
    /// Payload bytes of the past payload
    ///
    /// Note that this is only the specific payload that
    /// belongs to the payload builder.
    pub payload: &'a [u8],
}

/// Indicates that this component can build batch payloads.
///
/// A batch payload has the following properties:
/// - Variable and possibly unbounded size
/// - Content of the payload is opaque to consensus and only relevant for upper layers
/// - Payload is not bound to a particular block height
/// - Internally composed of a number of similarly shaped messages
///
/// # Ordering
/// The `past_payloads` in [`BatchPayloadBuilder::build_payload`] and
/// [`BatchPayloadBuilder::validate_payload`] MUST be in descending `height` order.
pub trait BatchPayloadBuilder: Send {
    /// Builds a payload and returns it in serialized form.
    ///
    /// # Arguments
    /// - `max_size`: The maximum size the payload is supposed to have
    /// - `past_payloads`: A collection of past payloads. Allows the payload builder
    ///     to deduplicate messages.
    /// - `context`: [`ValidationContext`] under which the payload is supposed to be validated
    ///
    /// # Returns
    ///
    /// The payload in its serialized form
    fn build_payload(
        &self,
        height: Height,
        max_size: NumBytes,
        past_payloads: &[PastPayload],
        context: &ValidationContext,
    ) -> Vec<u8>;

    /// Checks whether a payload is valid.
    ///
    /// # Arguments
    /// - `payload`: The payload to validate
    /// - `past_payloads`: A collection of past payloads. Allows the payload builder
    ///     to deduplicate messages
    /// - `context`: [`ValidationContext`] under which to validate the payload
    ///
    /// # Returns
    ///
    /// - `Ok(())` on success
    /// - A [`BatchPayloadValidationError`] describing the problem otherwise
    fn validate_payload(
        &self,
        height: Height,
        payload: &[u8],
        past_payloads: &[PastPayload],
        context: &ValidationContext,
    ) -> ValidationResult<BatchPayloadValidationError>;
}

/// Indicates that a payload can be transformed into a set of messages, which
/// can be passed to message routing as part of the batch delivery.
pub trait IntoMessages<M> {
    /// Parse the payload into the message type to be included in the batch.
    ///
    /// # Guarantees
    ///
    /// This function must be infallible if the corresponding [`BatchPayloadBuilder::validate_payload`]
    /// returns `Ok(())` on the same payload.
    fn into_messages(payload: &[u8]) -> M;
}
