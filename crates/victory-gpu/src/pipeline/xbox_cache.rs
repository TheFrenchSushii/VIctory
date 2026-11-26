//! Xbox pipelines supplied as native binaries and serialized device state.

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::sync::{Arc, OnceLock};

use crate::object::{HRESULT, Iid, This};

static COMPUTE: OnceLock<CachedCompute> = OnceLock::new();
static GRAPHICS: OnceLock<CachedGraphics> = OnceLock::new();

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

/// Graphics pipeline templates reconstructed from a title's pipeline pack.
#[derive(Default)]
pub struct CachedGraphics {
    streams: HashMap<blake3::Hash, Option<GraphicsTemplate>>,
}

struct GraphicsTemplate {
    states: [u64; 3],
    stream: super::StreamParts,
}

impl CachedGraphics {
    /// Associate raw Xbox description parts and their render state IDs with
    /// a stream containing the original shader containers and render state.
    ///
    /// # Safety
    /// All pointers encoded in `stream` must remain valid for the stream
    /// parser and its reads of shader bytecode and input layout strings.
    pub unsafe fn insert(&mut self, parts: [&[u8]; 16], states: [u64; 3], stream: &[u8]) -> bool {
        let desc = super::StreamDesc {
            size: stream.len(),
            stream: stream.as_ptr(),
        };
        let Ok(stream) = super::read_stream(&desc) else {
            return false;
        };
        if stream.shaders.is_empty() {
            return false;
        }
        match self.streams.entry(key(parts)) {
            Entry::Vacant(entry) => {
                entry.insert(Some(GraphicsTemplate { states, stream }));
            }
            Entry::Occupied(mut entry) => {
                let identical = entry.get().as_ref().is_some_and(|old| {
                    old.states == states
                        && old.stream.shaders.len() == stream.shaders.len()
                        && old
                            .stream
                            .shaders
                            .iter()
                            .zip(&stream.shaders)
                            .all(|(a, b)| Arc::ptr_eq(a, b))
                });
                if !identical {
                    entry.insert(None);
                }
            }
        }
        true
    }

    /// Publish the reconstructed streams for the device methods to query.
    pub fn install(self) -> Result<usize, &'static str> {
        let count = self.streams.values().filter(|s| s.is_some()).count();
        GRAPHICS
            .set(self)
            .map_err(|_| "graphics catalog already loaded")?;
        Ok(count)
    }
}

fn key<const N: usize>(parts: [&[u8]; N]) -> blake3::Hash {
    let mut hash = blake3::Hasher::new();
    for part in parts {
        hash.update(&(part.len() as u64).to_le_bytes());
        hash.update(part);
    }
    hash.finalize()
}

fn read_parts<const N: usize>(desc: *const u8) -> Option<[Vec<u8>; N]> {
    const MAX_PART: u64 = 16 << 20;
    // Root signature followed by (guest address, byte count) pairs.
    let mut bytes = [0; 8 + 16 * 16];
    let header = bytes.get_mut(..8 + 16 * N)?;
    if !crate::host::read_into(desc, header) {
        return None;
    }
    let words = header.as_chunks::<8>().0;
    if u64::from_le_bytes(words[0]) == 0 {
        return None;
    }
    let mut parts = [const { Vec::new() }; N];
    for (pair, part) in words[1..].as_chunks::<2>().0.iter().zip(&mut parts) {
        let [ptr, size] = pair.map(u64::from_le_bytes);
        if size == 0 {
            continue;
        }
        if ptr == 0 || size > MAX_PART {
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
    let parts = read_parts::<4>(desc)?;
    let bytes = catalog.find(parts.each_ref().map(Vec::as_slice))?;
    let shader = super::shader("compute", bytes);
    tracing::debug!(hash = %shader.hash, "restored an Xbox cached compute pipeline");
    Some(super::hand_out(vec![shader], None, iid, out))
}

/// Slot 0x1c8. Graphics carries two state blocks and a code/metadata pair
/// for each of the seven hardware stage slots of the serialized pipeline.
pub(crate) fn create_cached_graphics(
    desc: *const u8,
    iid: *const Iid,
    out: *mut This,
) -> Option<HRESULT> {
    let catalog = GRAPHICS.get()?;
    let parts = read_parts::<16>(desc)?;
    let id = key(parts.each_ref().map(Vec::as_slice));
    let template = catalog.streams.get(&id)?.as_ref()?;
    let stream = &template.stream;
    tracing::debug!(shaders = ?stream.shaders, "restored an Xbox cached graphics pipeline");
    Some(super::hand_out(
        stream.shaders.clone(),
        Some(Arc::clone(&stream.graphics)),
        iid,
        out,
    ))
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
        let parts = read_parts::<4>(raw).unwrap();
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
        assert!(read_parts::<4>(desc.as_ptr().cast()).is_none());
        desc[8] = 4;
        desc[7] = 1;
        assert!(read_parts::<4>(desc.as_ptr().cast()).is_none());
    }

    #[test]
    fn graphics_lookup_preserves_stage_positions_and_refuses_conflicting_state() {
        let code = b"shader";
        let stream = [1u64, code.as_ptr() as u64, code.len() as u64].map(u64::to_le_bytes);
        let bytes = stream.as_flattened();
        let mut parts = [&[][..]; 16];
        parts[0] = b"packet";
        parts[1] = b"state";
        parts[4] = code;
        let mut desc = [0u64; 33];
        desc[0] = 1;
        for (pair, part) in desc[1..].as_chunks_mut::<2>().0.iter_mut().zip(parts) {
            *pair = [part.as_ptr() as u64, part.len() as u64];
        }
        let recovered = read_parts::<16>(desc.as_ptr().cast()).unwrap();
        assert_eq!(recovered.each_ref().map(Vec::as_slice), parts);

        let mut catalog = CachedGraphics::default();
        // SAFETY: the stream's only pointer refers to the static shader bytes.
        unsafe {
            assert!(catalog.insert(parts, [1, 2, 3], bytes));
            assert!(catalog.insert(parts, [1, 2, 3], bytes));
        }
        let id = key(parts);
        assert_eq!(
            catalog.streams[&id].as_ref().unwrap().stream.shaders[0].stage,
            "vertex"
        );
        parts.swap(2, 4);
        assert!(!catalog.streams.contains_key(&key(parts)));
        parts.swap(2, 4);
        // SAFETY: same stream with conflicting recovered state IDs.
        assert!(unsafe { catalog.insert(parts, [2, 1, 3], bytes) });
        assert!(catalog.streams[&id].is_none());
    }
}
