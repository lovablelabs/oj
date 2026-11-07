use crate::Tristate;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TypeAcquisition {
    pub enable: Tristate,
    pub include: Option<Vec<String>>,
    pub exclude: Option<Vec<String>>,
    pub disable_filename_based_type_acquisition: Tristate,
}

impl TypeAcquisition {
    pub fn equals(ta: Option<&TypeAcquisition>, other: Option<&TypeAcquisition>) -> bool {
        match (ta, other) {
            (None, None) => true,
            (Some(ta), Some(other)) => {
                std::ptr::eq(ta, other)
                    || (ta.enable == other.enable
                        && ta.include.as_deref().unwrap_or_default()
                            == other.include.as_deref().unwrap_or_default()
                        && ta.exclude.as_deref().unwrap_or_default()
                            == other.exclude.as_deref().unwrap_or_default()
                        && ta.disable_filename_based_type_acquisition
                            == other.disable_filename_based_type_acquisition)
            }
            _ => false,
        }
    }
}
