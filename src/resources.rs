use std::{
    env,
    error::Error,
    fs::File,
    io::Read,
    path::{Path, PathBuf},
};

const RESOURCES_ARCHIVE_ENV: &str = "MINECRAFT_PLUS_RESOURCES";
const BUNDLED_ARCHIVE: &str = "assets/resources.zip";
const REPOSITORY_ARCHIVE: &str = "../MinecraftPlus/pkg/resources.zip";

/// Read one original resource file from the runtime ZIP archive.
fn load_resource(resource_name: &str) -> Result<Vec<u8>, Box<dyn Error>> {
    let archive_path = resources_archive_path()?;
    let archive_file = File::open(&archive_path)?;
    let mut archive = zip::ZipArchive::new(archive_file)?;
    let mut resource = archive.by_name(resource_name)?;
    let mut encoded = Vec::new();
    resource.read_to_end(&mut encoded)?;
    Ok(encoded)
}

/// Decode a PNG from the original resource archive at runtime.
///
/// `MINECRAFT_PLUS_RESOURCES` can point at an alternate `resources.zip`; when
/// absent, the bundled `assets/resources.zip` is used. The repository archive
/// remains a development fallback for older checkouts.
pub(crate) fn load_rgba_png(resource_name: &str) -> Result<image::RgbaImage, Box<dyn Error>> {
    let encoded = load_resource(resource_name)?;
    Ok(image::load_from_memory_with_format(&encoded, image::ImageFormat::Png)?.to_rgba8())
}

/// Read a UTF-8 text resource from the original archive at runtime.
pub(crate) fn load_utf8(resource_name: &str) -> Result<String, Box<dyn Error>> {
    Ok(String::from_utf8(load_resource(resource_name)?)?)
}

fn resources_archive_path() -> Result<PathBuf, Box<dyn Error>> {
    if let Some(path) = env::var_os(RESOURCES_ARCHIVE_ENV) {
        return Ok(PathBuf::from(path));
    }

    let manifest_directory = Path::new(env!("CARGO_MANIFEST_DIR"));
    for archive in [
        manifest_directory.join(BUNDLED_ARCHIVE),
        manifest_directory.join(REPOSITORY_ARCHIVE),
    ] {
        if archive.is_file() {
            return Ok(archive);
        }
    }

    Err(
        format!("could not find assets/resources.zip; set {RESOURCES_ARCHIVE_ENV} to its path")
            .into(),
    )
}

#[cfg(test)]
mod tests {
    use super::load_rgba_png;

    #[test]
    fn squid_atlas_is_loaded_from_resources_zip() {
        let atlas = load_rgba_png("squids.png").unwrap();
        assert_eq!(atlas.dimensions(), (128, 640));
    }
}
