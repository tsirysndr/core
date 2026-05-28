use bytes::Bytes;
use jacquard_common::DefaultStr;
use jacquard_common::types::string::{AtUri, Cid};

#[derive(Clone, Debug)]
pub struct RecordBody {
    pub uri: AtUri<DefaultStr>,
    pub cid: Cid<DefaultStr>,
    pub value: Bytes,
}

impl RecordBody {
    const FIXED_OVERHEAD: u64 = 128;

    pub fn weight(&self) -> u64 {
        Self::FIXED_OVERHEAD
            + self.value.len() as u64
            + self.uri.as_ref().len() as u64
            + self.cid.as_ref().len() as u64
    }
}
