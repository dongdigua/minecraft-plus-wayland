use std::{
    env,
    error::Error,
    fs::{self, File},
    io::{Read, Seek},
    path::PathBuf,
};

const RESOURCES_ARCHIVE_ENV: &str = "MINECRAFT_PLUS_RESOURCES";
const WEB_WASM_ENV: &str = "MINECRAFT_PLUS_WEB_WASM";
#[cfg(not(feature = "embed-assets"))]
const BUNDLED_ARCHIVE: &str = "assets/resources.zip";
#[cfg(not(feature = "embed-assets"))]
const BUNDLED_WEB_WASM: &str = "assets/mcse_web_bg.wasm";

#[cfg(feature = "embed-assets")]
const EMBEDDED_ARCHIVE: &[u8] =
    include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/assets/resources.zip"));
#[cfg(feature = "embed-assets")]
const EMBEDDED_WEB_WASM: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/assets/mcse_web_bg.wasm"
));

/// Read one original resource file from the configured ZIP archive.
fn load_resource(resource_name: &str) -> Result<Vec<u8>, Box<dyn Error>> {
    if let Some(path) = env::var_os(RESOURCES_ARCHIVE_ENV) {
        return load_resource_from_reader(File::open(PathBuf::from(path))?, resource_name);
    }

    #[cfg(feature = "embed-assets")]
    {
        load_resource_from_reader(std::io::Cursor::new(EMBEDDED_ARCHIVE), resource_name)
    }

    #[cfg(not(feature = "embed-assets"))]
    {
        let archive_path = resources_archive_path()?;
        load_resource_from_reader(File::open(archive_path)?, resource_name)
    }
}

fn load_resource_from_reader<R>(reader: R, resource_name: &str) -> Result<Vec<u8>, Box<dyn Error>>
where
    R: Read + Seek,
{
    let mut archive = zip::ZipArchive::new(reader)?;
    let mut resource = archive.by_name(resource_name)?;
    let mut encoded = Vec::new();
    resource.read_to_end(&mut encoded)?;
    Ok(encoded)
}

/// Decode a PNG from the original resource archive at runtime.
///
/// `MINECRAFT_PLUS_RESOURCES` can point at an alternate `resources.zip` and
/// takes precedence over an archive embedded with the `embed-assets` feature.
/// Without either source, the local `assets/resources.zip` is used as a
/// development fallback.
pub(crate) fn load_rgba_png(resource_name: &str) -> Result<image::RgbaImage, Box<dyn Error>> {
    let encoded = load_resource(resource_name)?;
    Ok(image::load_from_memory_with_format(&encoded, image::ImageFormat::Png)?.to_rgba8())
}

/// Read a UTF-8 text resource from the original archive.
pub(crate) fn load_utf8(resource_name: &str) -> Result<String, Box<dyn Error>> {
    Ok(String::from_utf8(load_resource(resource_name)?)?)
}

/// Read the original Web WASM used by the alpha-fluid modules.
///
/// `MINECRAFT_PLUS_WEB_WASM` takes precedence over a module embedded with the
/// `embed-assets` feature. Without either source, the local asset is used as a
/// development fallback.
pub(crate) fn load_web_wasm() -> Result<Vec<u8>, Box<dyn Error>> {
    if let Some(path) = env::var_os(WEB_WASM_ENV) {
        return Ok(fs::read(PathBuf::from(path))?);
    }

    #[cfg(feature = "embed-assets")]
    {
        Ok(EMBEDDED_WEB_WASM.to_vec())
    }

    #[cfg(not(feature = "embed-assets"))]
    {
        let path = first_existing_path(BUNDLED_WEB_WASM).ok_or_else(|| {
            format!("could not find assets/mcse_web_bg.wasm; set {WEB_WASM_ENV} to its path")
        })?;
        Ok(fs::read(path)?)
    }
}

#[cfg(not(feature = "embed-assets"))]
fn resources_archive_path() -> Result<PathBuf, Box<dyn Error>> {
    first_existing_path(BUNDLED_ARCHIVE).ok_or_else(|| {
        format!("could not find assets/resources.zip; set {RESOURCES_ARCHIVE_ENV} to its path")
            .into()
    })
}

#[cfg(not(feature = "embed-assets"))]
fn first_existing_path(candidate: &str) -> Option<PathBuf> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(candidate);
    path.is_file().then_some(path)
}

#[cfg(test)]
mod tests {
    use super::{load_rgba_png, load_web_wasm};

    #[test]
    fn squid_atlas_is_loaded_from_resources_zip() {
        let atlas = load_rgba_png("squids.png").unwrap();
        assert_eq!(atlas.dimensions(), (128, 640));
    }

    #[test]
    fn original_web_wasm_is_loaded() {
        let wasm = load_web_wasm().unwrap();
        assert_eq!(wasm.get(..4), Some(b"\0asm".as_slice()));
    }
}
