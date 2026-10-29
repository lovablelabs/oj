pub trait TypeValidatedField {
    fn is_present(&self) -> bool;
    fn is_valid(&self) -> bool;
    fn expected_json_type(&self) -> &'static str;
    fn actual_json_type(&self) -> &'static str;
}
