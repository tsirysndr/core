knot_types::scalar_newtype! {
    pub(crate) struct Crc32(u32);
    pub struct MaxWireBytes(usize);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub(crate) struct PackOffset(u64);

impl PackOffset {
    pub(crate) fn new(value: u64) -> Self {
        Self(value)
    }

    pub(crate) fn get(self) -> u64 {
        self.0
    }

    // Hostile packs will ask to go back past the start of a file,
    // so just making sure.
    pub(crate) fn checked_sub_distance(self, distance: u64) -> Option<Self> {
        self.0.checked_sub(distance).map(Self)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DeltaDepth(usize);

impl DeltaDepth {
    pub(crate) const ZERO: Self = Self(0);

    pub const fn new(depth: usize) -> Self {
        Self(depth)
    }

    pub(crate) const fn get(self) -> usize {
        self.0
    }

    pub(crate) fn deeper(self) -> Self {
        Self(self.0 + 1)
    }

    pub(crate) fn exceeds(self, max: DeltaDepth) -> bool {
        self.0 > max.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct MaxObjectBytes(u64);

impl MaxObjectBytes {
    pub const fn new(bytes: u64) -> Self {
        Self(bytes)
    }

    pub const fn get(self) -> u64 {
        self.0
    }

    pub(crate) const fn exceeded_by(self, size: u64) -> bool {
        size > self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct MaxTotalBytes(u64);

impl MaxTotalBytes {
    pub const fn new(bytes: u64) -> Self {
        Self(bytes)
    }

    pub const fn get(self) -> u64 {
        self.0
    }

    pub(crate) const fn exceeded_by(self, size: u64) -> bool {
        size > self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Rounds(usize);

impl Rounds {
    pub(crate) fn new(rounds: usize) -> Self {
        Self(rounds)
    }

    // Ensuring chains deeper than the limit return `None` instead of
    // wrapping and doing like 18 quintillion more passes.
    pub(crate) fn next(self) -> Option<Self> {
        self.0.checked_sub(1).map(Self)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct RefsDigest([u8; 32]);

impl RefsDigest {
    pub(crate) fn new(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub(crate) fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}
