use std::{
    error::Error,
    fs::{self, File},
    io::{Read, Seek},
    path::{Path, PathBuf},
};

const RESOURCES_ARCHIVE_ENV: &str = "MINECRAFT_PLUS_RESOURCES";
const WEB_WASM_ENV: &str = "MINECRAFT_PLUS_WEB_WASM";
const ASSETS_ROOT_ENV: &str = "MINECRAFT_PLUS_ASSETS";
const RESOURCES_ARCHIVE_RELATIVE: &str = "resources.zip";
const WEB_WASM_RELATIVE: &str = "mcse_web_bg.wasm";
const TORCH_ASSETS_RELATIVE: &str = "lock/torch";

#[cfg(feature = "embed-assets")]
const EMBEDDED_ARCHIVE: &[u8] = include_bytes!("../assets/resources.zip");
#[cfg(feature = "embed-assets")]
const EMBEDDED_WEB_WASM: &[u8] = include_bytes!("../assets/mcse_web_bg.wasm");

#[cfg(feature = "embed-assets")]
const EMBEDDED_TORCH_TEXTURES: [&[u8]; 6] = [
    include_bytes!("../assets/lock/torch/redstone_torch.png"),
    include_bytes!("../assets/lock/torch/copper_torch.png"),
    include_bytes!("../assets/lock/torch/soul_torch.png"),
    include_bytes!("../assets/lock/torch/torch.png"),
    include_bytes!("../assets/lock/torch/smooth_stone.png"),
    include_bytes!("../assets/lock/torch/redstone_torch_off.png"),
];

#[derive(Clone)]
enum AssetSource {
    Path(PathBuf),
    Embedded(&'static [u8]),
}

/// Read one original resource file from the configured ZIP archive.
fn load_resource(resource_name: &str) -> Result<Vec<u8>, Box<dyn Error>> {
    let source = select_source(
        configured_path(RESOURCES_ARCHIVE_ENV),
        configured_asset_path(RESOURCES_ARCHIVE_RELATIVE),
        embedded_archive(),
        RESOURCES_ARCHIVE_RELATIVE,
    );

    match source {
        AssetSource::Path(path) => {
            let file = File::open(&path).map_err(|error| {
                source_io_error(RESOURCES_ARCHIVE_ENV, &path, "open resource archive", error)
            })?;
            load_resource_from_reader(file, resource_name)
        }
        AssetSource::Embedded(bytes) => {
            load_resource_from_reader(std::io::Cursor::new(bytes), resource_name)
        }
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
/// `MINECRAFT_PLUS_RESOURCES` can point at an alternate `resources.zip`; otherwise
/// `MINECRAFT_PLUS_ASSETS` can select an entire assets root. Both take precedence over an archive
/// embedded with the `embed-assets` feature and the current-directory `assets` fallback.
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
/// `MINECRAFT_PLUS_WEB_WASM` takes precedence, followed by `MINECRAFT_PLUS_ASSETS`, an embedded
/// module, and the current-directory `assets` fallback.
pub(crate) fn load_web_wasm() -> Result<Vec<u8>, Box<dyn Error>> {
    let source = select_source(
        configured_path(WEB_WASM_ENV),
        configured_asset_path(WEB_WASM_RELATIVE),
        embedded_web_wasm(),
        WEB_WASM_RELATIVE,
    );

    match source {
        AssetSource::Path(path) => {
            log::debug!(
                target: "minecraft_plus_wayland::wasm",
                "loading Web WASM from path {}",
                path.display(),
            );
            let bytes = fs::read(&path)
                .map_err(|error| source_io_error(WEB_WASM_ENV, &path, "read Web WASM", error))?;
            log::debug!(
                target: "minecraft_plus_wayland::wasm",
                "loaded Web WASM: source={}, bytes={}",
                path.display(),
                bytes.len(),
            );
            Ok(bytes)
        }
        AssetSource::Embedded(bytes) => {
            log::debug!(
                target: "minecraft_plus_wayland::wasm",
                "using embedded Web WASM: bytes={}",
                bytes.len(),
            );
            Ok(bytes.to_vec())
        }
    }
}

/// Load the six fixed torch-scene texture layers in
/// redstone/copper/soul/torch/stone/redstone-off order.
pub(crate) fn load_torch_textures() -> Result<[image::RgbaImage; 6], Box<dyn Error>> {
    const NAMES: [&str; 6] = [
        "redstone_torch.png",
        "copper_torch.png",
        "soul_torch.png",
        "torch.png",
        "smooth_stone.png",
        "redstone_torch_off.png",
    ];
    let embedded = embedded_torch_textures();
    let sources: [AssetSource; 6] = std::array::from_fn(|index| {
        select_source(
            None,
            configured_asset_path(&format!("{TORCH_ASSETS_RELATIVE}/{}", NAMES[index])),
            embedded.map(|textures| textures[index]),
            &format!("{TORCH_ASSETS_RELATIVE}/{}", NAMES[index]),
        )
    });
    let images = sources
        .into_iter()
        .map(|source| {
            let encoded = match source {
                AssetSource::Path(path) => fs::read(&path).map_err(|error| {
                    source_io_error(ASSETS_ROOT_ENV, &path, "read torch texture", error)
                })?,
                AssetSource::Embedded(bytes) => bytes.to_vec(),
            };
            Ok::<_, Box<dyn Error>>(
                image::load_from_memory_with_format(&encoded, image::ImageFormat::Png)?.to_rgba8(),
            )
        })
        .collect::<Result<Vec<_>, _>>()?;
    images
        .try_into()
        .map_err(|_| "torch texture table must contain exactly six images".into())
}

fn configured_path(variable: &str) -> Option<PathBuf> {
    std::env::var_os(variable).map(PathBuf::from)
}

fn configured_asset_path(relative: &str) -> Option<PathBuf> {
    configured_path(ASSETS_ROOT_ENV).map(|root| root.join(relative))
}

fn select_source(
    dedicated_path: Option<PathBuf>,
    assets_path: Option<PathBuf>,
    embedded: Option<&'static [u8]>,
    relative_path: &str,
) -> AssetSource {
    if let Some(path) = dedicated_path {
        AssetSource::Path(path)
    } else if let Some(path) = assets_path {
        AssetSource::Path(path)
    } else if let Some(bytes) = embedded {
        AssetSource::Embedded(bytes)
    } else {
        AssetSource::Path(PathBuf::from("assets").join(relative_path))
    }
}

#[cfg(feature = "embed-assets")]
fn embedded_archive() -> Option<&'static [u8]> {
    Some(EMBEDDED_ARCHIVE)
}

#[cfg(not(feature = "embed-assets"))]
fn embedded_archive() -> Option<&'static [u8]> {
    None
}

#[cfg(feature = "embed-assets")]
fn embedded_web_wasm() -> Option<&'static [u8]> {
    Some(EMBEDDED_WEB_WASM)
}

#[cfg(not(feature = "embed-assets"))]
fn embedded_web_wasm() -> Option<&'static [u8]> {
    None
}

#[cfg(feature = "embed-assets")]
fn embedded_torch_textures() -> Option<[&'static [u8]; 6]> {
    Some(EMBEDDED_TORCH_TEXTURES)
}

#[cfg(not(feature = "embed-assets"))]
fn embedded_torch_textures() -> Option<[&'static [u8]; 6]> {
    None
}

fn source_io_error(
    variable: &str,
    path: &Path,
    action: &str,
    error: std::io::Error,
) -> std::io::Error {
    std::io::Error::new(
        error.kind(),
        format!(
            "could not {action} from path {} (override with {variable}): {error}",
            path.display()
        ),
    )
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{AssetSource, RESOURCES_ARCHIVE_RELATIVE, select_source};

    #[test]
    fn dedicated_path_has_highest_priority() {
        let dedicated = PathBuf::from("configured/resources.zip");
        let source = select_source(
            Some(dedicated.clone()),
            Some(PathBuf::from("root/resources.zip")),
            Some(b"embedded"),
            RESOURCES_ARCHIVE_RELATIVE,
        );
        assert!(matches!(source, AssetSource::Path(path) if path == dedicated));
    }

    #[test]
    fn assets_root_precedes_embedded_bytes() {
        let assets = PathBuf::from("root/resources.zip");
        let source = select_source(
            None,
            Some(assets.clone()),
            Some(b"embedded"),
            RESOURCES_ARCHIVE_RELATIVE,
        );
        assert!(matches!(source, AssetSource::Path(path) if path == assets));
    }

    #[test]
    fn embedded_bytes_precede_the_cwd_fallback() {
        let source = select_source(None, None, Some(b"embedded"), RESOURCES_ARCHIVE_RELATIVE);
        assert!(matches!(source, AssetSource::Embedded(bytes) if bytes == b"embedded"));
    }

    #[test]
    fn default_is_relative_to_the_cwd_assets_directory() {
        let source = select_source(None, None, None, RESOURCES_ARCHIVE_RELATIVE);
        assert!(
            matches!(source, AssetSource::Path(path) if path == std::path::Path::new("assets/resources.zip"))
        );
    }
}
