use std::fmt;
use std::iter;
use std::mem;
use std::slice;

use byteorder::{ByteOrder, BigEndian, LittleEndian, NativeEndian};

use builder::DFABuilder;
use error::{Error, Result};
use dfa_ref::DFARef;
use minimize::Minimizer;
use state_id::{StateID, dead_id, next_state_id, premultiply_overflow_error};

/// The size of the alphabet in a standard DFA.
///
/// Specifically, this length controls the number of transitions present in
/// each DFA state. However, when the byte class optimization is enabled,
/// then each DFA maps the space of all possible 256 byte values to at most
/// 256 distinct equivalence classes. In this case, the number of distinct
/// equivalence classes corresponds to the internal alphabet of the DFA, in the
/// sense that each DFA state has a number of transitions equal to the number
/// of equivalence classes despite supporting matching on all possible byte
/// values.
pub const ALPHABET_LEN: usize = 256;

/// A heap allocated table-based deterministic finite automaton (DFA).
///
/// A DFA represents the core matching primitive in this crate. That is,
/// logically, all DFAs have a single start state, one or more match states
/// and a transition table that maps the current state and the current byte of
/// input to the next state. A DFA can use this information to implement fast
/// searching. In particular, the use of a DFA generally makes the trade off
/// that match speed is the most valuable characteristic, even if building the
/// regex may take significant time *and* space. As such, the processing of
/// every byte of input is done with a small constant number of operations
/// that does not vary with the pattern, its size or the size of the alphabet.
/// If your needs don't line up with this trade off, then a DFA may not be an
/// adequate solution to your problem.
///
/// A DFA can be built using the default configuration via the
/// [`DFA::new`](struct.DFA.html#method.new) constructor. Otherwise, one can
/// configure various aspects via the [`DFABuilder`](struct.DFABuilder.html).
///
/// A single DFA fundamentally supports the following operations:
///
/// 1. Detection of a match.
/// 2. Location of the end of the first possible match.
/// 3. Location of the end of the leftmost-first match.
///
/// A notable absence from the above list of capabilities is the location of
/// the *start* of a match. In order to provide both the start and end of a
/// match, *two* DFAs are required. This functionality is provided by a
/// [`Regex`](struct.Regex.html), which can be built with its basic
/// constructor, [`Regex::new`](struct.Regex.html#method.new), or with
/// a [`RegexBuilder`](struct.RegexBuilder.html).
///
/// # State size
///
/// A `DFA` has a single type parameter, `S`, which corresponds to the
/// representation used for the DFA's state identifiers as described by the
/// [`StateID`](trait.StateID.html) trait. This type parameter is, by default,
/// set to `usize`. Other valid choices provided by this crate include `u8`,
/// `u16`, `u32` and `u64`. The primary reason for choosing a different state
/// identifier representation than the default is to reduce the amount of
/// memory used by a DFA. Note though, that if the chosen representation cannot
/// accommodate the size of your DFA, then building the DFA will fail and
/// return an error.
///
/// While the reduction in heap memory used by a DFA is one reason for choosing
/// a smaller state identifier representation, another possible reason is for
/// decreasing the serialization size of a DFA, as returned by
/// [`to_bytes_little_endian`](struct.DFA.html#method.to_bytes_little_endian),
/// [`to_bytes_big_endian`](struct.DFA.html#method.to_bytes_big_endian)
/// or
/// [`to_bytes_native_endian`](struct.DFA.html#method.to_bytes_native_endian).
#[derive(Clone)]
pub struct DFA<S = usize> {
    /// The type of DFA. This enum controls how the state transition table
    /// is interpreted. It is never correct to read the transition table
    /// without knowing the DFA's kind.
    kind: DFAKind,
    /// The initial start state ID.
    start: S,
    /// The total number of states in this DFA. Note that a DFA always has at
    /// least one state---the dead state---even the empty DFA. In particular,
    /// the dead state always has ID 0 and is correspondingly always the first
    /// state. The dead state is never a match state.
    state_count: usize,
    /// States in a DFA have a *partial* ordering such that a match state
    /// always precedes any non-match state (except for the special dead
    /// state).
    ///
    /// `max_match` corresponds to the last state that is a match state. This
    /// encoding has two critical benefits. Firstly, we are not required to
    /// store any additional per-state information about whether it is a match
    /// state or not. Secondly, when searching with the DFA, we can do a single
    /// comparison with `max_match` for each byte instead of two comparisons
    /// for each byte (one testing whether it is a match and the other testing
    /// whether we've reached a dead state). Namely, to determine the status
    /// of the next state, we can do this:
    ///
    ///   next_state = transition[cur_state * ALPHABET_LEN + cur_byte]
    ///   if next_state <= max_match:
    ///       // next_state is either dead (no-match) or a match
    ///       return next_state != dead
    max_match: S,
    /// The total number of bytes in this DFA's alphabet. This is always
    /// equivalent to 256, unless the DFA was built with byte classes, in which
    /// case, this is equal to the number of byte classes.
    alphabet_len: usize,
    /// A set of equivalence classes, where a single equivalence class
    /// represents a set of bytes that never discriminate between a match
    /// and a non-match in the DFA. Each equivalence class corresponds to
    /// a single letter in this DFA's alphabet, where the maximum number of
    /// letters is 256 (each possible value of a byte). Consequently, the
    /// number of equivalence classes corresponds to the number of transitions
    /// for each DFA state.
    ///
    /// The only time the number of equivalence classes is fewer than 256 is
    /// if the DFA's kind uses byte classes. If the DFA doesn't use byte
    /// classes, then this vector is empty.
    byte_classes: Vec<u8>,
    /// A contiguous region of memory representing the transition table in
    /// row-major order. The representation is dense. That is, every state has
    /// precisely the same number of transitions. The maximum number of
    /// transitions is 256. If a DFA has been instructed to use byte classes,
    /// then the number of transitions can be much less.
    trans: Vec<S>,
}

impl DFA {
    /// Parse the given regular expression using a default configuration and
    /// return the corresponding DFA.
    ///
    /// The default configuration uses `usize` for state IDs, premultiplies
    /// them and reduces the alphabet size by splitting bytes into equivalence
    /// classes. The DFA is *not* minimized.
    ///
    /// If you want a non-default configuration, then use the
    /// [`DFABuilder`](struct.DFABuilder.html)
    /// to set your own configuration.
    ///
    /// # Example
    ///
    /// ```
    /// use regex_automata::DFA;
    ///
    /// # fn example() -> Result<(), regex_automata::Error> {
    /// let dfa = DFA::new("foo[0-9]+bar")?;
    /// assert_eq!(Some(11), dfa.find(b"foo12345bar"));
    /// # Ok(()) }; example().unwrap()
    /// ```
    pub fn new(pattern: &str) -> Result<DFA> {
        DFABuilder::new().build(pattern)
    }
}

impl<S: StateID> DFA<S> {
    /// Parse the given regular expression using a default configuration and
    /// Create a new empty DFA that never matches any input.
    ///
    /// # Example
    ///
    /// In order to build an empty DFA, callers must provide a type hint
    /// indicating their choice of state identifier representation.
    ///
    /// ```
    /// use regex_automata::DFA;
    ///
    /// # fn example() -> Result<(), regex_automata::Error> {
    /// let dfa: DFA<usize> = DFA::empty();
    /// assert_eq!(None, dfa.find(b""));
    /// assert_eq!(None, dfa.find(b"foo"));
    /// # Ok(()) }; example().unwrap()
    /// ```
    pub fn empty() -> DFA<S> {
        DFA::empty_with_byte_classes(vec![])
    }

    /// Create a new empty DFA with the given set of byte equivalence classes.
    /// An empty DFA never matches any input.
    pub(crate) fn empty_with_byte_classes(byte_classes: Vec<u8>) -> DFA<S> {
        assert!(byte_classes.is_empty() || byte_classes.len() == 256);

        let (kind, alphabet_len) =
            if byte_classes.is_empty() {
                (DFAKind::Basic, ALPHABET_LEN)
            } else {
                (DFAKind::ByteClass, byte_classes[255] as usize + 1)
            };
        let mut dfa = DFA {
            kind: kind,
            start: dead_id(),
            state_count: 0,
            max_match: S::from_usize(1),
            alphabet_len: alphabet_len,
            byte_classes: byte_classes,
            trans: vec![],
        };
        // Every state ID repr must be able to fit at least one state.
        dfa.add_empty_state().unwrap();
        dfa
    }

    /// Returns true if and only if the given bytes match this DFA.
    ///
    /// This routine may short circuit if it knows that scanning future input
    /// will never lead to a different result. In particular, if a DFA enters
    /// a match state or a dead state, then this routine will return `true` or
    /// `false`, respectively, without inspecting any future input.
    ///
    /// # Example
    ///
    /// ```
    /// use regex_automata::DFA;
    ///
    /// # fn example() -> Result<(), regex_automata::Error> {
    /// let dfa = DFA::new("foo[0-9]+bar")?;
    /// assert_eq!(true, dfa.is_match(b"foo12345bar"));
    /// assert_eq!(false, dfa.is_match(b"foobar"));
    /// # Ok(()) }; example().unwrap()
    /// ```
    pub fn is_match(&self, bytes: &[u8]) -> bool {
        self.as_dfa_ref().is_match_inline(bytes)
    }

    /// Returns the first position at which a match is found.
    ///
    /// This routine stops scanning input in precisely the same circumstances
    /// as `is_match`. The key difference is that this routine returns the
    /// position at which it stopped scanning input if and only if a match
    /// was found. If no match is found, then `None` is returned.
    ///
    /// # Example
    ///
    /// ```
    /// use regex_automata::DFA;
    ///
    /// # fn example() -> Result<(), regex_automata::Error> {
    /// let dfa = DFA::new("foo[0-9]+")?;
    /// assert_eq!(Some(4), dfa.shortest_match(b"foo12345"));
    ///
    /// // Normally, the end of the leftmost first match here would be 3,
    /// // but the shortest match semantics detect a match earlier.
    /// let dfa = DFA::new("abc|a")?;
    /// assert_eq!(Some(1), dfa.shortest_match(b"abc"));
    /// # Ok(()) }; example().unwrap()
    /// ```
    pub fn shortest_match(&self, bytes: &[u8]) -> Option<usize> {
        self.as_dfa_ref().shortest_match_inline(bytes)
    }

    /// Returns the end offset of the leftmost first match. If no match exists,
    /// then `None` is returned.
    ///
    /// The "leftmost first" match corresponds to the match with the smallest
    /// starting offset, but where the end offset is determined by preferring
    /// earlier branches in the original regular expression. For example,
    /// `Sam|Samwise` will match `Sam` in `Samwise`, but `Samwise|Sam` will
    /// match `Samwise` in `Samwise`.
    ///
    /// Generally speaking, the "leftmost first" match is how most backtracking
    /// regular expressions tend to work. This is in contrast to POSIX-style
    /// regular expressions that yield "leftmost longest" matches. Namely,
    /// both `Sam|Samwise` and `Samwise|Sam` match `Samwise` when using
    /// leftmost longest semantics.
    ///
    /// # Example
    ///
    /// ```
    /// use regex_automata::DFA;
    ///
    /// # fn example() -> Result<(), regex_automata::Error> {
    /// let dfa = DFA::new("foo[0-9]+")?;
    /// assert_eq!(Some(8), dfa.find(b"foo12345"));
    ///
    /// // Even though a match is found after reading the first byte (`a`),
    /// // the leftmost first match semantics demand that we find the earliest
    /// // match that prefers earlier parts of the pattern over latter parts.
    /// let dfa = DFA::new("abc|a")?;
    /// assert_eq!(Some(3), dfa.find(b"abc"));
    /// # Ok(()) }; example().unwrap()
    /// ```
    pub fn find(&self, bytes: &[u8]) -> Option<usize> {
        self.as_dfa_ref().find_inline(bytes)
    }

    /// Returns the start offset of the leftmost first match in reverse, by
    /// searching from the end of the input towards the start of the input. If
    /// no match exists, then `None` is returned.
    ///
    /// This routine is principally useful when used in conjunction with the
    /// [`DFABuilder::reverse`](struct.DFABuilder.html#method.reverse)
    /// configuration knob. In general, it's unlikely to be correct to use both
    /// `find` and `rfind` with the same DFA.
    ///
    /// # Example
    ///
    /// ```
    /// use regex_automata::DFABuilder;
    ///
    /// # fn example() -> Result<(), regex_automata::Error> {
    /// let dfa = DFABuilder::new().reverse(true).build("foo[0-9]+")?;
    /// assert_eq!(Some(0), dfa.rfind(b"foo12345"));
    /// # Ok(()) }; example().unwrap()
    /// ```
    pub fn rfind(&self, bytes: &[u8]) -> Option<usize> {
        self.as_dfa_ref().rfind_inline(bytes)
    }

    /// Return a borrowed version of this DFA.
    ///
    /// This is useful if your code demands a borrowed version of the DFA.
    /// In particular, a `DFARef` does not specifically require any heap
    /// memory and can be used without Rust's standard library.
    pub fn as_dfa_ref<'a>(&'a self) -> DFARef<'a, S> {
        DFARef {
            kind: self.kind,
            start: self.start,
            state_count: self.state_count,
            max_match: self.max_match,
            alphabet_len: self.alphabet_len,
            byte_classes: &self.byte_classes,
            trans: &self.trans,
        }
    }

    /// Build an owned DFA from a borrowed DFA.
    pub fn from_dfa_ref(dfa_ref: DFARef<S>) -> DFA<S> {
        DFA {
            kind: dfa_ref.kind,
            start: dfa_ref.start,
            state_count: dfa_ref.state_count,
            max_match: dfa_ref.max_match,
            alphabet_len: dfa_ref.alphabet_len,
            byte_classes: dfa_ref.byte_classes.to_vec(),
            trans: dfa_ref.trans.to_vec(),
        }
    }

    /// Returns the memory usage, in bytes, of this DFA.
    ///
    /// The memory usage is computed based on the number of bytes used to
    /// represent this DFA's transition table. This corresponds to heap memory
    /// usage.
    ///
    /// This does **not** include the stack size used up by this DFA. To
    /// compute that, used `std::mem::size_of::<DFA>()`.
    pub fn memory_usage(&self) -> usize {
        self.as_dfa_ref().memory_usage()
    }

    /// Deserialize a DFA with a specific state identifier representation.
    ///
    /// Deserializing a DFA using this routine will allocate new heap memory
    /// for the transition table.
    ///
    /// The bytes given should be generated by the serialization of a DFA with
    /// either the
    /// [`to_bytes_little_endian`](struct.DFA.html#method.to_bytes_little_endian)
    /// method or the
    /// [`to_bytes_big_endian`](struct.DFA.html#method.to_bytes_big_endian)
    /// endian, depending on the endianness of the machine you are
    /// deserializing this DFA from.
    ///
    /// If the state identifier representation is `usize`, then deserialization
    /// is dependent on the pointer size. For this reason, it is best to
    /// serialize DFAs using a fixed size representation for your state
    /// identifiers, such as `u8`, `u16`, `u32` or `u64`.
    ///
    /// If you're loading a DFA from a memory mapped file or static memory,
    /// then you probably want to use
    /// [`DFARef::from_bytes`](struct.DFARef.html#method.from_bytes)
    /// instead. In particular, using `DFARef` will not use any heap memory,
    /// is suitable for `no_std` environments and is a constant time operation.
    ///
    /// # Panics
    ///
    /// The bytes given should be *trusted*. In particular, if the bytes are
    /// not a valid serialization of a DFA, or if the bytes are not aligned to
    /// an 8 byte boundary, or if the endianness of the serialized bytes is
    /// different than the endianness of the machine that is deserializing the
    /// DFA, then this routine will panic.
    ///
    /// # Safety
    ///
    /// This routine is unsafe because it permits callers to provide an
    /// arbitrary transition table with possibly incorrect transitions. While
    /// the various serialization routines will never return an incorrect
    /// transition table, there is no guarantee that the bytes provided here
    /// are correct. While deserialization does many checks (as documented
    /// above in the panic conditions), this routine does not check that the
    /// transition table is correct. Given an incorrect transition table, it is
    /// possible for the search routines to access out-of-bounds memory because
    /// of explicit bounds check elision.
    ///
    /// # Example
    ///
    /// This example shows how to serialize a DFA to raw bytes, deserialize it
    /// and then use it for searching. Note that we first convert the DFA to
    /// using `u16` for its state identifier representation before serializing
    /// it. While this isn't strictly necessary, it's good practice in order to
    /// decrease the size of the DFA and to avoid platform specific pitfalls
    /// such as differing pointer sizes.
    ///
    /// ```
    /// use regex_automata::DFA;
    ///
    /// # fn example() -> Result<(), regex_automata::Error> {
    /// let initial = DFA::new("foo[0-9]+")?;
    /// let bytes = initial.to_u16()?.to_bytes_native_endian()?;
    /// let dfa: DFA<u16> = unsafe { DFA::from_bytes(&bytes) };
    ///
    /// assert_eq!(Some(8), dfa.find(b"foo12345"));
    /// # Ok(()) }; example().unwrap()
    /// ```
    pub unsafe fn from_bytes(buf: &[u8]) -> DFA<S> {
        let dfa_ref = DFARef::from_bytes(buf);
        DFA {
            kind: dfa_ref.kind,
            start: dfa_ref.start,
            state_count: dfa_ref.state_count,
            max_match: dfa_ref.max_match,
            alphabet_len: dfa_ref.alphabet_len,
            byte_classes: dfa_ref.byte_classes.to_vec(),
            trans: dfa_ref.trans.to_vec(),
        }
    }

    /// Serialize a DFA to raw bytes, aligned to an 8 byte boundary, in little
    /// endian format.
    ///
    /// If the state identifier representation of this DFA has a size different
    /// than 1, 2, 4 or 8 bytes, then this returns an error. All
    /// implementations of `StateID` provided by this crate satisfy this
    /// requirement.
    pub fn to_bytes_little_endian(&self) -> Result<Vec<u8>> {
        self.to_bytes::<LittleEndian>()
    }

    /// Serialize a DFA to raw bytes, aligned to an 8 byte boundary, in big
    /// endian format.
    ///
    /// If the state identifier representation of this DFA has a size different
    /// than 1, 2, 4 or 8 bytes, then this returns an error. All
    /// implementations of `StateID` provided by this crate satisfy this
    /// requirement.
    pub fn to_bytes_big_endian(&self) -> Result<Vec<u8>> {
        self.to_bytes::<BigEndian>()
    }

    /// Serialize a DFA to raw bytes, aligned to an 8 byte boundary, in native
    /// endian format. Generally, it is better to pick an explicit endianness
    /// using either `to_bytes_little_endian` or `to_bytes_big_endian`. This
    /// routine is useful in tests where the DFA is serialized and deserialized
    /// on the same platform.
    ///
    /// If the state identifier representation of this DFA has a size different
    /// than 1, 2, 4 or 8 bytes, then this returns an error. All
    /// implementations of `StateID` provided by this crate satisfy this
    /// requirement.
    pub fn to_bytes_native_endian(&self) -> Result<Vec<u8>> {
        self.to_bytes::<NativeEndian>()
    }

    /// Serialize a DFA to raw bytes, aligned to an 8 byte boundary.
    ///
    /// If the state identifier representation of this DFA has a size different
    /// than 1, 2, 4 or 8 bytes, then this returns an error. All
    /// implementations of `StateID` provided by this crate satisfy this
    /// requirement.
    fn to_bytes<T: ByteOrder>(&self) -> Result<Vec<u8>> {
        let label = b"rust-regex-automata-dfa\x00";
        assert_eq!(24, label.len());

        let trans_size = mem::size_of::<S>() * self.trans.len();
        let size =
            // For human readable label.
            label.len()
            // endiannes check, must be equal to 0xFEFF for native endian
            + 2
            // For version number.
            + 2
            // Size of state ID representation, in bytes.
            // Must be 1, 2, 4 or 8.
            + 2
            // For DFA kind.
            + 2
            // For start state.
            + 8
            // For state count.
            + 8
            // For max match state.
            + 8
            // For alphabet length.
            + 8
            // For byte class map.
            + 256
            // For transition table.
            + trans_size;
        // sanity check, this can be updated if need be
        assert_eq!(320 + trans_size, size);
        // This must always pass. It checks that the transition table is at
        // a properly aligned address.
        assert_eq!(0, (size - trans_size) % 8);

        let mut buf = vec![0; size];
        let mut i = 0;

        // write label
        for &b in label {
            buf[i] = b;
            i += 1;
        }
        // endianness check
        T::write_u16(&mut buf[i..], 0xFEFF);
        i += 2;
        // version number
        T::write_u16(&mut buf[i..], 1);
        i += 2;
        // size of state ID
        let state_size = mem::size_of::<S>();
        if ![1, 2, 4, 8].contains(&state_size) {
            return Err(Error::serialize(&format!(
                "state size of {} not supported, must be 1, 2, 4 or 8",
                state_size
            )));
        }
        T::write_u16(&mut buf[i..], state_size as u16);
        i += 2;
        // DFA kind
        T::write_u16(&mut buf[i..], self.kind.to_byte() as u16);
        i += 2;
        // start state
        T::write_u64(&mut buf[i..], self.start.to_usize() as u64);
        i += 8;
        // state count
        T::write_u64(&mut buf[i..], self.state_count as u64);
        i += 8;
        // max match state
        T::write_u64(
            &mut buf[i..],
            self.max_match.to_usize() as u64,
        );
        i += 8;
        // alphabet length
        T::write_u64(&mut buf[i..], self.alphabet_len as u64);
        i += 8;
        // byte class map
        if self.byte_classes.is_empty() {
            for b in (0..256).map(|b| b as u8) {
                buf[i] = b;
                i += 1;
            }
        } else {
            for &b in &self.byte_classes {
                buf[i] = b;
                i += 1;
            }
        }
        // transition table
        for &id in &self.trans {
            if state_size == 1 {
                buf[i] = id.to_usize() as u8;
            } else if state_size == 2 {
                T::write_u16(&mut buf[i..], id.to_usize() as u16);
            } else if state_size == 4 {
                T::write_u32(&mut buf[i..], id.to_usize() as u32);
            } else {
                assert_eq!(8, state_size);
                T::write_u64(&mut buf[i..], id.to_usize() as u64);
            }
            i += state_size;
        }
        assert_eq!(size, i, "expected to consume entire buffer");

        Ok(buf)
    }
}

impl<S: StateID> DFA<S> {
    /// Create a new DFA whose match semantics are equivalent to this DFA,
    /// but attempt to use `u8` for the representation of state identifiers.
    /// If `u8` is insufficient to represent all state identifiers in this
    /// DFA, then this returns an error.
    ///
    /// This is a convenience routine for `to_sized::<u8>()`.
    pub fn to_u8(&self) -> Result<DFA<u8>> {
        self.to_sized()
    }

    /// Create a new DFA whose match semantics are equivalent to this DFA,
    /// but attempt to use `u16` for the representation of state identifiers.
    /// If `u16` is insufficient to represent all state identifiers in this
    /// DFA, then this returns an error.
    ///
    /// This is a convenience routine for `to_sized::<u16>()`.
    pub fn to_u16(&self) -> Result<DFA<u16>> {
        self.to_sized()
    }

    /// Create a new DFA whose match semantics are equivalent to this DFA,
    /// but attempt to use `u32` for the representation of state identifiers.
    /// If `u32` is insufficient to represent all state identifiers in this
    /// DFA, then this returns an error.
    ///
    /// This is a convenience routine for `to_sized::<u32>()`.
    pub fn to_u32(&self) -> Result<DFA<u32>> {
        self.to_sized()
    }

    /// Create a new DFA whose match semantics are equivalent to this DFA,
    /// but attempt to use `u64` for the representation of state identifiers.
    /// If `u64` is insufficient to represent all state identifiers in this
    /// DFA, then this returns an error.
    ///
    /// This is a convenience routine for `to_sized::<u64>()`.
    pub fn to_u64(&self) -> Result<DFA<u64>> {
        self.to_sized()
    }

    /// Create a new DFA whose match semantics are equivalent to this DFA, but
    /// attempt to use `T` for the representation of state identifiers. If `T`
    /// is insufficient to represent all state identifiers in this DFA, then
    /// this returns an error.
    ///
    /// An alternative way to construct such a DFA is to use
    /// [`DFABuilder::build_with_size`](struct.DFABuilder.html#method.build_with_size).
    /// In general, using the builder is preferred since it will use the given
    /// state identifier representation throughout determinization (and
    /// minimization, if done), and thereby using less memory throughout the
    /// entire construction process. However, these routines are necessary
    /// in cases where, say, a minimized DFA could fit in a smaller state
    /// identifier representation, but the initial determinized DFA would not.
    pub fn to_sized<T: StateID>(&self) -> Result<DFA<T>> {
        // Check that this DFA can fit into T's representation.
        let mut last_state_id = self.state_count - 1;
        if self.kind.is_premultiplied() {
            last_state_id *= self.alphabet_len();
        }
        if last_state_id > T::max_id() {
            return Err(Error::state_id_overflow(T::max_id()));
        }

        // We're off to the races. The new DFA is the same as the old one,
        // but its transition table is truncated.
        let mut new = DFA {
            kind: self.kind,
            start: T::from_usize(self.start.to_usize()),
            state_count: self.state_count,
            max_match: T::from_usize(self.max_match.to_usize()),
            alphabet_len: self.alphabet_len,
            byte_classes: self.byte_classes.clone(),
            trans: vec![dead_id::<T>(); self.trans.len()],
        };
        for (i, id) in new.trans.iter_mut().enumerate() {
            *id = T::from_usize(self.trans[i].to_usize());
        }
        Ok(new)
    }
}

impl<S: StateID> DFA<S> {
    pub(crate) fn state_id_to_offset(&self, id: S) -> usize {
        if self.kind.is_premultiplied() {
            id.to_usize()
        } else {
            id.to_usize() * self.alphabet_len()
        }
    }

    pub(crate) fn byte_to_class(&self, b: u8) -> u8 {
        if self.kind.is_byte_class() {
            self.byte_classes[b as usize]
        } else {
            b
        }
    }

    pub(crate) fn equiv_bytes(&self) -> Vec<u8> {
        if !self.kind.is_byte_class() {
            return (0..ALPHABET_LEN).map(|b| b as u8).collect();
        }

        let mut equivs = vec![];
        let mut last_equiv = None;
        for b in 0usize..256 {
            let equiv = self.byte_classes[b];
            if last_equiv != Some(equiv) {
                equivs.push(b as u8);
                last_equiv = Some(equiv);
            }
        }
        equivs
    }

    pub(crate) fn len(&self) -> usize {
        self.state_count
    }

    pub(crate) fn alphabet_len(&self) -> usize {
        self.alphabet_len
    }

    pub(crate) fn start(&self) -> S {
        self.start
    }

    pub(crate) fn kind(&self) -> &DFAKind {
        &self.kind
    }

    pub(crate) fn is_match_state(&self, id: S) -> bool {
        id <= self.max_match && id != dead_id()
    }

    pub(crate) fn max_match_state(&self) -> S {
        self.max_match
    }

    pub(crate) fn set_start_state(&mut self, start: S) {
        assert!(start.to_usize() < self.len());
        self.start = start;
    }

    pub(crate) fn set_transition(
        &mut self,
        from: S,
        input: u8,
        to: S,
    ) {
        let input = self.byte_to_class(input);
        let i = self.state_id_to_offset(from) + input as usize;
        self.trans[i] = to;
    }

    pub(crate) fn add_empty_state(&mut self) -> Result<S> {
        let id =
            if self.state_count == 0 {
                S::from_usize(0)
            } else {
                next_state_id(S::from_usize(self.state_count - 1))?
            };
        let alphabet_len = self.alphabet_len();
        self.trans.extend(iter::repeat(dead_id::<S>()).take(alphabet_len));
        // This should never panic, since state_count is a usize. The
        // transition table size would have run out of room long ago.
        self.state_count = self.state_count.checked_add(1).unwrap();
        Ok(id)
    }

    pub(crate) fn get_state_mut(&mut self, id: S) -> StateMut<S> {
        let i = self.state_id_to_offset(id);
        let alphabet_len = self.alphabet_len();
        StateMut {
            transitions: &mut self.trans[i..i+alphabet_len],
        }
    }

    pub(crate) fn set_max_match_state(&mut self, id: S) {
        self.max_match = id;
    }

    pub(crate) fn iter(&self) -> StateIter<S> {
        let it = self.trans.chunks(self.alphabet_len());
        StateIter { dfa: self, it: it.enumerate() }
    }

    pub(crate) fn swap_states(&mut self, id1: S, id2: S) {
        let o1 = self.state_id_to_offset(id1);
        let o2 = self.state_id_to_offset(id2);
        for b in 0..self.alphabet_len() {
            self.trans.swap(o1 + b, o2 + b);
        }
    }

    pub(crate) fn truncate_states(&mut self, count: usize) {
        let alphabet_len = self.alphabet_len();
        self.trans.truncate(count * alphabet_len);
        self.state_count = count;
    }

    pub(crate) fn shuffle_match_states(&mut self, is_match: &[bool]) {
        assert!(
            !self.kind.is_premultiplied(),
            "cannot finish construction of premultiplied DFA"
        );

        if self.len() <= 2 {
            return;
        }

        let mut first_non_match = 1;
        while first_non_match < self.len() && is_match[first_non_match] {
            first_non_match += 1;
        }

        let mut swaps: Vec<S> = vec![dead_id(); self.len()];
        let mut cur = self.len() - 1;
        while cur > first_non_match {
            if is_match[cur] {
                self.swap_states(
                    S::from_usize(cur),
                    S::from_usize(first_non_match),
                );
                swaps[cur] = S::from_usize(first_non_match);
                swaps[first_non_match] = S::from_usize(cur);

                first_non_match += 1;
                while first_non_match < cur && is_match[first_non_match] {
                    first_non_match += 1;
                }
            }
            cur -= 1;
        }
        for id in (0..self.len()).map(S::from_usize) {
            for (_, next) in self.get_state_mut(id).iter_mut() {
                if swaps[next.to_usize()] != dead_id() {
                    *next = swaps[next.to_usize()];
                }
            }
        }
        if swaps[self.start.to_usize()] != dead_id() {
            self.start = swaps[self.start.to_usize()];
        }
        self.max_match = S::from_usize(first_non_match - 1);
    }

    pub(crate) fn minimize(&mut self) {
        assert!(!self.kind.is_premultiplied());
        Minimizer::new(self).run();
    }

    pub(crate) fn premultiply(&mut self) -> Result<()> {
        if self.kind.is_premultiplied() || self.len() == 0 {
            return Ok(());
        }

        let alpha_len = self.alphabet_len();
        premultiply_overflow_error(S::from_usize(self.len() - 1), alpha_len)?;

        for id in (0..self.len()).map(S::from_usize) {
            for (_, next) in self.get_state_mut(id).iter_mut() {
                *next = S::from_usize(next.to_usize() * alpha_len);
            }
        }
        self.kind = self.kind.premultiplied();
        self.start = S::from_usize(self.start.to_usize() * alpha_len);
        self.max_match = S::from_usize(self.max_match.to_usize() * alpha_len);
        Ok(())
    }
}

#[derive(Debug)]
pub struct StateIter<'a, S: StateID> {
    dfa: &'a DFA<S>,
    it: iter::Enumerate<slice::Chunks<'a, S>>,
}

impl<'a, S: StateID> Iterator for StateIter<'a, S> {
    type Item = (S, State<'a, S>);

    fn next(&mut self) -> Option<(S, State<'a, S>)> {
        self.it.next().map(|(id, chunk)| {
            let state = State { transitions: chunk };
            let id =
                if self.dfa.kind().is_premultiplied() {
                    id * self.dfa.alphabet_len()
                } else {
                    id
                };
            (S::from_usize(id), state)
        })
    }
}

pub struct State<'a, S> {
    transitions: &'a [S],
}

impl<'a, S: StateID> State<'a, S> {
    pub fn iter(&self) -> StateTransitionIter<S> {
        StateTransitionIter { it: self.transitions.iter().enumerate() }
    }

    pub(crate) fn sparse_transitions(&self) -> Vec<(u8, u8, S)> {
        let mut ranges = vec![];
        let mut cur = None;
        for (i, &next_id) in self.transitions.iter().enumerate() {
            let b = i as u8;
            let (prev_start, prev_end, prev_next) = match cur {
                Some(range) => range,
                None => {
                    cur = Some((b, b, next_id));
                    continue;
                }
            };
            if prev_next == next_id {
                cur = Some((prev_start, b, prev_next));
            } else {
                ranges.push((prev_start, prev_end, prev_next));
                cur = Some((b, b, next_id));
            }
        }
        ranges.push(cur.unwrap());
        ranges
    }
}

#[derive(Debug)]
pub struct StateTransitionIter<'a, S> {
    it: iter::Enumerate<slice::Iter<'a, S>>,
}

impl<'a, S: StateID> Iterator for StateTransitionIter<'a, S> {
    type Item = (u8, S);

    fn next(&mut self) -> Option<(u8, S)> {
        self.it.next().map(|(i, &id)| (i as u8, id))
    }
}

pub struct StateMut<'a, S> {
    transitions: &'a mut [S],
}

impl<'a, S: StateID> StateMut<'a, S> {
    pub fn iter_mut(&mut self) -> StateTransitionIterMut<S> {
        StateTransitionIterMut { it: self.transitions.iter_mut().enumerate() }
    }
}

#[derive(Debug)]
pub struct StateTransitionIterMut<'a, S> {
    it: iter::Enumerate<slice::IterMut<'a, S>>,
}

impl<'a, S: StateID> Iterator for StateTransitionIterMut<'a, S> {
    type Item = (u8, &'a mut S);

    fn next(&mut self) -> Option<(u8, &'a mut S)> {
        self.it.next().map(|(i, id)| (i as u8, id))
    }
}

#[derive(Clone, Copy, Debug)]
pub enum DFAKind {
    Basic,
    Premultiplied,
    ByteClass,
    PremultipliedByteClass,
}

impl DFAKind {
    pub fn is_byte_class(&self) -> bool {
        match *self {
            DFAKind::Basic | DFAKind::Premultiplied => false,
            DFAKind::ByteClass | DFAKind::PremultipliedByteClass => true,
        }
    }

    pub fn is_premultiplied(&self) -> bool {
        match *self {
            DFAKind::Basic | DFAKind::ByteClass => false,
            DFAKind::Premultiplied | DFAKind::PremultipliedByteClass => true,
        }
    }

    fn premultiplied(self) -> DFAKind {
        match self {
            DFAKind::Basic => DFAKind::Premultiplied,
            DFAKind::ByteClass => DFAKind::PremultipliedByteClass,
            DFAKind::Premultiplied | DFAKind::PremultipliedByteClass => {
                panic!("DFA already has pre-multiplied state IDs")
            }
        }
    }

    fn to_byte(&self) -> u8 {
        match *self {
            DFAKind::Basic => 0,
            DFAKind::Premultiplied => 1,
            DFAKind::ByteClass => 2,
            DFAKind::PremultipliedByteClass => 3,
        }
    }

    pub(crate) fn from_byte(b: u8) -> DFAKind {
        match b {
            0 => DFAKind::Basic,
            1 => DFAKind::Premultiplied,
            2 => DFAKind::ByteClass,
            3 => DFAKind::PremultipliedByteClass,
            _ => panic!("invalid DFA kind: 0x{:X}", b),
        }
    }
}

impl<S: StateID> fmt::Debug for DFA<S> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        fn state_status<S: StateID>(
            dfa: &DFA<S>,
            id: S,
        ) -> String {
            let mut status = vec![b' ', b' '];
            if id == dead_id() {
                status[0] = b'D';
            } else if id == dfa.start {
                status[0] = b'>';
            }
            if dfa.is_match_state(id) {
                status[1] = b'*';
            }
            String::from_utf8(status).unwrap()
        }

        for (id, state) in self.iter() {
            let status = state_status(self, id);
            writeln!(f, "{}{:04}: {:?}", status, id.to_usize(), state)?;
        }
        Ok(())
    }
}

impl<'a, S: StateID> fmt::Debug for State<'a, S> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let mut transitions = vec![];
        for (start, end, next_id) in self.sparse_transitions() {
            if next_id == dead_id() {
                continue;
            }
            let line =
                if start == end {
                    format!("{} => {}", escape(start), next_id.to_usize())
                } else {
                    format!(
                        "{}-{} => {}",
                        escape(start), escape(end), next_id.to_usize(),
                    )
                };
            transitions.push(line);
        }
        write!(f, "{}", transitions.join(", "))?;
        Ok(())
    }
}

/// Return the given byte as its escaped string form.
fn escape(b: u8) -> String {
    use std::ascii;

    String::from_utf8(ascii::escape_default(b).collect::<Vec<_>>()).unwrap()
}

#[cfg(test)]
mod tests {
    use builder::DFABuilder;
    use nfa::NFA;
    use super::*;

    #[test]
    fn errors_when_converting_to_smaller_dfa() {
        let pattern = r"\w";
        let dfa = DFABuilder::new()
            .byte_classes(false)
            .anchored(true)
            .premultiply(false)
            .build_with_size::<u16>(pattern)
            .unwrap();
        assert!(dfa.to_u8().is_err());
    }

    #[test]
    fn errors_when_determinization_would_overflow() {
        let pattern = r"\w";

        let mut builder = DFABuilder::new();
        builder.byte_classes(false).anchored(true).premultiply(false);
        // using u16 is fine
        assert!(builder.build_with_size::<u16>(pattern).is_ok());
        // // ... but u8 results in overflow (because there are >256 states)
        assert!(builder.build_with_size::<u8>(pattern).is_err());
    }

    #[test]
    fn errors_when_premultiply_would_overflow() {
        let pattern = r"[a-z]";

        let mut builder = DFABuilder::new();
        builder.byte_classes(false).anchored(true).premultiply(false);
        // without premultiplication is OK
        assert!(builder.build_with_size::<u8>(pattern).is_ok());
        // ... but with premultiplication overflows u8
        builder.premultiply(true);
        assert!(builder.build_with_size::<u8>(pattern).is_err());
    }

    fn print_automata(pattern: &str) {
        println!("BUILDING AUTOMATA");
        let (_, dfa, mdfa) = build_automata(pattern);

        println!("{}", "#".repeat(100));
        // println!("PATTERN: {:?}", pattern);
        // println!("NFA:");
        // for (i, state) in nfa.states.borrow().iter().enumerate() {
            // println!("{:03X}: {:X?}", i, state);
        // }

        println!("{}", "~".repeat(79));

        println!("DFA:");
        print!("{:?}", dfa);
        println!("{}", "~".repeat(79));

        println!("Minimal DFA:");
        print!("{:?}", mdfa);
        println!("{}", "~".repeat(79));

        println!("{}", "#".repeat(100));
    }

    fn print_automata_counts(pattern: &str) {
        let (nfa, dfa, mdfa) = build_automata(pattern);
        println!("nfa # states: {:?}", nfa.len());
        println!("dfa # states: {:?}", dfa.len());
        println!("minimal dfa # states: {:?}", mdfa.len());
    }

    fn build_automata(pattern: &str) -> (NFA, DFA, DFA) {
        let mut builder = DFABuilder::new();
        builder.anchored(true).allow_invalid_utf8(true).byte_classes(false).premultiply(false);
        let nfa = builder.build_nfa(pattern).unwrap();
        let dfa = builder.build(pattern).unwrap();
        let min = builder.minimize(true).build(pattern).unwrap();
        (nfa, dfa, min)
    }

    #[test]
    fn scratch() {
        // let data = ::std::fs::read_to_string("/usr/share/dict/words").unwrap();
        // let mut words: Vec<&str> = data.lines().collect();
        // println!("{} words", words.len());
        // words.sort_by(|w1, w2| w1.len().cmp(&w2.len()).reverse());
        // let pattern = words.join("|");
        // print_automata_counts(&pattern);
        // print_automata(&pattern);

        // print_automata(r"[01]*1[01]{5}");
        // print_automata(r"X(.?){0,8}Y");
        // print_automata_counts(r"\p{alphabetic}");
        // print_automata(r"a*b+|cdefg");
        // print_automata(r"(..)*(...)*");
        print_automata(r"(?-u:\w)");
        print_automata_counts(r"(?-u:\w)");
        // print_automata_counts(r"(?-u:\w)");
    }
}
