//! Xbox pipelines supplied as native binaries and serialized device state.

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::sync::OnceLock;

use crate::object::{HRESULT, Iid, This};

static COMPUTE: OnceLock<CachedCompute> = OnceLock::new();

/// Original shader containers indexed by all four serialized parts of an
/// Xbox compute pipeline. The native instruction bytes alone do not identify
/// its resource bindings or device state.
#[derive(Default)]
pub struct CachedCompute {
    shaders: HashMap<blake3::Hash, Option<Vec<u8>>>,
}

impl CachedCompute {
    /// Associate the packet, pipeline metadata, program and stage metadata
    /// with the related original shader container. Refuse ambiguous entries.
    pub fn insert(&mut self, parts: [&[u8]; 4], container: Vec<u8>) {
        match self.shaders.entry(key(parts)) {
            Entry::Vacant(entry) => {
                entry.insert(Some(container));
            }
            Entry::Occupied(mut entry) if entry.get().as_deref() != Some(container.as_slice()) => {
                entry.insert(None);
            }
            Entry::Occupied(_) => {}
        }
    }

    /// Publish a completed catalog before the title starts calling D3D12.X.
    pub fn install(self) -> Result<usize, &'static str> {
        let count = self.shaders.values().filter(|s| s.is_some()).count();
        COMPUTE
            .set(self)
            .map_err(|_| "compute catalog already loaded")?;
        Ok(count)
    }

    fn find(&self, parts: [&[u8]; 4]) -> Option<&[u8]> {
        self.shaders.get(&key(parts))?.as_deref()
    }
}

fn key(parts: [&[u8]; 4]) -> blake3::Hash {
    let mut hash = blake3::Hasher::new();
    for part in parts {
        hash.update(&(part.len() as u64).to_le_bytes());
        hash.update(part);
    }
    hash.finalize()
}

fn read_compute_parts(desc: *const u8) -> Option<[Vec<u8>; 4]> {
    const MAX_PART: u64 = 16 << 20;
    // Root signature followed by four (guest address, byte count) pairs.
    // Words past these pairs are unnecessary for identifying the pipeline.
    let mut header = [0; 72];
    if !crate::host::read_into(desc, &mut header) {
        return None;
    }
    let words = header.as_chunks::<8>().0;
    if u64::from_le_bytes(words[0]) == 0 {
        return None;
    }
    let mut parts = [const { Vec::new() }; 4];
    for (pair, part) in words[1..].as_chunks::<2>().0.iter().zip(&mut parts) {
        let [ptr, size] = pair.map(u64::from_le_bytes);
        if ptr == 0 || size == 0 || size > MAX_PART {
            return None;
        }
        part.resize(size as usize, 0);
        if !crate::host::read_into(ptr as *const u8, part) {
            return None;
        }
    }
    Some(parts)
}

/// Slot 0x218. An exact catalog match recovers the original bytecode and
/// uses the same compute pipeline path as the ordinary creation method.
pub(crate) fn create_cached_compute(
    desc: *const u8,
    iid: *const Iid,
    out: *mut This,
) -> Option<HRESULT> {
    let catalog = COMPUTE.get()?;
    let parts = read_compute_parts(desc)?;
    let bytes = catalog.find(parts.each_ref().map(Vec::as_slice))?;
    let shader = super::shader("compute", bytes);
    tracing::debug!(hash = %shader.hash, "restored an Xbox cached compute pipeline");
    Some(super::hand_out(vec![shader], None, iid, out))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_compute_description_recovers_only_an_exact_unambiguous_match() {
        let initial = [b"packet".as_slice(), b"metadata", b"program", b"bindings"];
        let mut desc = [0u64; 9];
        desc[0] = 1;
        for (pair, part) in desc[1..].as_chunks_mut::<2>().0.iter_mut().zip(initial) {
            *pair = [part.as_ptr() as u64, part.len() as u64];
        }
        let raw = desc.as_ptr().cast();
        let parts = read_compute_parts(raw).unwrap();
        let read = parts.each_ref().map(Vec::as_slice);
        assert_eq!(read, initial);

        let mut catalog = CachedCompute::default();
        catalog.insert(initial, vec![1]);
        catalog.insert(initial, vec![1]);
        assert_eq!(catalog.find(read), Some(&[1][..]));
        assert_eq!(
            catalog.find([initial[0], initial[1], initial[2], b"other"]),
            None
        );
        catalog.insert(initial, vec![2]);
        catalog.insert(initial, vec![1]);
        assert_eq!(catalog.find(read), None);

        desc[8] = u64::MAX;
        assert!(read_compute_parts(desc.as_ptr().cast()).is_none());
        desc[8] = 4;
        desc[7] = 1;
        assert!(read_compute_parts(desc.as_ptr().cast()).is_none());
    }
}
