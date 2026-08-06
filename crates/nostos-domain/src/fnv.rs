//! FNV-1a 64-bit (Fowler–Noll–Vo), hand-rolled rather than pulling in a
//! hashing crate for a couple of non-cryptographic checksums. Shared by
//! [`crate::rules`] (rules-file checksum, ADR-0031 D2) and
//! [`crate::sync_epoch`] (composed slot-epoch + rules-checksum, ADR-0031 +
//! ADR-0025 slice 4b). Private: nothing outside `nostos-domain` needs the raw
//! hash, only the checksums built from it.

const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

pub(crate) fn fnv1a_64(bytes: &[u8]) -> u64 {
    let mut hash = FNV_OFFSET_BASIS;
    for &byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}
