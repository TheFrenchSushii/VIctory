//! Resolve the title's native pipeline pack back to original shader containers.

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::io::Read;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use victory_game::{pipelines, sgd5, shaderpack};
use victory_hle::modules::d3d12x::{CachedCompute, CachedGraphics};
use victory_shader::{Container, FourCC, HwStage, native_programs};

pub(crate) fn load(path: &Path) -> Result<()> {
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).with_context(|| format!("opening {}", path.display())),
    };
    let limit = crate::shader::MAX_FILE;
    anyhow::ensure!(
        file.metadata()?.len() <= limit,
        "shader archive exceeds the scan limit"
    );
    let mut archive = Vec::new();
    file.take(limit + 1).read_to_end(&mut archive)?;
    anyhow::ensure!(
        archive.len() as u64 <= limit,
        "shader archive exceeds the scan limit"
    );

    let packed = shaderpack::find(&archive).context("Xbox pipeline pack not found")?;
    let pack = shaderpack::Pack::parse(&packed).map_err(anyhow::Error::msg)?;
    let databases = sgd5::databases(&archive);
    compute(&pack, &databases)?;
    graphics(&pack, &databases)
}

fn compute(pack: &shaderpack::Pack<'_>, databases: &[sgd5::Database<'_>]) -> Result<()> {
    let mut shaders: HashMap<u64, Vec<sgd5::Record<'_>>> = HashMap::new();
    for shader in databases.iter().flat_map(|db| &db.shaders[5]) {
        shaders.entry(shader.hash).or_default().push(shader.record);
    }
    let mut catalog = CachedCompute::default();
    let mut total = 0;
    for entry in pack.compute() {
        total += 1;
        let Some((records, layout)) = shaders.get(&entry.shader_hash).zip(entry.stage.metadata)
        else {
            continue;
        };
        for record in records {
            if let Some(bytes) = record.unpack() {
                catalog.insert(
                    [entry.packet, entry.metadata, entry.stage.program, layout],
                    bytes,
                );
            }
        }
    }
    let usable = catalog.install().map_err(anyhow::Error::msg)?;
    tracing::info!(
        usable,
        total,
        "Xbox compute pipelines indexed from the title's shader archive"
    );
    Ok(())
}

struct Shader {
    stage: HwStage,
    bytes: Arc<[u8]>,
}

#[derive(PartialEq, Eq, Hash)]
enum ShaderIdentity {
    First([u8; 24], u64),
    Pixel([u8; 24]),
}

fn graphics_shaders(databases: &[sgd5::Database<'_>]) -> HashMap<ShaderIdentity, Option<Shader>> {
    let mut binaries = HashMap::new();
    for shader in databases
        .iter()
        .flat_map(|db| db.shaders[..2].iter().flatten())
    {
        let Some(bytes) = shader.record.unpack() else {
            continue;
        };
        let Ok(container) = Container::parse(&bytes) else {
            continue;
        };
        let Some(dxil) = container.part(FourCC::DXIL) else {
            continue;
        };
        let Ok(programs) = native_programs(dxil.data) else {
            continue;
        };
        let bytes: Arc<[u8]> = bytes.into();
        for program in programs {
            let Some(id) = shaderpack::program_id(&program.code) else {
                continue;
            };
            let identity = match program.stage {
                HwStage::Vs | HwStage::Ms => ShaderIdentity::First(id, shader.hash),
                HwStage::Ps => ShaderIdentity::Pixel(id),
                _ => continue,
            };
            match binaries.entry(identity) {
                Entry::Vacant(entry) => {
                    entry.insert(Some(Shader {
                        stage: program.stage,
                        bytes: bytes.clone(),
                    }));
                }
                Entry::Occupied(mut entry) => {
                    let identical = entry
                        .get()
                        .as_ref()
                        .is_some_and(|old| old.stage == program.stage && old.bytes == bytes);
                    if !identical {
                        entry.insert(None);
                    }
                }
            }
        }
    }
    binaries
}

fn graphics(pack: &shaderpack::Pack<'_>, databases: &[sgd5::Database<'_>]) -> Result<()> {
    let shaders = graphics_shaders(databases);
    let state = pipelines::Named::find(pack, databases);
    let mut catalog = CachedGraphics::default();
    let mut total = 0;
    let mut unsupported_stages = 0;
    let mut unresolved_shaders = 0;
    let mut unresolved_state = 0;
    for entry in pack.graphics() {
        total += 1;
        if entry.stages[3..].iter().any(Option::is_some) {
            unsupported_stages += 1;
            continue;
        }
        let (first_slot, expected_stage) = match (entry.stages[1], entry.stages[2]) {
            (Some(vs), None) => (vs, HwStage::Vs),
            // Xbox executes mesh programs in the native geometry slot.
            (None, Some(ms)) => (ms, HwStage::Ms),
            _ => {
                unsupported_stages += 1;
                continue;
            }
        };
        let first = shaderpack::program_id(first_slot.program)
            .and_then(|id| shaders.get(&ShaderIdentity::First(id, entry.shader_hash)))
            .and_then(Option::as_ref)
            .filter(|shader| shader.stage == expected_stage);
        let pixel = entry.stages[0]
            .and_then(|stage| shaderpack::program_id(stage.program))
            .and_then(|id| shaders.get(&ShaderIdentity::Pixel(id)))
            .and_then(Option::as_ref);
        let Some(first) = first.filter(|_| entry.stages[0].is_none() || pixel.is_some()) else {
            unresolved_shaders += 1;
            continue;
        };
        if !entry
            .samples()
            .is_some_and(|(_, count, _)| [1, 2, 4, 8, 16].contains(&count))
        {
            unresolved_state += 1;
            continue;
        }
        let stream = pipelines::stream(
            &entry,
            &state,
            &first.bytes,
            first.stage == HwStage::Ms,
            pixel.map(|p| p.bytes.as_ref()),
        );
        let Ok(stream) = stream else {
            unresolved_state += 1;
            continue;
        };
        let mut parts = [&[][..]; 16];
        parts[..2].copy_from_slice(&[entry.packet, entry.metadata]);
        for (pair, stage) in parts[2..]
            .as_chunks_mut::<2>()
            .0
            .iter_mut()
            .zip(entry.stages)
        {
            if let Some(stage) = stage {
                *pair = [stage.program, stage.metadata.unwrap_or_default()];
            }
        }
        let state_ids = [
            entry.depth_stencil_hash,
            entry.rasterizer_hash,
            entry.blend_hash,
        ];
        // SAFETY: the builder owns its input layout and borrows the shader
        // bytes. Both stay live during insertion.
        if !unsafe { catalog.insert(parts, state_ids, stream.bytes()) } {
            unresolved_state += 1;
        }
    }
    let usable = catalog.install().map_err(anyhow::Error::msg)?;
    tracing::info!(
        usable,
        total,
        unsupported_stages,
        unresolved_shaders,
        unresolved_state,
        "Xbox graphics pipelines indexed from the title's shader archive"
    );
    Ok(())
}
