//! What to start: the fake backend (`--fake`) or the real one with
//! credentials from the environment (plan section 12). Credentials are read at
//! runtime only and never logged.

use std::ffi::{OsStr, OsString};

use abyss_core::Credentials;

pub(crate) const API_ID_VAR: &str = "ABYSS_API_ID";
pub(crate) const API_HASH_VAR: &str = "ABYSS_API_HASH";
pub(crate) const FAKE_FLAG: &str = "--fake";

#[derive(Debug)]
pub(crate) enum Launch {
    /// The in-process fake backend with a demo account.
    #[cfg(feature = "fake")]
    Fake,
    Telegram(Credentials),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum LaunchError {
    /// Names of the variables that are unset or empty.
    MissingCredentials(Vec<&'static str>),
    /// `ABYSS_API_ID` is set but is not a positive integer.
    InvalidApiId,
    /// `--fake` was passed to a build without the `fake` feature.
    #[cfg_attr(
        feature = "fake",
        expect(
            dead_code,
            reason = "only builds without the fake backend reject --fake"
        )
    )]
    FakeNotBuilt,
}

/// Decides what to start from the command line and an environment lookup.
pub(crate) fn resolve(
    args: impl IntoIterator<Item = OsString>,
    var: impl Fn(&str) -> Option<OsString>,
) -> Result<Launch, LaunchError> {
    // Skip argv[0]; ignore anything else (macOS may add `-psn_…`).
    if args.into_iter().skip(1).any(|a| a == OsStr::new(FAKE_FLAG)) {
        #[cfg(feature = "fake")]
        return Ok(Launch::Fake);
        #[cfg(not(feature = "fake"))]
        return Err(LaunchError::FakeNotBuilt);
    }
    let id = var(API_ID_VAR).filter(|v| !v.is_empty());
    let hash = var(API_HASH_VAR).filter(|v| !v.is_empty());
    let (id, hash) = match (id, hash) {
        (Some(id), Some(hash)) => (id, hash),
        (id, hash) => {
            let missing = [(API_ID_VAR, id.is_none()), (API_HASH_VAR, hash.is_none())]
                .into_iter()
                .filter_map(|(name, absent)| absent.then_some(name))
                .collect();
            return Err(LaunchError::MissingCredentials(missing));
        }
    };
    let api_id = id
        .to_str()
        .and_then(|s| s.trim().parse::<i32>().ok())
        .filter(|id| *id > 0)
        .ok_or(LaunchError::InvalidApiId)?;
    let api_hash = hash.into_string().map_err(|_| LaunchError::InvalidApiId)?;
    Ok(Launch::Telegram(Credentials { api_id, api_hash }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<OsString> {
        std::iter::once("abyss")
            .chain(list.iter().copied())
            .map(OsString::from)
            .collect()
    }

    fn env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<OsString> + 'a {
        move |name| {
            pairs
                .iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| OsString::from(v))
        }
    }

    #[test]
    fn missing_credentials_name_the_variables() {
        assert_eq!(
            resolve(args(&[]), env(&[])).unwrap_err(),
            LaunchError::MissingCredentials(vec![API_ID_VAR, API_HASH_VAR])
        );
        assert_eq!(
            resolve(args(&[]), env(&[(API_ID_VAR, "1"), (API_HASH_VAR, "")])).unwrap_err(),
            LaunchError::MissingCredentials(vec![API_HASH_VAR])
        );
    }

    #[test]
    fn invalid_api_id() {
        for bad in ["abc", "-1", "0", "99999999999"] {
            assert_eq!(
                resolve(args(&[]), env(&[(API_ID_VAR, bad), (API_HASH_VAR, "h")])).unwrap_err(),
                LaunchError::InvalidApiId
            );
        }
    }

    #[test]
    fn credentials_from_env() {
        let launch = resolve(
            args(&["-psn_0_1"]),
            env(&[(API_ID_VAR, " 42 "), (API_HASH_VAR, "h")]),
        );
        match launch.unwrap() {
            Launch::Telegram(c) => {
                assert_eq!(c.api_id, 42);
                assert_eq!(c.api_hash, "h");
            }
            #[cfg(feature = "fake")]
            Launch::Fake => panic!("expected credentials"),
        }
    }

    #[test]
    fn fake_flag_skips_credentials() {
        let launch = resolve(args(&[FAKE_FLAG]), env(&[]));
        #[cfg(feature = "fake")]
        assert!(matches!(launch, Ok(Launch::Fake)));
        #[cfg(not(feature = "fake"))]
        assert_eq!(launch.unwrap_err(), LaunchError::FakeNotBuilt);
    }
}
