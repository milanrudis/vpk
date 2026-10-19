use serde::{Deserialize, Serialize};
use std::{
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    process,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstalledPackage {
    pub name: String,
    pub version: String,
    pub description: String,
    pub files: Vec<String>,
    #[serde(default)]
    pub dependencies: Vec<String>,
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

pub fn replace_install_record(root: &Path, package: InstalledPackage) -> io::Result<()> {
    // Only replace a record that already exists for this package.
    let existing = get_installed(root, &package.name)?.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("Package '{}' is not installed", package.name),
        )
    })?;

    if existing.name != package.name {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Package name does not match existing record",
        ));
    }

    let directory = database_directory(root);
    let destination = directory.join(format!("{}.toml", package.name));
    let contents = toml::to_string_pretty(&package).map_err(io::Error::other)?;

    // Write the complete replacement to a unique temporary file first.
    let mut temporary = None;

    for attempt in 0..100 {
        let path = directory.join(format!(
            ".{}.toml.upgrade-{}-{attempt}",
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

    let result = (|| -> io::Result<()> {
        file.write_all(contents.as_bytes())?;
        file.sync_all()?;
        drop(file);

        // On Linux, rename atomically replaces the old record.
        fs::rename(&temporary_path, &destination)?;
        Ok(())
    })();

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
            dependencies: vec![],
        };

        record_install(&root, original).unwrap();

        let duplicate = InstalledPackage {
            name: "duplicate-test".into(),
            version: "2.0.0".into(),
            description: "Replacement package".into(),
            files: vec!["usr/bin/replacement".into()],
            dependencies: vec![],
        };

        let result = record_install(&root, duplicate);

        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::AlreadyExists);

        let saved = get_installed(&root, "duplicate-test").unwrap().unwrap();

        assert_eq!(saved.version, "1.0.0");
        assert_eq!(saved.description, "Original package");
        assert_eq!(saved.files, vec!["usr/bin/original"]);

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn upgrade_replaces_existing_install_record() {
        let root = std::env::temp_dir().join(format!("vpk-db-upgrade-test-{}", std::process::id()));

        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();

        record_install(
            &root,
            InstalledPackage {
                name: "upgrade-test".into(),
                version: "1.0.0".into(),
                description: "Old version".into(),
                files: vec!["usr/bin/old".into()],
                dependencies: vec![],
            },
        )
        .unwrap();

        replace_install_record(
            &root,
            InstalledPackage {
                name: "upgrade-test".into(),
                version: "2.0.0".into(),
                description: "New version".into(),
                files: vec!["usr/bin/new".into()],
                dependencies: vec!["dependency-test".into()],
            },
        )
        .unwrap();

        let installed = get_installed(&root, "upgrade-test").unwrap().unwrap();

        assert_eq!(installed.version, "2.0.0");
        assert_eq!(installed.description, "New version");
        assert_eq!(installed.files, vec!["usr/bin/new"]);
        assert_eq!(installed.dependencies, vec!["dependency-test"]);

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn upgrade_refuses_missing_install_record() {
        let root = std::env::temp_dir().join(format!(
            "vpk-db-upgrade-missing-test-{}",
            std::process::id()
        ));

        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();

        let result = replace_install_record(
            &root,
            InstalledPackage {
                name: "not-installed".into(),
                version: "2.0.0".into(),
                description: "New version".into(),
                files: vec![],
                dependencies: vec![],
            },
        );

        assert!(result.is_err());
        assert!(get_installed(&root, "not-installed").unwrap().is_none());

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn detects_files_owned_by_another_package() {
        let root =
            std::env::temp_dir().join(format!("vpk-db-ownership-test-{}", std::process::id()));

        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();

        record_install(
            &root,
            InstalledPackage {
                name: "package-a".into(),
                version: "1.0.0".into(),
                description: "Package A".into(),
                files: vec!["usr/bin/shared-tool".into()],
                dependencies: vec![],
            },
        )
        .unwrap();

        record_install(
            &root,
            InstalledPackage {
                name: "package-b".into(),
                version: "1.0.0".into(),
                description: "Package B".into(),
                files: vec!["usr/bin/other-tool".into()],
                dependencies: vec![],
            },
        )
        .unwrap();

        assert!(other_package_owns_file(&root, "package-b", "usr/bin/shared-tool").unwrap());

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn does_not_treat_current_package_as_another_owner() {
        let root =
            std::env::temp_dir().join(format!("vpk-db-ownership-self-test-{}", std::process::id()));

        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();

        record_install(
            &root,
            InstalledPackage {
                name: "package-a".into(),
                version: "1.0.0".into(),
                description: "Package A".into(),
                files: vec!["usr/bin/my-tool".into()],
                dependencies: vec![],
            },
        )
        .unwrap();

        assert!(!other_package_owns_file(&root, "package-a", "usr/bin/my-tool").unwrap());

        fs::remove_dir_all(&root).unwrap();
    }
}

pub fn other_package_owns_file(
    root: &Path,
    current_package: &str,
    file_path: &str,
) -> io::Result<bool> {
    let packages = list_installed(root)?;

    Ok(packages.iter().any(|package| {
        package.name != current_package && package.files.iter().any(|file| file == file_path)
    }))
}
