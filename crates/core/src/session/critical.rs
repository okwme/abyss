//! The persisted critical state and its versioned postcard encoding.
//!
//! Wire format (`BLOB_VERSION` 1): `u16` version, then `WireState`. Auth keys
//! are borrowed from (encode) and into (decode) zeroizing buffers, so no
//! unwiped copy of a key is made on our side apart from the `DcOption` values
//! grammers itself requires.

use std::collections::BTreeMap;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddrV4, SocketAddrV6};

use grammers_session::SessionData;
use grammers_session::types::{DcOption, PeerAuth, PeerId, PeerInfo};
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

/// Current blob version. Any other version is discarded (forces a new login).
pub(crate) const BLOB_VERSION: u16 = 1;
/// Bound on stored DC options. Telegram has five production DCs; grammers
/// stores one option per DC id.
pub(crate) const MAX_DC_OPTIONS: usize = 16;
/// Upper bound of an encoded blob: 16 options of at most ~310 bytes each plus
/// the header and self user, rounded up.
pub(crate) const MAX_BLOB: usize = 8 * 1024;
const AUTH_KEY_LEN: usize = 256;

/// The logged-in account's identity, persisted so that a restart knows who is
/// signed in without a network round trip.
#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SelfUser {
    pub(crate) id: i64,
    pub(crate) access_hash: i64,
    pub(crate) bot: bool,
}

impl std::fmt::Debug for SelfUser {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SelfUser")
            .field("bot", &self.bot)
            .finish_non_exhaustive()
    }
}

impl SelfUser {
    pub(crate) fn to_info(self) -> PeerInfo {
        PeerInfo::User {
            id: self.id,
            auth: Some(PeerAuth::from_hash(self.access_hash)),
            bot: Some(self.bot),
            is_self: Some(true),
        }
    }
}

/// One DC option, with its auth key in a zeroizing buffer.
#[derive(Clone)]
pub(crate) struct DcEntry {
    ipv4: SocketAddrV4,
    ipv6: SocketAddrV6,
    auth_key: Option<Zeroizing<[u8; AUTH_KEY_LEN]>>,
}

impl DcEntry {
    fn from_option(option: &DcOption) -> Self {
        Self {
            ipv4: option.ipv4,
            ipv6: option.ipv6,
            auth_key: option.auth_key.map(Zeroizing::new),
        }
    }

    pub(crate) fn to_option(&self, id: i32) -> DcOption {
        DcOption {
            id,
            ipv4: self.ipv4,
            ipv6: self.ipv6,
            auth_key: self.auth_key.as_deref().copied(),
        }
    }
}

/// Everything that is persisted. Peers and update state are not.
#[derive(Clone)]
pub(crate) struct CriticalState {
    pub(crate) home_dc: i32,
    /// Ordered by id, so encoding is deterministic and byte comparison works.
    dcs: BTreeMap<i32, DcEntry>,
    pub(crate) self_user: Option<SelfUser>,
}

impl std::fmt::Debug for CriticalState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CriticalState")
            .field("home_dc", &self.home_dc)
            .field("dcs", &self.dcs.keys().collect::<Vec<_>>())
            .field("signed_in", &self.self_user.is_some())
            .finish()
    }
}

impl Default for CriticalState {
    /// grammers' defaults: an arbitrary home DC and the statically known DC
    /// options, without auth keys.
    fn default() -> Self {
        let data = SessionData::default();
        let dcs = data
            .dc_options
            .values()
            .take(MAX_DC_OPTIONS)
            .map(|o| (o.id, DcEntry::from_option(o)))
            .collect();
        Self {
            home_dc: data.home_dc,
            dcs,
            self_user: None,
        }
    }
}

/// Why a critical mutation was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Refused {
    TooManyDcs,
}

impl CriticalState {
    pub(crate) fn dc_option(&self, id: i32) -> Option<DcOption> {
        self.dcs.get(&id).map(|e| e.to_option(id))
    }

    pub(crate) fn set_dc_option(&mut self, option: &DcOption) -> Result<(), Refused> {
        if self.dcs.len() >= MAX_DC_OPTIONS && !self.dcs.contains_key(&option.id) {
            return Err(Refused::TooManyDcs);
        }
        self.dcs.insert(option.id, DcEntry::from_option(option));
        Ok(())
    }

    /// The self user as a `PeerInfo`, if signed in.
    pub(crate) fn self_info(&self) -> Option<PeerInfo> {
        self.self_user.map(SelfUser::to_info)
    }

    /// Whether `info` is (or claims to be) the self user, so it belongs here
    /// rather than in the in-memory peer map.
    pub(crate) fn is_self(&self, info: &PeerInfo) -> bool {
        match info {
            PeerInfo::User { id, is_self, .. } => {
                *is_self == Some(true) || self.self_user.is_some_and(|s| s.id == *id)
            }
            PeerInfo::Chat { .. } | PeerInfo::Channel { .. } => false,
        }
    }

    /// Merges self-user info with `extend_info` semantics: a `None` field never
    /// clears a known value. A different user flagged `is_self` replaces the
    /// self user. Out-of-range ids are ignored.
    pub(crate) fn merge_self(&mut self, info: &PeerInfo) {
        let PeerInfo::User {
            id,
            auth,
            bot,
            is_self,
        } = *info
        else {
            return;
        };
        if PeerId::user(id).is_none() {
            return;
        }
        match &mut self.self_user {
            Some(current) if current.id == id => {
                if let Some(auth) = auth {
                    current.access_hash = auth.hash();
                }
                if let Some(bot) = bot {
                    current.bot = bot;
                }
            }
            _ if is_self == Some(true) => {
                self.self_user = Some(SelfUser {
                    id,
                    access_hash: auth.unwrap_or_default().hash(),
                    bot: bot.unwrap_or(false),
                });
            }
            _ => {}
        }
    }
}

#[derive(Serialize, Deserialize)]
struct WireState<'a> {
    home_dc: i32,
    #[serde(borrow)]
    dcs: Vec<WireDc<'a>>,
    self_user: Option<SelfUser>,
}

#[derive(Serialize, Deserialize)]
struct WireDc<'a> {
    id: i32,
    ipv4: [u8; 4],
    port4: u16,
    ipv6: [u8; 16],
    port6: u16,
    flowinfo: u32,
    scope_id: u32,
    #[serde(borrow)]
    auth_key: Option<&'a [u8]>,
}

/// Error from encoding the state. Unreachable within the bounds above.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct EncodeError;

/// Serializes into a zeroizing buffer preallocated to [`MAX_BLOB`], so
/// encoding never reallocates and leaves no unwiped intermediate copy.
pub(crate) fn encode(state: &CriticalState) -> Result<Zeroizing<Vec<u8>>, EncodeError> {
    let wire = WireState {
        home_dc: state.home_dc,
        dcs: state
            .dcs
            .iter()
            .map(|(&id, e)| WireDc {
                id,
                ipv4: e.ipv4.ip().octets(),
                port4: e.ipv4.port(),
                ipv6: e.ipv6.ip().octets(),
                port6: e.ipv6.port(),
                flowinfo: e.ipv6.flowinfo(),
                scope_id: e.ipv6.scope_id(),
                auth_key: e.auth_key.as_deref().map(<[u8; AUTH_KEY_LEN]>::as_slice),
            })
            .collect(),
        self_user: state.self_user,
    };
    let mut buf = Zeroizing::new(vec![0u8; MAX_BLOB]);
    let used = postcard::to_slice(&(BLOB_VERSION, &wire), buf.as_mut_slice())
        .map_err(|_| EncodeError)?
        .len();
    buf.truncate(used);
    Ok(buf)
}

/// Result of decoding a stored blob.
#[derive(Debug)]
pub(crate) enum Decoded {
    State(CriticalState),
    /// Written by another format version: discarded, the user signs in again.
    UnknownVersion(u16),
    /// Truncated, oversized, trailing bytes or invalid values: discarded.
    Corrupt,
}

pub(crate) fn decode(blob: &[u8]) -> Decoded {
    if blob.len() > MAX_BLOB {
        return Decoded::Corrupt;
    }
    let Ok((version, rest)) = postcard::take_from_bytes::<u16>(blob) else {
        return Decoded::Corrupt;
    };
    if version != BLOB_VERSION {
        return Decoded::UnknownVersion(version);
    }
    match postcard::take_from_bytes::<WireState<'_>>(rest) {
        Ok((wire, [])) => from_wire(&wire).map_or(Decoded::Corrupt, Decoded::State),
        Ok(_) | Err(_) => Decoded::Corrupt,
    }
}

/// Starts from grammers' defaults and overlays the stored options, so a DC
/// added to the static list later is still known.
fn from_wire(wire: &WireState<'_>) -> Option<CriticalState> {
    if wire.dcs.len() > MAX_DC_OPTIONS {
        return None;
    }
    if wire.self_user.is_some_and(|s| PeerId::user(s.id).is_none()) {
        return None;
    }
    let mut state = CriticalState {
        home_dc: wire.home_dc,
        self_user: wire.self_user,
        ..CriticalState::default()
    };
    let mut seen = std::collections::BTreeSet::new();
    for dc in &wire.dcs {
        if !seen.insert(dc.id) {
            return None;
        }
        let auth_key = match dc.auth_key {
            Some(bytes) => {
                let mut key = Zeroizing::new([0u8; AUTH_KEY_LEN]);
                key.copy_from_slice(<&[u8; AUTH_KEY_LEN]>::try_from(bytes).ok()?);
                Some(key)
            }
            None => None,
        };
        let entry = DcEntry {
            ipv4: SocketAddrV4::new(Ipv4Addr::from(dc.ipv4), dc.port4),
            ipv6: SocketAddrV6::new(Ipv6Addr::from(dc.ipv6), dc.port6, dc.flowinfo, dc.scope_id),
            auth_key,
        };
        state.dcs.insert(dc.id, entry);
    }
    (state.dcs.len() <= MAX_DC_OPTIONS).then_some(state)
}
