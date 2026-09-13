//! Project membership roles.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectRole {
    Reader,
    Writer,
    Admin,
    Owner,
}

impl ProjectRole {
    pub fn rank(self) -> u8 {
        match self {
            Self::Reader => 1,
            Self::Writer => 2,
            Self::Admin => 3,
            Self::Owner => 4,
        }
    }

    pub fn at_least(self, min: Self) -> bool {
        self.rank() >= min.rank()
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Reader => "reader",
            Self::Writer => "writer",
            Self::Admin => "admin",
            Self::Owner => "owner",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "reader" => Some(Self::Reader),
            "writer" => Some(Self::Writer),
            "admin" => Some(Self::Admin),
            "owner" => Some(Self::Owner),
            _ => None,
        }
    }
}

impl std::fmt::Display for ProjectRole {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::ProjectRole::*;
    use super::*;

    const ASCENDING: [ProjectRole; 4] = [Reader, Writer, Admin, Owner];

    #[test]
    fn the_ladder_is_strictly_ascending() {
        // reader < writer < admin < owner. A tie or an inversion here silently widens
        // every `require_*` gate in fiber-api.
        for pair in ASCENDING.windows(2) {
            assert!(
                pair[0].rank() < pair[1].rank(),
                "{} must outrank below {}",
                pair[0],
                pair[1]
            );
        }
    }

    #[test]
    fn at_least_holds_exactly_for_self_and_above() {
        for (i, holder) in ASCENDING.iter().enumerate() {
            for (j, min) in ASCENDING.iter().enumerate() {
                assert_eq!(
                    holder.at_least(*min),
                    i >= j,
                    "{holder}.at_least({min}) should be {}",
                    i >= j
                );
            }
        }
    }

    #[test]
    fn a_reader_can_never_satisfy_a_mutation_gate() {
        // Mutations gate on writer or above (convention 8); spelled out so a change to
        // `rank` cannot quietly let readers write.
        assert!(!Reader.at_least(Writer));
        assert!(!Reader.at_least(Admin));
        assert!(!Reader.at_least(Owner));
        assert!(Writer.at_least(Writer));
    }

    #[test]
    fn parse_round_trips_every_variant_through_as_str() {
        for role in ASCENDING {
            assert_eq!(ProjectRole::parse(role.as_str()), Some(role));
        }
    }

    #[test]
    fn parse_tolerates_case_and_surrounding_whitespace() {
        assert_eq!(ProjectRole::parse("  Owner "), Some(Owner));
        assert_eq!(ProjectRole::parse("ADMIN"), Some(Admin));
        assert_eq!(ProjectRole::parse("\twriter\n"), Some(Writer));
    }

    #[test]
    fn an_unrecognised_role_is_none_rather_than_a_default() {
        // `member_role` maps None to "forbidden", so parsing must fail closed. Returning
        // a default here would grant that level to any unknown string in the column.
        for bad in ["", "  ", "root", "superuser", "reader ish", "own", "owners"] {
            assert_eq!(ProjectRole::parse(bad), None, "{bad:?} must not parse");
        }
    }

    #[test]
    fn serde_uses_the_same_lowercase_wire_spelling_as_as_str() {
        // The role crosses the wire to apps/ui, which compares against these strings
        // by hand (convention 9).
        for role in ASCENDING {
            let json = serde_json::to_string(&role).unwrap();
            assert_eq!(json, format!("\"{}\"", role.as_str()));
            assert_eq!(
                serde_json::from_str::<ProjectRole>(&json).unwrap(),
                role,
                "{role} must survive a serde round trip"
            );
        }
    }
}
