//! Which peers an instance keeps pinned in its session (plan section 2,
//! rebuild step 5: the retired snapshot carries only pinned peers).
//!
//! Core owns the set (`Client::retain_peers`): pending sends, open windows,
//! then the retained dialogs in list order, at most `MAX_PINNED_PEERS`. The
//! backend does not guess. The selected set is:
//! 1. the base: core's last required set or, until core's first call on this
//!    instance, the peers inherited from the retired instance, kept as they
//!    are (an unordered snapshot carries no priority to reinterpret);
//! 2. then the peers of in-flight calls that the base does not list (core
//!    has not caught up with them yet).
//!
//! Deduplicated and cut at `MAX_PINNED_PEERS`, so it always fits the session
//! cap; the base comes first because core already ranks sends and windows
//! (the chats calls address) ahead of dialogs. The session is repinned
//! whenever the selected set changes, and while any listed peer is unknown.
//! A listed peer whose access hash the session lacks is not pinned, only
//! counted; the session keeps the selection as its desired set and pins such
//! a peer itself when any insertion (a response, an update) makes it
//! addressable.
//!
//! The session is told which ids are core's (the base once core has set it)
//! and which are transient (in-flight extras, or the inherited base): only
//! core's ids end a dialogs handoff's hold.

use std::collections::HashSet;

use grammers_session::types::PeerId;

use super::map;
use crate::backend::MAX_CARRIED_PEERS;
use crate::model::ChatId;
use crate::session::MAX_PINNED_PEERS;

/// Distinct chats tracked as in flight: core admits at most this many
/// windows plus sends; beyond it a call is simply not tracked.
pub(crate) const MAX_IN_FLIGHT: usize = MAX_CARRIED_PEERS;

/// Where the base came from. Only core's own set ends a dialogs handoff's
/// hold in the session (see `session::PinRequest`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Base {
    Inherited,
    Core,
}

/// The pins of one instance.
#[derive(Debug)]
pub(crate) struct Pins {
    /// Core's last required set, or the inherited pins before it.
    base: Vec<PeerId>,
    origin: Base,
    /// Chats of calls in progress, with their call counts, oldest first.
    in_flight: Vec<(PeerId, u32)>,
    /// The set last handed to the session.
    applied: HashSet<PeerId>,
    /// Listed peers the session could not pin at the last application.
    unknown: usize,
}

impl Pins {
    /// `inherited` are the pins the session started with (the retired
    /// instance's set); they are already pinned.
    pub(crate) fn new(mut inherited: Vec<PeerId>) -> Self {
        inherited.truncate(MAX_PINNED_PEERS);
        let applied = inherited.iter().copied().collect();
        Self {
            base: inherited,
            origin: Base::Inherited,
            in_flight: Vec::with_capacity(MAX_IN_FLIGHT),
            applied,
            unknown: 0,
        }
    }

    /// Core's required set replaces the base. Returns how many ids were out
    /// of range (never pinnable, so not counted as unknown).
    pub(crate) fn require(&mut self, chats: &[ChatId]) -> usize {
        let mut seen = HashSet::with_capacity(chats.len().min(MAX_PINNED_PEERS));
        let mut base = Vec::with_capacity(chats.len().min(MAX_PINNED_PEERS));
        let mut invalid = 0_usize;
        for &chat in chats {
            if base.len() >= MAX_PINNED_PEERS {
                break;
            }
            match map::peer_id(chat) {
                Some(id) => {
                    if seen.insert(id) {
                        base.push(id);
                    }
                }
                None => invalid = invalid.saturating_add(1),
            }
        }
        self.base = base;
        self.origin = Base::Core;
        invalid
    }

    /// Splits `selected` (this tracker's [`Self::selected`]) into core's
    /// required ids and the transient rest: the base leads the selection
    /// (it is distinct and capped), and an inherited base is transient.
    pub(crate) fn split<'a>(&self, selected: &'a [PeerId]) -> (&'a [PeerId], &'a [PeerId]) {
        let required = match self.origin {
            Base::Core => self.base.len(),
            Base::Inherited => 0,
        };
        selected
            .split_at_checked(required)
            .unwrap_or((&[], selected))
    }

    /// A call addressing `id` started. Returns whether it is tracked (and so
    /// must be ended with [`Self::end`]).
    pub(crate) fn begin(&mut self, id: PeerId) -> bool {
        if let Some(entry) = self.in_flight.iter_mut().find(|(p, _)| *p == id) {
            entry.1 = entry.1.saturating_add(1);
            return true;
        }
        if self.in_flight.len() >= MAX_IN_FLIGHT {
            return false;
        }
        self.in_flight.push((id, 1));
        true
    }

    /// A tracked call addressing `id` ended.
    pub(crate) fn end(&mut self, id: PeerId) {
        let Some(at) = self.in_flight.iter().position(|(p, _)| *p == id) else {
            return;
        };
        let last = self.in_flight.get_mut(at).is_some_and(|entry| {
            entry.1 = entry.1.saturating_sub(1);
            entry.1 == 0
        });
        if last {
            self.in_flight.remove(at);
        }
    }

    /// The selected set: the base, then in-flight extras, distinct, at most
    /// `MAX_PINNED_PEERS`.
    pub(crate) fn selected(&self) -> Vec<PeerId> {
        let cap = self
            .base
            .len()
            .saturating_add(self.in_flight.len())
            .min(MAX_PINNED_PEERS);
        let mut seen = HashSet::with_capacity(cap);
        let mut out = Vec::with_capacity(cap);
        let extras = self.in_flight.iter().map(|(id, _)| *id);
        for id in self.base.iter().copied().chain(extras) {
            if out.len() >= MAX_PINNED_PEERS {
                break;
            }
            if seen.insert(id) {
                out.push(id);
            }
        }
        out
    }

    /// The selected set when it differs from the one last applied.
    pub(crate) fn changed(&self) -> Option<Vec<PeerId>> {
        let selected = self.selected();
        let same = selected.len() == self.applied.len()
            && selected.iter().all(|id| self.applied.contains(id));
        (!same).then_some(selected)
    }

    /// Records what was handed to the session and how many it could not pin.
    pub(crate) fn applied(&mut self, set: &[PeerId], unknown: usize) {
        self.applied = set.iter().copied().collect();
        self.unknown = unknown;
    }

    /// Whether the last application left listed peers unpinned.
    pub(crate) fn has_unknown(&self) -> bool {
        self.unknown > 0
    }

    #[cfg(test)]
    pub(crate) fn unknown(&self) -> usize {
        self.unknown
    }
}
