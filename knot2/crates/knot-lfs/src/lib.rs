mod admission;
mod batch;
mod error;
mod gc;
mod pointer;
mod scan;
mod store;
mod transfer;
mod types;

pub use admission::{StoreAdmission, UploadAdmission, UploadPermit};
pub use batch::{
    BASIC_TRANSFER, BATCH_MEDIA_TYPE, BatchAction, BatchActions, BatchObject, BatchObjectError,
    BatchOperation, BatchRef, BatchRequest, BatchResponse, BatchResponseObject, HASH_ALGO,
    HashAlgo, MAX_BATCH_OBJECTS, TransferAdapter,
};
pub use error::LfsError;
pub use gc::{GcError, GcReport, collect_repo};
pub use pointer::{LfsPointer, POINTER_MAX_BYTES, parse_pointer};
pub use scan::scan_pointers;
pub use store::{
    DiskStore, LfsHandle, LfsStore, MemoryStore, OrphanSweep, Reclaimed, StoredObject,
};
pub use transfer::{TransferOp, serve_transfer};
pub use types::{ClaimedSize, FreeSpaceFloor, LfsOid, LfsSize, LfsStorePath, ObjectRelPath};

#[doc(hidden)]
pub mod fuzz {
    use knot_types::RepoDid;

    use crate::{
        BatchRequest, FreeSpaceFloor, LfsSize, LfsStore, LfsStorePath, MAX_BATCH_OBJECTS,
        MemoryStore, StoreAdmission, TransferOp, parse_pointer, serve_transfer,
    };

    pub fn transfer(data: &[u8]) {
        let admission = StoreAdmission::new(
            LfsStorePath::new("/"),
            LfsSize::new(u64::MAX),
            FreeSpaceFloor::new(0),
        );
        let repo = RepoDid::new("did:plc:squid").expect("static DID is valid");
        let messages = &knot_messages::default_catalog().lfs;
        [TransferOp::Upload, TransferOp::Download]
            .iter()
            .for_each(|op| {
                let store = MemoryStore::new();
                let _ = serve_transfer(
                    &store,
                    &admission,
                    &repo,
                    *op,
                    messages,
                    data,
                    std::io::sink(),
                );
            });
    }

    pub fn batch(data: &[u8]) {
        let Ok(request) = serde_json::from_slice::<BatchRequest>(data) else {
            return;
        };
        if request.objects.len() > MAX_BATCH_OBJECTS {
            return;
        }
        let repo = RepoDid::new("did:plc:squid").expect("static DID is valid");
        let store = MemoryStore::new();
        request.objects.iter().for_each(|object| {
            let _ = store.probe(&repo, &object.oid);
        });
    }

    pub fn pointer(data: &[u8]) {
        let _ = parse_pointer(data);
    }
}
