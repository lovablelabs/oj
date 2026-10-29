#[repr(u8)]
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Default)]
pub enum Tristate {
    #[default]
    Unknown = 0,
    False = 1,
    True = 2,
}

impl Tristate {
    #[inline]
    pub fn is_true(self) -> bool {
        self == Tristate::True
    }

    #[inline]
    pub fn is_true_or_unknown(self) -> bool {
        self == Tristate::True || self == Tristate::Unknown
    }

    #[inline]
    pub fn is_false(self) -> bool {
        self == Tristate::False
    }

    #[inline]
    pub fn is_false_or_unknown(self) -> bool {
        self == Tristate::False || self == Tristate::Unknown
    }

    #[inline]
    pub fn is_unknown(self) -> bool {
        self == Tristate::Unknown
    }

    #[must_use]
    pub fn default_if_unknown(self, value: Tristate) -> Tristate {
        if self == Tristate::Unknown {
            return value;
        }
        self
    }

    fn string(self) -> &'static str {
        match self {
            Tristate::Unknown => "TSUnknown",
            Tristate::False => "TSFalse",
            Tristate::True => "TSTrue",
        }
    }
}

impl std::fmt::Display for Tristate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.string())
    }
}

#[inline]
pub fn bool_to_tristate(b: bool) -> Tristate {
    if b { Tristate::True } else { Tristate::False }
}
