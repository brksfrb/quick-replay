//! Platform-independent conversion policy, also exercised on Linux.
pub fn quality_processing(fast: bool) -> bool {
    !fast
}

#[cfg(test)]
mod tests {
    #[test]
    fn quality_processing_includes_native_size() {
        // Size is deliberately absent: native, downscale and upscale all use quality.
        assert!(super::quality_processing(false));
        assert!(!super::quality_processing(true));
    }
}
