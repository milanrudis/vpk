
mod database;
use database::InstalledPackage;
use serde::Deserialize;
use sha2::{Digest, Sha256};

const REPO_INDEX_URL: &str =
    "https://raw.githubusercontent.com/milanrudis/vast-packages/main/repo/index.toml";

#[derive(Debug, Deserialize)]
struct RepoIndex {
    package: Vec<RepoPackage>,
}

#[derive(Debug, Deserialize)]
struct RepoPackage {
    name: String,
    version: String,
    description: String,
    url: String,
    sha256: String,
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
    let file = File::open(path)
        .map_err(|e| format!("Could not open '{path}': {e}"))?;

    let decoder = zstd::Decoder::new(file)
        .map_err(|e| format!("Could not decompress package: {e}"))?;

    let mut archive = Archive::new(decoder);

    let entries = archive.entries()
        .map_err(|e| format!("Could not read archive: {e}"))?;

    for entry in entries {
        let mut entry = entry
            .map_err(|e| format!("Could not read archive entry: {e}"))?;

        let path = entry.path()
            .map_err(|e| format!("Invalid archive path: {e}"))?
            .into_owned();

        if path == Path::new("manifest.toml") {
            if !entry.header().entry_type().is_file() {
                return Err("Package manifest is not a regular file".into());
            }

            let mut contents = String::new();

            entry.read_to_string(&mut contents)
                .map_err(|e| format!("Could not read manifest: {e}"))?;

            return toml::from_str(&contents)
                .map_err(|e| format!("Invalid package manifest: {e}"));
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

    archive.append_path_with_name(
        root.join("manifest.toml"),
        "manifest.toml",
    )?;

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
            return Err(format!(
                "Unexpected path in package: {}",
                path.display()
            ));
        }
    }

    let mut relative = PathBuf::new();

    for component in components {
        match component {
            Component::Normal(name) => relative.push(name),
            _ => {
                return Err(format!(
                    "Unsafe path in package: {}",
                    path.display()
                ));
            }
        }
    }

    Ok(relative)
}

// Create directories without following existing symbolic links.
fn ensure_directory(root: &Path, relative: &Path) -> io::Result<()> {
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
                if metadata.file_type().is_symlink()
                    || !metadata.is_dir()
                {
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
            }
            Err(error) => return Err(error),
        }
    }

    Ok(())
}

// Install files into a chosen directory, not the live operating system.
fn install_package(
    package_path: &str,
    target: &str,
) -> Result<Package, String> {
    // Validate the manifest before installing anything.
    let package = inspect_package(package_path)?;

    fs::create_dir_all(target)
        .map_err(|e| format!("Could not create install directory: {e}"))?;

    let root = fs::canonicalize(target)
        .map_err(|e| format!("Could not resolve install directory: {e}"))?;

    let mut installed_files: Vec<String> = Vec::new();

    let file = File::open(package_path)
        .map_err(|e| format!("Could not open package: {e}"))?;

    let decoder = zstd::Decoder::new(file)
        .map_err(|e| format!("Could not decompress package: {e}"))?;

    let mut archive = Archive::new(decoder);

    let entries = archive.entries()
        .map_err(|e| format!("Could not read archive: {e}"))?;

    for entry in entries {
        let mut entry = entry
            .map_err(|e| format!("Could not read archive entry: {e}"))?;

        let archive_path = entry.path()
            .map_err(|e| format!("Invalid archive path: {e}"))?
            .into_owned();

        // The manifest describes the package; it is not installed as a file.
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
            ensure_directory(&root, &relative)
                .map_err(|e| format!(
                    "Could not create directory '{}': {e}",
                    relative.display()
                ))?;
        } else if is_file {
            let parent = relative.parent().unwrap_or(Path::new(""));

            ensure_directory(&root, parent)
                .map_err(|e| format!(
                    "Could not prepare directory for '{}': {e}",
                    relative.display()
                ))?;

            installed_files.push(relative.to_string_lossy().into_owned());

            let destination = root.join(&relative);

            // Never overwrite an existing file.
            let mut output = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&destination)
                .map_err(|e| format!(
                    "Could not create '{}': {e}. Existing files are not overwritten.",
                    destination.display()
                ))?;

            io::copy(&mut entry, &mut output)
                .map_err(|e| format!(
                    "Could not write '{}': {e}",
                    destination.display()
                ))?;
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
            files: installed_files,
        },
    )
    .map_err(|e| {
        format!("Could not save installed-package record: {e}")
    })?;

    Ok(package)
}

fn fetch_repository_index() -> Result<RepoIndex, String> {
    let response = ureq::get(REPO_INDEX_URL)
        .call()
        .map_err(|e| format!("could not fetch repository index: {e}"))?;

    let mut contents = String::new();

    response
        .into_reader()
        .read_to_string(&mut contents)
        .map_err(|e| format!("could not read repository index: {e}"))?;

    toml::from_str(&contents)
        .map_err(|e| format!("could not parse repository index: {e}"))
}


fn install_repository_package(name: &str) -> Result<(Package, PathBuf), String> {
    fn safe_component(value: &str) -> bool {
        !value.is_empty()
            && value != "."
            && value != ".."
            && value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.')
    }

    if !safe_component(name) {
        return Err("Invalid package name".into());
    }

    let index = fetch_repository_index()?;

    let repo_package = index
        .package
        .iter()
        .find(|package| package.name == name)
        .ok_or_else(|| format!("Package '{name}' was not found in the repository"))?;

    if !safe_component(&repo_package.version) {
        return Err("Invalid package version in repository index".into());
    }

    if !repo_package.url.starts_with("https://") {
        return Err("Package URL must use HTTPS".into());
    }

    println!("Downloading {} {}...", repo_package.name, repo_package.version);

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

    fs::create_dir_all(&cache)
        .map_err(|e| format!("Could not create package cache: {e}"))?;

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

    if let Err(error) = (|| -> io::Result<()> {
        use io::Write;
        archive_file.write_all(&bytes)?;
        archive_file.flush()
    })() {
        let _ = fs::remove_file(&archive_path);
        return Err(format!("Could not save downloaded package: {error}"));
    }

    let result = (|| -> Result<(Package, PathBuf), String> {
        let archive_string = archive_path
            .to_str()
            .ok_or_else(|| "Invalid temporary package path".to_string())?;

        let manifest = inspect_package(archive_string)?;

        if manifest.name != repo_package.name
            || manifest.version != repo_package.version
        {
            return Err("Downloaded package manifest does not match repository index".into());
        }

        let target = home.join(".local/share/vpk/root");
        let target_string = target
            .to_str()
            .ok_or_else(|| "Invalid installation path".to_string())?;

        let installed = install_package(archive_string, target_string)?;

        Ok((installed, target))
    })();

    let _ = fs::remove_file(&archive_path);

    result
}


fn main() {
    let args: Vec<String> = env::args().collect();

    if args.len() < 2 {
        eprintln!("Usage: vpk <command> [arguments]");
        eprintln!("Try 'vpk --help' for more information.");
        process::exit(1);
    }

    match args[1].as_str() {
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
                    println!("{} {} - {}", package.name, package.version, package.description);
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
            if args.len() != 4 {
                eprintln!("Usage: vpk remove <package-name> <target-directory>");
                process::exit(1);
            }

            let name = &args[2];
            let target = &args[3];

            let root = match fs::canonicalize(target) {
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
                    println!(
                        "Successfully removed {} {}.",
                        package.name, package.version
                    );
                }
                Err(error) => {
                    eprintln!("vpk: could not remove package: {error}");
                    process::exit(1);
                }
            }
        }

	"list" => {
            if args.len() != 3 {
                eprintln!("Usage: vpk list <target-directory>");
                process::exit(1);
            }

            let root = match fs::canonicalize(&args[2]) {
                Ok(root) => root,
                Err(error) => {
                    eprintln!(
                        "vpk: could not access target directory: {error}"
                    );
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
            } else if args.len() == 4 {
                let name = &args[2];
                let target = &args[3];

                let root = match fs::canonicalize(target) {
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
                eprintln!("  vpk info <package.vpk>");
                eprintln!("  vpk info <package-name> <target-directory>");
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
                    Ok((package, target)) => {
                        println!(
                            "Installed {} {} successfully!",
                            package.name, package.version
                        );
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
            println!("VPK - The Vesper Package Manager");
            println!();
            println!("Commands:");
	    println!("  remove <name> <target-directory> Remove a package");
	    println!("  list <target-directory>        List installed packages");
            println!("  pack <directory> <output.vpk>  Create a package");
            println!("  info <package.vpk>             Show package information");
	    println!("  inspect <package.vpk>          Inspect a package (alias for info)");
            println!("  install <package.vpk> <dir>    Install into a directory");
            println!();
            println!("Options:");
            println!("  -V, --version                  Show version");
            println!("  -h, --help                     Show this help");
        }

        _ => {
            eprintln!("vpk: unknown command '{}'", args[1]);
            eprintln!("Try 'vpk --help' for more information.");
            process::exit(1);
        }
    }
}
