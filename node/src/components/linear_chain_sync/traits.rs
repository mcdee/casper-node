use crate::{
    effect::requests::{
        BlockExecutorRequest, BlockValidationRequest, FetcherRequest, StateStoreRequest,
        StorageRequest,
    },
    types::{Block, BlockByHeight, BlockHeader},
};
pub trait ReactorEventT<I>:
    From<StorageRequest>
    + From<FetcherRequest<I, Block>>
    + From<FetcherRequest<I, BlockByHeight>>
    + From<BlockValidationRequest<BlockHeader, I>>
    + From<BlockExecutorRequest>
    + From<StateStoreRequest>
    + Send
{
}

impl<I, REv> ReactorEventT<I> for REv where
    REv: From<StorageRequest>
        + From<FetcherRequest<I, Block>>
        + From<FetcherRequest<I, BlockByHeight>>
        + From<BlockValidationRequest<BlockHeader, I>>
        + From<BlockExecutorRequest>
        + From<StateStoreRequest>
        + Send
{
}
