// Copyright (c) 2018-2020 MobileCoin Inc.

//! APIs for MobileCoin Consensus Node Enclaves

#![no_std]

extern crate alloc;

mod error;
mod messages;

pub use crate::{error::Error, messages::EnclaveCall};

use alloc::vec::Vec;
use attest::{IasNonce, Quote, QuoteNonce, Report, TargetInfo, VerificationReport};
use attest_enclave_api::{
    ClientAuthRequest, ClientAuthResponse, ClientSession, EnclaveMessage, PeerAuthRequest,
    PeerAuthResponse, PeerSession,
};
use common::ResponderId;
use core::{hash::Hash, result::Result as StdResult};
use keys::{Ed25519Public, X25519Public};
use serde::{Deserialize, Serialize};
use transaction::{
    ring_signature::KeyImage,
    tx::{Tx, TxHash, TxOutMembershipProof},
    Block, BlockSignature, RedactedTx,
};

/// A generic result type for enclave calls
pub type Result<T> = StdResult<T, Error>;

/// A `transaction::Tx` that has been encrypted for the local enclave, to be used during the
/// two-step is-wellformed check.
#[derive(Clone, Debug, Default, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub struct LocallyEncryptedTx(pub Vec<u8>);

/// A `WellformedTx` encrypted for the current enclave.
#[derive(Clone, Debug, Default, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub struct WellFormedEncryptedTx(pub Vec<u8>);

/// Tx data we wish to expose to untrusted from well-formed Txs
#[derive(Clone, Debug, Default, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub struct WellFormedTxContext {
    /// Tx hash.
    tx_hash: TxHash,

    /// Fee included in the tx.
    fee: u64,

    /// Tombstone block.
    tombstone_block: u64,

    /// Key images.
    key_images: Vec<KeyImage>,

    /// Highest membership proofs indices.
    highest_indices: Vec<u64>,
}

impl WellFormedTxContext {
    pub fn tx_hash(&self) -> &TxHash {
        &self.tx_hash
    }

    pub fn fee(&self) -> u64 {
        self.fee
    }

    pub fn tombstone_block(&self) -> u64 {
        self.tombstone_block
    }

    pub fn key_images(&self) -> &Vec<KeyImage> {
        &self.key_images
    }

    pub fn highest_indices(&self) -> &Vec<u64> {
        &self.highest_indices
    }
}

impl From<&Tx> for WellFormedTxContext {
    fn from(tx: &Tx) -> Self {
        Self {
            tx_hash: tx.tx_hash(),
            fee: tx.prefix.fee,
            tombstone_block: tx.tombstone_block,
            key_images: tx.key_images().clone(),
            highest_indices: tx.get_membership_proof_highest_indices(),
        }
    }
}

/// An intermediate struct for holding data required to perform the two-step is-well-formed test.
/// This is returned by `txs_propose` and allows untrusted to gather data required for the
/// in-enclave well-formedness test that takes place in `tx_is_well_formed`.
#[derive(Clone, Debug, Default, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub struct TxContext {
    pub locally_encrypted_tx: LocallyEncryptedTx,
    pub tx_hash: TxHash,
    pub highest_indices: Vec<u64>,
    pub key_images: Vec<KeyImage>,
}

pub type SealedBlockSigningKey = Vec<u8>;

/// The API for interacting with a consensus node's enclave.
pub trait ConsensusEnclave {
    // UTILITY METHODS

    /// Perform one-time initialization upon enclave startup.
    fn enclave_init(
        &self,
        self_peer_id: &ResponderId,
        self_client_id: &ResponderId,
        sealed_key: &Option<SealedBlockSigningKey>,
    ) -> Result<SealedBlockSigningKey>;

    /// Retrieve the public identity of the enclave.
    fn get_identity(&self) -> Result<X25519Public>;

    /// Retreive the block signing public key from the enclave.
    fn get_signer(&self) -> Result<Ed25519Public>;

    /// Retrieve a new report for this enclave, targetted for the given
    /// quoting enclave. Untrusted code should call this on startup as
    /// part of the initialization process.
    fn new_ereport(&self, qe_info: TargetInfo) -> Result<(Report, QuoteNonce)>;

    /// Checks the quote and it's generating enclave for validity.
    ///
    /// Untrusted code should create a quote using the output of
    /// `new_ereport()`, then pass the resulting quote here in order to
    /// sanity-check the quoting enclave, advance the cache state machine.
    ///
    /// The implementing enclave will verify the quoted report matches
    /// the one generated by the last call to `new_ereport()`, and cache
    /// the results, which will be used to verify
    fn verify_quote(&self, quote: Quote, qe_report: Report) -> Result<IasNonce>;

    /// Cache the verification report for this enclave.
    ///
    /// Untrusted code should transmit the quote previously checked by
    /// `check_quote()` to IAS, and construct the verification report structure
    /// from the results. That result should be given back to the enclave
    /// for future use.
    ///
    /// The enclave will verify the IAS report was signed by a trusted IAS
    /// certifcate, and the contents match the previously checked quote.
    /// After that check has been performed, the enclave will use the
    /// verification report for all requests until another verfication report
    /// has been successfully loaded in it's place.
    fn verify_ias_report(&self, ias_report: VerificationReport) -> Result<()>;

    /// Retrieve a copy of the cached verification report.
    fn get_ias_report(&self) -> Result<VerificationReport>;

    // CLIENT-FACING METHODS

    /// Accept an inbound authentication request
    fn client_accept(&self, req: ClientAuthRequest) -> Result<(ClientAuthResponse, ClientSession)>;

    /// Destroy a peer association
    fn client_close(&self, channel_id: ClientSession) -> Result<()>;

    /// Decrypts a message from a client and then immediately discard it. This is useful when we
    /// want to skip processing an incoming message, but still properly maintain our AKE state in
    /// sync with the client.
    fn client_discard_message(&self, msg: EnclaveMessage<ClientSession>) -> Result<()>;

    // NODE-FACING METHODS

    /// Start a new outbound connection.
    fn peer_init(&self, peer_id: &ResponderId) -> Result<PeerAuthRequest>;

    /// Accept an inbound authentication request
    fn peer_accept(&self, req: PeerAuthRequest) -> Result<(PeerAuthResponse, PeerSession)>;

    /// Complete the connection
    fn peer_connect(&self, peer_id: &ResponderId, res: PeerAuthResponse) -> Result<PeerSession>;

    /// Destroy a peer association
    fn peer_close(&self, channel_id: &PeerSession) -> Result<()>;

    // TRANSACTION-HANDLING API

    /// Performs the first steps in accepting transactions from a remote client:
    /// 1) Re-encrypt all txs for the local enclave
    /// 2) Extract context data to be handed back to untrusted so that it could collect the
    ///    information required by `tx_is_well_formed`.
    fn client_tx_propose(&self, msg: EnclaveMessage<ClientSession>) -> Result<TxContext>;

    /// Performs the first steps in accepting transactions from a remote peer:
    /// 1) Re-encrypt all txs for the local enclave
    /// 2) Extract context data to be handed back to untrusted so that it could collect the
    ///    information required by `tx_is_well_formed`.
    /// TODO: rename to txs_propose since this operates on multiple txs?
    fn peer_tx_propose(&self, msg: EnclaveMessage<PeerSession>) -> Result<Vec<TxContext>>;

    /// Checks a LocallyEncryptedTx for well-formedness using the given membership proofs and current block index.
    fn tx_is_well_formed(
        &self,
        locally_encrypted_tx: LocallyEncryptedTx,
        block_index: u64,
        proofs: Vec<TxOutMembershipProof>,
    ) -> Result<(WellFormedEncryptedTx, WellFormedTxContext)>;

    /// Re-encrypt sealed transactions for the given peer session, using the given authenticated
    /// data for the peer.
    fn txs_for_peer(
        &self,
        encrypted_txs: &[WellFormedEncryptedTx],
        aad: &[u8],
        peer: &PeerSession,
    ) -> Result<EnclaveMessage<PeerSession>>;

    /// Redact txs in order to form a new block.
    /// Returns a block, the set of redacted transactions included in it, and a signature over the
    /// block's digest.
    fn form_block(
        &self,
        parent_block: &Block,
        txs: &[(WellFormedEncryptedTx, Vec<TxOutMembershipProof>)],
    ) -> Result<(Block, Vec<RedactedTx>, BlockSignature)>;
}

/// Helper trait which reduces boiler-plate in untrusted side
/// The trusted object which implements consensus_enclave usually cannot implement
/// Clone, Send, Sync, etc., but the untrusted side can and usually having a "handle to an enclave"
/// is what is most useful for a webserver.
/// This marker trait can be implemented for the untrusted-side representation of the enclave.
pub trait ConsensusEnclaveProxy: ConsensusEnclave + Clone + Send + Sync + 'static {}