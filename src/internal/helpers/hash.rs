// Port of upstream internal/helpers/hash.go.

/// From: <http://boost.sourceforge.net/doc/html/boost/hash_combine.html>
#[must_use]
pub const fn hash_combine(seed: u32, hash: u32) -> u32 {
    seed ^ hash
        .wrapping_add(0x9e37_79b9)
        .wrapping_add(seed.wrapping_shl(6))
        .wrapping_add(seed.wrapping_shr(2))
}

#[must_use]
/// # Panics
///
/// Panics if `text` is larger than `u32::MAX`, the maximum string size
/// supported by esbuild's internal data structures.
pub fn hash_combine_string(mut seed: u32, text: &str) -> u32 {
    seed = hash_combine(
        seed,
        u32::try_from(text.len()).expect("esbuild strings must fit in 32 bits"),
    );
    for c in text.chars() {
        seed = hash_combine(seed, u32::from(c));
    }
    seed
}

#[cfg(test)]
mod tests {
    use super::{hash_combine, hash_combine_string};

    #[test]
    fn wraps_like_go_uint32_arithmetic() {
        assert_eq!(hash_combine(0, 0), 0x9e37_79b9);
        assert_eq!(hash_combine(u32::MAX, u32::MAX), 0x21c8_8688);
        assert_eq!(hash_combine_string(0, ""), 0x9e37_79b9);
        assert_eq!(hash_combine_string(123, "λ🙂"), 0xf53d_e1e6);
    }
}
