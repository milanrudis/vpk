use serde::{Deserialize, Serialize};
use std::{
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    process,
};

#[derive(Debug, Serialize, Deserialize)]
pub struct InstalledPackage {
    pub name: String,
    pub version: String,
    pub description: String,
    pub files: Vec<String>,
}

fn database_directory(root: &Path) -> PathBuf {
    root.join("var/lib/vpk/installed")
}

pub fn record_install(root: &Path, package: InstalledPackage) -> io::Result<()> {
    // Only allow simple package names in database filenames.
    if package.name.is_empty()
        || !package
            .name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
        || package.name == "."
        || package.name == ".."
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Package name contains unsupported characters",
        ));
    }

    let directory = database_directory(root);
    fs::create_dir_all(&directory)?;

    let destination = directory.join(format!("{}.toml", package.name));
    let contents = toml::to_string_pretty(&package).map_err(io::Error::other)?;

    // Create a unique temporary file without overwriting anything.
    let mut temporary = None;

    for attempt in 0..100 {
        let path = directory.join(format!(
            ".{}.toml.tmp-{}-{attempt}",
            package.name,
            process::id()
        ));

        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => {
                temporary = Some((path, file));
                break;
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }

    let (temporary_path, mut file) = temporary.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::AlreadyExists,
            "Could not find an available temporary database filename",
        )
    })?;

    // Write and flush the complete record before publishing it.
    let result = (|| -> io::Result<()> {
        file.write_all(contents.as_bytes())?;
        file.sync_all()?;
        drop(file);

        // A hard link publishes the record atomically and fails if the
        // destination already exists, rather than replacing it.
        fs::hard_link(&temporary_path, &destination)?;

        Ok(())
    })();

    // Once published, the destination is the valid record. Removing the
    // temporary link is best-effort so cleanup failure cannot report a
    // failed install while leaving a successfully published record behind.
    let _ = fs::remove_file(&temporary_path);

    result
}

pub fn list_installed(root: &Path) -> io::Result<Vec<InstalledPackage>> {
    let directory = database_directory(root);

    if !directory.exists() {
        return Ok(Vec::new());
    }

    let mut packages = Vec::new();

    for entry in fs::read_dir(&directory)? {
        let entry = entry?;
        let path = entry.path();

        if path.extension().and_then(|ext| ext.to_str()) != Some("toml") {
            continue;
        }

        let contents = fs::read_to_string(&path)?;

        let package: InstalledPackage = toml::from_str(&contents).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "Could not read package record '{}': {error}",
                    path.display()
                ),
            )
        })?;

        packages.push(package);
    }

    packages.sort_by(|a, b| a.name.cmp(&b.name));

    Ok(packages)
}
pub fn get_installed(root: &Path, name: &str) -> io::Result<Option<InstalledPackage>> {
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
        || name == "."
        || name == ".."
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Invalid package name",
        ));
    }

    let path = database_directory(root).join(format!("{name}.toml"));

    match fs::read_to_string(&path) {
        Ok(contents) => {
            let package: InstalledPackage = toml::from_str(&contents).map_err(|error| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("Invalid package record: {error}"),
                )
            })?;

            if package.name != name {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Package record name does not match its filename",
                ));
            }

            Ok(Some(package))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

pub fn remove_installed(root: &Path, name: &str) -> io::Result<InstalledPackage> {
    let package = get_installed(root, name)?.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("Package '{name}' is not installed"),
        )
    })?;

    let directory = database_directory(root);

    // Refuse removal if another package claims any of these files.
    for entry in fs::read_dir(&directory)? {
        let entry = entry?;
        let record_path = entry.path();

        if record_path == directory.join(format!("{name}.toml"))
            || record_path.extension().and_then(|e| e.to_str()) != Some("toml")
        {
            continue;
        }

        let contents = fs::read_to_string(&record_path)?;
        let other: InstalledPackage = toml::from_str(&contents).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("Invalid package record: {error}"),
            )
        })?;

        for file in &package.files {
            if other.files.contains(file) {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!(
                        "Cannot remove '{}': another package also claims '{}'",
                        name, file
                    ),
                ));
            }
        }
    }

    // Validate every path before deleting anything.
    let mut paths = Vec::new();

    for file in &package.files {
        let relative = Path::new(file);

        if relative.as_os_str().is_empty()
            || !relative
                .components()
                .all(|component| matches!(component, std::path::Component::Normal(_)))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("Unsafe path in package record: {file}"),
            ));
        }

        let mut current = root.to_path_buf();

        for component in relative.components() {
            current.push(component);

            match fs::symlink_metadata(&current) {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        format!("Refusing to follow symbolic link: {}", current.display()),
                    ));
                }
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    break;
                }
                Err(error) => return Err(error),
            }
        }

        paths.push((current, file.clone()));
    }

    // Remove only regular files, never directories or symbolic links.
    for (path, relative) in &paths {
        match fs::symlink_metadata(path) {
            Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {
                fs::remove_file(path)?;
            }
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!("Refusing to remove non-regular file: {relative}"),
                ));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }

    fs::remove_file(directory.join(format!("{name}.toml")))?;

    Ok(package)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duplicate_install_does_not_overwrite_record() {
        let root = std::env::temp_dir().join(format!("vpk-db-test-{}", std::process::id()));

        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();

        let original = InstalledPackage {
            name: "duplicate-test".into(),
            version: "1.0.0".into(),
            description: "Original package".into(),
            files: vec!["usr/bin/original".into()],
        };

        record_install(&root, original).unwrap();

        let duplicate = InstalledPackage {
            name: "duplicate-test".into(),
            version: "2.0.0".into(),
            description: "Replacement package".into(),
            files: vec!["usr/bin/replacement".into()],
        };

        let result = record_install(&root, duplicate);

        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::AlreadyExists);

        let saved = get_installed(&root, "duplicate-test").unwrap().unwrap();

        assert_eq!(saved.version, "1.0.0");
        assert_eq!(saved.description, "Original package");
        assert_eq!(saved.files, vec!["usr/bin/original"]);

        fs::remove_dir_all(&root).unwrap();
    }
}
