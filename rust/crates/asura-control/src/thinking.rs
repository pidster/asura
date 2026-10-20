//! The thinking level mapping (thinking-level.md): the person's level and the
//! loaded model's controls give the form sent for one turn. Pure; the service
//! records the result and the TUI uses the same function for display.

/// The person's setting. Codes equal the journal byte and the control enum.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Level {
    /// Model default: Asura sends nothing.
    #[default]
    Default = 0,
    Off = 1,
    Low = 2,
    Medium = 3,
    High = 4,
}
/// The form sent. Codes equal the journal byte and both wire enums.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Request {
    #[default]
    None = 0,
    Off = 1,
    On = 2,
    Low = 3,
    Medium = 4,
    High = 5,
}
/// Control bits the helper reports in Hello field 23.
pub const OFF: u32 = 1;
pub const ON: u32 = 2;
pub const LOW: u32 = 4;
pub const MEDIUM: u32 = 8;
pub const HIGH: u32 = 16;
/// Every defined control bit.
pub const ALL: u32 = OFF | ON | LOW | MEDIUM | HIGH;

impl Level {
    pub const ALL: [Level; 5] = [
        Level::Default,
        Level::Off,
        Level::Low,
        Level::Medium,
        Level::High,
    ];
    pub fn from_code(code: u8) -> Option<Self> {
        Self::ALL.into_iter().find(|level| *level as u8 == code)
    }
    /// The configuration and command spelling.
    pub fn name(self) -> &'static str {
        match self {
            Level::Default => "default",
            Level::Off => "off",
            Level::Low => "low",
            Level::Medium => "medium",
            Level::High => "high",
        }
    }
    pub fn parse(text: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|level| level.name() == text)
    }
}
impl Request {
    pub const ALL: [Request; 6] = [
        Request::None,
        Request::Off,
        Request::On,
        Request::Low,
        Request::Medium,
        Request::High,
    ];
    pub fn from_code(code: u8) -> Option<Self> {
        Self::ALL.into_iter().find(|request| *request as u8 == code)
    }
    pub fn name(self) -> &'static str {
        match self {
            Request::None => "none",
            Request::Off => "off",
            Request::On => "on",
            Request::Low => "low",
            Request::Medium => "medium",
            Request::High => "high",
        }
    }
    /// The control bit this request needs; `None` needs none.
    pub fn control(self) -> u32 {
        match self {
            Request::None => 0,
            Request::Off => OFF,
            Request::On => ON,
            Request::Low => LOW,
            Request::Medium => MEDIUM,
            Request::High => HIGH,
        }
    }
}

/// The form sent for `level` to a model with `controls`. A model without
/// reasoning support reports controls 0, so every level gives `None`.
pub fn request(level: Level, controls: u32) -> Request {
    let has = |bit: u32| controls & bit != 0;
    match level {
        Level::Default => Request::None,
        Level::Off if has(OFF) => Request::Off,
        Level::Off => Request::None,
        Level::Low if has(LOW) => Request::Low,
        Level::Medium if has(MEDIUM) => Request::Medium,
        Level::High if has(HIGH) => Request::High,
        Level::Low | Level::Medium | Level::High if has(ON) => Request::On,
        Level::Low | Level::Medium | Level::High => Request::None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// TL-01 through TL-07: the mapping over every level and representative
    /// controls, including on/off-only and no-reasoning models.
    #[test]
    fn tl01_to_tl07_mapping_table() {
        use Level as L;
        use Request as R;
        let cases: [(u32, [R; 5]); 6] = [
            // No reasoning, or reasoning with no control.
            (0, [R::None, R::None, R::None, R::None, R::None]),
            // Ollama `values: [false, true]`, MLX templateFlag.
            (OFF | ON, [R::None, R::Off, R::On, R::On, R::On]),
            // MLX alwaysOn.
            (ON, [R::None, R::None, R::On, R::On, R::On]),
            // Ollama named levels with boolean off (gpt-oss style).
            (ALL, [R::None, R::Off, R::Low, R::Medium, R::High]),
            // Named levels only: off cannot be sent.
            (
                LOW | MEDIUM | HIGH,
                [R::None, R::None, R::Low, R::Medium, R::High],
            ),
            // A partial named set falls back to on.
            (ON | HIGH, [R::None, R::None, R::On, R::On, R::High]),
        ];
        for (controls, expected) in cases {
            for (level, want) in L::ALL.into_iter().zip(expected) {
                assert_eq!(request(level, controls), want, "{level:?} {controls}");
                // Never a form the model does not accept.
                assert_eq!(request(level, controls).control() & !controls, 0);
            }
        }
    }

    #[test]
    fn codes_and_names_round_trip() {
        for level in Level::ALL {
            assert_eq!(Level::from_code(level as u8), Some(level));
            assert_eq!(Level::parse(level.name()), Some(level));
        }
        for request in Request::ALL {
            assert_eq!(Request::from_code(request as u8), Some(request));
        }
        assert_eq!(Level::from_code(5), None);
        assert_eq!(Request::from_code(6), None);
        for invalid in ["", "High", "on", "max", " high", "medium "] {
            assert_eq!(Level::parse(invalid), None, "{invalid:?}");
        }
    }
}
