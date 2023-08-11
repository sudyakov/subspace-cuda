// Copyright (C) 2023 Subspace Labs, Inc.
// SPDX-License-Identifier: GPL-3.0-or-later

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

mod piece_validator;
mod segment_header_downloader;

use crate::dsn::import_blocks::piece_validator::SegmentCommitmentPieceValidator;
use crate::dsn::import_blocks::segment_header_downloader::SegmentHeaderDownloader;
use futures::stream::FuturesUnordered;
use futures::StreamExt;
use parity_scale_codec::Encode;
use sc_client_api::{AuxStore, BlockBackend, HeaderBackend};
use sc_consensus::import_queue::ImportQueueService;
use sc_consensus::IncomingBlock;
use sc_consensus_subspace::SegmentHeadersStore;
use sc_tracing::tracing::{debug, trace};
use sp_consensus::BlockOrigin;
use sp_runtime::traits::{Block as BlockT, Header, NumberFor, One};
use std::time::Duration;
use subspace_archiving::reconstructor::Reconstructor;
use subspace_core_primitives::crypto::kzg::{embedded_kzg_settings, Kzg};
use subspace_core_primitives::{
    ArchivedHistorySegment, BlockNumber, Piece, RecordedHistorySegment, SegmentIndex,
};
use subspace_networking::utils::piece_provider::{PieceProvider, RetryPolicy};
use subspace_networking::Node;
use tokio::sync::Semaphore;
use tracing::warn;

/// How many blocks to queue before pausing and waiting for blocks to be imported
const QUEUED_BLOCKS_LIMIT: BlockNumber = 2048;
/// Time to wait for blocks to import if import is too slow
const WAIT_FOR_BLOCKS_TO_IMPORT: Duration = Duration::from_secs(1);

// TODO: Only download segment headers starting with the first segment that node doesn't have rather
//  than from genesis
/// Starts the process of importing blocks.
///
/// Returns number of downloaded blocks.
pub async fn import_blocks_from_dsn<Block, AS, IQS, Client>(
    segment_headers_store: &SegmentHeadersStore<AS>,
    node: &Node,
    client: &Client,
    import_queue_service: &mut IQS,
    force: bool,
) -> Result<u64, sc_service::Error>
where
    Block: BlockT,
    AS: AuxStore + Send + Sync + 'static,
    Client: HeaderBackend<Block> + BlockBackend<Block> + Send + Sync + 'static,
    IQS: ImportQueueService<Block> + ?Sized,
{
    let segment_headers = SegmentHeaderDownloader::new(node.clone())
        .get_segment_headers()
        .await
        .map_err(|error| error.to_string())?;

    debug!("Found {} segment headers", segment_headers.len());

    if segment_headers.is_empty() {
        return Ok(0);
    }

    segment_headers_store.add_segment_headers(&segment_headers)?;

    let segments_found = segment_headers.len();
    let piece_provider = &PieceProvider::<SegmentCommitmentPieceValidator<AS>>::new(
        node.clone(),
        Some(SegmentCommitmentPieceValidator::new(
            node.clone(),
            Kzg::new(embedded_kzg_settings()),
            segment_headers_store,
        )),
    );

    let mut downloaded_blocks = 0;
    let mut reconstructor = Reconstructor::new().map_err(|error| error.to_string())?;
    let mut segment_indices_iter = (SegmentIndex::ZERO..)
        .take(segments_found)
        .skip(1)
        .peekable();

    // Skip the first segment, everyone has it locally
    while let Some(segment_index) = segment_indices_iter.next() {
        debug!(%segment_index, "Processing segment");

        if let Some(segment_header) = segment_headers.get(u64::from(segment_index) as usize) {
            trace!(
                %segment_index,
                last_archived_block_number = %segment_header.last_archived_block().number,
                last_archived_block_progress = ?segment_header.last_archived_block().archived_progress,
                "Checking segment header"
            );

            let last_archived_block =
                NumberFor::<Block>::from(segment_header.last_archived_block().number);
            let last_archived_block_partial = segment_header
                .last_archived_block()
                .archived_progress
                .partial()
                .is_some();
            // We already have this block imported or we have only a part of the very next block and
            // this was the last segment available, so nothing to import
            if last_archived_block <= client.info().best_number
                || (last_archived_block == client.info().best_number + One::one()
                    && last_archived_block_partial
                    && segment_indices_iter.peek().is_none())
            {
                // Reset reconstructor instance
                reconstructor = Reconstructor::new().map_err(|error| error.to_string())?;
                continue;
            }
        }

        debug!(%segment_index, "Retrieving pieces of the segment");

        let semaphore = &Semaphore::new(RecordedHistorySegment::NUM_RAW_RECORDS);

        let mut received_segment_pieces = segment_index
            .segment_piece_indexes_source_first()
            .into_iter()
            .map(|piece_index| {
                // Source pieces will acquire permit here right away
                let maybe_permit = semaphore.try_acquire().ok();

                async move {
                    let permit = match maybe_permit {
                        Some(permit) => permit,
                        None => {
                            // Other pieces will acquire permit here instead
                            match semaphore.acquire().await {
                                Ok(permit) => permit,
                                Err(error) => {
                                    warn!(
                                        %piece_index,
                                        %error,
                                        "Semaphore was closed, interrupting piece retrieval"
                                    );
                                    return None;
                                }
                            }
                        }
                    };
                    let maybe_piece = match piece_provider
                        .get_piece(piece_index, RetryPolicy::Limited(0))
                        .await
                    {
                        Ok(maybe_piece) => maybe_piece,
                        Err(error) => {
                            trace!(
                                %error,
                                ?piece_index,
                                "Piece request failed",
                            );
                            return None;
                        }
                    };

                    trace!(
                        ?piece_index,
                        piece_found = maybe_piece.is_some(),
                        "Piece request succeeded",
                    );

                    maybe_piece.map(|received_piece| {
                        // Piece was received successfully, "remove" this slot from semaphore
                        permit.forget();
                        (piece_index, received_piece)
                    })
                }
            })
            .collect::<FuturesUnordered<_>>();

        let mut segment_pieces = vec![None::<Piece>; ArchivedHistorySegment::NUM_PIECES];
        let mut pieces_received = 0;

        while let Some(maybe_result) = received_segment_pieces.next().await {
            let Some((piece_index, piece)) = maybe_result else {
                continue;
            };

            segment_pieces
                .get_mut(piece_index.position() as usize)
                .expect("Piece position is by definition within segment; qed")
                .replace(piece);

            pieces_received += 1;

            if pieces_received >= RecordedHistorySegment::NUM_RAW_RECORDS {
                trace!(%segment_index, "Received half of the segment.");
                break;
            }
        }

        let reconstructed_contents = reconstructor
            .add_segment(segment_pieces.as_ref())
            .map_err(|error| error.to_string())?;
        drop(segment_pieces);

        trace!(%segment_index, "Segment reconstructed successfully");

        let mut blocks_to_import = Vec::with_capacity(QUEUED_BLOCKS_LIMIT as usize);

        let mut best_block_number = client.info().best_number;
        for (block_number, block_bytes) in reconstructed_contents.blocks {
            {
                let block_number = block_number.into();
                if block_number <= best_block_number {
                    if block_number == 0u32.into() {
                        let block = client
                            .block(client.hash(block_number)?.expect(
                                "Block before best block number must always be found; qed",
                            ))?
                            .expect("Block before best block number must always be found; qed");

                        if block.encode() != block_bytes {
                            return Err(sc_service::Error::Other(
                                "Wrong genesis block, block import failed".to_string(),
                            ));
                        }
                    }

                    continue;
                }

                // Limit number of queued blocks for import
                while block_number - best_block_number >= QUEUED_BLOCKS_LIMIT.into() {
                    if !blocks_to_import.is_empty() {
                        // Import queue handles verification and importing it into the client
                        import_queue_service.import_blocks(
                            BlockOrigin::NetworkInitialSync,
                            blocks_to_import.clone(),
                        );
                        blocks_to_import.clear();
                    }
                    trace!(
                        %block_number,
                        %best_block_number,
                        "Number of importing blocks reached queue limit, waiting before retrying"
                    );
                    tokio::time::sleep(WAIT_FOR_BLOCKS_TO_IMPORT).await;
                    best_block_number = client.info().best_number;
                }
            }

            let block =
                Block::decode(&mut block_bytes.as_slice()).map_err(|error| error.to_string())?;

            let (header, extrinsics) = block.deconstruct();
            let hash = header.hash();

            blocks_to_import.push(IncomingBlock {
                hash,
                header: Some(header),
                body: Some(extrinsics),
                indexed_body: None,
                justifications: None,
                origin: None,
                allow_missing_state: false,
                import_existing: force,
                state: None,
                skip_execution: false,
            });

            downloaded_blocks += 1;

            if downloaded_blocks % 1000 == 0 {
                debug!("Adding block {} from DSN to the import queue", block_number);
            }
        }

        if blocks_to_import.is_empty() {
            break;
        }

        // Import queue handles verification and importing it into the client
        let last_segment = segment_indices_iter.peek().is_none();
        if last_segment {
            let last_block = blocks_to_import
                .pop()
                .expect("Not empty, checked above; qed");
            import_queue_service.import_blocks(BlockOrigin::NetworkInitialSync, blocks_to_import);
            // This will notify Substrate's sync mechanism and allow regular Substrate sync to continue gracefully
            import_queue_service.import_blocks(BlockOrigin::NetworkBroadcast, vec![last_block]);
        } else {
            import_queue_service.import_blocks(BlockOrigin::NetworkInitialSync, blocks_to_import);
        }
    }

    Ok(downloaded_blocks)
}
