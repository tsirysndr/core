use std::io;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

// 3 kinds of u64 that denote "bytes" in their own way
// & look identical at a callsite.
// `reserve(path, floor)` *used to* compile perfectly happily.
knot_types::scalar_newtype! {
    pub struct DiskFloorBytes(u64);
    pub struct ReserveBytes(u64);
    pub struct FreeBytes(u64);
}

pub fn free_bytes(path: &Path) -> io::Result<FreeBytes> {
    rustix::fs::statvfs(path)
        .map(|stat| FreeBytes::new(stat.f_bavail.saturating_mul(stat.f_frsize)))
        .map_err(io::Error::from)
}

#[derive(Debug)]
pub enum ReserveError {
    BelowFloor {
        free: FreeBytes,
        floor: DiskFloorBytes,
    },
    Probe(io::Error),
}

struct Ledger {
    floor: DiskFloorBytes,
    reserved: AtomicU64,
}

#[derive(Clone)]
pub struct DiskGovernor(Arc<Ledger>);

impl DiskGovernor {
    pub fn new(floor: DiskFloorBytes) -> Self {
        Self(Arc::new(Ledger {
            floor,
            reserved: AtomicU64::new(0),
        }))
    }

    pub fn reserved_bytes(&self) -> u64 {
        self.0.reserved.load(Ordering::SeqCst)
    }

    pub fn reserve(
        &self,
        path: &Path,
        bytes: ReserveBytes,
    ) -> Result<DiskReservation, ReserveError> {
        let amount = bytes.get();
        let projected = self.0.reserved.fetch_add(amount, Ordering::SeqCst) + amount;
        let free = match free_bytes(path) {
            Ok(free) => free,
            Err(source) => {
                self.0.reserved.fetch_sub(amount, Ordering::SeqCst);
                return Err(ReserveError::Probe(source));
            }
        };
        if free.get() < self.0.floor.get().saturating_add(projected) {
            self.0.reserved.fetch_sub(amount, Ordering::SeqCst);
            return Err(ReserveError::BelowFloor {
                free,
                floor: self.0.floor,
            });
        }
        Ok(DiskReservation {
            ledger: Arc::clone(&self.0),
            bytes: amount,
        })
    }
}

pub struct DiskReservation {
    ledger: Arc<Ledger>,
    bytes: u64,
}

impl Drop for DiskReservation {
    fn drop(&mut self) {
        self.ledger.reserved.fetch_sub(self.bytes, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_real_filesystem_reports_headroom() {
        let dir = std::env::temp_dir();
        assert!(free_bytes(&dir).unwrap().get() > 0);
    }

    #[test]
    fn a_missing_path_reports_the_fault() {
        assert!(free_bytes(Path::new("/definitely/not/a/mounted/path")).is_err());
    }

    #[test]
    fn a_reservation_holds_bytes_until_it_drops() {
        let dir = std::env::temp_dir();
        let governor = DiskGovernor::new(DiskFloorBytes::new(0));
        assert_eq!(governor.reserved_bytes(), 0);
        {
            let _held = governor.reserve(&dir, ReserveBytes::new(4_096)).unwrap();
            assert_eq!(governor.reserved_bytes(), 4_096);
            let _also = governor.reserve(&dir, ReserveBytes::new(1_024)).unwrap();
            assert_eq!(governor.reserved_bytes(), 5_120);
        }
        assert_eq!(governor.reserved_bytes(), 0);
    }

    #[test]
    fn concurrent_reservations_cannot_jointly_punch_through_the_floor() {
        let dir = std::env::temp_dir();
        let free = free_bytes(&dir).unwrap();
        let floor = DiskFloorBytes::new(free.get().saturating_sub(6_144));
        let governor = DiskGovernor::new(floor);
        let first = governor.reserve(&dir, ReserveBytes::new(4_096)).unwrap();
        let denied = governor.reserve(&dir, ReserveBytes::new(4_096));
        assert!(
            matches!(denied, Err(ReserveError::BelowFloor { .. })),
            "the second reservation must see the first still held"
        );
        assert_eq!(governor.reserved_bytes(), 4_096);
        drop(first);
        assert_eq!(governor.reserved_bytes(), 0);
        assert!(governor.reserve(&dir, ReserveBytes::new(4_096)).is_ok());
    }

    #[test]
    fn a_probe_fault_leaves_the_ledger_untouched() {
        let governor = DiskGovernor::new(DiskFloorBytes::new(0));
        let fault = governor.reserve(
            Path::new("/definitely/not/a/mounted/path"),
            ReserveBytes::new(4_096),
        );
        assert!(matches!(fault, Err(ReserveError::Probe(_))));
        assert_eq!(governor.reserved_bytes(), 0);
    }
}
