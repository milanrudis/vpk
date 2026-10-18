# VPK

A lightweight package manager for Vast Linux, written in Rust.

VPK manages `.vpk` packages, downloads packages from a repository, verifies package checksums, and tracks installed software.

## Features

- **Package management:** Install, remove, list, inspect, search, and pack packages.
- **Dependency resolution:** Automatically installs required dependencies before installing a package.
- **Circular dependency detection:** Detects dependency cycles and missing packages in the repository.
- **Removal warnings:** Shows which installed packages depend on a package before removing it.
- **Checksum verification:** Verifies downloaded packages using SHA-256 checksums.
- **Installation tracking:** Records installed package versions, files, and dependencies.
- **Safer installation:** Validates package paths and rolls back files and directories after a failed installation.
- **Compressed packages:** Uses tar archives compressed with Zstandard.

## Requirements

- Rust and Cargo to build from source.
- A configured VPK package repository to install packages from.

## Build

Clone the repository and build VPK with Cargo:

```bash
git clone https://github.com/milanrudis/vpk.git
cd vpk
cargo build --release
```

The compiled executable will be located at `target/release/vpk`.

## Package repository

VPK uses a TOML repository index to locate packages. Each package entry includes its name, version, description, download URL, and SHA-256 checksum.

## Package format

A VPK package contains a `manifest.toml` describing the package and a `data/` directory containing the files to install. Packages are distributed as tar archives compressed with Zstandard.

## Project status

VPK 0.2.0 introduces dependency installation and dependency-aware removal warnings. The project is under active development.
