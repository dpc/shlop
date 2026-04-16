//! Shared protocol types and CBOR stream codec helpers.

/// Crate marker used while the workspace is still being bootstrapped.
pub const CRATE_NAME: &str = "shlop-proto";

#[cfg(test)]
mod tests {
    use super::CRATE_NAME;

    #[test]
    fn crate_name_matches() {
        assert_eq!(CRATE_NAME, "shlop-proto");
    }
}
