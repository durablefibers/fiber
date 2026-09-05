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
