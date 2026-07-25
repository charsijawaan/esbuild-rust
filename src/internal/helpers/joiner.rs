// Port of upstream internal/helpers/joiner.go.

/// Efficiently joins large strings and byte arrays with one final allocation.
///
/// This mirrors esbuild's two-list representation, including the zero-copy
/// fast path when the joiner only receives a single byte array.
#[derive(Debug, Default)]
pub struct Joiner {
    strings: Vec<JoinerString>,
    bytes: Vec<JoinerBytes>,
    length: u32,
    last_byte: u8,
}

#[derive(Debug)]
struct JoinerString {
    data: String,
    offset: u32,
}

#[derive(Debug)]
struct JoinerBytes {
    data: Vec<u8>,
    offset: u32,
}

impl Joiner {
    /// # Panics
    ///
    /// Panics if the combined output length exceeds `u32::MAX`, matching the
    /// size assumption in the upstream implementation.
    pub fn add_string(&mut self, data: impl Into<String>) {
        let data = data.into();
        if let Some(last_byte) = data.as_bytes().last() {
            self.last_byte = *last_byte;
        }
        let data_len = u32::try_from(data.len()).expect("joiner input must fit in 32 bits");
        self.strings.push(JoinerString {
            data,
            offset: self.length,
        });
        self.length = self
            .length
            .checked_add(data_len)
            .expect("joiner output must fit in 32 bits");
    }

    /// # Panics
    ///
    /// Panics if the combined output length exceeds `u32::MAX`, matching the
    /// size assumption in the upstream implementation.
    pub fn add_bytes(&mut self, data: Vec<u8>) {
        if let Some(last_byte) = data.last() {
            self.last_byte = *last_byte;
        }
        let data_len = u32::try_from(data.len()).expect("joiner input must fit in 32 bits");
        self.bytes.push(JoinerBytes {
            data,
            offset: self.length,
        });
        self.length = self
            .length
            .checked_add(data_len)
            .expect("joiner output must fit in 32 bits");
    }

    #[must_use]
    pub const fn last_byte(&self) -> u8 {
        self.last_byte
    }

    #[must_use]
    pub const fn len(&self) -> u32 {
        self.length
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.length == 0
    }

    pub fn ensure_newline_at_end(&mut self) {
        if self.length > 0 && self.last_byte != b'\n' {
            self.add_string("\n");
        }
    }

    #[must_use]
    /// # Panics
    ///
    /// Panics only if the joiner's private offset bookkeeping is internally
    /// inconsistent.
    pub fn done(mut self) -> Vec<u8> {
        if self.strings.is_empty() && self.bytes.len() == 1 && self.bytes[0].offset == 0 {
            return self.bytes.pop().expect("single byte array").data;
        }

        let mut buffer = vec![0; self.length as usize];
        for item in self.strings {
            let offset = item.offset as usize;
            buffer[offset..offset + item.data.len()].copy_from_slice(item.data.as_bytes());
        }
        for item in self.bytes {
            let offset = item.offset as usize;
            buffer[offset..offset + item.data.len()].copy_from_slice(&item.data);
        }
        buffer
    }

    #[must_use]
    pub fn contains(&self, string: &str, bytes: &[u8]) -> bool {
        self.strings.iter().any(|item| item.data.contains(string))
            || self
                .bytes
                .iter()
                .any(|item| contains_bytes(&item.data, bytes))
    }
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    needle.is_empty()
        || haystack
            .windows(needle.len())
            .any(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use super::Joiner;

    #[test]
    fn joins_in_insertion_order_across_storage_lists() {
        let mut joiner = Joiner::default();
        joiner.add_string("a");
        joiner.add_bytes(b"b".to_vec());
        joiner.add_string("c");
        assert_eq!(joiner.len(), 3);
        assert_eq!(joiner.last_byte(), b'c');
        assert!(joiner.contains("a", b"b"));
        assert_eq!(joiner.done(), b"abc");
    }

    #[test]
    fn appends_newline_only_when_needed() {
        let mut joiner = Joiner::default();
        joiner.ensure_newline_at_end();
        assert!(joiner.is_empty());
        joiner.add_string("x");
        joiner.ensure_newline_at_end();
        joiner.ensure_newline_at_end();
        assert_eq!(joiner.done(), b"x\n");
    }

    #[test]
    fn contains_empty_byte_slice_like_bytes_contains() {
        let mut joiner = Joiner::default();
        joiner.add_bytes(vec![1, 2, 3]);
        assert!(joiner.contains("not present", b""));
    }
}
