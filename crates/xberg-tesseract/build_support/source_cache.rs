use sha2::{Digest, Sha256};
use std::fs;
use std::io::{self, Read, Seek, Write};
use std::path::{Path, PathBuf};
use tempfile::{Builder, NamedTempFile};

#[cfg(windows)]
use std::ffi::OsString;

#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};

#[cfg(windows)]
use std::os::windows::ffi::{OsStrExt, OsStringExt};
#[cfg(windows)]
use std::os::windows::fs::OpenOptionsExt;

#[cfg(unix)]
type DirectoryIdentity = fs::Metadata;
#[cfg(windows)]
type DirectoryIdentity = same_file::Handle;
#[cfg(not(any(unix, windows)))]
type DirectoryIdentity = fs::Metadata;

const SOURCE_ROOT_MARKER: &str = "CMakeLists.txt";
const SHA256_HEX_LENGTH: usize = 64;
#[cfg(any(test, windows))]
const WINDOWS_SEPARATOR: u16 = b'\\' as u16;
#[cfg(any(test, windows))]
const WINDOWS_VERBATIM_PREFIX: [u16; 4] = [WINDOWS_SEPARATOR, WINDOWS_SEPARATOR, b'?' as u16, WINDOWS_SEPARATOR];
#[cfg(any(test, windows))]
const WINDOWS_VERBATIM_UNC_PREFIX: [u16; 8] = [
    WINDOWS_SEPARATOR,
    WINDOWS_SEPARATOR,
    b'?' as u16,
    WINDOWS_SEPARATOR,
    b'U' as u16,
    b'N' as u16,
    b'C' as u16,
    WINDOWS_SEPARATOR,
];
#[cfg(unix)]
const PRIVATE_DIRECTORY_MODE: u32 = 0o700;
#[cfg(unix)]
const NON_OWNER_PERMISSION_MASK: u32 = 0o077;
#[cfg(windows)]
const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SourceArtifact<'a> {
    pub(crate) name: &'a str,
    pub(crate) cache_key: &'a str,
    pub(crate) sha256: &'a str,
    pub(crate) expected_size: u64,
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) struct VerifiedArtifact {
    pub(crate) path: PathBuf,
    pub(crate) bytes: Vec<u8>,
    pub(crate) downloaded: bool,
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) struct PreparedSourceTree {
    pub(crate) path: PathBuf,
    pub(crate) downloaded: bool,
}

pub(crate) fn source_tree_is_complete(source_dir: &Path) -> bool {
    source_dir.is_dir() && source_dir.join(SOURCE_ROOT_MARKER).is_file()
}

pub(crate) fn canonicalize_trusted_build_root(path: &Path) -> io::Result<PathBuf> {
    let canonical = fs::canonicalize(path)?;
    let metadata = fs::symlink_metadata(&canonical)?;
    if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() {
        return Ok(canonical);
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        format!("Cargo build root is not a regular directory: {}", path.display()),
    ))
}

#[cfg(any(test, windows))]
pub(crate) fn windows_cmake_source_units(path: &[u16]) -> Vec<u16> {
    if let Some(remainder) = path.strip_prefix(&WINDOWS_VERBATIM_UNC_PREFIX) {
        let mut normalized = Vec::with_capacity(remainder.len() + 2);
        normalized.extend_from_slice(&[WINDOWS_SEPARATOR, WINDOWS_SEPARATOR]);
        normalized.extend_from_slice(remainder);
        return normalized;
    }
    path.strip_prefix(&WINDOWS_VERBATIM_PREFIX).unwrap_or(path).to_vec()
}

#[cfg(windows)]
pub(crate) fn cmake_source_path(path: &Path) -> io::Result<PathBuf> {
    let units = path.as_os_str().encode_wide().collect::<Vec<_>>();
    let normalized = PathBuf::from(OsString::from_wide(&windows_cmake_source_units(&units)));
    if same_file::is_same_file(path, &normalized)? {
        return Ok(normalized);
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        format!(
            "CMake source alias does not identify the canonical source: {}",
            path.display()
        ),
    ))
}

#[cfg(all(not(windows), not(test)))]
pub(crate) fn cmake_source_path(path: &Path) -> io::Result<PathBuf> {
    Ok(path.to_path_buf())
}

pub(crate) fn prepare_verified_artifact(
    cache_root: &Path,
    artifact: &SourceArtifact<'_>,
    fetch: impl FnOnce() -> io::Result<Vec<u8>>,
) -> io::Result<VerifiedArtifact> {
    validate_artifact(artifact)?;
    ensure_directory(cache_root)?;

    let cache_key_dir = cache_root.join(artifact.cache_key);
    ensure_directory(&cache_key_dir)?;
    let digest_dir = cache_key_dir.join(artifact.sha256);
    ensure_directory(&digest_dir)?;
    let artifact_path = digest_dir.join(artifact.name);
    match fs::symlink_metadata(&artifact_path) {
        Ok(_) => {
            let bytes = read_verified_file(&artifact_path, artifact)?;
            return Ok(VerifiedArtifact {
                path: artifact_path,
                bytes,
                downloaded: false,
            });
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }

    let bytes = fetch()?;
    verify_artifact_bytes(&bytes, artifact)?;
    let mut temporary = NamedTempFile::new_in(&digest_dir)?;
    write_and_verify_temporary(&mut temporary, &bytes, artifact.sha256, artifact.name)?;

    match temporary.persist_noclobber(&artifact_path) {
        Ok(_) => {}
        Err(error) if error.error.kind() == io::ErrorKind::AlreadyExists => {
            let bytes = read_verified_file(&artifact_path, artifact)?;
            return Ok(VerifiedArtifact {
                path: artifact_path,
                bytes,
                downloaded: false,
            });
        }
        Err(error) => {
            return Err(error.error);
        }
    }

    Ok(VerifiedArtifact {
        path: artifact_path,
        bytes,
        downloaded: true,
    })
}

pub(crate) fn prepare_source_tree(
    third_party_dir: &Path,
    source_name: &str,
    archive: &VerifiedArtifact,
    extract: impl FnOnce(&[u8], &Path) -> io::Result<()>,
) -> io::Result<PreparedSourceTree> {
    validate_path_component(source_name, "source name")?;
    let owner_reference = third_party_dir.parent().unwrap_or(third_party_dir);
    ensure_private_cache_root(third_party_dir, owner_reference)?;

    let source_dir = third_party_dir.join(source_name);
    let staging = Builder::new()
        .prefix(&format!(".{source_name}."))
        .tempdir_in(third_party_dir)?;
    let staging_dir = staging.path();
    let staging_identity = capture_directory_identity(staging_dir)?;
    extract(&archive.bytes, staging_dir)?;
    verify_same_directory(staging_dir, &staging_identity)?;

    if !source_tree_is_complete(staging_dir) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "extracted {source_name} source is incomplete: missing {}",
                staging_dir.join(SOURCE_ROOT_MARKER).display()
            ),
        ));
    }

    remove_if_exists(&source_dir)?;
    verify_same_directory(staging_dir, &staging_identity)?;
    let staging_dir = staging.keep();
    if let Err(error) = fs::rename(&staging_dir, &source_dir) {
        let _ = remove_if_exists(&staging_dir);
        return Err(error);
    }

    Ok(PreparedSourceTree {
        path: source_dir,
        downloaded: archive.downloaded,
    })
}

#[cfg(unix)]
fn capture_directory_identity(path: &Path) -> io::Result<DirectoryIdentity> {
    fs::symlink_metadata(path)
}

#[cfg(windows)]
fn capture_directory_identity(path: &Path) -> io::Result<DirectoryIdentity> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() {
        return same_file::Handle::from_path(path);
    }
    Err(replaced_directory_error(path))
}

#[cfg(not(any(unix, windows)))]
fn capture_directory_identity(path: &Path) -> io::Result<DirectoryIdentity> {
    fs::symlink_metadata(path)
}

#[cfg(unix)]
fn verify_same_directory(path: &Path, expected: &DirectoryIdentity) -> io::Result<()> {
    let current = fs::symlink_metadata(path)?;
    if current.file_type().is_dir()
        && !current.file_type().is_symlink()
        && current.dev() == expected.dev()
        && current.ino() == expected.ino()
    {
        return Ok(());
    }
    Err(replaced_directory_error(path))
}

#[cfg(windows)]
fn verify_same_directory(path: &Path, expected: &DirectoryIdentity) -> io::Result<()> {
    let current = fs::symlink_metadata(path)?;
    if current.file_type().is_dir() && !current.file_type().is_symlink() {
        let current_identity = same_file::Handle::from_path(path)?;
        if current_identity.eq(expected) {
            return Ok(());
        }
    }
    Err(replaced_directory_error(path))
}

#[cfg(not(any(unix, windows)))]
fn verify_same_directory(path: &Path, _expected: &DirectoryIdentity) -> io::Result<()> {
    let current = fs::symlink_metadata(path)?;
    if current.file_type().is_dir() && !current.file_type().is_symlink() {
        return Ok(());
    }
    Err(replaced_directory_error(path))
}

fn replaced_directory_error(path: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!(
            "source staging directory was replaced during extraction: {}",
            path.display()
        ),
    )
}

pub(crate) fn copy_verified_artifact(artifact: &VerifiedArtifact, destination: &Path) -> io::Result<()> {
    let expected = sha256_hex(&artifact.bytes);
    match fs::symlink_metadata(destination) {
        Ok(_) => {
            let bytes = read_regular_file_exact(destination, artifact.bytes.len() as u64)?;
            return verify_bytes(&bytes, &expected, &destination.display().to_string());
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }

    let parent = destination.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("artifact destination has no parent: {}", destination.display()),
        )
    })?;
    ensure_directory(parent)?;

    let mut temporary = NamedTempFile::new_in(parent)?;
    write_and_verify_temporary(
        &mut temporary,
        &artifact.bytes,
        &expected,
        &destination.display().to_string(),
    )?;

    match temporary.persist_noclobber(destination) {
        Ok(_) => Ok(()),
        Err(error) if error.error.kind() == io::ErrorKind::AlreadyExists => {
            let bytes = read_regular_file_exact(destination, artifact.bytes.len() as u64)?;
            verify_bytes(&bytes, &expected, &destination.display().to_string())
        }
        Err(error) => Err(error.error),
    }
}

fn read_verified_file(path: &Path, artifact: &SourceArtifact<'_>) -> io::Result<Vec<u8>> {
    let bytes = read_regular_file_exact(path, artifact.expected_size)?;
    verify_artifact_bytes(&bytes, artifact)?;
    Ok(bytes)
}

fn read_regular_file_exact(path: &Path, expected_size: u64) -> io::Result<Vec<u8>> {
    read_regular_file_exact_after_check(path, expected_size, || Ok(()))
}

fn read_regular_file_exact_after_check(
    path: &Path,
    expected_size: u64,
    after_check: impl FnOnce() -> io::Result<()>,
) -> io::Result<Vec<u8>> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("artifact is not a regular file: {}", path.display()),
        ));
    }
    if metadata.len() != expected_size {
        return Err(unexpected_size(path, metadata.len(), expected_size));
    }

    after_check()?;
    let mut file = open_regular_file_nofollow(path)?;
    let opened_metadata = file.metadata()?;
    if !opened_metadata.file_type().is_file() || opened_metadata.file_type().is_symlink() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("artifact is not a regular file: {}", path.display()),
        ));
    }
    if opened_metadata.len() != expected_size {
        return Err(unexpected_size(path, opened_metadata.len(), expected_size));
    }
    read_exact_size(&mut file, expected_size, &path.display().to_string())
}

#[cfg(test)]
pub(crate) fn read_regular_file_exact_with_swap(
    path: &Path,
    expected_size: u64,
    after_check: impl FnOnce() -> io::Result<()>,
) -> io::Result<Vec<u8>> {
    read_regular_file_exact_after_check(path, expected_size, after_check)
}

#[cfg(unix)]
fn open_regular_file_nofollow(path: &Path) -> io::Result<fs::File> {
    fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
}

#[cfg(windows)]
fn open_regular_file_nofollow(path: &Path) -> io::Result<fs::File> {
    fs::OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
}

#[cfg(not(any(unix, windows)))]
fn open_regular_file_nofollow(path: &Path) -> io::Result<fs::File> {
    fs::File::open(path)
}

pub(crate) fn read_exact_size(reader: &mut impl Read, expected_size: u64, label: &str) -> io::Result<Vec<u8>> {
    let capacity = usize::try_from(expected_size).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{label} is too large for this platform"),
        )
    })?;
    let mut bytes = Vec::with_capacity(capacity);
    reader.take(expected_size.saturating_add(1)).read_to_end(&mut bytes)?;
    let actual_size = bytes.len() as u64;
    if actual_size != expected_size {
        return Err(unexpected_size(Path::new(label), actual_size, expected_size));
    }
    Ok(bytes)
}

fn write_and_verify_temporary(
    temporary: &mut NamedTempFile,
    bytes: &[u8],
    expected_sha256: &str,
    label: &str,
) -> io::Result<()> {
    temporary.write_all(bytes)?;
    temporary.flush()?;
    temporary.as_file_mut().rewind()?;
    let stored = read_exact_size(temporary.as_file_mut(), bytes.len() as u64, label)?;
    verify_bytes(&stored, expected_sha256, label)
}

fn verify_artifact_bytes(bytes: &[u8], artifact: &SourceArtifact<'_>) -> io::Result<()> {
    if bytes.len() as u64 != artifact.expected_size {
        return Err(unexpected_size(
            Path::new(artifact.name),
            bytes.len() as u64,
            artifact.expected_size,
        ));
    }
    verify_bytes(bytes, artifact.sha256, artifact.name)
}

fn unexpected_size(path: &Path, actual: u64, expected: u64) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!(
            "unexpected size for {}: got {actual} bytes, expected exactly {expected}",
            path.display()
        ),
    )
}

fn verify_bytes(bytes: &[u8], expected_sha256: &str, label: &str) -> io::Result<()> {
    let actual_sha256 = sha256_hex(bytes);
    if actual_sha256 == expected_sha256 {
        return Ok(());
    }

    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        format!("SHA-256 mismatch for {label}: expected {expected_sha256}, got {actual_sha256}"),
    ))
}

fn sha256_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let digest = Sha256::digest(bytes);
    let mut encoded = String::with_capacity(SHA256_HEX_LENGTH);
    for byte in digest {
        encoded.push(HEX[usize::from(byte >> 4)] as char);
        encoded.push(HEX[usize::from(byte & 0x0f)] as char);
    }
    encoded
}

fn validate_artifact(artifact: &SourceArtifact<'_>) -> io::Result<()> {
    validate_path_component(artifact.name, "artifact name")?;
    validate_path_component(artifact.cache_key, "artifact cache key")?;

    if artifact.sha256.len() != SHA256_HEX_LENGTH
        || !artifact
            .sha256
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid SHA-256 digest for {}", artifact.name),
        ));
    }

    Ok(())
}

fn validate_path_component(value: &str, label: &str) -> io::Result<()> {
    let valid = !value.is_empty()
        && value != "."
        && value != ".."
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'));
    if valid {
        return Ok(());
    }

    Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("invalid {label}: {value}"),
    ))
}

pub(crate) fn ensure_directory(path: &Path) -> io::Result<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty() && *parent != path)
    {
        ensure_directory(parent)?;
    }

    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() => Ok(()),
        Ok(metadata) if trusted_directory_symlink(path, &metadata) => Ok(()),
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("cache path is not a regular directory: {}", path.display()),
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => match fs::create_dir(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => ensure_directory(path),
            Err(error) => Err(error),
        },
        Err(error) => Err(error),
    }
}

pub(crate) fn ensure_private_cache_root(path: &Path, owner_reference: &Path) -> io::Result<()> {
    ensure_directory(path)?;
    verify_private_cache_root(path, owner_reference)
}

#[cfg(unix)]
fn verify_private_cache_root(path: &Path, owner_reference: &Path) -> io::Result<()> {
    let expected_owner = fs::metadata(owner_reference)?.uid();
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() || metadata.uid() != expected_owner {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "automatic cache root is not owned by the Cargo build user: {}",
                path.display()
            ),
        ));
    }

    fs::set_permissions(path, fs::Permissions::from_mode(PRIVATE_DIRECTORY_MODE))?;
    let secured = fs::symlink_metadata(path)?;
    if secured.mode() & NON_OWNER_PERMISSION_MASK != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("automatic cache root is not private: {}", path.display()),
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
fn verify_private_cache_root(path: &Path, _owner_reference: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() {
        return Ok(());
    }
    Err(io::Error::new(
        io::ErrorKind::PermissionDenied,
        format!("automatic cache root is not a regular directory: {}", path.display()),
    ))
}

#[cfg(unix)]
fn trusted_directory_symlink(path: &Path, metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
        && metadata.uid() == 0
        && fs::metadata(path).is_ok_and(|target| target.file_type().is_dir())
}

#[cfg(not(unix))]
fn trusted_directory_symlink(_path: &Path, _metadata: &fs::Metadata) -> bool {
    false
}

fn remove_if_exists(path: &Path) -> io::Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };

    if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
}
