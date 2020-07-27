use serde::{Deserialize, Serialize};

use super::{state::State, validators::ValidatorIndex};
use crate::{
    components::consensus::{highway_core::vertex::SignedWireVote, traits::Context},
    types::Timestamp,
};

/// The observed behavior of a validator at some point in time.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(bound(
    serialize = "C::Hash: Serialize",
    deserialize = "C::Hash: Deserialize<'de>",
))]
pub(crate) enum Observation<C: Context> {
    /// No vote by that validator was observed yet.
    None,
    /// The validator's latest vote.
    Correct(C::Hash),
    /// The validator has been seen
    Faulty,
}

impl<C: Context> Observation<C> {
    /// Returns the vote hash, if this is a correct observation.
    pub(crate) fn correct(&self) -> Option<&C::Hash> {
        match self {
            Self::None | Self::Faulty => None,
            Self::Correct(hash) => Some(hash),
        }
    }

    fn is_correct(&self) -> bool {
        match self {
            Self::None | Self::Faulty => false,
            Self::Correct(_) => true,
        }
    }

    pub(crate) fn is_faulty(&self) -> bool {
        match self {
            Self::Faulty => true,
            Self::None | Self::Correct(_) => false,
        }
    }
}

/// The observed behavior of all validators at some point in time.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(bound(
    serialize = "C::Hash: Serialize",
    deserialize = "C::Hash: Deserialize<'de>",
))]
pub(crate) struct Panorama<C: Context>(pub(crate) Vec<Observation<C>>);

impl<C: Context> Panorama<C> {
    /// Creates a new, empty panorama.
    pub(crate) fn new(num_validators: usize) -> Panorama<C> {
        Panorama(vec![Observation::None; num_validators])
    }

    /// Returns the observation for the given validator. Panics if the index is out of range.
    pub(crate) fn get(&self, idx: ValidatorIndex) -> &Observation<C> {
        &self.0[idx.0 as usize]
    }

    /// Returns `true` if there is no correct observation yet.
    pub(crate) fn is_empty(&self) -> bool {
        !self.iter().any(Observation::is_correct)
    }

    /// Returns an iterator over all hashes of the honest validators' latest messages.
    pub(crate) fn iter_correct(&self) -> impl Iterator<Item = &C::Hash> {
        self.iter().filter_map(Observation::correct)
    }

    /// Returns an iterator over all observations.
    pub(crate) fn iter(&self) -> impl Iterator<Item = &Observation<C>> {
        self.0.iter()
    }

    /// Returns an iterator over all observations, by validator index.
    pub(crate) fn enumerate(&self) -> impl Iterator<Item = (ValidatorIndex, &Observation<C>)> {
        self.iter()
            .enumerate()
            .map(|(idx, obs)| (ValidatorIndex(idx as u32), obs))
    }

    /// Returns an iterator over all correct latest votes, by validator index.
    pub(crate) fn enumerate_correct(&self) -> impl Iterator<Item = (ValidatorIndex, &C::Hash)> {
        self.enumerate()
            .filter_map(|(idx, obs)| obs.correct().map(|vhash| (idx, vhash)))
    }

    /// Updates this panorama by adding one vote. Assumes that all justifications of that vote are
    /// already seen.
    pub(crate) fn update(&mut self, idx: ValidatorIndex, obs: Observation<C>) {
        self.0[idx.0 as usize] = obs;
    }

    /// Returns the number of entries in the panorama. This must equal the number of validators.
    pub(crate) fn len(&self) -> usize {
        self.0.len()
    }
}

/// A vote sent to or received from the network.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Vote<C: Context> {
    /// The list of latest messages and faults observed by the creator of this message.
    pub(crate) panorama: Panorama<C>,
    /// The number of earlier messages by the same creator.
    pub(crate) seq_number: u64,
    /// The validator who created and sent this vote.
    pub(crate) creator: ValidatorIndex,
    /// The block this is a vote for. Either it or its parent must be the fork choice.
    pub(crate) block: C::Hash,
    /// A skip list index of the creator's swimlane, i.e. the previous vote by the same creator.
    ///
    /// For every `p = 1 << i` that divides `seq_number`, this contains an `i`-th entry pointing to
    /// the older vote with `seq_number - p`.
    pub(crate) skip_idx: Vec<C::Hash>,
    /// This vote's timestamp, in milliseconds since the epoch.
    pub(crate) timestamp: Timestamp,
    /// Original signature of the `SignedWireVote`.
    pub(crate) signature: C::Signature,
    /// The round exponent of the current round, that this message belongs to.
    ///
    /// The current round consists of all timestamps that agree with this one in all but the last
    /// `round_exp` bits.
    pub(crate) round_exp: u8,
}

impl<C: Context> Vote<C> {
    /// Creates a new `Vote` from the `WireVote`, and returns the value if it contained any.
    /// Values must be stored as a block, with the same hash.
    pub(crate) fn new(
        swvote: SignedWireVote<C>,
        fork_choice: Option<&C::Hash>,
        state: &State<C>,
    ) -> (Vote<C>, Option<C::ConsensusValue>) {
        let SignedWireVote {
            wire_vote: wvote,
            signature,
        } = swvote;
        let block = if wvote.value.is_some() {
            wvote.hash() // A vote with a new block votes for itself.
        } else {
            // If the vote didn't introduce a new block, it votes for the fork choice itself.
            // `Highway::add_vote` checks that the panorama is not empty.
            fork_choice
                .cloned()
                .expect("nonempty panorama has nonempty fork choice")
        };
        let mut skip_idx = Vec::new();
        if let Some(hash) = wvote.panorama.get(wvote.creator).correct() {
            skip_idx.push(hash.clone());
            for i in 0..wvote.seq_number.trailing_zeros() as usize {
                let old_vote = state.vote(&skip_idx[i]);
                skip_idx.push(old_vote.skip_idx[i].clone());
            }
        }
        let vote = Vote {
            panorama: wvote.panorama,
            seq_number: wvote.seq_number,
            creator: wvote.creator,
            block,
            skip_idx,
            timestamp: wvote.timestamp,
            signature,
            round_exp: wvote.round_exp,
        };
        (vote, wvote.value)
    }

    /// Returns the creator's previous message.
    pub(crate) fn previous(&self) -> Option<&C::Hash> {
        self.skip_idx.first()
    }
}
