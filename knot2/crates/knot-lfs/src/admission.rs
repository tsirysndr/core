use knot_resource::{DiskGovernor, DiskReservation, ReserveError};

use crate::{ClaimedSize, FreeSpaceFloor, LfsError, LfsSize, LfsStorePath};

pub trait UploadAdmission: Send + Sync {
    fn admit(&self, declared: ClaimedSize) -> Result<UploadPermit, LfsError>;

    fn max_object(&self) -> LfsSize;
}

pub struct UploadPermit {
    _reservation: Option<DiskReservation>,
}

impl UploadPermit {
    fn unreserved() -> Self {
        Self { _reservation: None }
    }
}

pub struct StoreAdmission {
    root: LfsStorePath,
    max_object: LfsSize,
    floor: FreeSpaceFloor,
    governor: DiskGovernor,
}

impl StoreAdmission {
    pub fn new(root: LfsStorePath, max_object: LfsSize, floor: FreeSpaceFloor) -> Self {
        let governor = DiskGovernor::new(knot_resource::DiskFloorBytes::new(floor.get()));
        Self {
            root,
            max_object,
            floor,
            governor,
        }
    }
}

impl UploadAdmission for StoreAdmission {
    fn admit(&self, declared: ClaimedSize) -> Result<UploadPermit, LfsError> {
        if declared.get() > self.max_object.get() {
            tracing::warn!(
                declared = declared.get(),
                limit = self.max_object.get(),
                "lfs upload denied by the object size limit"
            );
            return Err(LfsError::SizeLimitExceeded {
                declared,
                limit: self.max_object,
            });
        }
        if self.floor.get() == 0 {
            return Ok(UploadPermit::unreserved());
        }
        match self.governor.reserve(
            self.root.as_path(),
            knot_resource::ReserveBytes::new(declared.get()),
        ) {
            Ok(reservation) => Ok(UploadPermit {
                _reservation: Some(reservation),
            }),
            Err(ReserveError::BelowFloor { free, .. }) => {
                tracing::warn!(
                    declared = declared.get(),
                    free = free.get(),
                    floor = self.floor.get(),
                    "lfs upload denied below the free-space floor"
                );
                Err(LfsError::FreeSpaceDenied {
                    free: LfsSize::new(free.get()),
                    floor: self.floor,
                })
            }
            Err(ReserveError::Probe(source)) => Err(LfsError::Io {
                op: "probe free space under",
                path: self.root.as_path().to_path_buf(),
                source,
            }),
        }
    }

    fn max_object(&self) -> LfsSize {
        self.max_object
    }
}

#[cfg(test)]
pub(crate) struct Unbounded;

#[cfg(test)]
impl UploadAdmission for Unbounded {
    fn admit(&self, _declared: ClaimedSize) -> Result<UploadPermit, LfsError> {
        Ok(UploadPermit::unreserved())
    }

    fn max_object(&self) -> LfsSize {
        LfsSize::new(u64::MAX)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_size_limit_rejects_before_touching_the_disk() {
        let gate = StoreAdmission::new(
            LfsStorePath::new("/definitely/not/mounted"),
            LfsSize::new(8),
            FreeSpaceFloor::new(0),
        );
        assert!(matches!(
            gate.admit(ClaimedSize::new(9)),
            Err(LfsError::SizeLimitExceeded { .. })
        ));
        assert!(gate.admit(ClaimedSize::new(8)).is_ok());
    }

    #[test]
    fn an_absurd_floor_denies_and_a_zero_floor_opts_out() {
        let dir = tempfile::tempdir().unwrap();
        let strict = StoreAdmission::new(
            LfsStorePath::new(dir.path()),
            LfsSize::new(u64::MAX),
            FreeSpaceFloor::new(u64::MAX),
        );
        assert!(matches!(
            strict.admit(ClaimedSize::new(1)),
            Err(LfsError::FreeSpaceDenied { .. })
        ));
        let opted_out = StoreAdmission::new(
            LfsStorePath::new(dir.path()),
            LfsSize::new(u64::MAX),
            FreeSpaceFloor::new(0),
        );
        assert!(opted_out.admit(ClaimedSize::new(u64::MAX)).is_ok());
    }

    #[test]
    fn a_held_permit_reserves_against_the_next_admission() {
        let dir = tempfile::tempdir().unwrap();
        let free = knot_resource::disk_free_bytes(dir.path()).unwrap();
        let gate = StoreAdmission::new(
            LfsStorePath::new(dir.path()),
            LfsSize::new(u64::MAX),
            FreeSpaceFloor::new(free.get().saturating_sub(6_144)),
        );
        let held = gate.admit(ClaimedSize::new(4_096)).unwrap();
        assert!(
            matches!(
                gate.admit(ClaimedSize::new(4_096)),
                Err(LfsError::FreeSpaceDenied { .. })
            ),
            "a second upload cannot pass the floor while the first is in flight"
        );
        drop(held);
        assert!(gate.admit(ClaimedSize::new(4_096)).is_ok());
    }
}
