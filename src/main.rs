
mod database;
use database::InstalledPackage;
use serde::Deserialize;
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

fn main() {
    let args: Vec<String> = env::args().collect();

    if args.len() < 2 {
        eprintln!("Usage: vpk <command> [arguments]");
        eprintln!("Try 'vpk --help' for more information.");
        process::exit(1);
    }

    match args[1].as_str() {
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
            if args.len() != 4 {
                eprintln!(
                    "Usage: vpk install <package.vpk> <target-directory>"
                );
                process::exit(1);
            }

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
