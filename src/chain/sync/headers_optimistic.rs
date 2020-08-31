//! Optimistic headers-only syncing.
//!
//! Optimistic syncing consists in assuming that all sources of blocks form the same chain. A
//! query for blocks is performed on a random source, and the response is verified. If it turns
//! out that the source doesn't belong to the same chain (or is malicious), a different source
//! is tried.
//!
//! While this syncing strategy is very simplistic, it is the most effective when the majority of
//! sources are well-behaved, which is normally the case.
//!
//! The [`OptimisticHeadersSync`] makes it possible to sync the finalized blocks of a chain, but
//! not the non-finalized blocks.

// TODO: document usage
// TODO: the quality of this module's code is sub-par compared to what we want

use super::super::blocks_tree;

use alloc::collections::VecDeque;
use core::{
    cmp,
    convert::TryFrom as _,
    fmt, iter,
    marker::PhantomData,
    mem,
    num::{NonZeroU32, NonZeroU64},
};

/// Configuration for the [`OptimisticHeadersSync`].
#[derive(Debug)]
pub struct Config {
    /// Configuration for the tree of blocks.
    pub chain_config: blocks_tree::Config,

    /// Pre-allocated capacity for the number of block sources.
    pub sources_capacity: usize,

    /// Maximum number of blocks returned by a response.
    ///
    /// > **Note**: If blocks are requested from the network, this should match the network
    /// >           protocol enforced limit.
    pub blocks_request_granularity: NonZeroU32,

    /// Number of blocks to download ahead of the best block.
    ///
    /// Whenever the latest best block is updated, the state machine will start block
    /// requests for the block `best_block_height + download_ahead_blocks` and all its
    /// ancestors. Considering that requesting blocks has some latency, downloading blocks ahead
    /// of time ensures that verification isn't blocked waiting for a request to be finished.
    ///
    /// The ideal value here depends on the speed of blocks verification speed and latency of
    /// block requests.
    pub download_ahead_blocks: u32,
}

/// Optimistic headers-only syncing.
pub struct OptimisticHeadersSync<TRq, TSrc> {
    // TODO: to reduce memory usage, keep the finalized block of this chain close to its best block, and maintain a `ChainInformation` in parallel of the actual finalized block
    chain: blocks_tree::NonFinalizedTree<()>,

    /// List of sources of blocks.
    sources: slab::Slab<Source<TSrc>>,

    cancelling_requests: bool,

    /// Queue of block requests, either to be started, in progress, or completed.
    verification_queue: VecDeque<VerificationQueueEntry<TRq>>,

    /// Value passed by [`Config::blocks_request_granularity`].
    blocks_request_granularity: NonZeroU32,

    /// Value passed by [`Config::download_ahead_blocks`].
    download_ahead_blocks: u32,

    /// Identifier to assign to the next request.
    next_request_id: RequestId,
}

struct VerificationQueueEntry<TRq> {
    block_height: NonZeroU64,
    ty: VerificationQueueEntryTy<TRq>,
}

struct Source<TSrc> {
    user_data: TSrc,
    banned: bool, // TODO: ban shouldn't be held forever
}

enum VerificationQueueEntryTy<TRq> {
    Missing,
    Requested {
        id: RequestId,
        /// User-chosen data for this request.
        user_data: TRq,
        // Index of this source within [`OptimisticHeadersSync::sources`].
        source: usize,
    },
    Queued(Vec<RequestSuccessBlock>),
}

impl<TRq, TSrc> OptimisticHeadersSync<TRq, TSrc> {
    /// Builds a new [`OptimisticHeadersSync`].
    pub fn new(config: Config) -> Self {
        OptimisticHeadersSync {
            chain: blocks_tree::NonFinalizedTree::new(config.chain_config),
            sources: slab::Slab::with_capacity(config.sources_capacity),
            cancelling_requests: false,
            verification_queue: VecDeque::with_capacity(
                usize::try_from(
                    config.download_ahead_blocks / config.blocks_request_granularity.get(),
                )
                .unwrap()
                .saturating_add(1),
            ),
            blocks_request_granularity: config.blocks_request_granularity,
            download_ahead_blocks: config.download_ahead_blocks,
            next_request_id: RequestId(0),
        }
    }

    /// Inform the [`OptimisticHeadersSync`] of a new potential source of blocks.
    pub fn add_source(&mut self, source: TSrc) -> SourceId {
        SourceId(self.sources.insert(Source {
            user_data: source,
            banned: false,
        }))
    }

    /// Inform the [`OptimisticHeadersSync`] that a source of blocks is no longer available.
    ///
    /// This automatically cancels all the requests that have been emitted for this source.
    /// This list of requests is returned as part of this function.
    ///
    /// # Panic
    ///
    /// Panics if the [`SourceId`] is invalid.
    ///
    pub fn remove_source(
        &mut self,
        source: SourceId,
    ) -> (TSrc, impl Iterator<Item = (RequestId, TRq)>) {
        let src_user_data = self.sources.remove(source.0).user_data;
        (src_user_data, iter::empty()) // TODO:
    }

    /// Returns an iterator that extracts all requests that need to be started and requests that
    /// need to be cancelled.
    pub fn next_request_action(&mut self) -> Option<RequestAction<TRq, TSrc>> {
        if self.cancelling_requests {
            while let Some(queue_elem) = self.verification_queue.pop_back() {
                match queue_elem.ty {
                    VerificationQueueEntryTy::Requested {
                        id,
                        source,
                        user_data,
                    } => {
                        return Some(RequestAction::Cancel {
                            request_id: id,
                            user_data,
                            source_id: SourceId(source),
                            source: &mut self.sources[source].user_data,
                        });
                    }
                    _ => {}
                }
            }

            self.cancelling_requests = false;
        }

        let best_block = self.chain.best_block_header().number;
        while self.verification_queue.back().map_or(true, |rq| {
            rq.block_height.get() + u64::from(self.blocks_request_granularity.get())
                < best_block + u64::from(self.download_ahead_blocks)
        }) {
            let block_height = self
                .verification_queue
                .back()
                .map(|rq| rq.block_height.get() + u64::from(self.blocks_request_granularity.get()))
                .unwrap_or(best_block + 1);
            self.verification_queue.push_back(VerificationQueueEntry {
                block_height: NonZeroU64::new(block_height).unwrap(),
                ty: VerificationQueueEntryTy::Missing,
            });
        }

        for missing_pos in self
            .verification_queue
            .iter()
            .enumerate()
            .filter(|(_, e)| matches!(e.ty, VerificationQueueEntryTy::Missing))
            .map(|(n, _)| n)
        {
            let source = self.sources.iter().filter(|(_, src)| !src.banned).next()?.0; // TODO: some sort of round-robin source selection

            let block_height = self.verification_queue[missing_pos].block_height;

            let num_blocks = if let Some(next) = self.verification_queue.get(missing_pos + 1) {
                NonZeroU32::new(
                    u32::try_from(cmp::min(
                        u64::from(self.blocks_request_granularity.get()),
                        next.block_height
                            .get()
                            .checked_sub(block_height.get())
                            .unwrap(),
                    ))
                    .unwrap(),
                )
                .unwrap()
            } else {
                self.blocks_request_granularity
            };

            return Some(RequestAction::Start {
                source_id: SourceId(source),
                source: &mut self.sources[source].user_data,
                block_height,
                num_blocks,
                start: Start {
                    verification_queue: &mut self.verification_queue,
                    missing_pos,
                    next_request_id: &mut self.next_request_id,
                    source,
                    marker: PhantomData,
                },
            });
        }

        None
    }

    /// Update the [`OptimisticHeadersSync`] with the outcome of a request.
    ///
    /// Returns the user data that was associated to that request.
    ///
    /// # Panic
    ///
    /// Panics if the [`RequestId`] is invalid.
    ///
    pub fn finish_request<'a>(
        &'a mut self,
        request_id: RequestId,
        outcome: Result<impl Iterator<Item = RequestSuccessBlock>, RequestFail>,
    ) -> (TRq, FinishRequestOutcome<'a, TSrc>) {
        let (verification_queue_entry, source_id) = self
            .verification_queue
            .iter()
            .enumerate()
            .filter_map(|(pos, entry)| match entry.ty {
                VerificationQueueEntryTy::Requested { id, source, .. } if id == request_id => {
                    Some((pos, source))
                }
                _ => None,
            })
            .next()
            .expect("invalid RequestId");

        let blocks = match outcome {
            Ok(blocks) => blocks.collect(),
            Err(_) => {
                let user_data = match mem::replace(
                    &mut self.verification_queue[verification_queue_entry].ty,
                    VerificationQueueEntryTy::Missing,
                ) {
                    VerificationQueueEntryTy::Requested { user_data, .. } => user_data,
                    _ => unreachable!(),
                };

                return (
                    user_data,
                    FinishRequestOutcome::SourcePunished(&mut self.sources[source_id].user_data),
                );
            }
        };

        // TODO: handle if blocks.len() < expected_number_of_blocks

        let user_data = match mem::replace(
            &mut self.verification_queue[verification_queue_entry].ty,
            VerificationQueueEntryTy::Queued(blocks),
        ) {
            VerificationQueueEntryTy::Requested { user_data, .. } => user_data,
            _ => unreachable!(),
        };

        (user_data, FinishRequestOutcome::Queued)
    }

    /// Process a single block in the queue of verification.
    // TODO: return value
    pub fn process_one(&mut self) -> Option<ChainStateUpdate> {
        if self.cancelling_requests {
            return None;
        }

        let blocks = match &mut self.verification_queue.get_mut(0)?.ty {
            VerificationQueueEntryTy::Queued(blocks) => mem::replace(blocks, Default::default()),
            _ => return None,
        };

        let mut expected_block_height = self.verification_queue[0].block_height.get();

        self.verification_queue.pop_front();

        self.chain.reserve(blocks.len());

        for block in blocks {
            match self.chain.verify_header(block.scale_encoded_header.into()) {
                Ok(blocks_tree::HeaderVerifySuccess::Insert {
                    block_height,
                    is_new_best,
                    insert,
                }) => {
                    if !is_new_best || block_height != expected_block_height {
                        panic!(
                            "is new best = {:?} block height: {:?} expected = {:?}",
                            is_new_best, block_height, expected_block_height
                        );
                        self.cancelling_requests = true;
                        self.chain.clear();
                        break;
                    }

                    insert.insert(())
                } // TODO:
                Ok(blocks_tree::HeaderVerifySuccess::Duplicate) => {
                    // TODO: don't really know what this implies and seems to happen in practice; investigate
                }
                Err(err) => {
                    // TODO: remove panic
                    panic!("verify error: {:?}", err);
                    self.cancelling_requests = true;
                    self.chain.clear();
                    break;
                }
            }

            if let Some(justification) = block.scale_encoded_justification {
                match self.chain.verify_justification(justification.as_ref()) {
                    Ok(apply) => apply.apply(),
                    Err(err) => {
                        // TODO: remove panic
                        panic!("verify error: {:?}", err);
                        self.cancelling_requests = true;
                        self.chain.clear();
                        break;
                    }
                }
            }

            expected_block_height += 1;
        }

        // TODO: consider finer granularity in report
        Some(ChainStateUpdate {
            finalized_block_hash: self.chain.finalized_block_hash(),
            finalized_block_number: self.chain.finalized_block_header().number,
            best_block_hash: self.chain.best_block_hash(),
            best_block_number: self.chain.best_block_header().number,
        })
    }
}

/// Identifier for an ongoing request in the [`OptimisticHeadersSync`].
#[derive(Debug, Copy, Clone, Ord, PartialOrd, Eq, PartialEq, Hash)]
pub struct RequestId(u64);

/// Identifier for a source in the [`OptimisticHeadersSync`].
#[derive(Debug, Copy, Clone, Ord, PartialOrd, Eq, PartialEq, Hash)]
pub struct SourceId(usize);

/// Request that should be emitted towards a certain source.
#[derive(Debug)]
pub enum RequestAction<'a, TRq, TSrc> {
    /// A request must be emitted for the given source.
    ///
    /// The request has **not** been acknowledged when this event is emitted. You **must** call
    /// [`Start::start`] to notify the [`OptimisticHeadersSync`] that the request has been sent
    /// out.
    Start {
        /// Source where to request blocks from.
        source_id: SourceId,
        /// User data of source where to request blocks from.
        source: &'a mut TSrc,
        /// Must be used to accept the request.
        start: Start<'a, TRq, TSrc>,
        /// Height of the block to request.
        block_height: NonZeroU64,
        /// Number of blocks to request. Always smaller than the value passed through
        /// [`Config::blocks_request_granularity`].
        num_blocks: NonZeroU32,
    },

    /// The given [`RequestId`] is no longer valid.
    ///
    /// > **Note**: The request can either be cancelled, or the request can be let through but
    /// >           marked in a way that [`OptimisticHeadersSync::finish_request`] isn't called.
    Cancel {
        /// Identifier for the request. No longer valid.
        request_id: RequestId,
        /// User data associated with the request.
        user_data: TRq,
        /// Source where to request blocks from.
        source_id: SourceId,
        /// User data of source where to request blocks from.
        source: &'a mut TSrc,
    },
}

/// Must be used to accept the request.
#[must_use]
pub struct Start<'a, TRq, TSrc> {
    verification_queue: &'a mut VecDeque<VerificationQueueEntry<TRq>>,
    source: usize,
    missing_pos: usize,
    next_request_id: &'a mut RequestId,
    marker: PhantomData<&'a TSrc>,
}

impl<'a, TRq, TSrc> Start<'a, TRq, TSrc> {
    /// Updates the [`OptimisticHeadersSync`] with the fact that the request has actually been
    /// started. Returns the identifier for the request that must later be passed back to
    /// [`OptimisticHeadersSync::finish_request`].
    pub fn start(self, user_data: TRq) -> RequestId {
        let request_id = *self.next_request_id;
        self.next_request_id.0 += 1;

        self.verification_queue[self.missing_pos].ty = VerificationQueueEntryTy::Requested {
            id: request_id,
            source: self.source,
            user_data,
        };

        request_id
    }
}

impl<'a, TRq, TSrc> fmt::Debug for Start<'a, TRq, TSrc> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_tuple("Start").finish()
    }
}

pub enum FinishRequestOutcome<'a, TSrc> {
    Queued,
    SourcePunished(&'a mut TSrc),
}

pub struct RequestSuccessBlock {
    pub scale_encoded_header: Vec<u8>,
    pub scale_encoded_justification: Option<Vec<u8>>,
}

/// Reason why a request has failed.
pub enum RequestFail {
    /// Requested blocks aren't available from this source.
    BlocksUnavailable,
}

#[derive(Debug)]
pub struct ChainStateUpdate {
    pub best_block_hash: [u8; 32],
    pub best_block_number: u64,
    pub finalized_block_hash: [u8; 32],
    pub finalized_block_number: u64,
}