// Copyright 2020 Parity Technologies (UK) Ltd.
// This file is part of Polkadot.

// Polkadot is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Polkadot is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Polkadot.  If not, see <http://www.gnu.org/licenses/>.

//! TODO
#![warn(missing_docs)]

use codec::{Decode, Encode};
use futures::{channel::mpsc, select, stream::FusedStream, SinkExt, StreamExt};
use sc_client_api::{BlockBackend, BlockImportNotification};
use sp_api::{ApiError, BlockT, ProvideRuntimeApi};
use sp_blockchain::HeaderBackend;
use sp_consensus_slots::Slot;
use sp_executor::{ExecutorApi, OpaqueBundle, SignedExecutionReceipt, SignedOpaqueBundle};
use sp_runtime::{
	generic::{BlockId, DigestItem},
	traits::{Header as HeaderT, NumberFor, One, Saturating},
	OpaqueExtrinsic,
};
use std::{
	borrow::Cow,
	collections::{hash_map::Entry, HashMap},
	fmt::Debug,
	future::Future,
	pin::Pin,
	sync::Arc,
};
use subspace_core_primitives::{Randomness, Tag};
use subspace_runtime_primitives::Hash as PHash;

/// Data required to produce bundles on executor node.
#[derive(PartialEq, Clone, Debug)]
pub struct ExecutorSlotInfo {
	/// Slot
	pub slot: Slot,
	/// Global slot challenge
	pub global_challenge: Tag,
}

/// Process function.
///
/// Will be called with the hash of the primary chain block.
///
/// Returns an optional [`OpaqueExecutionReceipt`].
pub type ProcessorFn<PHash, Number, Hash> = Box<
	dyn Fn(
			(PHash, Number),
			Vec<OpaqueBundle>,
			Randomness,
			Option<Cow<'static, [u8]>>,
		) -> Pin<Box<dyn Future<Output = Option<SignedExecutionReceipt<Hash>>> + Send>>
		+ Send
		+ Sync,
>;

/// Configuration for the collation generator
pub struct CollationGenerationConfig<PHash, Number, Hash> {
	/// State processor function. See [`ProcessorFn`] for more details.
	pub processor: ProcessorFn<PHash, Number, Hash>,
}

impl<PHash, Number, Hash> std::fmt::Debug for CollationGenerationConfig<PHash, Number, Hash> {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(f, "CollationGenerationConfig {{ ... }}")
	}
}

const LOG_TARGET: &str = "overseer";

/// Apply the transaction bundles for given primary block as follows:
///
/// 1. Extract the transaction bundles from the block.
/// 2. Pass the bundles to secondary node and do the computation there.
async fn process_primary_block<PBlock, PClient, SecondaryHash>(
	primary_chain_client: &PClient,
	processor: &ProcessorFn<PBlock::Hash, NumberFor<PBlock>, SecondaryHash>,
	(block_hash, block_number): (PBlock::Hash, NumberFor<PBlock>),
) -> Result<(), ApiError>
where
	PBlock: BlockT,
	PClient: HeaderBackend<PBlock>
		+ BlockBackend<PBlock>
		+ ProvideRuntimeApi<PBlock>
		+ Send
		+ Sync
		+ 'static,
	PClient::Api: ExecutorApi<PBlock, SecondaryHash>,
	SecondaryHash: Encode + Decode,
{
	let block_id = BlockId::Hash(block_hash);
	let extrinsics = match primary_chain_client.block_body(&block_id) {
		Err(err) => {
			tracing::error!(
				target: LOG_TARGET,
				?err,
				"Failed to get block body from primary chain"
			);
			return Ok(())
		},
		Ok(None) => {
			tracing::error!(target: LOG_TARGET, ?block_hash, "BlockBody unavailable");
			return Ok(())
		},
		Ok(Some(body)) => body,
	};

	let bundles = primary_chain_client.runtime_api().extract_bundles(
		&block_id,
		extrinsics
			.into_iter()
			.map(|xt| {
				OpaqueExtrinsic::from_bytes(&xt.encode()).expect("Certainly a correct extrinsic")
			})
			.collect(),
	)?;

	let header = match primary_chain_client.header(block_id) {
		Err(err) => {
			tracing::error!(target: LOG_TARGET, ?err, "Failed to get block from primary chain");
			return Ok(())
		},
		Ok(None) => {
			tracing::error!(target: LOG_TARGET, ?block_hash, "BlockHeader unavailable");
			return Ok(())
		},
		Ok(Some(header)) => header,
	};

	let maybe_new_runtime = if header
		.digest()
		.logs
		.iter()
		.any(|item| *item == DigestItem::RuntimeEnvironmentUpdated)
	{
		Some(primary_chain_client.runtime_api().execution_wasm_bundle(&block_id)?)
	} else {
		None
	};

	let shuffling_seed = primary_chain_client
		.runtime_api()
		.extrinsics_shuffling_seed(&block_id, header)?;

	let execution_receipt =
		match processor((block_hash, block_number), bundles, shuffling_seed, maybe_new_runtime)
			.await
		{
			Some(execution_receipt) => execution_receipt,
			None => {
				tracing::debug!(
					target: LOG_TARGET,
					"Skip sending the execution receipt because executor is not elected",
				);
				return Ok(())
			},
		};

	let best_hash = primary_chain_client.info().best_hash;

	let () = primary_chain_client
		.runtime_api()
		.submit_execution_receipt_unsigned(&BlockId::Hash(best_hash), execution_receipt)?;

	Ok(())
}

/// A handle used to communicate with the [`Overseer`].
///
/// [`Overseer`]: struct.Overseer.html
#[derive(Clone)]
pub struct OverseerHandle<PBlock: BlockT>(mpsc::Sender<Event<PBlock>>);

impl<PBlock> OverseerHandle<PBlock>
where
	PBlock: BlockT,
{
	/// Create a new [`Handle`].
	fn new(raw: mpsc::Sender<Event<PBlock>>) -> Self {
		Self(raw)
	}

	/// Inform the `Overseer` that that some block was imported.
	async fn block_imported(&mut self, block: BlockInfo<PBlock>) {
		self.send_and_log_error(Event::BlockImported(block)).await
	}

	/// Most basic operation, to stop a server.
	async fn send_and_log_error(&mut self, event: Event<PBlock>) {
		if self.0.send(event).await.is_err() {
			tracing::info!(target: LOG_TARGET, "Failed to send an event to Overseer");
		}
	}
}

/// An event telling the `Overseer` on the particular block
/// that has been imported or finalized.
///
/// This structure exists solely for the purposes of decoupling
/// `Overseer` code from the client code and the necessity to call
/// `HeaderBackend::block_number_from_id()`.
#[derive(Debug, Clone)]
pub struct BlockInfo<Block>
where
	Block: BlockT,
{
	/// hash of the block.
	pub hash: Block::Hash,
	/// hash of the parent block.
	pub parent_hash: Block::Hash,
	/// block's number.
	pub number: NumberFor<Block>,
}

impl<Block> From<BlockImportNotification<Block>> for BlockInfo<Block>
where
	Block: BlockT,
{
	fn from(n: BlockImportNotification<Block>) -> Self {
		BlockInfo { hash: n.hash, parent_hash: *n.header.parent_hash(), number: *n.header.number() }
	}
}

/// An event from outside the overseer scope, such
/// as the substrate framework or user interaction.
enum Event<PBlock>
where
	PBlock: BlockT,
{
	/// A new block was imported.
	BlockImported(BlockInfo<PBlock>),
}

/// Glues together the [`Overseer`] and `BlockchainEvents` by forwarding
/// import and finality notifications to it.
pub async fn forward_events<PBlock, PClient, BundlerFn, SecondaryHash>(
	primary_chain_client: &PClient,
	bundler: BundlerFn,
	mut imports: impl FusedStream<Item = NumberFor<PBlock>> + Unpin,
	mut slots: impl FusedStream<Item = ExecutorSlotInfo> + Unpin,
	mut handle: OverseerHandle<PBlock>,
) where
	PBlock: BlockT,
	PClient: HeaderBackend<PBlock> + ProvideRuntimeApi<PBlock>,
	PClient::Api: ExecutorApi<PBlock, SecondaryHash>,
	BundlerFn: Fn(
			PHash,
			ExecutorSlotInfo,
		) -> Pin<Box<dyn Future<Output = Option<SignedOpaqueBundle>> + Send>>
		+ Send
		+ Sync,
	SecondaryHash: Encode + Decode,
{
	loop {
		select! {
			i = imports.next() => {
				match i {
					Some(block_number) => {
						let header = primary_chain_client
							.header(BlockId::Number(block_number))
							.expect("Header of imported block must exist; qed")
							.expect("Header of imported block must exist; qed");
						let block = BlockInfo {
							hash: header.hash(),
							parent_hash: *header.parent_hash(),
							number: *header.number(),
						};
						handle.block_imported(block).await;
					}
					None => break,
				}
			},
			s = slots.next() => {
				match s {
					Some(executor_slot_info) => {
						if let Err(error) = on_new_slot(primary_chain_client, &bundler, executor_slot_info).await {
							tracing::error!(
								target: LOG_TARGET,
								error = ?error,
								"Failed to submit transaction bundle"
							);
							break;
						}
					}
					None => break,
				}
			}
			complete => break,
		}
	}
}

async fn on_new_slot<PBlock, PClient, BundlerFn, SecondaryHash>(
	primary_chain_client: &PClient,
	bundler: &BundlerFn,
	executor_slot_info: ExecutorSlotInfo,
) -> Result<(), ApiError>
where
	PBlock: BlockT,
	PClient: HeaderBackend<PBlock> + ProvideRuntimeApi<PBlock>,
	PClient::Api: ExecutorApi<PBlock, SecondaryHash>,
	BundlerFn: Fn(
			PHash,
			ExecutorSlotInfo,
		) -> Pin<Box<dyn Future<Output = Option<SignedOpaqueBundle>> + Send>>
		+ Send
		+ Sync,
	SecondaryHash: Encode + Decode,
{
	let best_hash = primary_chain_client.info().best_hash;

	let non_generic_best_hash =
		PHash::decode(&mut best_hash.encode().as_slice()).expect("Hash type must be correct");

	let opaque_bundle = match bundler(non_generic_best_hash, executor_slot_info).await {
		Some(opaque_bundle) => opaque_bundle,
		None => {
			tracing::debug!(target: LOG_TARGET, "executor returned no bundle on bundling",);
			return Ok(())
		},
	};

	let () = primary_chain_client
		.runtime_api()
		.submit_transaction_bundle_unsigned(&BlockId::Hash(best_hash), opaque_bundle)?;

	Ok(())
}

/// Capacity of a signal channel between a subsystem and the overseer.
const SIGNAL_CHANNEL_CAPACITY: usize = 64usize;
/// The overseer.
// TODO: temporarily suppress clippy and will be removed in the refactoring https://github.com/subspace/subspace/pull/429
#[allow(clippy::type_complexity)]
pub struct Overseer<PBlock, PClient, Hash>
where
	PBlock: BlockT,
{
	primary_chain_client: Arc<PClient>,
	overseer_config: Arc<CollationGenerationConfig<PBlock::Hash, NumberFor<PBlock>, Hash>>,
	/// A user specified addendum field.
	leaves: Vec<(PBlock::Hash, NumberFor<PBlock>)>,
	/// A user specified addendum field.
	active_leaves: HashMap<PBlock::Hash, NumberFor<PBlock>>,
	/// Events that are sent to the overseer from the outside world.
	events_rx: mpsc::Receiver<Event<PBlock>>,
}

impl<PBlock, PClient, Hash> Overseer<PBlock, PClient, Hash>
where
	PBlock: BlockT,
	PClient: HeaderBackend<PBlock>
		+ BlockBackend<PBlock>
		+ ProvideRuntimeApi<PBlock>
		+ Send
		+ 'static
		+ Sync,
	PClient::Api: ExecutorApi<PBlock, Hash>,
	Hash: Encode + Decode,
{
	/// Create a new overseer.
	pub fn new(
		primary_chain_client: Arc<PClient>,
		leaves: Vec<(PBlock::Hash, NumberFor<PBlock>)>,
		active_leaves: HashMap<PBlock::Hash, NumberFor<PBlock>>,
		overseer_config: CollationGenerationConfig<PBlock::Hash, NumberFor<PBlock>, Hash>,
	) -> (Self, OverseerHandle<PBlock>) {
		let (handle, events_rx) = mpsc::channel(SIGNAL_CHANNEL_CAPACITY);
		let overseer = Overseer {
			primary_chain_client,
			overseer_config: Arc::new(overseer_config),
			leaves,
			active_leaves,
			events_rx,
		};
		(overseer, OverseerHandle::new(handle))
	}

	/// Run the `Overseer`.
	pub async fn run(mut self) -> Result<(), ApiError> {
		// Notify about active leaves on startup before starting the loop
		for (hash, number) in std::mem::take(&mut self.leaves) {
			let _ = self.active_leaves.insert(hash, number);
			if let Err(error) = process_primary_block(
				self.primary_chain_client.as_ref(),
				&self.overseer_config.processor,
				(hash, number),
			)
			.await
			{
				tracing::error!(
					target: LOG_TARGET,
					"Collation generation processing error: {error}"
				);
			}
		}

		while let Some(msg) = self.events_rx.next().await {
			match msg {
				// TODO: we still need the context of block, e.g., executor gossips no message
				// to the primary node during the major sync.
				Event::BlockImported(block) => {
					self.block_imported(block).await?;
				},
			}
		}

		Ok(())
	}

	async fn block_imported(&mut self, block: BlockInfo<PBlock>) -> Result<(), ApiError> {
		match self.active_leaves.entry(block.hash) {
			Entry::Vacant(entry) => entry.insert(block.number),
			Entry::Occupied(entry) => {
				debug_assert_eq!(*entry.get(), block.number);
				return Ok(())
			},
		};

		if let Some(number) = self.active_leaves.remove(&block.parent_hash) {
			debug_assert_eq!(block.number.saturating_sub(One::one()), number);
		}

		if let Err(error) = process_primary_block(
			self.primary_chain_client.as_ref(),
			&self.overseer_config.processor,
			(block.hash, block.number),
		)
		.await
		{
			tracing::error!(target: LOG_TARGET, "Collation generation processing error: {error}");
		}

		Ok(())
	}
}
