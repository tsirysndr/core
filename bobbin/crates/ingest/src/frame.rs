use jacquard_common::DefaultStr;
use jacquard_common::types::did::Did;
use jacquard_common::types::nsid::Nsid;
use jacquard_common::types::recordkey::Rkey;
use jacquard_common::types::string::Cid;
use jacquard_common::types::tid::Tid;
use serde::Deserialize;
use serde_json::value::RawValue;

#[derive(Debug, Deserialize)]
pub struct HydrantFrame {
    pub id: u64,
    #[serde(rename = "type")]
    pub kind: FrameKind,
    #[serde(default)]
    pub record: Option<RecordFrame>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FrameKind {
    Record,
    Identity,
    Account,
    Other,
}

impl<'de> Deserialize<'de> for FrameKind {
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = <std::borrow::Cow<'de, str>>::deserialize(d)?;
        Ok(match s.as_ref() {
            "record" => Self::Record,
            "identity" => Self::Identity,
            "account" => Self::Account,
            _ => Self::Other,
        })
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct HydrantStreamErrorFrame {
    pub error: String,
    #[serde(default)]
    pub message: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct RecordFrame {
    pub live: bool,
    pub did: Did<DefaultStr>,
    pub rev: Tid,
    pub collection: Nsid<DefaultStr>,
    pub rkey: Rkey<DefaultStr>,
    pub action: RecordAction,
    #[serde(default)]
    pub record: Option<Box<RawValue>>,
    #[serde(default)]
    pub cid: Option<Cid<DefaultStr>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecordAction {
    Create,
    Update,
    Delete,
    Other,
}

impl<'de> Deserialize<'de> for RecordAction {
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = <std::borrow::Cow<'de, str>>::deserialize(d)?;
        Ok(match s.as_ref() {
            "create" => Self::Create,
            "update" => Self::Update,
            "delete" => Self::Delete,
            _ => Self::Other,
        })
    }
}
