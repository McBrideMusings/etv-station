//! Which Plex account a channel's scorer ranks against (#112, #278, #281).
//!
//! A channel's `scoring.user` names the account `taste_scope: single_user`
//! ranks for. The station resolves it to a Plex account id and hands that id to
//! a scorer plugin as `ctx.account_id`; what the plugin does with the id —
//! read that account's taste vector, fetch that account's watch history from
//! whatever service it reaches itself — is the plugin's (ADR 0014).
//!
//! Plex's own `/accounts` is the only source consulted, so the id that comes
//! back is already in the id space `plexdb_reader` keys its store on.

use crate::catalog::ingest::plex::PlexAccount;

/// Whose taste a channel ranks against.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum AccountScope {
    /// Every account pooled, with no user dimension — the default.
    AllUsers,
    /// One account, named by Plex account id (digits) or Plex account name.
    User(String),
}

/// Every character an ASCII digit, and at least one of them.
///
/// Deliberately narrower than `parse::<i64>()`: a signed parse accepts `"+5"`
/// and `"-3"`, neither of which is an account id. A name made only of digits
/// reads as an id; one field cannot tell the two apart.
fn is_numeric_id(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_digit())
}

/// The Plex account id `scope` resolves to against `accounts`, Plex's own
/// `/accounts` listing.
///
/// `Ok(None)` for [`AccountScope::AllUsers`]. A digit-only `User` is an
/// account id and must be one `accounts` lists; any other `User` must equal a
/// listed account's name exactly. `Err`, naming the user, when nothing
/// matches — a typo in `user:` must fail the generation rather than let
/// `ctx.account_id` arrive as unit and read like a pooled channel.
pub fn resolve(scope: &AccountScope, accounts: &[PlexAccount]) -> Result<Option<i64>, String> {
    let AccountScope::User(user) = scope else {
        return Ok(None);
    };
    if is_numeric_id(user) {
        let id: i64 = user
            .parse()
            .map_err(|e| format!("user {user:?} does not fit a 64-bit account id: {e}"))?;
        return accounts
            .iter()
            .find(|a| a.id == id)
            .map(|a| Some(a.id))
            .ok_or_else(|| format!("no Plex account has id {id} (scoring.user {user:?})"));
    }
    accounts
        .iter()
        .find(|a| a.name == *user)
        .map(|a| Some(a.id))
        .ok_or_else(|| format!("no Plex account is named {user:?} (scoring.user)"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn accounts() -> Vec<PlexAccount> {
        vec![
            PlexAccount {
                id: 1,
                name: "McBrideMusings".into(),
            },
            PlexAccount {
                id: 568385169,
                name: "Pierce".into(),
            },
        ]
    }

    #[test]
    fn a_pooled_scope_resolves_to_no_account() {
        assert_eq!(resolve(&AccountScope::AllUsers, &accounts()), Ok(None));
    }

    #[test]
    fn a_name_resolves_to_the_plex_account_of_that_name() {
        let scope = AccountScope::User("Pierce".into());
        assert_eq!(resolve(&scope, &accounts()), Ok(Some(568385169)));
    }

    #[test]
    fn digits_resolve_to_the_plex_account_with_that_id() {
        let scope = AccountScope::User("1".into());
        assert_eq!(resolve(&scope, &accounts()), Ok(Some(1)));
    }

    #[test]
    fn an_unknown_name_fails_naming_the_user() {
        let err = resolve(&AccountScope::User("carol".into()), &accounts()).unwrap_err();
        assert!(err.contains("\"carol\""), "{err}");
    }

    #[test]
    fn an_unknown_id_fails_naming_the_id() {
        let err = resolve(&AccountScope::User("22831969".into()), &accounts()).unwrap_err();
        assert!(err.contains("22831969"), "{err}");
    }

    #[test]
    fn a_name_match_is_exact() {
        let err = resolve(&AccountScope::User("pierce".into()), &accounts()).unwrap_err();
        assert!(err.contains("\"pierce\""), "{err}");
    }

    #[test]
    fn a_signed_number_is_a_name_not_an_id() {
        assert!(resolve(&AccountScope::User("+1".into()), &accounts()).is_err());
    }
}
