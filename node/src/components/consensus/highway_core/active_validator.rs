use std::fmt::{self, Debug};

use tracing::warn;

use super::{
    highway::ValidVertex,
    state::State,
    validators::ValidatorIndex,
    vertex::{Vertex, WireVote},
    vote::{Observation, Panorama, Vote},
};

use crate::{
    components::consensus::{
        consensus_protocol::BlockContext, highway_core::vertex::SignedWireVote, traits::Context,
    },
    types::{TimeDiff, Timestamp},
};

/// An action taken by a validator.
#[derive(Clone, Eq, PartialEq, Debug)]
pub(crate) enum Effect<C: Context> {
    /// Newly vertex that should be gossiped to peers and added to the protocol state.
    NewVertex(ValidVertex<C>),
    /// `handle_timer` needs to be called at the specified time.
    ScheduleTimer(Timestamp),
    /// `propose` needs to be called with a value for a new block with the specified timestamp.
    // TODO: Add more information required by the deploy buffer.
    RequestNewBlock(BlockContext),
}

/// A validator that actively participates in consensus by creating new vertices.
///
/// It implements the Highway schedule. The protocol proceeds in rounds, and in each round one
/// validator is the _leader_.
/// * In the beginning of the round, the leader sends a _proposal_ vote, containing a consensus
///   value (i.e. a block).
/// * Upon receiving the proposal, all the other validators send a _confirmation_ vote, citing only
///   the proposal, their own previous message, and resulting transitive justifications.
/// * At a fixed point in time later in the round, everyone unconditionally sends a _witness_ vote,
///   citing every vote they have received so far.
///
/// If the rounds are long enough (i.e. message delivery is fast enough) and there are enough
/// honest validators, there will be a lot of confirmations for the proposal, and enough witness
/// votes citing all those confirmations, to create a summit and finalize the proposal.
pub(crate) struct ActiveValidator<C: Context> {
    /// Our own validator index.
    vidx: ValidatorIndex,
    /// The validator's secret signing key.
    secret: C::ValidatorSecret,
    /// The next round exponent: Our next round will be `1 << next_round_exp` milliseconds long.
    next_round_exp: u8,
    /// The latest timer we scheduled.
    next_timer: Timestamp,
}

impl<C: Context> Debug for ActiveValidator<C> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("ActiveValidator")
            .field("vidx", &self.vidx)
            .field("next_round_exp", &self.next_round_exp)
            .field("next_timer", &self.next_timer)
            .finish()
    }
}

impl<C: Context> ActiveValidator<C> {
    /// Creates a new `ActiveValidator` and the timer effect for the first call.
    pub(crate) fn new(
        vidx: ValidatorIndex,
        secret: C::ValidatorSecret,
        next_round_exp: u8,
        timestamp: Timestamp,
        state: &State<C>,
    ) -> (Self, Vec<Effect<C>>) {
        let mut av = ActiveValidator {
            vidx,
            secret,
            next_round_exp,
            next_timer: Timestamp::zero(),
        };
        let effects = av.schedule_timer(timestamp, state);
        (av, effects)
    }

    /// Returns actions a validator needs to take at the specified `timestamp`, with the given
    /// protocol `state`.
    pub(crate) fn handle_timer(
        &mut self,
        timestamp: Timestamp,
        state: &State<C>,
    ) -> Vec<Effect<C>> {
        let mut effects = self.schedule_timer(timestamp, state);
        if self.earliest_vote_time(state) > timestamp {
            warn!(%timestamp, "skipping outdated timer event");
            return effects;
        }
        let round_id = self.round_id(state, timestamp);
        let round_len = self.round_len(state, timestamp);
        if timestamp == round_id && state.leader(round_id) == self.vidx {
            let bctx = BlockContext::new(timestamp);
            effects.push(Effect::RequestNewBlock(bctx));
        } else if timestamp == round_id + self.witness_offset(round_len) {
            let panorama = state.panorama_cutoff(state.panorama(), timestamp);
            if !panorama.is_empty() {
                let witness_vote = self.new_vote(panorama, timestamp, None, state);
                effects.push(Effect::NewVertex(ValidVertex(Vertex::Vote(witness_vote))))
            }
        }
        effects
    }

    /// Returns actions a validator needs to take upon receiving a new vote.
    pub(crate) fn on_new_vote(
        &self,
        vhash: &C::Hash,
        timestamp: Timestamp,
        state: &State<C>,
    ) -> Vec<Effect<C>> {
        if self.earliest_vote_time(state) > timestamp {
            warn!(%timestamp, "skipping outdated confirmation");
        } else if self.should_send_confirmation(vhash, timestamp, state) {
            let panorama = self.confirmation_panorama(vhash, state);
            if !panorama.is_empty() {
                let confirmation_vote = self.new_vote(panorama, timestamp, None, state);
                let vv = ValidVertex(Vertex::Vote(confirmation_vote));
                return vec![Effect::NewVertex(vv)];
            }
        }
        vec![]
    }

    /// Proposes a new block with the given consensus value.
    pub(crate) fn propose(
        &self,
        value: C::ConsensusValue,
        block_context: BlockContext,
        state: &State<C>,
    ) -> Vec<Effect<C>> {
        let timestamp = block_context.timestamp();
        if self.earliest_vote_time(state) > timestamp {
            warn!(?block_context, "skipping outdated proposal");
            return vec![];
        }
        let panorama = state.panorama_cutoff(state.panorama(), timestamp);
        let proposal_vote = self.new_vote(panorama, timestamp, Some(value), state);
        vec![Effect::NewVertex(ValidVertex(Vertex::Vote(proposal_vote)))]
    }

    /// Returns whether the incoming message is a proposal that we need to send a confirmation for.
    fn should_send_confirmation(
        &self,
        vhash: &C::Hash,
        timestamp: Timestamp,
        state: &State<C>,
    ) -> bool {
        let vote = state.vote(vhash);
        if vote.timestamp > timestamp {
            warn!(%vote.timestamp, %timestamp, "added a vote with a future timestamp");
            return false;
        }
        let round_exp = self.round_exp(state, timestamp);
        timestamp >> round_exp == vote.timestamp >> round_exp // Current round.
            && state.leader(vote.timestamp) == vote.creator // The creator is the round's leader.
            && vote.creator != self.vidx // We didn't send it ourselves.
            && !state.has_evidence(vote.creator) // The creator is not faulty.
            && self.latest_vote(state)
                .map_or(true, |vote| {
                    !state.sees_correct(&vote.panorama, vhash)
                }) // We haven't confirmed it already.
    }

    /// Returns the panorama of the confirmation for the leader vote `vhash`.
    fn confirmation_panorama(&self, vhash: &C::Hash, state: &State<C>) -> Panorama<C> {
        let vote = state.vote(vhash);
        let mut panorama;
        if let Some(prev_hash) = state.panorama().get(self.vidx).correct().cloned() {
            let own_vote = state.vote(&prev_hash);
            panorama = state.merge_panoramas(&vote.panorama, &own_vote.panorama);
            panorama.update(self.vidx, Observation::Correct(prev_hash));
        } else {
            panorama = vote.panorama.clone();
        }
        panorama.update(vote.creator, Observation::Correct(vhash.clone()));
        for faulty_v in state.faulty_validators() {
            panorama.update(faulty_v, Observation::Faulty);
        }
        panorama
    }

    /// Returns a new vote with the given data, and the correct sequence number.
    fn new_vote(
        &self,
        panorama: Panorama<C>,
        timestamp: Timestamp,
        value: Option<C::ConsensusValue>,
        state: &State<C>,
    ) -> SignedWireVote<C> {
        let add1 = |vh: &C::Hash| state.vote(vh).seq_number + 1;
        let seq_number = panorama.get(self.vidx).correct().map_or(0, add1);
        let wvote = WireVote {
            panorama,
            creator: self.vidx,
            value,
            seq_number,
            timestamp,
            round_exp: self.round_exp(state, timestamp),
        };
        SignedWireVote::new(wvote, &self.secret)
    }

    /// Returns a `ScheduleTimer` effect for the next time we need to be called.
    ///
    /// If the time is before the current round's witness vote, schedule the witness vote.
    /// Otherwise, if we are the next round's leader, schedule the proposal vote.
    /// Otherwise schedule the next round's witness vote.
    fn schedule_timer(&mut self, timestamp: Timestamp, state: &State<C>) -> Vec<Effect<C>> {
        if self.next_timer > timestamp {
            return Vec::new(); // We already scheduled the next call; nothing to do.
        }
        let round_id = self.round_id(state, timestamp);
        let round_len = self.round_len(state, timestamp);
        self.next_timer = if timestamp < round_id + self.witness_offset(round_len) {
            round_id + self.witness_offset(round_len)
        } else {
            let next_round_id = round_id + round_len;
            if state.leader(next_round_id) == self.vidx {
                next_round_id
            } else {
                let next_round_len = self.round_len(state, next_round_id);
                next_round_id + self.witness_offset(next_round_len)
            }
        };
        vec![Effect::ScheduleTimer(self.next_timer)]
    }

    /// Returns the earliest timestamp where we can cast our next vote without equivocating, i.e.
    /// the timestamp of our previous vote, or 0 if there is none.
    fn earliest_vote_time(&self, state: &State<C>) -> Timestamp {
        self.latest_vote(state)
            .map_or_else(Timestamp::zero, |vh| vh.timestamp)
    }

    /// Returns the most recent vote by this validator.
    fn latest_vote<'a>(&self, state: &'a State<C>) -> Option<&'a Vote<C>> {
        state
            .panorama()
            .get(self.vidx)
            .correct()
            .map(|vh| state.vote(vh))
    }

    /// Returns the duration after the beginning of a round when the witness votes are sent.
    fn witness_offset(&self, round_len: TimeDiff) -> TimeDiff {
        round_len * 2 / 3
    }

    /// The round exponent of the round containing `timestamp`.
    ///
    /// This returns `self.next_round_exp`, if that is a valid round exponent for a vote cast at
    /// `timestamp`. Otherwise it returns the round exponent of our latest vote.
    fn round_exp(&self, state: &State<C>, timestamp: Timestamp) -> u8 {
        self.latest_vote(state).map_or(self.next_round_exp, |vote| {
            let max_re = self.next_round_exp.max(vote.round_exp);
            if timestamp < round_id(timestamp, max_re) {
                self.next_round_exp
            } else {
                vote.round_exp
            }
        })
    }

    /// The length of the round containing `timestamp`.
    fn round_len(&self, state: &State<C>, timestamp: Timestamp) -> TimeDiff {
        TimeDiff::from(1 << self.round_exp(state, timestamp))
    }

    /// The ID, i.e. the beginning, of the round containing `timestamp`.
    fn round_id(&self, state: &State<C>, timestamp: Timestamp) -> Timestamp {
        round_id(timestamp, self.round_exp(state, timestamp))
    }
}

/// Returns the time at which the round with the given timestamp and round exponent began.
///
/// The boundaries of rounds with length `1 << round_exp` are multiples of that length, in
/// milliseconds since the epoch. So the beginning of the current round is the greatest multiple
/// of `1 << round_exp` that is less or equal to `timestamp`.
fn round_id(timestamp: Timestamp, round_exp: u8) -> Timestamp {
    // The greatest multiple less or equal to the timestamp is the timestamp with the last
    // `round_exp` bits set to zero.
    (timestamp >> round_exp) << round_exp
}

#[cfg(test)]
mod tests {
    use std::fmt::Debug;

    use super::{
        super::{
            finality_detector::{FinalityDetector, FinalityOutcome},
            state::{tests::*, Weight},
            vertex::Vertex,
        },
        *,
    };

    type Eff = Effect<TestContext>;

    impl Eff {
        fn unwrap_vote(self) -> SignedWireVote<TestContext> {
            if let Eff::NewVertex(ValidVertex(Vertex::Vote(swvote))) = self {
                swvote
            } else {
                panic!("Unexpected effect: {:?}", self);
            }
        }
    }

    fn unwrap_single<T: Debug>(vec: Vec<T>) -> T {
        let mut iter = vec.into_iter();
        match (iter.next(), iter.next()) {
            (None, _) => panic!("Unexpected empty vec"),
            (Some(t), None) => t,
            (Some(t0), Some(t1)) => panic!("Expected only one element: {:?}, {:?}", t0, t1),
        }
    }

    #[test]
    #[allow(clippy::unreadable_literal)] // 0xC0FFEE is more readable than 0x00C0_FFEE.
    fn active_validator() -> Result<(), AddVoteError<TestContext>> {
        let mut state = State::<TestContext>::new(&[Weight(3), Weight(4)], 0);
        let mut fd = FinalityDetector::new(Weight(2));

        // We start at time 410, with round length 16, so the first leader tick is 416, and the
        // first witness tick 426.
        assert_eq!(ALICE, state.leader(416.into())); // Alice will be the first leader.
        assert_eq!(BOB, state.leader(432.into())); // Bob will be the second leader.
        let (mut alice_av, effects) =
            ActiveValidator::new(ALICE, TestSecret(0), 4, 410.into(), &state);
        assert_eq!([Eff::ScheduleTimer(416.into())], *effects);
        let (mut bob_av, effects) = ActiveValidator::new(BOB, TestSecret(1), 4, 410.into(), &state);
        assert_eq!([Eff::ScheduleTimer(426.into())], *effects);

        assert!(alice_av.handle_timer(415.into(), &state).is_empty()); // Too early: No new effects.

        // Alice wants to propose a block, and also make her witness vote at 426.
        let bctx = match &*alice_av.handle_timer(416.into(), &state) {
            [Eff::ScheduleTimer(timestamp), Eff::RequestNewBlock(bctx)]
                if *timestamp == 426.into() =>
            {
                bctx.clone()
            }
            effects => panic!("unexpected effects {:?}", effects),
        };
        assert_eq!(Timestamp::from(416), bctx.timestamp());

        // She has a pending deploy from Colin who wants to pay for a hot beverage.
        let effects = alice_av.propose(0xC0FFEE, bctx, &state);
        let proposal_wvote = unwrap_single(effects).unwrap_vote();
        let prop_hash = proposal_wvote.hash();
        state.add_vote(proposal_wvote)?;
        assert!(alice_av
            .on_new_vote(&prop_hash, 417.into(), &state)
            .is_empty());

        // Bob creates a confirmation vote for Alice's proposal.
        let effects = bob_av.on_new_vote(&prop_hash, 419.into(), &state);
        state.add_vote(unwrap_single(effects).unwrap_vote())?;

        // Bob creates his witness message 2/3 through the round.
        let mut effects = bob_av.handle_timer(426.into(), &state).into_iter();
        assert_eq!(Some(Eff::ScheduleTimer(432.into())), effects.next()); // Bob is the next leader.
        state.add_vote(effects.next().unwrap().unwrap_vote())?;
        assert_eq!(None, effects.next());

        assert_eq!(FinalityOutcome::None, fd.run_on_state(&state)); // Alice has not witnessed Bob's vote yet.

        // Alice also sends her own witness message, completing the summit for her proposal.
        let mut effects = alice_av.handle_timer(426.into(), &state).into_iter();
        assert_eq!(Some(Eff::ScheduleTimer(442.into())), effects.next()); // Timer for witness vote.
        state.add_vote(effects.next().unwrap().unwrap_vote())?;
        assert_eq!(None, effects.next());

        // Payment finalized! "One Pumpkin Spice Mochaccino for Corbyn!"
        assert_eq!(
            FinalityOutcome::Finalized {
                value: 0xC0FFEE,
                new_equivocators: Vec::new(),
                timestamp: 416.into(),
            },
            fd.run_on_state(&state)
        );
        Ok(())
    }
}
