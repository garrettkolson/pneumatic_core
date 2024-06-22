pub trait IsConfiguration {
    fn is_for_testing(&self) -> bool;
}