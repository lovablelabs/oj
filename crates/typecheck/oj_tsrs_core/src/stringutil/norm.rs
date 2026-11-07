// Go's Unicode categories stay on the pinned tables; ICU supplies canonical normalization.

use icu_normalizer::DecomposingNormalizerBorrowed;

use super::norm_generated::{UNICODE_MN, UNICODE_UPPER};
use super::rangetable::{Rune, unicode_is};

/// Go `unicode.Is(unicode.Mn, r)`.
pub fn unicode_is_mn(r: Rune) -> bool {
    unicode_is(&UNICODE_MN, r)
}

/// Go `unicode.IsUpper(r)`.
pub fn unicode_is_upper(r: Rune) -> bool {
    unicode_is(&UNICODE_UPPER, r)
}

/// Canonical decomposition with canonical ordering of combining marks.
/// Keep the existing omission of x/text's Stream-Safe Text Format joiners.
pub fn norm_nfd_string(s: &str) -> String {
    // The NFD-specific constructor keeps compatibility-decomposition tables out of the binary.
    DecomposingNormalizerBorrowed::new_nfd().normalize(s).into_owned()
}
