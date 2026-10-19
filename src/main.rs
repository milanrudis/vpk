mod database;
use database::InstalledPackage;
use serde::Deserialize;
use sha2::{Digest, Sha256};

const REPO_INDEX_URL: &str =
    "https://raw.githubusercontent.com/milanrudis/vast-packages/main/repo/index.toml";

#[derive(Debug, Deserialize, Clone)]
struct RepoIndex {
    package: Vec<RepoPackage>,
}

#[derive(Debug, Deserialize, Clone)]
struct RepoPackage {
    name: String,
    version: String,
    description: String,
    url: String,
    sha256: String,
}

fn is_newer_version(installed: &str, available: &str) -> Result<bool, String> {
    let installed_version = semver::Version::parse(installed)
        .map_err(|e| format!("Invalid installed version '{installed}': {e}"))?;

    let available_version = semver::Version::parse(available)
        .map_err(|e| format!("Invalid repository version '{available}': {e}"))?;

    Ok(available_version > installed_version)
}

use std::{
    env,
    fs::{self, File, OpenOptions},
    io::{self, Read},
    path::{Component, Path, PathBuf},
    process,
};
use tar::{Archive, Builder};

#[derive(Deserialize)]
struct Package {
    name: String,
    version: String,
    description: String,
    #[serde(default)]
    dependencies: Vec<String>,
}

// Read package information from a compressed .vpk archive.
fn inspect_package(path: &str) -> Result<Package, String> {
    let file = File::open(path).map_err(|e| format!("Could not open '{path}': {e}"))?;

    let decoder =
        zstd::Decoder::new(file).map_err(|e| format!("Could not decompress package: {e}"))?;

    let mut archive = Archive::new(decoder);

    let entries = archive
        .entries()
        .map_err(|e| format!("Could not read archive: {e}"))?;

    for entry in entries {
        let mut entry = entry.map_err(|e| format!("Could not read archive entry: {e}"))?;

        let path = entry
            .path()
            .map_err(|e| format!("Invalid archive path: {e}"))?
            .into_owned();

        if path == Path::new("manifest.toml") {
            if !entry.header().entry_type().is_file() {
                return Err("Package manifest is not a regular file".into());
            }

            let mut contents = String::new();

            entry
                .read_to_string(&mut contents)
                .map_err(|e| format!("Could not read manifest: {e}"))?;

            return toml::from_str(&contents).map_err(|e| format!("Invalid package manifest: {e}"));
        }
    }

    Err("Package archive does not contain manifest.toml".into())
}

// Create a compressed .vpk archive.
fn pack_package(directory: &str, output: &str) -> io::Result<()> {
    let root = Path::new(directory);

    if !root.join("manifest.toml").is_file() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "manifest.toml was not found in the package directory",
        ));
    }

    if !root.join("data").is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "data directory was not found in the package directory",
        ));
    }

    let file = File::create(output)?;
    let encoder = zstd::Encoder::new(file, 3)?;
    let mut archive = Builder::new(encoder);

    archive.append_path_with_name(root.join("manifest.toml"), "manifest.toml")?;

    archive.append_dir_all("data", root.join("data"))?;

    let encoder = archive.into_inner()?;
    encoder.finish()?;

    Ok(())
}

// Only accept paths beginning with data/.
// Reject absolute paths, parent traversal, and unusual path components.
fn package_data_path(path: &Path) -> Result<PathBuf, String> {
    let mut components = path.components();

    match components.next() {
        Some(Component::Normal(name)) if name == "data" => {}
        _ => {
            return Err(format!("Unexpected path in package: {}", path.display()));
        }
    }

    let mut relative = PathBuf::new();

    for component in components {
        match component {
            Component::Normal(name) => relative.push(name),
            _ => {
                return Err(format!("Unsafe path in package: {}", path.display()));
            }
        }
    }

    Ok(relative)
}

fn validate_package_archive(
    archive_path: &Path,
    expected_name: &str,
    expected_version: &str,
) -> Result<Package, String> {
    use std::collections::HashMap;

    let file =
        File::open(archive_path).map_err(|e| format!("Could not open package archive: {e}"))?;
    let decoder = zstd::Decoder::new(file)
        .map_err(|e| format!("Could not decompress package archive: {e}"))?;
    let mut archive = Archive::new(decoder);

    let entries = archive
        .entries()
        .map_err(|e| format!("Could not read package archive: {e}"))?;

    let mut manifest: Option<Package> = None;
    let mut paths: HashMap<PathBuf, bool> = HashMap::new();

    for entry_result in entries {
        let mut entry = entry_result.map_err(|e| format!("Invalid archive entry: {e}"))?;

        let path = entry
            .path()
            .map_err(|e| format!("Invalid archive path: {e}"))?
            .into_owned();

        let entry_type = entry.header().entry_type();

        if path == Path::new("manifest.toml") {
            if manifest.is_some() {
                return Err("Package contains multiple manifests".into());
            }

            if !entry_type.is_file() {
                return Err("Package manifest must be a regular file".into());
            }

            let mut contents = String::new();
            entry
                .read_to_string(&mut contents)
                .map_err(|e| format!("Could not read package manifest: {e}"))?;

            let parsed: Package =
                toml::from_str(&contents).map_err(|e| format!("Invalid package manifest: {e}"))?;

            if parsed.name != expected_name || parsed.version != expected_version {
                return Err("Package manifest does not match the expected name and version".into());
            }

            if !safe_package_component(&parsed.name) || !safe_package_component(&parsed.version) {
                return Err("Package manifest contains an invalid name or version".into());
            }

            manifest = Some(parsed);
            continue;
        }

        let is_file = entry_type.is_file();
        let is_dir = entry_type.is_dir();

        if !is_file && !is_dir {
            return Err(format!(
                "Unsupported entry type in package: {}",
                path.display()
            ));
        }

        let relative = package_data_path(&path)?;

        if relative.as_os_str().is_empty() {
            if is_file {
                return Err("Package contains a file named 'data'".into());
            }
            continue;
        }

        if paths.contains_key(&relative) {
            return Err(format!(
                "Package contains a duplicate path: {}",
                path.display()
            ));
        }

        // No parent component may be a file.
        let mut parent = relative.parent();
        while let Some(parent_path) = parent {
            if paths.get(parent_path) == Some(&true) {
                return Err(format!(
                    "Package path conflicts with a file: {}",
                    path.display()
                ));
            }
            parent = parent_path.parent();
        }

        // A new file cannot replace a directory already declared.
        if is_file && paths.keys().any(|existing| existing.starts_with(&relative)) {
            return Err(format!(
                "Package file conflicts with an existing directory: {}",
                path.display()
            ));
        }

        paths.insert(relative, is_file);
    }

    manifest.ok_or_else(|| "Package archive is missing manifest.toml".into())
}

fn package_archive_files(archive_path: &Path) -> Result<Vec<String>, String> {
    let file =
        File::open(archive_path).map_err(|e| format!("Could not open package archive: {e}"))?;

    let decoder = zstd::Decoder::new(file)
        .map_err(|e| format!("Could not decompress package archive: {e}"))?;

    let mut archive = Archive::new(decoder);
    let entries = archive
        .entries()
        .map_err(|e| format!("Could not read package archive: {e}"))?;

    let mut files = Vec::new();

    for entry_result in entries {
        let entry = entry_result.map_err(|e| format!("Invalid archive entry: {e}"))?;

        let path = entry
            .path()
            .map_err(|e| format!("Invalid archive path: {e}"))?
            .into_owned();

        if path == Path::new("manifest.toml") || entry.header().entry_type().is_dir() {
            continue;
        }

        if !entry.header().entry_type().is_file() {
            return Err(format!(
                "Unsupported archive entry type: {}",
                path.display()
            ));
        }

        let relative = package_data_path(&path)?;

        if relative.as_os_str().is_empty() {
            return Err("Package contains a file named 'data'".into());
        }

        files.push(relative.to_string_lossy().into_owned());
    }

    files.sort();
    Ok(files)
}

struct StagedPackageDirectory {
    path: PathBuf,
}

impl Drop for StagedPackageDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn create_staging_directory() -> Result<StagedPackageDirectory, String> {
    let base = env::temp_dir();

    for attempt in 0..100 {
        let path = base.join(format!("vpk-stage-{}-{attempt}", process::id()));

        let mut builder = fs::DirBuilder::new();

        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }

        match builder.create(&path) {
            Ok(()) => return Ok(StagedPackageDirectory { path }),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(format!("Could not create staging directory: {error}"));
            }
        }
    }

    Err("Could not find an available staging directory name".into())
}

fn stage_package_archive(
    archive_path: &Path,
    expected_name: &str,
    expected_version: &str,
) -> Result<(StagedPackageDirectory, Vec<String>), String> {
    validate_package_archive(archive_path, expected_name, expected_version)?;

    let staged = create_staging_directory()?;
    let mut created_dirs: Vec<PathBuf> = Vec::new();
    let mut staged_files: Vec<String> = Vec::new();

    let result = (|| -> Result<(), String> {
        let file =
            File::open(archive_path).map_err(|e| format!("Could not open package archive: {e}"))?;
        let decoder = zstd::Decoder::new(file)
            .map_err(|e| format!("Could not decompress package archive: {e}"))?;
        let mut archive = Archive::new(decoder);

        let entries = archive
            .entries()
            .map_err(|e| format!("Could not read package archive: {e}"))?;

        for entry_result in entries {
            let mut entry =
                entry_result.map_err(|e| format!("Could not read archive entry: {e}"))?;

            let archive_path = entry
                .path()
                .map_err(|e| format!("Invalid archive path: {e}"))?
                .into_owned();

            if archive_path == Path::new("manifest.toml") {
                continue;
            }

            let relative = package_data_path(&archive_path)?;
            let is_directory = entry.header().entry_type().is_dir();
            let is_file = entry.header().entry_type().is_file();

            if relative.as_os_str().is_empty() {
                if is_directory {
                    continue;
                }
                return Err("Package contains a file named 'data'".into());
            }

            if is_directory {
                ensure_directory(&staged.path, &relative, &mut created_dirs)
                    .map_err(|e| format!("Could not stage directory: {e}"))?;
            } else if is_file {
                let parent = relative.parent().unwrap_or(Path::new(""));

                ensure_directory(&staged.path, parent, &mut created_dirs)
                    .map_err(|e| format!("Could not prepare staging directory: {e}"))?;

                let destination = staged.path.join(&relative);

                let mut output = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&destination)
                    .map_err(|e| format!("Could not create staged file: {e}"))?;

                io::copy(&mut entry, &mut output)
                    .map_err(|e| format!("Could not write staged file: {e}"))?;

                staged_files.push(relative.to_string_lossy().into_owned());
            } else {
                return Err(format!(
                    "Unsupported archive entry type: {}",
                    archive_path.display()
                ));
            }
        }

        staged_files.sort();
        Ok(())
    })();

    if let Err(error) = result {
        // The directory guard removes all partial extraction on return.
        return Err(error);
    }

    Ok((staged, staged_files))
}

fn ensure_directory(
    root: &Path,
    relative: &Path,
    created_dirs: &mut Vec<PathBuf>,
) -> io::Result<()> {
    let mut current = root.to_path_buf();

    for component in relative.components() {
        let name = match component {
            Component::Normal(name) => name,
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "Unsafe directory path",
                ));
            }
        };

        current.push(name);

        match fs::symlink_metadata(&current) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() || !metadata.is_dir() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!(
                            "A path component is not a safe directory: {}",
                            current.display()
                        ),
                    ));
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                fs::create_dir(&current)?;
                created_dirs.push(current.clone());
            }
            Err(error) => return Err(error),
        }
    }

    Ok(())
}

fn install_staged_file(
    staging_root: &Path,
    root: &Path,
    relative: &Path,
    created_files: &mut Vec<PathBuf>,
    created_dirs: &mut Vec<PathBuf>,
) -> Result<(), String> {
    let safe_path = package_data_path(&PathBuf::from("data").join(relative))?;

    if safe_path != relative {
        return Err(format!("Unsafe staged file path: {}", relative.display()));
    }

    let source = staging_root.join(&safe_path);
    let source_metadata =
        fs::symlink_metadata(&source).map_err(|e| format!("Could not inspect staged file: {e}"))?;

    if source_metadata.file_type().is_symlink() || !source_metadata.is_file() {
        return Err(format!(
            "Staged source is not a regular file: {}",
            source.display()
        ));
    }

    if check_upgrade_path(root, relative)? {
        return Err(format!(
            "Refusing to overwrite existing file: {}",
            relative.display()
        ));
    }

    let parent = safe_path.parent().unwrap_or(Path::new(""));
    ensure_directory(root, parent, created_dirs)
        .map_err(|e| format!("Could not prepare upgrade destination: {e}"))?;

    let destination = root.join(&safe_path);

    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&destination)
        .map_err(|e| format!("Could not create upgrade destination: {e}"))?;

    created_files.push(destination.clone());

    let mut input = File::open(&source).map_err(|e| format!("Could not open staged file: {e}"))?;

    io::copy(&mut input, &mut output)
        .map_err(|e| format!("Could not write upgrade destination: {e}"))?;

    output
        .sync_all()
        .map_err(|e| format!("Could not sync upgrade destination: {e}"))?;

    Ok(())
}

fn check_upgrade_path(root: &Path, relative: &Path) -> Result<bool, String> {
    let root_metadata =
        fs::symlink_metadata(root).map_err(|e| format!("Could not inspect install root: {e}"))?;

    if root_metadata.file_type().is_symlink() || !root_metadata.is_dir() {
        return Err("Install root is not a safe directory".into());
    }
    let mut current = root.to_path_buf();
    let mut components = relative.components().peekable();

    while let Some(component) = components.next() {
        let name = match component {
            Component::Normal(name) => name,
            _ => return Err(format!("Unsafe upgrade path: {}", relative.display())),
        };

        current.push(name);

        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(format!(
                    "Upgrade path contains a symlink: {}",
                    current.display()
                ));
            }
            Ok(metadata) if components.peek().is_some() && !metadata.is_dir() => {
                return Err(format!(
                    "Upgrade parent is not a directory: {}",
                    current.display()
                ));
            }
            Ok(metadata) if components.peek().is_none() && !metadata.is_file() => {
                return Err(format!(
                    "Upgrade destination is not a regular file: {}",
                    current.display()
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(false);
            }
            Err(error) => {
                return Err(format!(
                    "Could not inspect upgrade path '{}': {error}",
                    current.display()
                ));
            }
        }
    }

    Ok(true)
}

fn preflight_upgrade_files(
    root: &Path,
    package_name: &str,
    files: &[String],
) -> Result<(), String> {
    for file in files {
        let relative = Path::new(file);

        // Revalidate the relative path before checking the filesystem.
        let safe_path = package_data_path(&PathBuf::from("data").join(relative))?;

        if safe_path.to_string_lossy() != *file {
            return Err(format!("Unsafe upgrade path: {file}"));
        }

        if database::other_package_owns_file(root, package_name, file)
            .map_err(|e| format!("Could not check file ownership: {e}"))?
        {
            return Err(format!(
                "Cannot upgrade '{package_name}': file '{file}' is claimed by another package"
            ));
        }

        check_upgrade_path(root, relative)?;
    }

    Ok(())
}

fn obsolete_package_files(old_files: &[String], new_files: &[String]) -> Vec<String> {
    let mut obsolete: Vec<String> = old_files
        .iter()
        .filter(|old_file| !new_files.contains(old_file))
        .cloned()
        .collect();

    obsolete.sort();
    obsolete
}

fn upgrade_files_to_backup(old_files: &[String]) -> Vec<String> {
    let mut files = old_files.to_vec();
    files.sort();
    files.dedup();
    files
}

#[cfg(unix)]
fn same_filesystem(first: &Path, second: &Path) -> Result<bool, String> {
    use std::os::unix::fs::MetadataExt;

    let first_metadata =
        fs::metadata(first).map_err(|e| format!("Could not inspect '{}': {e}", first.display()))?;

    let second_metadata = fs::metadata(second)
        .map_err(|e| format!("Could not inspect '{}': {e}", second.display()))?;

    Ok(first_metadata.dev() == second_metadata.dev())
}

fn create_upgrade_backup_dir(root: &Path) -> Result<PathBuf, String> {
    let parent = root
        .parent()
        .ok_or_else(|| "Install root has no parent directory".to_string())?;

    let parent_metadata = fs::symlink_metadata(parent)
        .map_err(|e| format!("Could not inspect install root parent: {e}"))?;

    if parent_metadata.file_type().is_symlink() || !parent_metadata.is_dir() {
        return Err("Install root parent is not a safe directory".into());
    }

    let backup_root = parent.join("vpk-upgrade-backups");

    if !same_filesystem(root, parent)? {
        return Err("Install root and upgrade backups must be on the same filesystem".into());
    }

    match fs::symlink_metadata(&backup_root) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err("Upgrade backup path is not a safe directory".into());
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let mut builder = fs::DirBuilder::new();

            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }

            builder
                .create(&backup_root)
                .map_err(|e| format!("Could not create upgrade backup directory: {e}"))?;
        }
        Err(error) => {
            return Err(format!(
                "Could not inspect upgrade backup directory: {error}"
            ));
        }
    }

    if !same_filesystem(root, &backup_root)? {
        return Err("Install root and upgrade backups must be on the same filesystem".into());
    }

    for attempt in 0..100 {
        let backup_dir = backup_root.join(format!("backup-{}-{attempt}", std::process::id()));

        let mut builder = fs::DirBuilder::new();

        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }

        match builder.create(&backup_dir) {
            Ok(()) => return Ok(backup_dir),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(format!("Could not create unique upgrade backup: {error}"));
            }
        }
    }

    Err("Could not create a unique upgrade backup directory after 100 attempts".into())
}

fn backup_upgrade_file(root: &Path, backup_dir: &Path, relative: &Path) -> Result<(), String> {
    if !check_upgrade_path(root, relative)? {
        return Err(format!(
            "Cannot back up missing file: {}",
            relative.display()
        ));
    }

    let backup_metadata = fs::symlink_metadata(backup_dir)
        .map_err(|e| format!("Could not inspect backup directory: {e}"))?;

    if backup_metadata.file_type().is_symlink() || !backup_metadata.is_dir() {
        return Err("Backup destination is not a safe directory".into());
    }

    let mut backup = backup_dir.to_path_buf();

    for component in relative.components() {
        let name = match component {
            Component::Normal(name) => name,
            _ => return Err(format!("Unsafe backup path: {}", relative.display())),
        };

        backup.push(name);
    }

    let mut current = backup_dir.to_path_buf();

    for component in relative.parent().unwrap_or(Path::new("")).components() {
        let name = match component {
            Component::Normal(name) => name,
            _ => return Err(format!("Unsafe backup path: {}", relative.display())),
        };

        current.push(name);

        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                return Err(format!("Unsafe backup directory: {}", current.display()));
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                fs::create_dir(&current).map_err(|e| {
                    format!(
                        "Could not create backup subdirectory '{}': {e}",
                        current.display()
                    )
                })?;
            }
            Err(error) => {
                return Err(format!(
                    "Could not inspect backup subdirectory '{}': {error}",
                    current.display()
                ));
            }
        }
    }

    match fs::symlink_metadata(&backup) {
        Ok(_) => return Err(format!("Backup already exists: {}", backup.display())),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(format!(
                "Could not inspect backup destination '{}': {error}",
                backup.display()
            ));
        }
    }

    fs::rename(root.join(relative), &backup)
        .map_err(|e| format!("Could not back up '{}': {e}", relative.display()))?;

    Ok(())
}

fn backup_existing_package_files(
    root: &Path,
    backup_dir: &Path,
    package_name: &str,
    old_files: &[String],
) -> Result<Vec<String>, String> {
    let mut backed_up: Vec<String> = Vec::new();

    let result = (|| -> Result<(), String> {
        for file in upgrade_files_to_backup(old_files) {
            let relative = Path::new(&file);

            let safe_path = package_data_path(&PathBuf::from("data").join(relative))?;
            if safe_path.to_string_lossy() != file {
                return Err(format!("Unsafe upgrade path: {file}"));
            }

            if database::other_package_owns_file(root, package_name, &file)
                .map_err(|e| format!("Could not check file ownership: {e}"))?
            {
                return Err(format!(
                    "Cannot upgrade '{package_name}': file '{file}' is claimed by another package"
                ));
            }

            if !check_upgrade_path(root, relative)? {
                continue;
            }

            backup_upgrade_file(root, backup_dir, relative)?;
            backed_up.push(file);
        }

        Ok(())
    })();

    if let Err(error) = result {
        let mut rollback_errors = Vec::new();

        for file in backed_up.iter().rev() {
            if let Err(restore_error) = restore_upgrade_file(root, backup_dir, Path::new(file)) {
                rollback_errors.push(format!("'{file}': {restore_error}"));
            }
        }

        if rollback_errors.is_empty() {
            return Err(error);
        }

        return Err(format!(
            "{error}; rollback failures: {}; remaining backup files may be in '{}'",
            rollback_errors.join("; "),
            backup_dir.display()
        ));
    }

    Ok(backed_up)
}

fn check_existing_parent_directories(base: &Path, relative: &Path) -> Result<(), String> {
    let mut current = base.to_path_buf();
    let mut components = relative.components().peekable();

    while let Some(component) = components.next() {
        let name = match component {
            Component::Normal(name) => name,
            _ => return Err(format!("Unsafe relative path: {}", relative.display())),
        };

        current.push(name);

        // The last component is the file, not a parent directory.
        if components.peek().is_none() {
            break;
        }

        let metadata = fs::symlink_metadata(&current).map_err(|e| {
            format!(
                "Could not inspect parent directory '{}': {e}",
                current.display()
            )
        })?;

        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(format!("Unsafe parent directory: {}", current.display()));
        }
    }

    Ok(())
}

fn restore_upgrade_file(root: &Path, backup_dir: &Path, relative: &Path) -> Result<(), String> {
    let safe_path = package_data_path(&PathBuf::from("data").join(relative))?;

    if safe_path != relative {
        return Err(format!("Unsafe restore path: {}", relative.display()));
    }

    let destination = root.join(&safe_path);
    let backup = backup_dir.join(&safe_path);

    if check_upgrade_path(root, relative)? {
        return Err(format!(
            "Cannot restore over an existing file: {}",
            relative.display()
        ));
    }

    check_existing_parent_directories(backup_dir, &safe_path)?;

    let backup_metadata =
        fs::symlink_metadata(&backup).map_err(|e| format!("Could not inspect backup file: {e}"))?;

    if backup_metadata.file_type().is_symlink() || !backup_metadata.is_file() {
        return Err(format!(
            "Backup is not a safe regular file: {}",
            backup.display()
        ));
    }

    check_existing_parent_directories(root, &safe_path)?;
    check_upgrade_path(root, relative)?;

    fs::rename(&backup, &destination)
        .map_err(|e| format!("Could not restore '{}': {e}", relative.display()))?;

    Ok(())
}

fn rollback_installed_files(created_files: &[PathBuf], created_dirs: &[PathBuf]) -> Vec<String> {
    let mut errors = Vec::new();

    for path in created_files.iter().rev() {
        match fs::remove_file(path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => errors.push(format!(
                "Could not remove newly installed file '{}': {error}",
                path.display()
            )),
        }
    }

    for path in created_dirs.iter().rev() {
        match fs::remove_dir(path) {
            Ok(()) => {}
            Err(error)
                if error.kind() == io::ErrorKind::NotFound
                    || error.kind() == io::ErrorKind::DirectoryNotEmpty => {}
            Err(error) => errors.push(format!(
                "Could not remove newly created directory '{}': {error}",
                path.display()
            )),
        }
    }

    errors
}

fn install_package(package_path: &str, target: &str) -> Result<Package, String> {
    let package = inspect_package(package_path)?;

    fs::create_dir_all(target).map_err(|e| format!("Could not create install directory: {e}"))?;

    let root = fs::canonicalize(target)
        .map_err(|e| format!("Could not resolve install directory: {e}"))?;

    let mut installed_files: Vec<String> = Vec::new();
    let mut created_files: Vec<PathBuf> = Vec::new();
    let mut created_dirs: Vec<PathBuf> = Vec::new();

    let result = (|| -> Result<(), String> {
        let file = File::open(package_path).map_err(|e| format!("Could not open package: {e}"))?;

        let decoder =
            zstd::Decoder::new(file).map_err(|e| format!("Could not decompress package: {e}"))?;

        let mut archive = Archive::new(decoder);
        let entries = archive
            .entries()
            .map_err(|e| format!("Could not read archive: {e}"))?;

        for entry in entries {
            let mut entry = entry.map_err(|e| format!("Could not read archive entry: {e}"))?;

            let archive_path = entry
                .path()
                .map_err(|e| format!("Invalid archive path: {e}"))?
                .into_owned();

            if archive_path == Path::new("manifest.toml") {
                if !entry.header().entry_type().is_file() {
                    return Err("Package manifest is not a regular file".into());
                }
                continue;
            }

            let relative = package_data_path(&archive_path)?;
            let is_directory = entry.header().entry_type().is_dir();
            let is_file = entry.header().entry_type().is_file();

            if is_directory {
                ensure_directory(&root, &relative, &mut created_dirs).map_err(|e| {
                    format!("Could not create directory '{}': {e}", relative.display())
                })?;
            } else if is_file {
                let parent = relative.parent().unwrap_or(Path::new(""));

                ensure_directory(&root, parent, &mut created_dirs).map_err(|e| {
                    format!(
                        "Could not prepare directory for '{}': {e}",
                        relative.display()
                    )
                })?;

                let destination = root.join(&relative);

                let mut output = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&destination)
                    .map_err(|e| {
                        format!(
                            "Could not create '{}': {e}. Existing files are not overwritten.",
                            destination.display()
                        )
                    })?;

                // Track the file immediately so even a partial write
                // can be rolled back.
                created_files.push(destination.clone());

                #[cfg(unix)]
                let file_mode = entry
                    .header()
                    .mode()
                    .map_err(|e| {
                        format!(
                            "Could not read permissions for '{}': {e}",
                            relative.display()
                        )
                    })?
                    & 0o777;

                io::copy(&mut entry, &mut output)
                    .map_err(|e| format!("Could not write '{}': {e}", destination.display()))?;

                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;

                    fs::set_permissions(
                        &destination,
                        fs::Permissions::from_mode(file_mode),
                    )
                    .map_err(|e| {
                        format!(
                            "Could not set permissions for '{}': {e}",
                            destination.display()
                        )
                    })?;
                }

                installed_files.push(relative.to_string_lossy().into_owned());
            } else {
                return Err(format!(
                    "Unsupported archive entry type: {}",
                    archive_path.display()
                ));
            }
        }

        database::record_install(
            &root,
            InstalledPackage {
                name: package.name.clone(),
                version: package.version.clone(),
                description: package.description.clone(),
                files: installed_files.clone(),
                dependencies: package.dependencies.clone(),
            },
        )
        .map_err(|e| format!("Could not save installed-package record: {e}"))?;

        Ok(())
    })();

    if let Err(error) = result {
        // Remove only files created by this installation.
        for path in created_files.iter().rev() {
            if let Err(cleanup_error) = fs::remove_file(path) {
                if cleanup_error.kind() != io::ErrorKind::NotFound {
                    eprintln!(
                        "vpk: warning: could not roll back '{}': {cleanup_error}",
                        path.display()
                    );
                }
            }
        }

        // Remove only newly created directories, and only if empty.
        for path in created_dirs.iter().rev() {
            if let Err(cleanup_error) = fs::remove_dir(path) {
                if cleanup_error.kind() != io::ErrorKind::NotFound
                    && cleanup_error.kind() != io::ErrorKind::DirectoryNotEmpty
                {
                    eprintln!(
                        "vpk: warning: could not clean up '{}': {cleanup_error}",
                        path.display()
                    );
                }
            }
        }

        return Err(error);
    }

    Ok(package)
}

fn repository_index_path() -> Result<PathBuf, String> {
    let home =
        env::var_os("HOME").ok_or_else(|| "HOME environment variable is not set".to_string())?;

    Ok(PathBuf::from(home)
        .join(".cache")
        .join("vpk")
        .join("index.toml"))
}

fn update_repository_index() -> Result<RepoIndex, String> {
    let response = ureq::get(REPO_INDEX_URL)
        .call()
        .map_err(|e| format!("could not fetch repository index: {e}"))?;

    let mut contents = String::new();

    response
        .into_reader()
        .read_to_string(&mut contents)
        .map_err(|e| format!("could not read repository index: {e}"))?;

    // Validate the new index before touching the existing cache.
    let index: RepoIndex =
        toml::from_str(&contents).map_err(|e| format!("could not parse repository index: {e}"))?;

    let path = repository_index_path()?;
    let parent = path
        .parent()
        .ok_or_else(|| "invalid repository cache path".to_string())?;

    fs::create_dir_all(parent).map_err(|e| format!("could not create repository cache: {e}"))?;

    // Write to a temporary file, then atomically replace the cached index.
    let temp_path = path.with_extension(format!("toml.tmp.{}", process::id()));

    fs::write(&temp_path, contents)
        .map_err(|e| format!("could not write temporary repository index: {e}"))?;

    if let Err(error) = fs::rename(&temp_path, &path) {
        let _ = fs::remove_file(&temp_path);
        return Err(format!("could not save repository index: {error}"));
    }

    Ok(index)
}

fn fetch_repository_index() -> Result<RepoIndex, String> {
    let path = repository_index_path()?;

    if path.is_file() {
        let contents = fs::read_to_string(&path)
            .map_err(|e| format!("could not read cached repository index: {e}"))?;

        return toml::from_str(&contents)
            .map_err(|e| format!("could not parse cached repository index: {e}"));
    }

    // First use: fetch and cache the repository index automatically.
    update_repository_index()
}

fn safe_package_component(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.')
}

fn install_repository_package_recursive(
    name: &str,
    index: &RepoIndex,
    target: &Path,
    visiting: &mut Vec<String>,
) -> Result<(Package, bool), String> {
    if !safe_package_component(name) {
        return Err(format!("Invalid package name: '{name}'"));
    }

    if visiting.iter().any(|item| item == name) {
        let mut cycle = visiting.join(" -> ");
        cycle.push_str(" -> ");
        cycle.push_str(name);
        return Err(format!("Circular dependency detected: {cycle}"));
    }

    if let Some(installed) = database::get_installed(target, name)
        .map_err(|e| format!("Could not check installed package '{name}': {e}"))?
    {
        println!(
            "{name} {} is already installed; skipping.",
            installed.version
        );

        return Ok((
            Package {
                name: installed.name,
                version: installed.version,
                description: installed.description,
                dependencies: Vec::new(),
            },
            false,
        ));
    }

    let repo_package = index
        .package
        .iter()
        .find(|package| package.name == name)
        .cloned()
        .ok_or_else(|| format!("Package '{name}' was not found in the repository"))?;

    if !safe_package_component(&repo_package.version) {
        return Err("Invalid package version in repository index".into());
    }

    if !repo_package.url.starts_with("https://") {
        return Err("Package URL must use HTTPS".into());
    }

    visiting.push(name.to_string());

    let result = (|| -> Result<(Package, bool), String> {
        println!(
            "Downloading {} {}...",
            repo_package.name, repo_package.version
        );

        let response = ureq::get(&repo_package.url)
            .call()
            .map_err(|e| format!("Could not download package: {e}"))?;

        let mut bytes = Vec::new();

        response
            .into_reader()
            .read_to_end(&mut bytes)
            .map_err(|e| format!("Could not read downloaded package: {e}"))?;

        let actual_hash = format!("{:x}", Sha256::digest(&bytes));
        let expected_hash = repo_package.sha256.trim().to_ascii_lowercase();

        if actual_hash != expected_hash {
            return Err(format!(
                "SHA-256 mismatch! Expected {}, got {}",
                expected_hash, actual_hash
            ));
        }

        println!("Checksum verified.");

        let home = env::var_os("HOME")
            .map(PathBuf::from)
            .ok_or_else(|| "Could not determine your home directory".to_string())?;

        let cache = home.join(".cache/vpk/packages");

        fs::create_dir_all(&cache).map_err(|e| format!("Could not create package cache: {e}"))?;

        let archive_path = cache.join(format!(
            "{}-{}-{}.vpk",
            name,
            repo_package.version,
            process::id()
        ));

        let mut archive_file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&archive_path)
            .map_err(|e| format!("Could not create temporary package file: {e}"))?;

        let write_result = (|| -> io::Result<()> {
            use io::Write;
            archive_file.write_all(&bytes)?;
            archive_file.flush()
        })();

        if let Err(error) = write_result {
            drop(archive_file);
            let _ = fs::remove_file(&archive_path);
            return Err(format!("Could not save downloaded package: {error}"));
        }

        drop(archive_file);

        let package_result = (|| -> Result<(Package, bool), String> {
            let archive_string = archive_path
                .to_str()
                .ok_or_else(|| "Invalid temporary package path".to_string())?;

            let manifest = inspect_package(archive_string)?;

            if manifest.name != repo_package.name || manifest.version != repo_package.version {
                return Err("Downloaded package manifest does not match repository index".into());
            }

            // Install dependencies before installing this package.
            for dependency in &manifest.dependencies {
                println!("Resolving dependency '{dependency}'...");

                install_repository_package_recursive(dependency, index, target, visiting)?;
            }

            let target_string = target
                .to_str()
                .ok_or_else(|| "Invalid installation path".to_string())?;

            let installed = install_package(archive_string, target_string)?;

            Ok((installed, true))
        })();

        let _ = fs::remove_file(&archive_path);

        package_result
    })();

    visiting.pop();

    result
}

fn download_verified_package(repo_package: &RepoPackage) -> Result<(Package, PathBuf), String> {
    if !safe_package_component(&repo_package.name) || !safe_package_component(&repo_package.version)
    {
        return Err("Invalid package name or version in repository index".into());
    }

    if !repo_package.url.starts_with("https://") {
        return Err("Package URL must use HTTPS".into());
    }

    println!(
        "Downloading {} {}...",
        repo_package.name, repo_package.version
    );

    let response = ureq::get(&repo_package.url)
        .call()
        .map_err(|e| format!("Could not download package: {e}"))?;

    let mut bytes = Vec::new();
    response
        .into_reader()
        .read_to_end(&mut bytes)
        .map_err(|e| format!("Could not read downloaded package: {e}"))?;

    let actual_hash = format!("{:x}", Sha256::digest(&bytes));
    let expected_hash = repo_package.sha256.trim().to_ascii_lowercase();

    if actual_hash != expected_hash {
        return Err(format!(
            "SHA-256 mismatch! Expected {}, got {}",
            expected_hash, actual_hash
        ));
    }

    println!("Checksum verified.");

    let home = env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| "Could not determine your home directory".to_string())?;

    let cache = home.join(".cache/vpk/packages");
    fs::create_dir_all(&cache).map_err(|e| format!("Could not create package cache: {e}"))?;

    let archive_path = cache.join(format!(
        "{}-{}-{}-upgrade.vpk",
        repo_package.name,
        repo_package.version,
        process::id()
    ));

    let mut archive_file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&archive_path)
        .map_err(|e| format!("Could not create temporary package file: {e}"))?;

    use io::Write;
    if let Err(error) = archive_file.write_all(&bytes) {
        drop(archive_file);
        let _ = fs::remove_file(&archive_path);
        return Err(format!("Could not save downloaded package: {error}"));
    }

    if let Err(error) = archive_file.flush() {
        drop(archive_file);
        let _ = fs::remove_file(&archive_path);
        return Err(format!("Could not save downloaded package: {error}"));
    }
    drop(archive_file);

    let manifest =
        match validate_package_archive(&archive_path, &repo_package.name, &repo_package.version) {
            Ok(package) => package,
            Err(error) => {
                let _ = fs::remove_file(&archive_path);
                return Err(error);
            }
        };

    Ok((manifest, archive_path))
}

fn install_repository_package(name: &str) -> Result<(Package, PathBuf, bool), String> {
    let index = fetch_repository_index()?;

    let home = env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| "Could not determine your home directory".to_string())?;

    let target = home.join(".local/share/vpk/root");

    let (package, newly_installed) =
        install_repository_package_recursive(name, &index, &target, &mut Vec::new())?;

    Ok((package, target, newly_installed))
}

fn build_dependent_tree(
    package_name: &str,
    packages: &[database::InstalledPackage],
    prefix: &str,
    ancestors: &mut Vec<String>,
    output: &mut String,
) -> usize {
    let mut dependents: Vec<&database::InstalledPackage> = packages
        .iter()
        .filter(|installed| {
            installed
                .dependencies
                .iter()
                .any(|dependency| dependency == package_name)
                && !ancestors.contains(&installed.name)
        })
        .collect();

    dependents.sort_by(|a, b| a.name.cmp(&b.name));

    let mut total = 0;

    for (index, dependent) in dependents.iter().enumerate() {
        let is_last = index + 1 == dependents.len();

        output.push_str(&format!(
            "{}{}{} depends on {}\n",
            prefix,
            if is_last { "└── " } else { "├── " },
            dependent.name,
            package_name
        ));

        let next_prefix = format!("{}{}", prefix, if is_last { "    " } else { "│   " });

        ancestors.push(dependent.name.clone());

        total +=
            1 + build_dependent_tree(&dependent.name, packages, &next_prefix, ancestors, output);

        ancestors.pop();
    }

    total
}

struct StagedUpgradeArchives(Vec<PathBuf>);

impl Drop for StagedUpgradeArchives {
    fn drop(&mut self) {
        for path in &self.0 {
            let _ = fs::remove_file(path);
        }
    }
}

struct StagedUpgrade {
    old_package: InstalledPackage,
    new_package: Package,
    archive_path: PathBuf,
    new_files: Vec<String>,
}

struct AppliedUpgrade {
    old_package: InstalledPackage,
    backup_dir: PathBuf,
    backed_up_files: Vec<String>,
    created_files: Vec<PathBuf>,
    created_dirs: Vec<PathBuf>,
}

fn upgraded_install_record(upgrade: &StagedUpgrade) -> InstalledPackage {
    InstalledPackage {
        name: upgrade.new_package.name.clone(),
        version: upgrade.new_package.version.clone(),
        description: upgrade.new_package.description.clone(),
        files: upgrade.new_files.clone(),
        dependencies: upgrade.new_package.dependencies.clone(),
    }
}

fn rollback_upgrade(
    root: &Path,
    backup_dir: &Path,
    backed_up_files: &[String],
    created_files: &[PathBuf],
    created_dirs: &[PathBuf],
) -> Vec<String> {
    let mut errors = rollback_installed_files(created_files, created_dirs);

    for file in backed_up_files.iter().rev() {
        if let Err(error) = restore_upgrade_file(root, backup_dir, Path::new(file)) {
            errors.push(format!("Could not restore '{file}': {error}"));
        }
    }

    errors
}

fn apply_single_upgrade(
    root: &Path,
    upgrade: &StagedUpgrade,
    staging_root: &Path,
    staged_files: &[String],
) -> Result<AppliedUpgrade, String> {
    if staged_files != upgrade.new_files {
        return Err(format!(
            "Staged file list changed unexpectedly for '{}'",
            upgrade.new_package.name
        ));
    }

    preflight_upgrade_files(root, &upgrade.old_package.name, staged_files)?;

    let backup_dir = create_upgrade_backup_dir(root)?;
    let old_package = upgrade.old_package.clone();

    let backed_up_files = backup_existing_package_files(
        root,
        &backup_dir,
        &upgrade.old_package.name,
        &upgrade.old_package.files,
    )?;

    let mut created_files = Vec::new();
    let mut created_dirs = Vec::new();

    let result = (|| -> Result<(), String> {
        for file in staged_files {
            install_staged_file(
                staging_root,
                root,
                Path::new(file),
                &mut created_files,
                &mut created_dirs,
            )?;
        }

        database::replace_install_record(root, upgraded_install_record(upgrade))
            .map_err(|e| format!("Could not update installed package record: {e}"))?;

        Ok(())
    })();

    if let Err(error) = result {
        let rollback_errors = rollback_upgrade(
            root,
            &backup_dir,
            &backed_up_files,
            &created_files,
            &created_dirs,
        );

        if rollback_errors.is_empty() {
            return Err(error);
        }

        return Err(format!(
            "{error}; rollback failures: {}",
            rollback_errors.join("; ")
        ));
    }

    Ok(AppliedUpgrade {
        old_package,
        backup_dir,
        backed_up_files,
        created_files,
        created_dirs,
    })
}

fn rollback_applied_upgrade(root: &Path, applied: &AppliedUpgrade) -> Vec<String> {
    let mut errors = rollback_upgrade(
        root,
        &applied.backup_dir,
        &applied.backed_up_files,
        &applied.created_files,
        &applied.created_dirs,
    );

    if let Err(error) = database::replace_install_record(root, applied.old_package.clone()) {
        errors.push(format!(
            "Could not restore installed record for '{}': {error}",
            applied.old_package.name
        ));
    }

    errors
}

fn apply_staged_upgrades(
    root: &Path,
    upgrades: &[StagedUpgrade],
    staged_packages: &[(StagedPackageDirectory, Vec<String>)],
) -> Result<Vec<AppliedUpgrade>, String> {
    if upgrades.len() != staged_packages.len() {
        return Err("Upgrade and staging counts do not match".to_string());
    }

    let mut applied: Vec<AppliedUpgrade> = Vec::new();

    for (upgrade, (staging, files)) in upgrades.iter().zip(staged_packages.iter()) {
        match apply_single_upgrade(root, upgrade, &staging.path, files) {
            Ok(journal) => applied.push(journal),
            Err(error) => {
                let mut rollback_errors = Vec::new();

                for previous in applied.iter().rev() {
                    rollback_errors.extend(rollback_applied_upgrade(root, previous));
                }

                if rollback_errors.is_empty() {
                    return Err(format!(
                        "Upgrade of '{}' failed: {error}. Earlier upgrades were rolled back.",
                        upgrade.new_package.name
                    ));
                }

                return Err(format!(
                    "Upgrade of '{}' failed: {error}. Rollback failures: {}",
                    upgrade.new_package.name,
                    rollback_errors.join("; ")
                ));
            }
        }
    }

    Ok(applied)
}

fn preview_upgrades(package_filter: Option<&str>) -> Result<(), String> {
    if let Some(name) = package_filter {
        if name.eq_ignore_ascii_case("vpk") {
            println!("VPK itself is excluded from package upgrades.");
            return Ok(());
        }
    }

    let index = update_repository_index()?;

    let home =
        env::var_os("HOME").ok_or_else(|| "HOME environment variable is not set".to_string())?;

    let root = PathBuf::from(home).join(".local/share/vpk/root");

    let installed = database::list_installed(&root)
        .map_err(|e| format!("could not read installed packages: {e}"))?;

    if let Some(name) = package_filter {
        if database::get_installed(&root, name)
            .map_err(|e| format!("could not read package record: {e}"))?
            .is_none()
        {
            return Err(format!("package '{name}' is not installed"));
        }
    }

    let mut staged: Vec<StagedUpgrade> = Vec::new();
    let mut staged_archives = StagedUpgradeArchives(Vec::new());

    println!("Upgrade preview (no files will be changed):\n");

    for package in &installed {
        if package.name.eq_ignore_ascii_case("vpk") {
            continue;
        }

        if let Some(filter) = package_filter {
            if package.name != filter {
                continue;
            }
        }

        let available = index
            .package
            .iter()
            .find(|candidate| candidate.name == package.name);

        let Some(available) = available else {
            if package_filter.is_some() {
                return Err(format!(
                    "package '{}' is not available in the repository",
                    package.name
                ));
            }
            continue;
        };

        if is_newer_version(&package.version, &available.version)? {
            let (new_package, archive_path) = download_verified_package(available)?;
            staged_archives.0.push(archive_path.clone());

            let new_files = package_archive_files(&archive_path)?;

            preflight_upgrade_files(&root, &package.name, &new_files)?;

            for file in &package.files {
                let owned_by_another =
                    database::other_package_owns_file(&root, &package.name, file)
                        .map_err(|e| format!("could not check file ownership: {e}"))?;

                if owned_by_another {
                    return Err(format!(
                        "cannot upgrade '{}': file '{}' is also claimed by another package",
                        package.name, file
                    ));
                }
            }

            staged.push(StagedUpgrade {
                old_package: package.clone(),
                new_package,
                archive_path,
                new_files,
            });
        }
    }

    if staged.is_empty() {
        println!("No package upgrades available.");
        return Ok(());
    }

    println!("\nUpgrade summary:");

    for upgrade in &staged {
        println!(
            "  {}: {} -> {} ({} files)",
            upgrade.old_package.name,
            upgrade.old_package.version,
            upgrade.new_package.version,
            upgrade.new_files.len()
        );
        println!("    Staged archive: {}", upgrade.archive_path.display());

        let obsolete = obsolete_package_files(&upgrade.old_package.files, &upgrade.new_files);

        for file in &obsolete {
            println!("    Remove obsolete file: {file}");
        }
    }

    println!("\n{} package upgrade(s) available.", staged.len());

    if !confirm_upgrade()? {
        println!("Upgrade cancelled.");
        return Ok(());
    }

    println!("\nPreparing package upgrades...");

    let mut staged_packages: Vec<(StagedPackageDirectory, Vec<String>)> =
        Vec::with_capacity(staged.len());

    // Stage and verify every archive before modifying installed files.
    for upgrade in &staged {
        let (staging, actual_files) = stage_package_archive(
            &upgrade.archive_path,
            &upgrade.new_package.name,
            &upgrade.new_package.version,
        )?;

        let mut actual_files_sorted = actual_files;
        actual_files_sorted.sort();

        let mut expected_files_sorted = upgrade.new_files.clone();
        expected_files_sorted.sort();

        if actual_files_sorted != expected_files_sorted {
            return Err(format!(
                "Staged files do not match the verified file list for '{}'",
                upgrade.new_package.name
            ));
        }

        staged_packages.push((staging, upgrade.new_files.clone()));
    }

    // Apply all upgrades as one transaction.
    let applied = apply_staged_upgrades(&root, &staged, &staged_packages)?;

    for (upgrade, journal) in staged.iter().zip(applied.iter()) {
        println!(
            "Successfully upgraded {}: {} -> {}",
            upgrade.new_package.name, upgrade.old_package.version, upgrade.new_package.version
        );
        println!("    Backup saved at: {}", journal.backup_dir.display());
    }

    println!("\nAll package upgrades completed successfully.");

    Ok(())
}

fn confirm_upgrade() -> Result<bool, String> {
    use std::io::Write;

    print!("Type 'yes' to apply these upgrades: ");
    io::stdout()
        .flush()
        .map_err(|e| format!("Could not display confirmation prompt: {e}"))?;

    let mut answer = String::new();

    io::stdin()
        .read_line(&mut answer)
        .map_err(|e| format!("Could not read confirmation: {e}"))?;

    Ok(answer.trim() == "yes")
}

fn main() {
    let args: Vec<String> = env::args().collect();

    if args.len() < 2 {
        eprintln!("Usage: vpk <command> [arguments]");
        eprintln!("Try 'vpk --help' for more information.");
        process::exit(1);
    }

    match args[1].as_str() {
        "update" => {
            if args.len() != 2 {
                eprintln!("Usage: vpk update");
                process::exit(1);
            }

            match update_repository_index() {
                Ok(index) => {
                    println!(
                        "Repository index updated successfully! {} package(s) available.",
                        index.package.len()
                    );
                    if let Ok(path) = repository_index_path() {
                        println!("Saved to: {}", path.display());
                    }
                }
                Err(error) => {
                    eprintln!("vpk: could not update repository: {error}");
                    process::exit(1);
                }
            }
        }

        "search" => {
            if args.len() != 3 {
                eprintln!("Usage: vpk search <query>");
                std::process::exit(1);
            }

            let query = args[2].to_lowercase();

            match fetch_repository_index() {
                Ok(index) => {
                    let matches: Vec<_> = index
                        .package
                        .iter()
                        .filter(|package| {
                            package.name.to_lowercase().contains(&query)
                                || package.description.to_lowercase().contains(&query)
                        })
                        .collect();

                    if matches.is_empty() {
                        println!("No packages found matching '{query}'.");
                    } else {
                        for package in matches {
                            println!(
                                "{} {} - {}",
                                package.name, package.version, package.description
                            );
                        }
                    }
                }
                Err(error) => {
                    eprintln!("vpk: {error}");
                    std::process::exit(1);
                }
            }
        }

        "remove" => {
            if args.len() != 3 && args.len() != 4 {
                eprintln!("Usage:");
                eprintln!("  vpk remove <package-name>");
                eprintln!("  vpk remove <package-name> <target-directory>");
                process::exit(1);
            }

            let name = &args[2];

            let target = if args.len() == 3 {
                match env::var_os("HOME") {
                    Some(home) => PathBuf::from(home).join(".local/share/vpk/root"),
                    None => {
                        eprintln!("vpk: could not determine your home directory");
                        process::exit(1);
                    }
                }
            } else {
                PathBuf::from(&args[3])
            };

            let root = match fs::canonicalize(&target) {
                Ok(root) => root,
                Err(error) => {
                    eprintln!("vpk: could not access target directory: {error}");
                    process::exit(1);
                }
            };

            let package = match database::get_installed(&root, name) {
                Ok(Some(package)) => package,
                Ok(None) => {
                    eprintln!("vpk: package '{name}' is not installed");
                    process::exit(1);
                }
                Err(error) => {
                    eprintln!("vpk: could not read package record: {error}");
                    process::exit(1);
                }
            };

            println!("Package: {}", package.name);
            println!("Version: {}", package.version);
            println!("Description: {}", package.description);

            let installed_packages = match database::list_installed(&root) {
                Ok(packages) => packages,
                Err(error) => {
                    eprintln!("vpk: could not check package dependencies: {error}");
                    process::exit(1);
                }
            };

            let mut tree = String::new();

            let affected = build_dependent_tree(
                &package.name,
                &installed_packages,
                "  ",
                &mut vec![package.name.clone()],
                &mut tree,
            );

            if !tree.is_empty() {
                println!("{} (being removed)", package.name);
                print!("{tree}");
            }

            if affected > 0 {
                println!();
                println!(
                    "WARNING: Removing '{}' may affect {affected} installed package(s):",
                    package.name
                );
            }
            println!();
            println!("The following files will be removed:");

            for file in &package.files {
                println!("  {file}");
            }

            println!();
            print!("Remove {}? [y/N]: ", package.name);

            use std::io::Write;
            io::stdout().flush().unwrap_or(());

            let mut answer = String::new();

            if io::stdin().read_line(&mut answer).is_err() {
                eprintln!("vpk: could not read your response");
                process::exit(1);
            }

            if !answer.trim().eq_ignore_ascii_case("y")
                && !answer.trim().eq_ignore_ascii_case("yes")
            {
                println!("Removal cancelled.");
                return;
            }

            match database::remove_installed(&root, name) {
                Ok(package) => {
                    println!("Successfully removed {} {}.", package.name, package.version);
                }
                Err(error) => {
                    eprintln!("vpk: could not remove package: {error}");
                    process::exit(1);
                }
            }
        }

        "upgrade" => {
            if args.len() > 3 {
                eprintln!("Usage:");
                eprintln!("  vpk upgrade");
                eprintln!("  vpk upgrade <package-name>");
                process::exit(1);
            }

            let package_filter = args.get(2).map(String::as_str);

            if let Err(error) = preview_upgrades(package_filter) {
                eprintln!("vpk: could not preview upgrades: {error}");
                process::exit(1);
            }
        }

        "list" => {
            if args.len() > 3 {
                eprintln!("Usage:");
                eprintln!("  vpk list");
                eprintln!("  vpk list <target-directory>");
                process::exit(1);
            }

            let target = if args.len() == 2 {
                match env::var_os("HOME") {
                    Some(home) => PathBuf::from(home).join(".local/share/vpk/root"),
                    None => {
                        eprintln!("vpk: could not determine your home directory");
                        process::exit(1);
                    }
                }
            } else {
                PathBuf::from(&args[2])
            };

            let root = match fs::canonicalize(&target) {
                Ok(root) => root,
                Err(error) => {
                    eprintln!("vpk: could not access target directory: {error}");
                    process::exit(1);
                }
            };

            match database::list_installed(&root) {
                Ok(packages) if packages.is_empty() => {
                    println!("No packages installed.");
                }
                Ok(packages) => {
                    println!("Installed packages:\n");

                    for package in packages {
                        println!("{}  {}", package.name, package.version);
                        println!("  {}", package.description);
                        println!();
                    }
                }
                Err(error) => {
                    eprintln!("vpk: could not list packages: {error}");
                    process::exit(1);
                }
            }
        }

        "inspect" | "info" => {
            if args.len() == 3 && args[2].ends_with(".vpk") {
                match inspect_package(&args[2]) {
                    Ok(package) => {
                        println!("Package:      {}", package.name);
                        println!("Version:      {}", package.version);
                        println!("Description:  {}", package.description);

                        if package.dependencies.is_empty() {
                            println!("Dependencies: None");
                        } else {
                            println!("Dependencies:");
                            for dependency in &package.dependencies {
                                println!("  - {dependency}");
                            }
                        }
                    }
                    Err(error) => {
                        eprintln!("vpk: {error}");
                        process::exit(1);
                    }
                }
            } else if args.len() == 3 || args.len() == 4 {
                let name = &args[2];

                let target = if args.len() == 3 {
                    match env::var_os("HOME") {
                        Some(home) => PathBuf::from(home).join(".local/share/vpk/root"),
                        None => {
                            eprintln!("vpk: could not determine your home directory");
                            process::exit(1);
                        }
                    }
                } else {
                    PathBuf::from(&args[3])
                };

                let root = match fs::canonicalize(&target) {
                    Ok(root) => root,
                    Err(error) => {
                        eprintln!("vpk: could not access target directory: {error}");
                        process::exit(1);
                    }
                };

                match database::get_installed(&root, name) {
                    Ok(Some(package)) => {
                        println!("Package:      {}", package.name);
                        println!("Version:      {}", package.version);
                        println!("Description:  {}", package.description);
                    }
                    Ok(None) => {
                        eprintln!("vpk: package '{name}' is not installed");
                        process::exit(1);
                    }
                    Err(error) => {
                        eprintln!("vpk: could not read package info: {error}");
                        process::exit(1);
                    }
                }
            } else {
                eprintln!("Usage:");
                eprintln!("  vpk info <package-name>");
                eprintln!("  vpk info <package-name> <target-directory>");
                eprintln!("  vpk info <package.vpk>");
                process::exit(1);
            }
        }

        "pack" => {
            if args.len() != 4 {
                eprintln!("Usage: vpk pack <directory> <output.vpk>");
                process::exit(1);
            }

            match pack_package(&args[2], &args[3]) {
                Ok(()) => {
                    println!("Package created successfully!");
                    println!("Output: {}", args[3]);
                }
                Err(error) => {
                    eprintln!("vpk: could not create package: {error}");
                    process::exit(1);
                }
            }
        }

        "install" => {
            if args.len() == 3 {
                let name = &args[2];

                match install_repository_package(name) {
                    Ok((package, target, newly_installed)) => {
                        if newly_installed {
                            println!(
                                "Installed {} {} successfully!",
                                package.name, package.version
                            );
                        } else {
                            println!("{} {} is already installed.", package.name, package.version);
                        }

                        println!("Location: {}", target.display());
                    }
                    Err(error) => {
                        eprintln!("vpk: could not install package: {error}");
                        process::exit(1);
                    }
                }
            } else if args.len() == 4 {
                match install_package(&args[2], &args[3]) {
                    Ok(package) => {
                        println!(
                            "Installed {} {} successfully!",
                            package.name, package.version
                        );
                        println!("Location: {}", args[3]);
                    }
                    Err(error) => {
                        eprintln!("vpk: could not install package: {error}");
                        process::exit(1);
                    }
                }
            } else {
                eprintln!("Usage:");
                eprintln!("  vpk install <package-name>");
                eprintln!("  vpk install <package.vpk> <target-directory>");
                process::exit(1);
            }
        }

        "--version" | "-V" => {
            println!("vpk {}", env!("CARGO_PKG_VERSION"));
        }

        "--help" | "-h" => {
            println!("VPK - The Vast Linux Package Manager");
            println!();
            println!("Usage:");
            println!("  vpk <command> [arguments]");
            println!();
            println!("Commands:");
            println!("  upgrade [package-name]          Upgrade packages after confirmation");
            println!("  update                         Refresh the repository index");
            println!("  search <query>                 Search available packages");
            println!("  install <package-name>         Install a package from the repository");
            println!("  install <package.vpk> <dir>     Install a local package archive");
            println!("  list [target-directory]        List installed packages");
            println!("  info <package-name>            Show installed package information");
            println!("  info <package-name> <dir>      Show package info in a custom root");
            println!("  info <package.vpk>              Inspect a package archive");
            println!("  remove <package-name>          Remove an installed package");
            println!("  remove <package-name> <dir>    Remove from a custom root");
            println!("  pack <directory> <output.vpk>  Create a package archive");
            println!("  inspect <package.vpk>          Inspect a package archive");
            println!("  --version                      Show VPK version");
            println!("  --help                         Show this help message");
            println!();
            println!("Default install root:");
            println!("  ~/.local/share/vpk/root")
        }

        _ => {
            eprintln!("vpk: unknown command '{}'", args[1]);
            eprintln!("Try 'vpk --help' for more information.");
            process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn applies_upgrade_and_removes_obsolete_files() {
        let temp = env::temp_dir().join(format!("vpk-test-apply-upgrade-{}", process::id()));

        let _ = fs::remove_dir_all(&temp);

        let root = temp.join("root");
        fs::create_dir_all(root.join("usr/share")).unwrap();

        let old_hello = root.join("usr/share/hello.txt");
        let obsolete = root.join("usr/share/obsolete.txt");

        fs::write(&old_hello, "old version").unwrap();
        fs::write(&obsolete, "obsolete file").unwrap();

        let old_package = InstalledPackage {
            name: "test-package".into(),
            version: "1.0.0".into(),
            description: "Old test package".into(),
            files: vec![
                "usr/share/hello.txt".into(),
                "usr/share/obsolete.txt".into(),
            ],
            dependencies: vec![],
        };

        database::record_install(&root, old_package.clone()).unwrap();

        let staged = create_staging_directory().unwrap();
        fs::create_dir_all(staged.path.join("usr/share")).unwrap();

        fs::write(staged.path.join("usr/share/hello.txt"), "upgraded version").unwrap();

        fs::write(staged.path.join("usr/share/new.txt"), "new file").unwrap();

        let upgrade = StagedUpgrade {
            old_package,
            new_package: Package {
                name: "test-package".into(),
                version: "2.0.0".into(),
                description: "Upgraded test package".into(),
                dependencies: vec![],
            },
            archive_path: PathBuf::new(),
            new_files: vec!["usr/share/hello.txt".into(), "usr/share/new.txt".into()],
        };

        let applied =
            apply_single_upgrade(&root, &upgrade, &staged.path, &upgrade.new_files).unwrap();

        assert_eq!(fs::read_to_string(&old_hello).unwrap(), "upgraded version");
        assert_eq!(
            fs::read_to_string(root.join("usr/share/new.txt")).unwrap(),
            "new file"
        );
        assert!(!obsolete.exists());

        assert_eq!(
            fs::read_to_string(applied.backup_dir.join("usr/share/hello.txt")).unwrap(),
            "old version"
        );
        assert_eq!(
            fs::read_to_string(applied.backup_dir.join("usr/share/obsolete.txt")).unwrap(),
            "obsolete file"
        );

        let installed = database::get_installed(&root, "test-package")
            .unwrap()
            .unwrap();

        assert_eq!(installed.version, "2.0.0");
        assert_eq!(installed.description, "Upgraded test package");
        assert_eq!(installed.files, upgrade.new_files);

        drop(staged);
        fs::remove_dir_all(&temp).unwrap();
    }

    #[test]
    fn rolls_back_upgrade_when_staged_file_is_missing() {
        let temp = env::temp_dir().join(format!("vpk-test-upgrade-rollback-{}", process::id()));

        let _ = fs::remove_dir_all(&temp);

        let root = temp.join("root");
        fs::create_dir_all(root.join("usr/share")).unwrap();

        let old_hello = root.join("usr/share/hello.txt");
        let obsolete = root.join("usr/share/obsolete.txt");

        fs::write(&old_hello, "original version").unwrap();
        fs::write(&obsolete, "original obsolete file").unwrap();

        let old_package = InstalledPackage {
            name: "test-package".into(),
            version: "1.0.0".into(),
            description: "Original package".into(),
            files: vec![
                "usr/share/hello.txt".into(),
                "usr/share/obsolete.txt".into(),
            ],
            dependencies: vec![],
        };

        database::record_install(&root, old_package.clone()).unwrap();

        let staged = create_staging_directory().unwrap();
        fs::create_dir_all(staged.path.join("usr/share")).unwrap();

        fs::write(
            staged.path.join("usr/share/hello.txt"),
            "partially upgraded version",
        )
        .unwrap();

        fs::write(staged.path.join("usr/share/new.txt"), "new file").unwrap();

        let upgrade = StagedUpgrade {
            old_package,
            new_package: Package {
                name: "test-package".into(),
                version: "2.0.0".into(),
                description: "Upgraded package".into(),
                dependencies: vec![],
            },
            archive_path: PathBuf::new(),
            new_files: vec!["usr/share/hello.txt".into(), "usr/share/new.txt".into()],
        };

        // Simulate a staging failure after the old files have been backed up.
        fs::remove_file(staged.path.join("usr/share/new.txt")).unwrap();

        let result = apply_single_upgrade(&root, &upgrade, &staged.path, &upgrade.new_files);

        assert!(result.is_err());
        assert_eq!(fs::read_to_string(&old_hello).unwrap(), "original version");
        assert_eq!(
            fs::read_to_string(&obsolete).unwrap(),
            "original obsolete file"
        );
        assert!(!root.join("usr/share/new.txt").exists());

        let installed = database::get_installed(&root, "test-package")
            .unwrap()
            .unwrap();

        assert_eq!(installed.version, "1.0.0");
        assert_eq!(installed.description, "Original package");

        drop(staged);
        fs::remove_dir_all(&temp).unwrap();
    }

    #[test]
    fn rolls_back_entire_batch_when_later_upgrade_fails() {
        let temp = env::temp_dir().join(format!("vpk-test-batch-rollback-{}", process::id()));
        let _ = fs::remove_dir_all(&temp);

        let root = temp.join("root");
        fs::create_dir_all(root.join("usr/share")).unwrap();

        let alpha_file = root.join("usr/share/alpha.txt");
        let beta_file = root.join("usr/share/beta.txt");
        fs::write(&alpha_file, "alpha original").unwrap();
        fs::write(&beta_file, "beta original").unwrap();

        let alpha_old = InstalledPackage {
            name: "alpha".into(),
            version: "1.0.0".into(),
            description: "Alpha original".into(),
            files: vec!["usr/share/alpha.txt".into()],
            dependencies: vec![],
        };
        let beta_old = InstalledPackage {
            name: "beta".into(),
            version: "1.0.0".into(),
            description: "Beta original".into(),
            files: vec!["usr/share/beta.txt".into()],
            dependencies: vec![],
        };

        database::record_install(&root, alpha_old.clone()).unwrap();
        database::record_install(&root, beta_old.clone()).unwrap();

        let alpha_staged = create_staging_directory().unwrap();
        fs::create_dir_all(alpha_staged.path.join("usr/share")).unwrap();
        fs::write(
            alpha_staged.path.join("usr/share/alpha.txt"),
            "alpha upgraded",
        )
        .unwrap();

        let beta_staged = create_staging_directory().unwrap();
        fs::create_dir_all(beta_staged.path.join("usr/share")).unwrap();
        fs::write(
            beta_staged.path.join("usr/share/beta.txt"),
            "beta partially upgraded",
        )
        .unwrap();

        let alpha_upgrade = StagedUpgrade {
            old_package: alpha_old,
            new_package: Package {
                name: "alpha".into(),
                version: "2.0.0".into(),
                description: "Alpha upgraded".into(),
                dependencies: vec![],
            },
            archive_path: PathBuf::new(),
            new_files: vec!["usr/share/alpha.txt".into()],
        };

        let beta_upgrade = StagedUpgrade {
            old_package: beta_old,
            new_package: Package {
                name: "beta".into(),
                version: "2.0.0".into(),
                description: "Beta upgraded".into(),
                dependencies: vec![],
            },
            archive_path: PathBuf::new(),
            new_files: vec!["usr/share/beta.txt".into(), "usr/share/beta-new.txt".into()],
        };

        // Force beta's upgrade to fail after alpha has already been upgraded.
        fs::remove_file(beta_staged.path.join("usr/share/beta.txt")).unwrap();

        let upgrades = vec![alpha_upgrade, beta_upgrade];
        let staged_packages = vec![
            (alpha_staged, vec!["usr/share/alpha.txt".to_string()]),
            (
                beta_staged,
                vec![
                    "usr/share/beta.txt".to_string(),
                    "usr/share/beta-new.txt".to_string(),
                ],
            ),
        ];

        let result = apply_staged_upgrades(&root, &upgrades, &staged_packages);
        assert!(
            result.is_err(),
            "batch should fail when beta's staged file is missing"
        );

        assert_eq!(fs::read_to_string(&alpha_file).unwrap(), "alpha original");
        assert_eq!(fs::read_to_string(&beta_file).unwrap(), "beta original");

        let alpha_installed = database::get_installed(&root, "alpha").unwrap().unwrap();
        let beta_installed = database::get_installed(&root, "beta").unwrap().unwrap();

        assert_eq!(alpha_installed.version, "1.0.0");
        assert_eq!(beta_installed.version, "1.0.0");

        drop(staged_packages);
        fs::remove_dir_all(&temp).unwrap();
    }

    #[test]
    fn creates_unique_upgrade_backup_directories() {
        let temp = env::temp_dir().join(format!("vpk-test-backup-{}", std::process::id()));

        let _ = fs::remove_dir_all(&temp);
        fs::create_dir_all(&temp).unwrap();

        let root = temp.join("root");
        fs::create_dir_all(&root).unwrap();

        let first = create_upgrade_backup_dir(&root).unwrap();

        assert!(same_filesystem(&root, first.parent().unwrap()).unwrap());

        assert!(first.is_dir());
        assert!(first.starts_with(temp.join("vpk-upgrade-backups")));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let permissions = fs::metadata(&first).unwrap().permissions();
            assert_eq!(permissions.mode() & 0o777, 0o700);
        }

        // Creating another backup with the same process ID must use a
        // different directory.
        let second = create_upgrade_backup_dir(&root).unwrap();
        assert!(second.is_dir());
        assert_ne!(first, second);

        fs::remove_dir_all(&temp).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn detects_paths_on_the_same_filesystem() {
        let temp = env::temp_dir().join(format!("vpk-test-filesystem-{}", std::process::id()));

        let _ = fs::remove_dir_all(&temp);
        fs::create_dir_all(&temp).unwrap();

        assert!(same_filesystem(&temp, &temp).unwrap());

        fs::remove_dir_all(&temp).unwrap();
    }

    #[test]
    fn backs_up_upgrade_file_and_preserves_contents() {
        let temp = env::temp_dir().join(format!("vpk-test-backup-file-{}", std::process::id()));

        let _ = fs::remove_dir_all(&temp);
        fs::create_dir_all(&temp).unwrap();

        let root = temp.join("root");
        fs::create_dir_all(root.join("bin")).unwrap();

        let backup_dir = temp.join("backup");
        fs::create_dir_all(&backup_dir).unwrap();

        let source = root.join("bin/tool");
        fs::write(&source, "original contents").unwrap();

        backup_upgrade_file(&root, &backup_dir, Path::new("bin/tool")).unwrap();

        assert!(!source.exists());

        assert_eq!(
            fs::read_to_string(backup_dir.join("bin/tool")).unwrap(),
            "original contents"
        );

        restore_upgrade_file(&root, &backup_dir, Path::new("bin/tool")).unwrap();

        assert_eq!(fs::read_to_string(&source).unwrap(), "original contents");
        assert!(!backup_dir.join("bin/tool").exists());

        fs::remove_dir_all(&temp).unwrap();
    }

    #[test]
    fn backs_up_existing_package_files_and_skips_missing_files() {
        let temp = env::temp_dir().join(format!(
            "vpk-test-backup-package-files-{}",
            std::process::id()
        ));

        let _ = fs::remove_dir_all(&temp);

        let root = temp.join("root");
        fs::create_dir_all(root.join("bin")).unwrap();

        let backup_dir = temp.join("backup");
        fs::create_dir_all(&backup_dir).unwrap();

        let source = root.join("bin/tool");
        fs::write(&source, "original contents").unwrap();

        let old_files = vec!["bin/tool".to_string(), "bin/missing-tool".to_string()];

        let backed_up =
            backup_existing_package_files(&root, &backup_dir, "demo", &old_files).unwrap();

        assert_eq!(backed_up, vec!["bin/tool".to_string()]);
        assert!(!source.exists());
        assert_eq!(
            fs::read_to_string(backup_dir.join("bin/tool")).unwrap(),
            "original contents"
        );

        restore_upgrade_file(&root, &backup_dir, Path::new("bin/tool")).unwrap();
        assert_eq!(fs::read_to_string(&source).unwrap(), "original contents");

        fs::remove_dir_all(&temp).unwrap();
    }

    #[test]
    fn restores_backed_up_files_when_later_file_is_owned_by_another_package() {
        let temp = env::temp_dir().join(format!("vpk-test-backup-rollback-{}", std::process::id()));

        let _ = fs::remove_dir_all(&temp);

        let root = temp.join("root");
        fs::create_dir_all(root.join("bin")).unwrap();

        let backup_dir = temp.join("backup");
        fs::create_dir_all(&backup_dir).unwrap();

        let first_file = root.join("bin/first-tool");
        let shared_file = root.join("bin/shared-tool");

        fs::write(&first_file, "original first file").unwrap();
        fs::write(&shared_file, "shared file").unwrap();

        database::record_install(
            &root,
            database::InstalledPackage {
                name: "other-package".into(),
                version: "1.0.0".into(),
                description: "Another package".into(),
                files: vec!["bin/shared-tool".into()],
                dependencies: vec![],
            },
        )
        .unwrap();

        let old_files = vec!["bin/first-tool".to_string(), "bin/shared-tool".to_string()];

        let result = backup_existing_package_files(&root, &backup_dir, "my-package", &old_files);

        assert!(result.is_err());
        assert_eq!(
            fs::read_to_string(&first_file).unwrap(),
            "original first file"
        );
        assert!(!backup_dir.join("bin/first-tool").exists());
        assert_eq!(fs::read_to_string(&shared_file).unwrap(), "shared file");

        fs::remove_dir_all(&temp).unwrap();
    }

    #[test]
    fn refuses_to_overwrite_existing_restore_destination() {
        let temp =
            env::temp_dir().join(format!("vpk-test-restore-existing-{}", std::process::id()));

        let _ = fs::remove_dir_all(&temp);

        let root = temp.join("root");
        let backup_dir = temp.join("backup");

        fs::create_dir_all(root.join("bin")).unwrap();
        fs::create_dir_all(backup_dir.join("bin")).unwrap();

        let destination = root.join("bin/tool");
        let backup = backup_dir.join("bin/tool");

        fs::write(&destination, "new file contents").unwrap();
        fs::write(&backup, "original contents").unwrap();

        assert!(restore_upgrade_file(&root, &backup_dir, Path::new("bin/tool")).is_err());

        assert_eq!(
            fs::read_to_string(&destination).unwrap(),
            "new file contents"
        );
        assert_eq!(fs::read_to_string(&backup).unwrap(), "original contents");

        fs::remove_dir_all(&temp).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn refuses_symlinked_backup_parent_during_restore() {
        use std::os::unix::fs::symlink;

        let temp = env::temp_dir().join(format!("vpk-test-restore-symlink-{}", std::process::id()));

        let _ = fs::remove_dir_all(&temp);

        let root = temp.join("root");
        let backup_dir = temp.join("backup");
        let outside = temp.join("outside");

        fs::create_dir_all(root.join("bin")).unwrap();
        fs::create_dir_all(&backup_dir).unwrap();
        fs::create_dir_all(&outside).unwrap();

        fs::write(outside.join("tool"), "outside contents").unwrap();
        symlink(&outside, backup_dir.join("bin")).unwrap();

        assert!(restore_upgrade_file(&root, &backup_dir, Path::new("bin/tool")).is_err());

        assert_eq!(
            fs::read_to_string(outside.join("tool")).unwrap(),
            "outside contents"
        );
        assert!(!root.join("bin/tool").exists());

        fs::remove_dir_all(&temp).unwrap();
    }

    #[test]
    fn refuses_to_overwrite_existing_upgrade_backup() {
        let temp = env::temp_dir().join(format!("vpk-test-backup-existing-{}", std::process::id()));

        let _ = fs::remove_dir_all(&temp);
        let root = temp.join("root");
        let backup_dir = temp.join("backup");

        fs::create_dir_all(root.join("bin")).unwrap();
        fs::create_dir_all(backup_dir.join("bin")).unwrap();

        let source = root.join("bin/tool");
        let backup = backup_dir.join("bin/tool");

        fs::write(&source, "original contents").unwrap();
        fs::write(&backup, "existing backup").unwrap();

        assert!(backup_upgrade_file(&root, &backup_dir, Path::new("bin/tool")).is_err());

        assert_eq!(fs::read_to_string(&source).unwrap(), "original contents");
        assert_eq!(fs::read_to_string(&backup).unwrap(), "existing backup");

        fs::remove_dir_all(&temp).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn refuses_symlinked_upgrade_backup_directory() {
        let temp = env::temp_dir().join(format!("vpk-test-backup-symlink-{}", std::process::id()));

        let _ = fs::remove_dir_all(&temp);

        let root = temp.join("root");
        let backup_dir = temp.join("backup");
        let outside = temp.join("outside");

        fs::create_dir_all(root.join("bin")).unwrap();
        fs::create_dir_all(&backup_dir).unwrap();
        fs::create_dir_all(&outside).unwrap();

        let source = root.join("bin/tool");
        fs::write(&source, "original contents").unwrap();

        std::os::unix::fs::symlink(&outside, backup_dir.join("bin")).unwrap();

        assert!(backup_upgrade_file(&root, &backup_dir, Path::new("bin/tool")).is_err());
        assert_eq!(fs::read_to_string(&source).unwrap(), "original contents");
        assert!(fs::read_dir(&outside).unwrap().next().is_none());

        fs::remove_dir_all(&temp).unwrap();
    }

    #[test]
    #[cfg(unix)]
    fn refuses_symlinked_existing_parent_directory() {
        let temp = env::temp_dir().join(format!("vpk-test-parent-symlink-{}", std::process::id()));

        let _ = fs::remove_dir_all(&temp);

        let base = temp.join("base");
        let outside = temp.join("outside");

        fs::create_dir_all(&base).unwrap();
        fs::create_dir_all(&outside).unwrap();

        std::os::unix::fs::symlink(&outside, base.join("bin")).unwrap();

        assert!(check_existing_parent_directories(&base, Path::new("bin/tool")).is_err());

        fs::remove_dir_all(&temp).unwrap();
    }

    #[test]
    fn lists_old_files_to_back_up_without_duplicates() {
        let old_files = vec![
            "usr/bin/tool".to_string(),
            "usr/share/tool/data.txt".to_string(),
            "usr/bin/tool".to_string(),
        ];

        assert_eq!(
            upgrade_files_to_backup(&old_files),
            vec![
                "usr/bin/tool".to_string(),
                "usr/share/tool/data.txt".to_string(),
            ]
        );
    }

    #[test]
    fn identifies_obsolete_package_files() {
        let old_files = vec![
            "bin/tool".to_string(),
            "share/old-data".to_string(),
            "etc/tool.conf".to_string(),
        ];

        let new_files = vec!["bin/tool".to_string(), "share/new-data".to_string()];

        assert_eq!(
            obsolete_package_files(&old_files, &new_files),
            vec!["etc/tool.conf".to_string(), "share/old-data".to_string()]
        );
    }

    #[test]
    fn finds_no_obsolete_files_when_all_files_are_retained() {
        let old_files = vec!["bin/tool".to_string(), "share/data".to_string()];
        let new_files = vec!["bin/tool".to_string(), "share/data".to_string()];

        assert!(obsolete_package_files(&old_files, &new_files).is_empty());
    }

    #[test]
    fn detects_newer_semantic_versions() {
        assert!(is_newer_version("0.2.9", "0.2.10").unwrap());
        assert!(is_newer_version("0.2.75", "0.3.0").unwrap());
    }

    #[test]
    fn validates_complete_package_archive() {
        let temp = env::temp_dir().join(format!("vpk-test-validate-{}", std::process::id()));

        let _ = fs::remove_dir_all(&temp);
        fs::create_dir_all(&temp).unwrap();

        let package_dir = create_test_package(&temp);
        let archive_path = temp.join("test-package.vpk");

        pack_package(
            package_dir.to_str().unwrap(),
            archive_path.to_str().unwrap(),
        )
        .unwrap();

        let package = validate_package_archive(&archive_path, "test-package", "1.0.0").unwrap();

        assert_eq!(package.name, "test-package");
        assert_eq!(package.version, "1.0.0");

        let files = package_archive_files(&archive_path).unwrap();

        assert_eq!(files, vec!["usr/share/hello.txt".to_string()]);

        assert!(validate_package_archive(&archive_path, "wrong-name", "1.0.0",).is_err());

        fs::remove_dir_all(&temp).unwrap();
    }

    #[test]
    fn stages_package_archive_and_cleans_up_on_drop() {
        let temp = env::temp_dir().join(format!("vpk-test-stage-{}", std::process::id()));

        let _ = fs::remove_dir_all(&temp);
        fs::create_dir_all(&temp).unwrap();

        let package_dir = create_test_package(&temp);
        let archive_path = temp.join("test-package.vpk");

        pack_package(
            package_dir.to_str().unwrap(),
            archive_path.to_str().unwrap(),
        )
        .unwrap();

        let (staged, files) =
            stage_package_archive(&archive_path, "test-package", "1.0.0").unwrap();

        assert_eq!(files, vec!["usr/share/hello.txt".to_string()]);
        assert_eq!(
            fs::read_to_string(staged.path.join("usr/share/hello.txt")).unwrap(),
            "Hello from VPK tests!"
        );

        let staging_path = staged.path.clone();
        drop(staged);

        assert!(!staging_path.exists());

        fs::remove_dir_all(&temp).unwrap();
    }

    #[test]
    fn installs_staged_file_into_target() {
        let temp = env::temp_dir().join(format!("vpk-test-staged-install-{}", std::process::id()));

        let _ = fs::remove_dir_all(&temp);

        let staging_root = temp.join("staging");
        let install_root = temp.join("install");

        fs::create_dir_all(staging_root.join("usr/share")).unwrap();
        fs::create_dir_all(&install_root).unwrap();

        fs::write(
            staging_root.join("usr/share/hello.txt"),
            "Hello from staged VPK!",
        )
        .unwrap();

        let mut created_files = Vec::new();
        let mut created_dirs = Vec::new();

        install_staged_file(
            &staging_root,
            &install_root,
            Path::new("usr/share/hello.txt"),
            &mut created_files,
            &mut created_dirs,
        )
        .unwrap();

        assert_eq!(
            fs::read_to_string(install_root.join("usr/share/hello.txt")).unwrap(),
            "Hello from staged VPK!"
        );

        assert_eq!(
            created_files,
            vec![install_root.join("usr/share/hello.txt")]
        );
        assert_eq!(
            created_dirs,
            vec![install_root.join("usr"), install_root.join("usr/share")]
        );

        fs::remove_dir_all(&temp).unwrap();
    }

    #[test]
    fn refuses_to_overwrite_existing_staged_install_target() {
        let temp =
            env::temp_dir().join(format!("vpk-test-staged-overwrite-{}", std::process::id()));

        let _ = fs::remove_dir_all(&temp);

        let staging_root = temp.join("staging");
        let install_root = temp.join("install");
        let relative = Path::new("usr/share/hello.txt");

        fs::create_dir_all(staging_root.join("usr/share")).unwrap();
        fs::create_dir_all(install_root.join("usr/share")).unwrap();

        fs::write(staging_root.join(relative), "New contents").unwrap();
        fs::write(install_root.join(relative), "Original contents").unwrap();

        let mut created_files = Vec::new();
        let mut created_dirs = Vec::new();

        let result = install_staged_file(
            &staging_root,
            &install_root,
            relative,
            &mut created_files,
            &mut created_dirs,
        );

        assert!(result.is_err());
        assert_eq!(
            fs::read_to_string(install_root.join(relative)).unwrap(),
            "Original contents"
        );
        assert!(created_files.is_empty());
        assert!(created_dirs.is_empty());

        fs::remove_dir_all(&temp).unwrap();
    }

    #[test]
    fn staging_directory_guard_removes_partial_files_on_drop() {
        let staged = create_staging_directory().unwrap();
        let staging_path = staged.path.clone();

        fs::create_dir_all(staged.path.join("usr/share")).unwrap();
        fs::write(staged.path.join("usr/share/partial.txt"), "partial").unwrap();

        assert!(staging_path.join("usr/share/partial.txt").exists());

        drop(staged);

        assert!(!staging_path.exists());
    }

    #[test]
    fn does_not_treat_equal_or_older_versions_as_upgrades() {
        assert!(!is_newer_version("0.2.10", "0.2.10").unwrap());
        assert!(!is_newer_version("0.3.0", "0.2.75").unwrap());
    }

    #[test]
    fn rejects_invalid_semantic_versions() {
        assert!(is_newer_version("not-a-version", "1.0.0").is_err());
        assert!(is_newer_version("1.0.0", "also-invalid").is_err());
    }

    fn create_test_package(base: &Path) -> PathBuf {
        let package_dir = base.join("test-package");
        fs::create_dir_all(package_dir.join("data/usr/share")).unwrap();

        fs::write(
            package_dir.join("manifest.toml"),
            r#"
name = "test-package"
version = "1.0.0"
description = "A test package"
"#,
        )
        .unwrap();

        fs::write(
            package_dir.join("data/usr/share/hello.txt"),
            "Hello from VPK tests!",
        )
        .unwrap();

        package_dir
    }
    use std::path::Path;

    #[test]
    fn accepts_normal_package_paths() {
        assert_eq!(
            package_data_path(Path::new("data/usr/share/hello.txt")).unwrap(),
            PathBuf::from("usr/share/hello.txt")
        );
    }

    #[test]
    fn checks_upgrade_paths_safely() {
        let temp =
            std::env::temp_dir().join(format!("vpk-upgrade-path-test-{}", std::process::id()));

        let _ = fs::remove_dir_all(&temp);
        let root = temp.join("root");
        fs::create_dir_all(root.join("bin")).unwrap();

        fs::write(root.join("bin/tool"), "old version").unwrap();

        assert!(check_upgrade_path(&root, Path::new("bin/tool")).unwrap());
        assert!(!check_upgrade_path(&root, Path::new("bin/missing")).unwrap());

        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("tool", root.join("bin/link")).unwrap();
            assert!(check_upgrade_path(&root, Path::new("bin/link")).is_err());
        }

        fs::remove_dir_all(&temp).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn refuses_symlinked_install_root() {
        let temp =
            std::env::temp_dir().join(format!("vpk-symlink-root-test-{}", std::process::id()));

        let _ = fs::remove_dir_all(&temp);

        let real_root = temp.join("real-root");
        let linked_root = temp.join("linked-root");

        fs::create_dir_all(&real_root).unwrap();
        std::os::unix::fs::symlink(&real_root, &linked_root).unwrap();

        assert!(check_upgrade_path(&linked_root, Path::new("bin/tool")).is_err());

        fs::remove_dir_all(&temp).unwrap();
    }

    #[test]
    fn preflight_rejects_files_owned_by_another_package() {
        let temp = std::env::temp_dir().join(format!("vpk-preflight-test-{}", std::process::id()));

        let _ = fs::remove_dir_all(&temp);
        fs::create_dir_all(&temp).unwrap();

        database::record_install(
            &temp,
            database::InstalledPackage {
                name: "other-package".into(),
                version: "1.0.0".into(),
                description: "Another package".into(),
                files: vec!["usr/bin/shared-tool".into()],
                dependencies: vec![],
            },
        )
        .unwrap();

        let files = vec!["usr/bin/shared-tool".to_string()];

        assert!(preflight_upgrade_files(&temp, "my-package", &files).is_err());

        fs::remove_dir_all(&temp).unwrap();
    }

    #[test]
    fn rejects_parent_directory_traversal() {
        assert!(package_data_path(Path::new("../../etc/passwd")).is_err());
        assert!(package_data_path(Path::new("data/../../etc/passwd")).is_err());
    }

    #[test]
    fn rejects_absolute_paths() {
        assert!(package_data_path(Path::new("/etc/passwd")).is_err());
    }

    #[test]
    fn rejects_paths_outside_data_directory() {
        assert!(package_data_path(Path::new("manifest.toml")).is_err());
        assert!(package_data_path(Path::new("other/file.txt")).is_err());
    }

    #[test]
    fn rejects_data_directory_without_a_filename() {
        assert!(
            package_data_path(Path::new("data"))
                .unwrap()
                .as_os_str()
                .is_empty()
        );
    }

    #[test]
    fn packs_and_inspects_package() {
        let temp = env::temp_dir().join(format!("vpk-test-pack-{}", std::process::id()));

        let _ = fs::remove_dir_all(&temp);
        fs::create_dir_all(&temp).unwrap();

        let package_dir = create_test_package(&temp);
        let archive_path = temp.join("test-package.vpk");

        pack_package(
            package_dir.to_str().unwrap(),
            archive_path.to_str().unwrap(),
        )
        .unwrap();

        let package = inspect_package(archive_path.to_str().unwrap()).unwrap();

        assert_eq!(package.name, "test-package");
        assert_eq!(package.version, "1.0.0");
        assert_eq!(package.description, "A test package");

        fs::remove_dir_all(&temp).unwrap();
    }

    #[test]
    fn refuses_to_pack_missing_manifest() {
        let temp = env::temp_dir().join(format!("vpk-test-invalid-{}", std::process::id()));

        let _ = fs::remove_dir_all(&temp);
        fs::create_dir_all(temp.join("data")).unwrap();

        let output = temp.join("invalid.vpk");

        let result = pack_package(temp.to_str().unwrap(), output.to_str().unwrap());

        assert!(result.is_err());

        fs::remove_dir_all(&temp).unwrap();
    }

    #[test]
    fn installs_package_file_into_target() {
        let temp = env::temp_dir().join(format!("vpk-test-install-{}", std::process::id()));

        let _ = fs::remove_dir_all(&temp);
        fs::create_dir_all(&temp).unwrap();

        let package_dir = create_test_package(&temp);
        let archive_path = temp.join("test-package.vpk");
        let target = temp.join("install-root");

        pack_package(
            package_dir.to_str().unwrap(),
            archive_path.to_str().unwrap(),
        )
        .unwrap();

        let package =
            install_package(archive_path.to_str().unwrap(), target.to_str().unwrap()).unwrap();

        assert_eq!(package.name, "test-package");
        assert_eq!(
            fs::read_to_string(target.join("usr/share/hello.txt")).unwrap(),
            "Hello from VPK tests!"
        );

        fs::remove_dir_all(&temp).unwrap();
    }

        #[cfg(unix)]
    #[test]
    fn preserves_executable_permissions_when_installing() {
        use std::os::unix::fs::PermissionsExt;

        let temp = env::temp_dir().join(format!(
            "vpk-test-executable-permissions-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&temp);
        fs::create_dir_all(&temp).unwrap();

        let package_dir = create_test_package(&temp);
        let executable = package_dir.join("data/usr/bin/vpk-test-executable");
        fs::create_dir_all(executable.parent().unwrap()).unwrap();
        fs::write(&executable, "#!/bin/sh\necho executable\n").unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();

        let archive_path = temp.join("test-package.vpk");
        let target = temp.join("install-root");

        pack_package(
            package_dir.to_str().unwrap(),
            archive_path.to_str().unwrap(),
        )
        .unwrap();

        install_package(archive_path.to_str().unwrap(), target.to_str().unwrap()).unwrap();

        let installed = target.join("usr/bin/vpk-test-executable");
        let mode = fs::metadata(&installed).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o755, "executable permissions were not preserved");

        fs::remove_dir_all(&temp).unwrap();
    }

    #[test]
    fn refuses_to_overwrite_existing_file() {
        let temp = env::temp_dir().join(format!("vpk-test-overwrite-{}", std::process::id()));

        let _ = fs::remove_dir_all(&temp);
        fs::create_dir_all(&temp).unwrap();

        let package_dir = create_test_package(&temp);
        let archive_path = temp.join("test-package.vpk");
        let target = temp.join("install-root");
        let existing_file = target.join("usr/share/hello.txt");

        fs::create_dir_all(existing_file.parent().unwrap()).unwrap();
        fs::write(&existing_file, "DO NOT OVERWRITE").unwrap();

        pack_package(
            package_dir.to_str().unwrap(),
            archive_path.to_str().unwrap(),
        )
        .unwrap();

        let result = install_package(archive_path.to_str().unwrap(), target.to_str().unwrap());

        assert!(result.is_err());
        assert_eq!(
            fs::read_to_string(&existing_file).unwrap(),
            "DO NOT OVERWRITE"
        );

        fs::remove_dir_all(&temp).unwrap();
    }

    #[test]
    fn rolls_back_files_and_directories_after_failed_install() {
        let temp = env::temp_dir().join(format!("vpk-test-rollback-{}", std::process::id()));

        let _ = fs::remove_dir_all(&temp);
        fs::create_dir_all(&temp).unwrap();

        let package_dir = temp.join("package");
        fs::create_dir_all(package_dir.join("data/aaa-new/nested")).unwrap();
        fs::create_dir_all(package_dir.join("data/usr/share")).unwrap();

        fs::write(
            package_dir.join("manifest.toml"),
            r#"
name = "rollback-test"
version = "1.0.0"
description = "Rollback test"
"#,
        )
        .unwrap();

        fs::write(
            package_dir.join("data/aaa-new/nested/new.txt"),
            "this file should be rolled back",
        )
        .unwrap();

        fs::write(
            package_dir.join("data/usr/share/hello.txt"),
            "package version",
        )
        .unwrap();

        let archive_path = temp.join("rollback-test.vpk");
        pack_package(
            package_dir.to_str().unwrap(),
            archive_path.to_str().unwrap(),
        )
        .unwrap();

        let target = temp.join("install-root");
        let existing_file = target.join("usr/share/hello.txt");

        fs::create_dir_all(existing_file.parent().unwrap()).unwrap();
        fs::write(&existing_file, "original contents").unwrap();

        let result = install_package(archive_path.to_str().unwrap(), target.to_str().unwrap());

        assert!(
            result.is_err(),
            "installation should fail on the existing file"
        );

        assert_eq!(
            fs::read_to_string(&existing_file).unwrap(),
            "original contents",
            "pre-existing files must remain untouched"
        );

        assert!(
            !target.join("aaa-new").exists(),
            "new directories should be removed during rollback"
        );

        assert!(
            !target.join("aaa-new/nested/new.txt").exists(),
            "new files should be removed during rollback"
        );

        assert!(
            database::get_installed(&fs::canonicalize(&target).unwrap(), "rollback-test")
                .unwrap()
                .is_none(),
            "failed installations must not leave a package record"
        );

        fs::remove_dir_all(&temp).unwrap();
    }

    #[test]
    fn detects_circular_dependencies() {
        let index = RepoIndex { package: vec![] };
        let target = env::temp_dir().join(format!("vpk-test-cycle-{}", std::process::id()));

        let mut visiting = vec!["hello".to_string()];

        let result = install_repository_package_recursive("hello", &index, &target, &mut visiting);

        let error = match result {
            Ok(_) => panic!("Expected the resolver to return an error"),
            Err(error) => error,
        };

        assert!(error.contains("Circular dependency detected"));
        assert!(error.contains("hello -> hello"));
    }

    #[test]
    fn reports_missing_repository_package() {
        let index = RepoIndex { package: vec![] };
        let target = env::temp_dir().join(format!("vpk-test-missing-{}", std::process::id()));

        let mut visiting = Vec::new();

        let result = install_repository_package_recursive(
            "nonexistent-package",
            &index,
            &target,
            &mut visiting,
        );

        let error = match result {
            Ok(_) => panic!("Expected the resolver to return an error"),
            Err(error) => error,
        };

        assert!(error.contains("was not found in the repository"));
    }

    #[test]
    fn finds_transitive_dependents_in_a_branching_tree() {
        let packages = vec![
            database::InstalledPackage {
                name: "A".into(),
                version: "1.0".into(),
                description: "A depends on B".into(),
                files: vec![],
                dependencies: vec!["B".into()],
            },
            database::InstalledPackage {
                name: "C".into(),
                version: "1.0".into(),
                description: "C depends on A".into(),
                files: vec![],
                dependencies: vec!["A".into()],
            },
            database::InstalledPackage {
                name: "D".into(),
                version: "1.0".into(),
                description: "D depends on A".into(),
                files: vec![],
                dependencies: vec!["A".into()],
            },
            database::InstalledPackage {
                name: "E".into(),
                version: "1.0".into(),
                description: "E depends on D".into(),
                files: vec![],
                dependencies: vec!["D".into()],
            },
            database::InstalledPackage {
                name: "F".into(),
                version: "1.0".into(),
                description: "F depends on B".into(),
                files: vec![],
                dependencies: vec!["B".into()],
            },
        ];

        let mut tree = String::new();

        let affected = build_dependent_tree("B", &packages, "", &mut vec!["B".into()], &mut tree);

        assert_eq!(affected, 5);
        assert_eq!(
            tree,
            concat!(
                "├── A depends on B\n",
                "│   ├── C depends on A\n",
                "│   └── D depends on A\n",
                "│       └── E depends on D\n",
                "└── F depends on B\n",
            )
        );
    }

    #[test]
    fn handles_cycles_in_dependent_tree() {
        let packages = vec![
            database::InstalledPackage {
                name: "A".into(),
                version: "1.0".into(),
                description: "A depends on B".into(),
                files: vec![],
                dependencies: vec!["B".into()],
            },
            database::InstalledPackage {
                name: "B".into(),
                version: "1.0".into(),
                description: "B depends on A".into(),
                files: vec![],
                dependencies: vec!["A".into()],
            },
        ];

        let mut tree = String::new();

        let affected = build_dependent_tree("B", &packages, "", &mut vec!["B".into()], &mut tree);

        assert_eq!(affected, 1);
        assert_eq!(tree, "└── A depends on B\n");
    }
}
