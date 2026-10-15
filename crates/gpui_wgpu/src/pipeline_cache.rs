use gpui::profiler::startup::StartupProfile;
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

const DIGEST_BYTES: usize = 32;
const MAX_CACHE_BYTES: u64 = 64 * 1024 * 1024;

pub(crate) struct PersistentPipelineCache {
    pub(crate) cache: wgpu::PipelineCache,
    path: PathBuf,
    loaded_digest: Option<[u8; DIGEST_BYTES]>,
}

impl PersistentPipelineCache {
    pub(crate) fn new(
        device: &wgpu::Device,
        adapter: &wgpu::AdapterInfo,
        directory: &Path,
        shaders: &[&str],
    ) -> Option<Self> {
        if !device.features().contains(wgpu::Features::PIPELINE_CACHE) {
            return None;
        }
        let adapter_key = wgpu::util::pipeline_cache_key(adapter)?;
        let mut profile = StartupProfile::new("gpui_pipeline_cache");
        let mut key = Sha256::new();
        key.update(b"gpui-pipelines-v1");
        key.update(format!("{adapter:?}"));
        for shader in shaders {
            key.update(shader.as_bytes());
        }
        let path = directory.join(format!("{adapter_key}-{:x}.bin", key.finalize()));
        let data = match read_cache(&path) {
            Ok(data) => Some(data),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => {
                log::warn!("Ignoring unreadable GPU pipeline cache: {error}");
                None
            }
        };
        let loaded_digest = data.as_ref().map(|data| Sha256::digest(data).into());
        log::info!(
            "GPU pipeline cache: loaded_bytes={}",
            data.as_ref().map_or(0, Vec::len)
        );
        // Only our private cache files, written from get_data(), are accepted.
        // The checksum rejects partial/corrupt files before handing bytes to the driver.
        // wgpu additionally rejects incompatible driver/version headers with fallback=true.
        let errors = device.push_error_scope(wgpu::ErrorFilter::Validation);
        let cache = unsafe {
            device.create_pipeline_cache(&wgpu::PipelineCacheDescriptor {
                label: Some("gpui_pipeline_cache"),
                data: data.as_deref(),
                fallback: true,
            })
        };
        if let Some(error) = gpui::block_on(errors.pop()) {
            log::warn!("GPU pipeline cache unavailable: {error}");
            return None;
        }
        profile.mark("load");
        Some(Self {
            cache,
            path,
            loaded_digest,
        })
    }

    pub(crate) fn save(&self) {
        let cache = self.cache.clone();
        let path = self.path.clone();
        let loaded_digest = self.loaded_digest;
        // Cache persistence must not add a synchronous write to window opening.
        if let Err(error) = std::thread::Builder::new()
            .name("gpui-pipeline-cache".into())
            .spawn(move || {
                let Some(data) = cache.get_data() else { return };
                let digest: [u8; DIGEST_BYTES] = Sha256::digest(&data).into();
                if Some(digest) == loaded_digest {
                    log::debug!("GPU pipeline cache unchanged");
                    return;
                }
                match write_cache(&path, &data) {
                    Ok(()) => log::info!("GPU pipeline cache saved: bytes={}", data.len()),
                    Err(error) => log::warn!("Could not persist GPU pipeline cache: {error}"),
                }
            })
        {
            log::warn!("Could not start GPU pipeline cache writer: {error}");
        }
    }
}

fn read_cache(path: &Path) -> io::Result<Vec<u8>> {
    let mut data = Vec::new();
    File::open(path)?
        .take(MAX_CACHE_BYTES + 1)
        .read_to_end(&mut data)?;
    if data.len() < DIGEST_BYTES || data.len() as u64 > MAX_CACHE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid pipeline cache size",
        ));
    }
    let digest = Sha256::digest(&data[DIGEST_BYTES..]);
    if data[..DIGEST_BYTES] != digest[..] {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "pipeline cache checksum mismatch",
        ));
    }
    Ok(data.split_off(DIGEST_BYTES))
}

fn write_cache(path: &Path, data: &[u8]) -> io::Result<()> {
    if data.len() as u64 > MAX_CACHE_BYTES - DIGEST_BYTES as u64 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "pipeline cache too large",
        ));
    }
    let directory = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "cache has no directory"))?;
    std::fs::create_dir_all(directory)?;
    let mut temporary = tempfile::NamedTempFile::new_in(directory)?;
    temporary.write_all(&Sha256::digest(data))?;
    temporary.write_all(data)?;
    temporary.persist(path).map_err(|error| error.error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_replacement_is_readable_and_corruption_is_rejected() -> io::Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("nested/cache.bin");
        write_cache(&path, b"first driver cache")?;
        assert_eq!(read_cache(&path)?, b"first driver cache");
        write_cache(&path, b"replacement driver cache")?;
        assert_eq!(read_cache(&path)?, b"replacement driver cache");
        let mut corrupt = std::fs::read(&path)?;
        if let Some(byte) = corrupt.last_mut() {
            *byte ^= 1;
        }
        std::fs::write(&path, corrupt)?;
        assert_eq!(
            read_cache(&path)
                .expect_err("corruption must be rejected")
                .kind(),
            io::ErrorKind::InvalidData
        );
        std::fs::write(&path, b"truncated")?;
        assert_eq!(
            read_cache(&path)
                .expect_err("truncation must be rejected")
                .kind(),
            io::ErrorKind::InvalidData
        );
        Ok(())
    }
}
