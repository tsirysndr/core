use knot_types::Oid;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WantOids(Vec<Oid>);

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HaveOids(Vec<Oid>);

impl WantOids {
    pub fn new(oids: Vec<Oid>) -> Self {
        Self(oids)
    }

    pub fn as_slice(&self) -> &[Oid] {
        &self.0
    }

    pub fn wants(&self) -> knot_git::Wants<'_> {
        knot_git::Wants::new(&self.0)
    }

    pub fn iter(&self) -> std::slice::Iter<'_, Oid> {
        self.0.iter()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl FromIterator<Oid> for WantOids {
    fn from_iter<I: IntoIterator<Item = Oid>>(iter: I) -> Self {
        Self(iter.into_iter().collect())
    }
}

impl HaveOids {
    pub fn new(oids: Vec<Oid>) -> Self {
        Self(oids)
    }

    pub fn as_slice(&self) -> &[Oid] {
        &self.0
    }

    pub fn haves(&self) -> knot_git::Haves<'_> {
        knot_git::Haves::new(&self.0)
    }

    pub fn iter(&self) -> std::slice::Iter<'_, Oid> {
        self.0.iter()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl FromIterator<Oid> for HaveOids {
    fn from_iter<I: IntoIterator<Item = Oid>>(iter: I) -> Self {
        Self(iter.into_iter().collect())
    }
}
