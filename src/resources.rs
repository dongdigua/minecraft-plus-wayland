use std::{
    error::Error,
    fs::{self, File},
    io::{Read, Seek},
    path::{Path, PathBuf},
};

const RESOURCES_ARCHIVE_ENV: &str = "MINECRAFT_PLUS_RESOURCES";
const WEB_WASM_ENV: &str = "MINECRAFT_PLUS_WEB_WASM";
const DEFAULT_RESOURCES_ARCHIVE: &str = "assets/resources.zip";
const DEFAULT_WEB_WASM: &str = "assets/mcse_web_bg.wasm";

#[cfg(feature = "embed-assets")]
const EMBEDDED_ARCHIVE: &[u8] = include_bytes!("../assets/resources.zip");
#[cfg(feature = "embed-assets")]
const EMBEDDED_WEB_WASM: &[u8] = include_bytes!("../assets/mcse_web_bg.wasm");

enum AssetSource {
    Path(PathBuf),
    Embedded(&'static [u8]),
}

/// Read one original resource file from the configured ZIP archive.
fn load_resource(resource_name: &str) -> Result<Vec<u8>, Box<dyn Error>> {
    let source = select_source(
        configured_path(RESOURCES_ARCHIVE_ENV),
        embedded_archive(),
        DEFAULT_RESOURCES_ARCHIVE,
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
/// `MINECRAFT_PLUS_RESOURCES` can point at an alternate `resources.zip` and
/// takes precedence over an archive embedded with the `embed-assets` feature.
/// Without either source, `assets/resources.zip` is resolved relative to the
/// current working directory.
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
/// `embed-assets` feature. Without either source,
/// `assets/mcse_web_bg.wasm` is resolved relative to the current working
/// directory.
pub(crate) fn load_web_wasm() -> Result<Vec<u8>, Box<dyn Error>> {
    let source = select_source(
        configured_path(WEB_WASM_ENV),
        embedded_web_wasm(),
        DEFAULT_WEB_WASM,
    );

    match source {
        AssetSource::Path(path) => fs::read(&path)
            .map_err(|error| source_io_error(WEB_WASM_ENV, &path, "read Web WASM", error).into()),
        AssetSource::Embedded(bytes) => Ok(bytes.to_vec()),
    }
}

fn configured_path(variable: &str) -> Option<PathBuf> {
    std::env::var_os(variable).map(PathBuf::from)
}

fn select_source(
    configured_path: Option<PathBuf>,
    embedded: Option<&'static [u8]>,
    default_path: &'static str,
) -> AssetSource {
    if let Some(path) = configured_path {
        AssetSource::Path(path)
    } else if let Some(bytes) = embedded {
        AssetSource::Embedded(bytes)
    } else {
        AssetSource::Path(PathBuf::from(default_path))
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

    use super::{AssetSource, DEFAULT_RESOURCES_ARCHIVE, DEFAULT_WEB_WASM, select_source};

    #[test]
    fn explicit_path_has_highest_priority() {
        let configured = PathBuf::from("configured/resources.zip");
        let source = select_source(
            Some(configured.clone()),
            Some(b"embedded"),
            DEFAULT_RESOURCES_ARCHIVE,
        );

        match source {
            AssetSource::Path(path) => assert_eq!(path, configured),
            AssetSource::Embedded(_) => panic!("explicit path did not take priority"),
        }
    }

    #[test]
    fn embedded_bytes_are_the_fallback_when_available() {
        let embedded = b"embedded";
        let source = select_source(None, Some(embedded), DEFAULT_RESOURCES_ARCHIVE);

        match source {
            AssetSource::Embedded(bytes) => assert_eq!(bytes, embedded),
            AssetSource::Path(_) => panic!("embedded fallback was not selected"),
        }
    }

    #[test]
    fn defaults_are_relative_paths_when_no_other_source_exists() {
        let archive = select_source(None, None, DEFAULT_RESOURCES_ARCHIVE);
        let wasm = select_source(None, None, DEFAULT_WEB_WASM);

        match archive {
            AssetSource::Path(path) => assert_eq!(path, PathBuf::from("assets/resources.zip")),
            AssetSource::Embedded(_) => panic!("unexpected embedded archive"),
        }
        match wasm {
            AssetSource::Path(path) => assert_eq!(path, PathBuf::from("assets/mcse_web_bg.wasm")),
            AssetSource::Embedded(_) => panic!("unexpected embedded Web WASM"),
        }
    }
}
