use clap::ValueEnum;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ValueEnum, Default)]
#[serde(rename_all = "lowercase")]
#[clap(rename_all = "lowercase")]
pub enum Mode {
    Fetch,
    #[default]
    Pull,
    Merge,
    Rebase,
    Push,
}

impl Mode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Fetch => "fetch",
            Self::Pull => "pull",
            Self::Merge => "merge",
            Self::Rebase => "rebase",
            Self::Push => "push",
        }
    }

    /// Cycle through the batch modes offered in the TUI. `Fetch` is not part
    /// of the cycle (startup auto-fetch covers it); it only remains reachable
    /// via the `f` key and headless `-q -m fetch`.
    pub const fn cycle(self) -> Self {
        match self {
            Self::Pull => Self::Merge,
            Self::Merge => Self::Rebase,
            Self::Rebase => Self::Push,
            Self::Push | Self::Fetch => Self::Pull,
        }
    }
}

impl std::fmt::Display for Mode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::Mode;

    #[test]
    fn cycle_skips_fetch() {
        let mut mode = Mode::Pull;
        for _ in 0..8 {
            mode = mode.cycle();
            assert_ne!(mode, Mode::Fetch);
        }
        // A stale Fetch mode re-enters the cycle at Pull.
        assert_eq!(Mode::Fetch.cycle(), Mode::Pull);
    }
}
