//! Copy for the first-run screens (plan section 17.3), in one place so the
//! plan and the UI stay in sync.
//!
//! The threat model is stated per network: what a server can read differs for
//! each one, and abyss is meant to reach more than Telegram. Every network
//! gets the same three parts, so adding a network means writing all three.

/// A network abyss can connect to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Network {
    Telegram,
}

pub(crate) const WELCOME_TITLE: &str = "Before you connect";
pub(crate) const CHANGES_LABEL: &str = "What abyss changes";
pub(crate) const MISSING_LABEL: &str = "Not in this version";

/// What abyss does and does not protect against on one network. Each part
/// is a plain statement of fact: no reassurance beyond what is true.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ThreatModel {
    /// What abyss changes. The client-side part; the same idea everywhere.
    pub(crate) changes: &'static str,
    /// The heading for `servers`, naming the network.
    pub(crate) servers_label: &'static str,
    /// What that network's servers can still read.
    pub(crate) servers: &'static str,
    /// What is missing in this version.
    pub(crate) missing: &'static str,
    /// The button that leaves the screen.
    pub(crate) connect: &'static str,
}

impl ThreatModel {
    /// The three parts in reading order, each with its heading.
    pub(crate) fn parts(&self) -> [(&'static str, &'static str); 3] {
        [
            (CHANGES_LABEL, self.changes),
            (self.servers_label, self.servers),
            (MISSING_LABEL, self.missing),
        ]
    }
}

pub(crate) fn threat_model(network: Network) -> ThreatModel {
    match network {
        Network::Telegram => ThreatModel {
            changes: "abyss narrows what a message can do to this Mac. Links from messages \
                      are copied, never opened, and photos, videos and files are not loaded.",
            servers_label: "What Telegram can still read",
            servers: "Telegram's servers can read your cloud chats. abyss cannot change that.",
            missing: "Secret chats, calls, stickers, a browser and Mini Apps.",
            connect: "Connect to Telegram",
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The voice rules that can be checked mechanically (`docs/brand/brand.md`).
    #[test]
    fn statements_are_plain_and_complete() {
        for network in [Network::Telegram] {
            let model = threat_model(network);
            let all = model
                .parts()
                .into_iter()
                .flat_map(|(label, text)| [label, text])
                .chain([model.connect]);
            for text in all {
                assert!(!text.is_empty());
                assert!(!text.contains('!'), "{text}");
                let lower = text.to_lowercase();
                for banned in ["secure", "military", "safe"] {
                    assert!(!lower.contains(banned), "{text}");
                }
            }
        }
    }
}
