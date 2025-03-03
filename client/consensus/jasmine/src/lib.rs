// This file is part of Substrate.

// Copyright (C) 2018-2022 Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: GPL-3.0-or-later WITH Classpath-exception-2.0

// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with this program. If not, see <https://www.gnu.org/licenses/>.

//! Jasmine (Authority-round) consensus in substrate.
//!
//! Jasmine works by having a list of authorities A who are expected to roughly
//! agree on the current time. Time is divided up into discrete slots of t
//! seconds each. For each slot s, the author of that slot is A[s % |A|].
//!
//! The author is allowed to issue one block but not more during that slot,
//! and it will be built upon the longest valid chain that has been seen.
//!
//! Blocks from future steps will be either deferred or rejected depending on how
//! far in the future they are.
//!
//! NOTE: Jasmine itself is designed to be generic over the crypto used.
#![forbid(missing_docs, unsafe_code)]
use std::{fmt::Debug, hash::Hash, marker::PhantomData, pin::Pin, sync::Arc};

use futures::prelude::*;
use log::{debug, info, trace};

use codec::{Codec, Decode, Encode};

use sc_client_api::{backend::AuxStore, BlockOf, UsageProvider};
use sc_consensus::{BlockImport, BlockImportParams, ForkChoiceStrategy, StateAction};
use sc_consensus_events::{
	BackoffAuthoringBlocksStrategy, InherentDataProviderExt, SimpleSlotWorkerToSlotWorker,
	SlotInfo, StorageChanges,
};
use sc_telemetry::TelemetryHandle;
use sp_api::ProvideRuntimeApi;
use sp_application_crypto::{AppKey, AppPublic};
use sp_blockchain::{HeaderBackend, Result as CResult};
use sp_consensus::{
	BlockOrigin, CanAuthorWith, Environment, Error as ConsensusError, Proposer, SelectChain,
};
use sp_consensus_slots::Slot;
use sp_core::crypto::{ByteArray, Pair, Public};
use sp_inherents::CreateInherentDataProviders;
use sp_keystore::{SyncCryptoStore, SyncCryptoStorePtr};
use sp_runtime::{
	generic::BlockId,
	traits::{Block as BlockT, Header, Member, NumberFor, Zero},
	DigestItem,
};

mod import_queue;

pub use import_queue::{
	build_verifier, import_queue, BuildVerifierParams, CheckForEquivocation, ImportQueueParams,
	JasmineVerifier,
};
pub use sc_consensus_events::SlotProportion;
pub use sp_consensus::SyncOracle;
pub use sp_consensus_jasmine::{
	digests::CompatibleDigestItem,
	inherents::{InherentDataProvider, InherentType as JasmineInherent, INHERENT_IDENTIFIER},
	AuraApi, ConsensusLog, SlotDuration, JASMINE_ENGINE_ID,
};
use sp_finality_jasmine::{LeaderInfo, SharedLeaderInfo};

type AuthorityId<P> = <P as Pair>::Public;

/// Get the slot duration for Jasmine.
pub fn slot_duration<A, B, C>(client: &C) -> CResult<SlotDuration>
where
	A: Codec,
	B: BlockT,
	C: AuxStore + ProvideRuntimeApi<B> + UsageProvider<B>,
	C::Api: AuraApi<B, A>,
{
	let best_block_id = BlockId::Hash(client.usage_info().chain.best_hash);
	client.runtime_api().slot_duration(&best_block_id).map_err(|err| err.into())
}

/// Get slot author for given block along with authorities.
fn slot_author<P: Pair>(round: u64, authorities: &[AuthorityId<P>]) -> Option<&AuthorityId<P>>
where
	<P as sp_application_crypto::Pair>::Public: std::fmt::Debug,
{
	if authorities.is_empty() {
		return None
	}

	info!("authorities: {:?}", authorities);

	let idx = round % (authorities.len() as u64);
	assert!(
		idx <= usize::MAX as u64,
		"It is impossible to have a vector with length beyond the address space; qed",
	);

	let current_author = authorities.get(idx as usize).expect(
		"authorities not empty; index constrained to list length;this is a valid index; qed",
	);

	Some(current_author)
}

/// Parameters of [`start_jasmine`].
pub struct StartJasmineParams<C, SC, I, PF, SO, L, CIDP, BS, CAW, B: BlockT> {
	/// The duration of a slot.
	pub slot_duration: SlotDuration,
	/// The client to interact with the chain.
	pub client: Arc<C>,
	/// A select chain implementation to select the best block.
	pub select_chain: SC,
	/// The block import.
	pub block_import: I,
	/// The proposer factory to build proposer instances.
	pub proposer_factory: PF,
	/// The sync oracle that can give us the current sync status.
	pub sync_oracle: SO,
	/// Hook into the sync module to control the justification sync process.
	pub justification_sync_link: L,
	/// Something that can create the inherent data providers.
	pub create_inherent_data_providers: CIDP,
	/// Should we force the authoring of blocks?
	pub force_authoring: bool,
	/// The backoff strategy when we miss slots.
	pub backoff_authoring_blocks: Option<BS>,
	/// The keystore used by the node.
	pub keystore: SyncCryptoStorePtr,
	/// Can we author a block with this node?
	pub can_author_with: CAW,
	/// The proportion of the slot dedicated to proposing.
	///
	/// The block proposing will be limited to this proportion of the slot from the starting of the
	/// slot. However, the proposing can still take longer when there is some lenience factor
	/// applied, because there were no blocks produced for some slots.
	pub block_proposal_slot_portion: SlotProportion,
	/// The maximum proportion of the slot dedicated to proposing with any lenience factor applied
	/// due to no blocks being produced.
	pub max_block_proposal_slot_portion: Option<SlotProportion>,
	/// Telemetry instance used to report telemetry metrics.
	pub telemetry: Option<TelemetryHandle>,
	/// Leader info
	pub leader_info: SharedLeaderInfo<B>,
}

/// Start the jasmine worker. The returned future should be run in a futures executor.
pub fn start_jasmine<P, B: BlockT, C, SC, I, PF, SO, L, CIDP, BS, CAW, Error>(
	StartJasmineParams {
		slot_duration,
		client,
		select_chain,
		block_import,
		proposer_factory,
		sync_oracle,
		justification_sync_link,
		create_inherent_data_providers,
		force_authoring,
		backoff_authoring_blocks,
		keystore,
		can_author_with,
		block_proposal_slot_portion,
		max_block_proposal_slot_portion,
		telemetry,
		leader_info,
	}: StartJasmineParams<C, SC, I, PF, SO, L, CIDP, BS, CAW, B>,
) -> Result<impl Future<Output = ()>, sp_consensus::Error>
where
	P: Pair + Send + Sync,
	P::Public: AppPublic + Hash + Member + Encode + Decode,
	P::Signature: TryFrom<Vec<u8>> + Hash + Member + Encode + Decode,
	B: BlockT,
	C: ProvideRuntimeApi<B> + BlockOf + AuxStore + HeaderBackend<B> + Send + Sync,
	C::Api: AuraApi<B, AuthorityId<P>>,
	SC: SelectChain<B>,
	I: BlockImport<B, Transaction = sp_api::TransactionFor<C, B>> + Send + Sync + 'static,
	PF: Environment<B, Error = Error> + Send + Sync + 'static,
	PF::Proposer: Proposer<B, Error = Error, Transaction = sp_api::TransactionFor<C, B>>,
	SO: SyncOracle + Send + Sync + Clone,
	L: sc_consensus::JustificationSyncLink<B>,
	CIDP: CreateInherentDataProviders<B, ()> + Send,
	CIDP::InherentDataProviders: InherentDataProviderExt + Send,
	BS: BackoffAuthoringBlocksStrategy<NumberFor<B>> + Send + Sync + 'static,
	CAW: CanAuthorWith<B> + Send,
	Error: std::error::Error + Send + From<sp_consensus::Error> + 'static,
{
	let worker = build_jasmine_worker::<P, _, _, _, _, _, _, _, _>(BuildJasmineWorkerParams {
		client,
		block_import,
		proposer_factory,
		keystore,
		sync_oracle: sync_oracle.clone(),
		justification_sync_link,
		force_authoring,
		backoff_authoring_blocks,
		telemetry,
		block_proposal_slot_portion,
		max_block_proposal_slot_portion,
		leader_info,
	});

	Ok(sc_consensus_events::start_slot_worker(
		slot_duration,
		select_chain,
		SimpleSlotWorkerToSlotWorker(worker),
		sync_oracle,
		create_inherent_data_providers,
		can_author_with,
	))
}

/// Parameters of [`build_jasmine_worker`].
pub struct BuildJasmineWorkerParams<C, I, PF, SO, L, BS, B: BlockT> {
	/// The client to interact with the chain.
	pub client: Arc<C>,
	/// The block import.
	pub block_import: I,
	/// The proposer factory to build proposer instances.
	pub proposer_factory: PF,
	/// The sync oracle that can give us the current sync status.
	pub sync_oracle: SO,
	/// Hook into the sync module to control the justification sync process.
	pub justification_sync_link: L,
	/// Should we force the authoring of blocks?
	pub force_authoring: bool,
	/// The backoff strategy when we miss slots.
	pub backoff_authoring_blocks: Option<BS>,
	/// The keystore used by the node.
	pub keystore: SyncCryptoStorePtr,
	/// The proportion of the slot dedicated to proposing.
	///
	/// The block proposing will be limited to this proportion of the slot from the starting of the
	/// slot. However, the proposing can still take longer when there is some lenience factor
	/// applied, because there were no blocks produced for some slots.
	pub block_proposal_slot_portion: SlotProportion,
	/// The maximum proportion of the slot dedicated to proposing with any lenience factor applied
	/// due to no blocks being produced.
	pub max_block_proposal_slot_portion: Option<SlotProportion>,
	/// Telemetry instance used to report telemetry metrics.
	pub telemetry: Option<TelemetryHandle>,
	/// Leader info
	pub leader_info: SharedLeaderInfo<B>,
}

/// Build the jasmine worker.
///
/// The caller is responsible for running this worker, otherwise it will do nothing.
pub fn build_jasmine_worker<P, B, C, PF, I, SO, L, BS, Error>(
	BuildJasmineWorkerParams {
		client,
		block_import,
		proposer_factory,
		sync_oracle,
		justification_sync_link,
		backoff_authoring_blocks,
		keystore,
		block_proposal_slot_portion,
		max_block_proposal_slot_portion,
		telemetry,
		force_authoring,
		leader_info,
	}: BuildJasmineWorkerParams<C, I, PF, SO, L, BS, B>,
) -> impl sc_consensus_events::SimpleSlotWorker<
	B,
	Proposer = PF::Proposer,
	BlockImport = I,
	SyncOracle = SO,
	JustificationSyncLink = L,
	Claim = P::Public,
	EpochData = Vec<AuthorityId<P>>,
>
where
	B: BlockT,
	C: ProvideRuntimeApi<B> + BlockOf + AuxStore + HeaderBackend<B> + Send + Sync,
	C::Api: AuraApi<B, AuthorityId<P>>,
	PF: Environment<B, Error = Error> + Send + Sync + 'static,
	PF::Proposer: Proposer<B, Error = Error, Transaction = sp_api::TransactionFor<C, B>>,
	P: Pair + Send + Sync,
	P::Public: AppPublic + Hash + Member + Encode + Decode,
	P::Signature: TryFrom<Vec<u8>> + Hash + Member + Encode + Decode,
	I: BlockImport<B, Transaction = sp_api::TransactionFor<C, B>> + Send + Sync + 'static,
	Error: std::error::Error + Send + From<sp_consensus::Error> + 'static,
	SO: SyncOracle + Send + Sync + Clone,
	L: sc_consensus::JustificationSyncLink<B>,
	BS: BackoffAuthoringBlocksStrategy<NumberFor<B>> + Send + Sync + 'static,
{
	JasmineWorker {
		client,
		block_import,
		env: proposer_factory,
		keystore,
		sync_oracle,
		justification_sync_link,
		force_authoring,
		backoff_authoring_blocks,
		telemetry,
		block_proposal_slot_portion,
		max_block_proposal_slot_portion,
		leader_info,
		_key_type: PhantomData::<P>,
	}
}

struct JasmineWorker<C, E, I, P, SO, L, BS, B: BlockT> {
	client: Arc<C>,
	block_import: I,
	env: E,
	keystore: SyncCryptoStorePtr,
	sync_oracle: SO,
	justification_sync_link: L,
	force_authoring: bool,
	backoff_authoring_blocks: Option<BS>,
	block_proposal_slot_portion: SlotProportion,
	max_block_proposal_slot_portion: Option<SlotProportion>,
	telemetry: Option<TelemetryHandle>,
	leader_info: SharedLeaderInfo<B>,
	_key_type: PhantomData<P>,
}

#[async_trait::async_trait]
impl<B, C, E, I, P, Error, SO, L, BS> sc_consensus_events::SimpleSlotWorker<B>
	for JasmineWorker<C, E, I, P, SO, L, BS, B>
where
	B: BlockT,
	C: ProvideRuntimeApi<B> + BlockOf + HeaderBackend<B> + Sync,
	C::Api: AuraApi<B, AuthorityId<P>>,
	E: Environment<B, Error = Error> + Send + Sync,
	E::Proposer: Proposer<B, Error = Error, Transaction = sp_api::TransactionFor<C, B>>,
	I: BlockImport<B, Transaction = sp_api::TransactionFor<C, B>> + Send + Sync + 'static,
	P: Pair + Send + Sync,
	P::Public: AppPublic + Public + Member + Encode + Decode + Hash,
	P::Signature: TryFrom<Vec<u8>> + Member + Encode + Decode + Hash + Debug,
	SO: SyncOracle + Send + Clone + Sync,
	L: sc_consensus::JustificationSyncLink<B>,
	BS: BackoffAuthoringBlocksStrategy<NumberFor<B>> + Send + Sync + 'static,
	Error: std::error::Error + Send + From<sp_consensus::Error> + 'static,
{
	type BlockImport = I;
	type SyncOracle = SO;
	type JustificationSyncLink = L;
	type CreateProposer =
		Pin<Box<dyn Future<Output = Result<E::Proposer, sp_consensus::Error>> + Send + 'static>>;
	type Proposer = E::Proposer;
	type Claim = P::Public;
	type EpochData = Vec<AuthorityId<P>>;

	fn logging_target(&self) -> &'static str {
		"jasmine"
	}

	fn block_import(&mut self) -> &mut Self::BlockImport {
		&mut self.block_import
	}

	fn epoch_data(
		&self,
		header: &B::Header,
		_slot: Slot,
	) -> Result<Self::EpochData, sp_consensus::Error> {
		authorities(self.client.as_ref(), &BlockId::Hash(header.hash()))
	}

	fn authorities_len(&self, epoch_data: &Self::EpochData) -> Option<usize> {
		Some(epoch_data.len())
	}

	async fn claim_slot(
		&self,
		_header: &B::Header,
		slot: Slot,
		round: u64,
		epoch_data: &Self::EpochData,
	) -> Option<Self::Claim> {
		let expected_author = slot_author::<P>(round, epoch_data);
		info!(target: "afj", "expected_author: {:?}, round: {:?}", expected_author, round);
		expected_author.and_then(|p| {
			if SyncCryptoStore::has_keys(
				&*self.keystore,
				&[(p.to_raw_vec(), sp_application_crypto::key_types::JASMINE)],
			) {
				Some(p.clone())
			} else {
				None
			}
		})
	}

	fn pre_digest_data(&self, slot: Slot, _claim: &Self::Claim) -> Vec<sp_runtime::DigestItem> {
		vec![<DigestItem as CompatibleDigestItem<P::Signature>>::aura_pre_digest(slot)]
	}

	async fn block_import_params(
		&self,
		header: B::Header,
		header_hash: &B::Hash,
		body: Vec<B::Extrinsic>,
		storage_changes: StorageChanges<<Self::BlockImport as BlockImport<B>>::Transaction, B>,
		public: Self::Claim,
		_epoch: Self::EpochData,
	) -> Result<
		sc_consensus::BlockImportParams<B, <Self::BlockImport as BlockImport<B>>::Transaction>,
		sp_consensus::Error,
	> {
		// sign the pre-sealed hash of the block and then
		// add it to a digest item.
		let public_type_pair = public.to_public_crypto_pair();
		let public = public.to_raw_vec();
		let signature = SyncCryptoStore::sign_with(
			&*self.keystore,
			<AuthorityId<P> as AppKey>::ID,
			&public_type_pair,
			header_hash.as_ref(),
		)
		.map_err(|e| sp_consensus::Error::CannotSign(public.clone(), e.to_string()))?
		.ok_or_else(|| {
			sp_consensus::Error::CannotSign(
				public.clone(),
				"Could not find key in keystore.".into(),
			)
		})?;
		let signature = signature
			.clone()
			.try_into()
			.map_err(|_| sp_consensus::Error::InvalidSignature(signature, public))?;

		let signature_digest_item =
			<DigestItem as CompatibleDigestItem<P::Signature>>::aura_seal(signature);

		let mut import_block = BlockImportParams::new(BlockOrigin::Own, header);
		import_block.post_digests.push(signature_digest_item);
		import_block.body = Some(body);
		import_block.state_action =
			StateAction::ApplyChanges(sc_consensus::StorageChanges::Changes(storage_changes));
		import_block.fork_choice = Some(ForkChoiceStrategy::LongestChain);

		Ok(import_block)
	}

	fn force_authoring(&self) -> bool {
		self.force_authoring
	}

	fn should_backoff(&self, slot: Slot, chain_head: &B::Header) -> bool {
		if let Some(ref strategy) = self.backoff_authoring_blocks {
			if let Ok(chain_head_slot) = find_pre_digest::<B, P::Signature>(chain_head) {
				return strategy.should_backoff(
					*chain_head.number(),
					chain_head_slot,
					self.client.info().finalized_number,
					slot,
					self.logging_target(),
				)
			}
		}
		false
	}

	fn sync_oracle(&mut self) -> &mut Self::SyncOracle {
		&mut self.sync_oracle
	}

	fn justification_sync_link(&mut self) -> &mut Self::JustificationSyncLink {
		&mut self.justification_sync_link
	}

	fn proposer(&mut self, block: &B::Header) -> Self::CreateProposer {
		self.env
			.init(block)
			.map_err(|e| sp_consensus::Error::ClientImport(format!("{:?}", e)))
			.boxed()
	}

	fn telemetry(&self) -> Option<TelemetryHandle> {
		self.telemetry.clone()
	}

	fn proposing_remaining_duration(&self, slot_info: &SlotInfo<B>) -> std::time::Duration {
		let parent_slot = find_pre_digest::<B, P::Signature>(&slot_info.chain_head).ok();

		sc_consensus_events::proposing_remaining_duration(
			parent_slot,
			slot_info,
			&self.block_proposal_slot_portion,
			self.max_block_proposal_slot_portion.as_ref(),
			sc_consensus_events::SlotLenienceType::Exponential,
			self.logging_target(),
		)
	}

	fn leader_info(&self) -> LeaderInfo<NumberFor<B>, B::Hash> {
		self.leader_info.leader_info()
	}
}

fn jasmine_err<B: BlockT>(error: Error<B>) -> Error<B> {
	debug!(target: "jasmine", "{}", error);
	error
}

/// Jasmine Errors
#[derive(Debug, thiserror::Error)]
pub enum Error<B: BlockT> {
	/// Multiple Jasmine pre-runtime headers
	#[error("Multiple Jasmine pre-runtime headers")]
	MultipleHeaders,
	/// No Jasmine pre-runtime digest found
	#[error("No Jasmine pre-runtime digest found")]
	NoDigestFound,
	/// Header is unsealed
	#[error("Header {0:?} is unsealed")]
	HeaderUnsealed(B::Hash),
	/// Header has a bad seal
	#[error("Header {0:?} has a bad seal")]
	HeaderBadSeal(B::Hash),
	/// Slot Author not found
	#[error("Slot Author not found")]
	SlotAuthorNotFound,
	/// Bad signature
	#[error("Bad signature on {0:?}")]
	BadSignature(B::Hash),
	/// Client Error
	#[error(transparent)]
	Client(sp_blockchain::Error),
	/// Unknown inherent error for identifier
	#[error("Unknown inherent error for identifier: {}", String::from_utf8_lossy(.0))]
	UnknownInherentError(sp_inherents::InherentIdentifier),
	/// Inherents Error
	#[error("Inherent error: {0}")]
	Inherent(sp_inherents::Error),
}

impl<B: BlockT> From<Error<B>> for String {
	fn from(error: Error<B>) -> String {
		error.to_string()
	}
}

/// Get pre-digests from the header
pub fn find_pre_digest<B: BlockT, Signature: Codec>(header: &B::Header) -> Result<Slot, Error<B>> {
	if header.number().is_zero() {
		return Ok(0.into())
	}

	let mut pre_digest: Option<Slot> = None;
	for log in header.digest().logs() {
		trace!(target: "jasmine", "Checking log {:?}", log);
		match (CompatibleDigestItem::<Signature>::as_aura_pre_digest(log), pre_digest.is_some()) {
			(Some(_), true) => return Err(jasmine_err(Error::MultipleHeaders)),
			(None, _) => trace!(target: "jasmine", "Ignoring digest not meant for us"),
			(s, false) => pre_digest = s,
		}
	}
	pre_digest.ok_or_else(|| jasmine_err(Error::NoDigestFound))
}

fn authorities<A, B, C>(client: &C, at: &BlockId<B>) -> Result<Vec<A>, ConsensusError>
where
	A: Codec + Debug,
	B: BlockT,
	C: ProvideRuntimeApi<B> + BlockOf,
	C::Api: AuraApi<B, A>,
{
	client
		.runtime_api()
		.authorities(at)
		.ok()
		.ok_or(sp_consensus::Error::InvalidAuthoritiesSet)
}

#[cfg(test)]
mod tests {
	// use super::*;
	// use futures::executor;
	// use parking_lot::Mutex;
	// use sc_block_builder::BlockBuilderProvider;
	// use sc_client_api::BlockchainEvents;
	// use sc_consensus::BoxJustificationImport;
	// use sc_consensus_events::{BackoffAuthoringOnFinalizedHeadLagging, SimpleSlotWorker};
	// use sc_keystore::LocalKeystore;
	// use sc_network::config::ProtocolConfig;
	// use sc_network_test::{Block as TestBlock, *};
	// use sp_application_crypto::key_types::JASMINE;
	// use sp_consensus::{
	// 	AlwaysCanAuthor, DisableProofRecording, NoNetwork as DummyOracle, Proposal,
	// };
	// use sp_consensus_jasmine::sr25519::AuthorityPair;
	// use sp_inherents::InherentData;
	// use sp_keyring::sr25519::Keyring;
	// use sp_runtime::{
	// 	traits::{Block as BlockT, Header as _},
	// 	Digest,
	// };
	// use sp_timestamp::InherentDataProvider as TimestampInherentDataProvider;
	// use std::{
	// 	task::Poll,
	// 	time::{Duration, Instant},
	// };
	// use substrate_test_runtime_client::{
	// 	runtime::{Header, H256},
	// 	TestClient,
	// };
	//
	// type Error = sp_blockchain::Error;
	//
	// struct DummyFactory(Arc<TestClient>);
	// struct DummyProposer(u64, Arc<TestClient>);
	//
	// impl Environment<TestBlock> for DummyFactory {
	// 	type Proposer = DummyProposer;
	// 	type CreateProposer = futures::future::Ready<Result<DummyProposer, Error>>;
	// 	type Error = Error;
	//
	// 	fn init(&mut self, parent_header: &<TestBlock as BlockT>::Header) -> Self::CreateProposer {
	// 		futures::future::ready(Ok(DummyProposer(parent_header.number + 1, self.0.clone())))
	// 	}
	// }
	//
	// impl Proposer<TestBlock> for DummyProposer {
	// 	type Error = Error;
	// 	type Transaction =
	// 		sc_client_api::TransactionFor<substrate_test_runtime_client::Backend, TestBlock>;
	// 	type Proposal = future::Ready<Result<Proposal<TestBlock, Self::Transaction, ()>, Error>>;
	// 	type ProofRecording = DisableProofRecording;
	// 	type Proof = ();
	//
	// 	fn propose(
	// 		self,
	// 		_: InherentData,
	// 		digests: Digest,
	// 		_: Duration,
	// 		_: Option<usize>,
	// 	) -> Self::Proposal {
	// 		let r = self.1.new_block(digests).unwrap().build().map_err(|e| e.into());
	//
	// 		future::ready(r.map(|b| Proposal {
	// 			block: b.block,
	// 			proof: (),
	// 			storage_changes: b.storage_changes,
	// 		}))
	// 	}
	// }
	//
	// const SLOT_DURATION: u64 = 1000;
	//
	// type JasmineVerifier = import_queue::JasmineVerifier<
	// 	PeersFullClient,
	// 	AuthorityPair,
	// 	AlwaysCanAuthor,
	// 	Box<
	// 		dyn CreateInherentDataProviders<
	// 			TestBlock,
	// 			(),
	// 			InherentDataProviders = (TimestampInherentDataProvider, InherentDataProvider),
	// 		>,
	// 	>,
	// >;
	// type JasminePeer = Peer<(), PeersClient>;
	//
	// pub struct JasmineTestNet {
	// 	peers: Vec<JasminePeer>,
	// }
	//
	// impl TestNetFactory for JasmineTestNet {
	// 	type Verifier = JasmineVerifier;
	// 	type PeerData = ();
	// 	type BlockImport = PeersClient;
	//
	// 	/// Create new test network with peers and given config.
	// 	fn from_config(_config: &ProtocolConfig) -> Self {
	// 		JasmineTestNet { peers: Vec::new() }
	// 	}
	//
	// 	fn make_verifier(
	// 		&self,
	// 		client: PeersClient,
	// 		_cfg: &ProtocolConfig,
	// 		_peer_data: &(),
	// 	) -> Self::Verifier {
	// 		let client = client.as_client();
	// 		let slot_duration = slot_duration(&*client).expect("slot duration available");
	//
	// 		assert_eq!(slot_duration.as_millis() as u64, SLOT_DURATION);
	// 		import_queue::JasmineVerifier::new(
	// 			client,
	// 			Box::new(|_, _| async {
	// 				let timestamp = TimestampInherentDataProvider::from_system_time();
	// 				let slot = InherentDataProvider::from_timestamp_and_slot_duration(
	// 					*timestamp,
	// 					SlotDuration::from_millis(6000),
	// 				);
	//
	// 				Ok((timestamp, slot))
	// 			}),
	// 			AlwaysCanAuthor,
	// 			CheckForEquivocation::Yes,
	// 			None,
	// 		)
	// 	}
	//
	// 	fn make_block_import(
	// 		&self,
	// 		client: PeersClient,
	// 	) -> (
	// 		BlockImportAdapter<Self::BlockImport>,
	// 		Option<BoxJustificationImport<Block>>,
	// 		Self::PeerData,
	// 	) {
	// 		(client.as_block_import(), None, ())
	// 	}
	//
	// 	fn peer(&mut self, i: usize) -> &mut JasminePeer {
	// 		&mut self.peers[i]
	// 	}
	//
	// 	fn peers(&self) -> &Vec<JasminePeer> {
	// 		&self.peers
	// 	}
	// 	fn mut_peers<F: FnOnce(&mut Vec<JasminePeer>)>(&mut self, closure: F) {
	// 		closure(&mut self.peers);
	// 	}
	// }
	//
	// #[test]
	// fn authoring_blocks() {
	// 	sp_tracing::try_init_simple();
	// 	let net = JasmineTestNet::new(3);
	//
	// 	let peers = &[(0, Keyring::Alice), (1, Keyring::Bob), (2, Keyring::Charlie)];
	//
	// 	let net = Arc::new(Mutex::new(net));
	// 	let mut import_notifications = Vec::new();
	// 	let mut jasmine_futures = Vec::new();
	//
	// 	let mut keystore_paths = Vec::new();
	// 	for (peer_id, key) in peers {
	// 		let mut net = net.lock();
	// 		let peer = net.peer(*peer_id);
	// 		let client = peer.client().as_client();
	// 		let select_chain = peer.select_chain().expect("full client has a select chain");
	// 		let keystore_path = tempfile::tempdir().expect("Creates keystore path");
	// 		let keystore = Arc::new(
	// 			LocalKeystore::open(keystore_path.path(), None).expect("Creates keystore."),
	// 		);
	//
	// 		SyncCryptoStore::sr25519_generate_new(&*keystore, JASMINE, Some(&key.to_seed()))
	// 			.expect("Creates authority key");
	// 		keystore_paths.push(keystore_path);
	//
	// 		let environ = DummyFactory(client.clone());
	// 		import_notifications.push(
	// 			client
	// 				.import_notification_stream()
	// 				.take_while(|n| {
	// 					future::ready(!(n.origin != BlockOrigin::Own && n.header.number() < &5))
	// 				})
	// 				.for_each(move |_| future::ready(())),
	// 		);
	//
	// 		let slot_duration = slot_duration(&*client).expect("slot duration available");
	//
	// 		jasmine_futures.push(
	// 			start_jasmine::<AuthorityPair, _, _, _, _, _, _, _, _, _, _, _>(
	// 				StartJasmineParams {
	// 					slot_duration,
	// 					block_import: client.clone(),
	// 					select_chain,
	// 					client,
	// 					proposer_factory: environ,
	// 					sync_oracle: DummyOracle,
	// 					justification_sync_link: (),
	// 					create_inherent_data_providers: |_, _| async {
	// 						let timestamp = TimestampInherentDataProvider::from_system_time();
	// 						let slot = InherentDataProvider::from_timestamp_and_slot_duration(
	// 							*timestamp,
	// 							SlotDuration::from_millis(6000),
	// 						);
	//
	// 						Ok((timestamp, slot))
	// 					},
	// 					force_authoring: false,
	// 					backoff_authoring_blocks: Some(
	// 						BackoffAuthoringOnFinalizedHeadLagging::default(),
	// 					),
	// 					keystore,
	// 					can_author_with: sp_consensus::AlwaysCanAuthor,
	// 					block_proposal_slot_portion: SlotProportion::new(0.5),
	// 					max_block_proposal_slot_portion: None,
	// 					telemetry: None,
	// 				},
	// 			)
	// 			.expect("Starts jasmine"),
	// 		);
	// 	}
	//
	// 	executor::block_on(future::select(
	// 		future::poll_fn(move |cx| {
	// 			net.lock().poll(cx);
	// 			Poll::<()>::Pending
	// 		}),
	// 		future::select(
	// 			future::join_all(jasmine_futures),
	// 			future::join_all(import_notifications),
	// 		),
	// 	));
	// }
	//
	// #[test]
	// fn authorities_call_works() {
	// 	let client = substrate_test_runtime_client::new();
	//
	// 	assert_eq!(client.chain_info().best_number, 0);
	// 	assert_eq!(
	// 		authorities(&client, &BlockId::Number(0)).unwrap(),
	// 		vec![
	// 			Keyring::Alice.public().into(),
	// 			Keyring::Bob.public().into(),
	// 			Keyring::Charlie.public().into()
	// 		]
	// 	);
	// }
	//
	// #[test]
	// fn current_node_authority_should_claim_slot() {
	// 	let net = JasmineTestNet::new(4);
	//
	// 	let mut authorities = vec![
	// 		Keyring::Alice.public().into(),
	// 		Keyring::Bob.public().into(),
	// 		Keyring::Charlie.public().into(),
	// 	];
	//
	// 	let keystore_path = tempfile::tempdir().expect("Creates keystore path");
	// 	let keystore = LocalKeystore::open(keystore_path.path(), None).expect("Creates keystore.");
	// 	let public = SyncCryptoStore::sr25519_generate_new(&keystore, AuthorityPair::ID, None)
	// 		.expect("Key should be created");
	// 	authorities.push(public.into());
	//
	// 	let net = Arc::new(Mutex::new(net));
	//
	// 	let mut net = net.lock();
	// 	let peer = net.peer(3);
	// 	let client = peer.client().as_client();
	// 	let environ = DummyFactory(client.clone());
	//
	// 	let worker = JasmineWorker {
	// 		client: client.clone(),
	// 		block_import: client,
	// 		env: environ,
	// 		keystore: keystore.into(),
	// 		sync_oracle: DummyOracle.clone(),
	// 		justification_sync_link: (),
	// 		force_authoring: false,
	// 		backoff_authoring_blocks: Some(BackoffAuthoringOnFinalizedHeadLagging::default()),
	// 		telemetry: None,
	// 		_key_type: PhantomData::<AuthorityPair>,
	// 		block_proposal_slot_portion: SlotProportion::new(0.5),
	// 		max_block_proposal_slot_portion: None,
	// 	};
	//
	// 	let head = Header::new(
	// 		1,
	// 		H256::from_low_u64_be(0),
	// 		H256::from_low_u64_be(0),
	// 		Default::default(),
	// 		Default::default(),
	// 	);
	// 	assert!(executor::block_on(worker.claim_slot(&head, 0.into(), &authorities)).is_none());
	// 	assert!(executor::block_on(worker.claim_slot(&head, 1.into(), &authorities)).is_none());
	// 	assert!(executor::block_on(worker.claim_slot(&head, 2.into(), &authorities)).is_none());
	// 	assert!(executor::block_on(worker.claim_slot(&head, 3.into(), &authorities)).is_some());
	// 	assert!(executor::block_on(worker.claim_slot(&head, 4.into(), &authorities)).is_none());
	// 	assert!(executor::block_on(worker.claim_slot(&head, 5.into(), &authorities)).is_none());
	// 	assert!(executor::block_on(worker.claim_slot(&head, 6.into(), &authorities)).is_none());
	// 	assert!(executor::block_on(worker.claim_slot(&head, 7.into(), &authorities)).is_some());
	// }
	//
	// #[test]
	// fn on_slot_returns_correct_block() {
	// 	let net = JasmineTestNet::new(4);
	//
	// 	let keystore_path = tempfile::tempdir().expect("Creates keystore path");
	// 	let keystore = LocalKeystore::open(keystore_path.path(), None).expect("Creates keystore.");
	// 	SyncCryptoStore::sr25519_generate_new(
	// 		&keystore,
	// 		AuthorityPair::ID,
	// 		Some(&Keyring::Alice.to_seed()),
	// 	)
	// 	.expect("Key should be created");
	//
	// 	let net = Arc::new(Mutex::new(net));
	//
	// 	let mut net = net.lock();
	// 	let peer = net.peer(3);
	// 	let client = peer.client().as_client();
	// 	let environ = DummyFactory(client.clone());
	//
	// 	let mut worker = JasmineWorker {
	// 		client: client.clone(),
	// 		block_import: client.clone(),
	// 		env: environ,
	// 		keystore: keystore.into(),
	// 		sync_oracle: DummyOracle.clone(),
	// 		justification_sync_link: (),
	// 		force_authoring: false,
	// 		backoff_authoring_blocks: Option::<()>::None,
	// 		telemetry: None,
	// 		_key_type: PhantomData::<AuthorityPair>,
	// 		block_proposal_slot_portion: SlotProportion::new(0.5),
	// 		max_block_proposal_slot_portion: None,
	// 	};
	//
	// 	let head = client.header(&BlockId::Number(0)).unwrap().unwrap();
	//
	// 	let res = executor::block_on(worker.on_slot(SlotInfo {
	// 		slot: 0.into(),
	// 		timestamp: 0.into(),
	// 		ends_at: Instant::now() + Duration::from_secs(100),
	// 		inherent_data: InherentData::new(),
	// 		duration: Duration::from_millis(1000),
	// 		chain_head: head,
	// 		block_size_limit: None,
	// 	}))
	// 	.unwrap();
	//
	// 	// The returned block should be imported and we should be able to get its header by now.
	// 	assert!(client.header(&BlockId::Hash(res.block.hash())).unwrap().is_some());
	// }
}
