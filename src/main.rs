use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{self, Write};
use std::path::Path;
use tar::Archive;
use zstd::stream::read::Decoder;
use terminal_size::{Width, terminal_size};

const ANDSTORE_ROOT: &str = "/data/local/andstore";
const PIE_DATA: &str = "/data/adb/pie";
const REPO_URL: &str =
    "https://raw.githubusercontent.com/andstore-org/andstore-repo/main/repo.json";

#[derive(Parser)]
#[command(name = "pie", about = "andstore package manager")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    #[command(alias = "add")]
    Install {
        package: String,
        #[arg(short = 'y', long = "no-confirm")]
        no_confirm: bool,
    },
    #[command(alias = "remove")]
    Uninstall {
        package: String,
    },
    Update,
    Search {
        query: Option<String>,
    },
    #[command(name = "list")]
    List,
}

#[derive(Deserialize)]
struct Repo {
    packages: HashMap<String, Package>,
}

#[derive(Deserialize)]
struct Package {
    version: String,
    min_api: Option<String>,
    dependencies: Vec<String>,
    conflicts: Vec<String>,
    architectures: HashMap<String, Architecture>,
}

#[derive(Deserialize)]
struct Architecture {
    url: String,
    sha256: String,
    size: u64,
    uncompressed_size: u64,
    contents: Vec<String>,
}

#[derive(Serialize, Deserialize)]
struct InstalledPackage {
    name: String,
    version: String,
    contents: Vec<String>,
}

#[derive(Serialize, Deserialize, Default)]
struct InstalledPackages {
    packages: HashMap<String, InstalledPackage>,
}

fn get_separator() -> String {
    let width = if let Some((Width(w), _)) = terminal_size() {
        w as usize
    } else {
        80 // fallback
    };
    "=".repeat(width.max(40).min(120)) // min 40, max 120 chars
}

fn main() {
    let cli = Cli::parse();

    if let Err(e) = run(cli) {
        eprintln!("Error: {e}");
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> Result<(), Box<dyn std::error::Error>> {
    match cli.command {
        Commands::Install {
            package,
            no_confirm,
        } => install_package(&package, no_confirm)?,
        Commands::Uninstall { package } => uninstall_package(&package)?,
        Commands::Update => update_repo()?,
        Commands::Search { query } => search_packages(query.as_deref())?,
        Commands::List => list_installed()?,
    }
    Ok(())
}

fn get_arch() -> Result<String, Box<dyn std::error::Error>> {
    let output = std::process::Command::new("getprop")
        .args(["ro.product.cpu.abi"])
        .output()?;

    let arch = String::from_utf8(output.stdout)?.trim().to_string();

    match arch.as_str() {
        "arm64-v8a" | "armeabi-v7a" | "x86" | "x86_64" | "riscv64" => Ok(arch),
        _ => Err(format!("Unsupported architecture: {arch}").into()),
    }
}

fn get_api_level() -> Result<u32, Box<dyn std::error::Error>> {
    let output = std::process::Command::new("getprop")
        .args(["ro.build.version.sdk"])
        .output()?;

    let binding = String::from_utf8(output.stdout)?;
    let api_str = binding.trim();
    let api_level: u32 = api_str.parse()?;
    Ok(api_level)
}

fn fetch_repo() -> Result<Repo, Box<dyn std::error::Error>> {
    let response = reqwest::blocking::get(REPO_URL)?;
    let repo: Repo = response.json()?;
    Ok(repo)
}

fn get_installed_packages() -> Result<InstalledPackages, Box<dyn std::error::Error>> {
    let installed_file = format!("{PIE_DATA}/installed.json");

    if !Path::new(&installed_file).exists() {
        return Ok(InstalledPackages::default());
    }

    let content = fs::read_to_string(&installed_file)?;
    let installed: InstalledPackages = serde_json::from_str(&content)?;
    Ok(installed)
}

fn check_api_compatibility(package: &Package) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(min_api_str) = &package.min_api {
        if min_api_str.trim().is_empty() {
            return Ok(());
        }
        
        let device_api = get_api_level()?;
        let min_api: u32 = min_api_str.parse()
            .map_err(|_| format!("Invalid min_api format: '{}'", min_api_str))?;
        
        if device_api < min_api {
            return Err(format!(
                "Package requires API level {} but device is API level {}", 
                min_api, device_api
            ).into());
        }
    }
    Ok(())
 }

fn save_installed_packages(
    installed: &InstalledPackages,
) -> Result<(), Box<dyn std::error::Error>> {
    fs::create_dir_all(PIE_DATA)?;
    let installed_file = format!("{PIE_DATA}/installed.json");
    let content = serde_json::to_string_pretty(installed)?;
    fs::write(&installed_file, content)?;
    Ok(())
}

fn find_package_by_content(repo: &Repo, query: &str) -> Option<String> {
    for (pkg_name, package) in &repo.packages {
        let arch = get_arch().ok()?;
        if let Some(architecture) = package.architectures.get(&arch) {
            for content in &architecture.contents {
                // Check if the content path ends with the query (for binaries)
                if content.ends_with(&format!("/{query}"))
                    || content.ends_with(&format!("bin/{query}"))
                    || content == query
                {
                    return Some(pkg_name.clone());
                }
            }
        }
    }
    None
}

fn format_size(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB"];
    let mut size = bytes as f64;
    let mut unit_index = 0;

    while size >= 1024.0 && unit_index < UNITS.len() - 1 {
        size /= 1024.0;
        unit_index += 1;
    }

    if unit_index == 0 {
        format!("{} {}", bytes, UNITS[unit_index])
    } else {
        format!("{:.1} {}", size, UNITS[unit_index])
    }
}

fn resolve_dependencies(
    repo: &Repo,
    package_name: &str,
    installed: &InstalledPackages,
) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    let mut to_install = Vec::new();
    let mut visited = HashSet::new();

    fn resolve_recursive(
        repo: &Repo,
        pkg_name: &str,
        installed: &InstalledPackages,
        to_install: &mut Vec<String>,
        visited: &mut HashSet<String>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if visited.contains(pkg_name) {
            return Ok(());
        }
        visited.insert(pkg_name.to_string());

        let package = repo
            .packages
            .get(pkg_name)
            .ok_or(format!("Dependency '{pkg_name}' not found"))?;

        for dep in &package.dependencies {
            if !installed.packages.contains_key(dep) && !to_install.contains(dep) {
                resolve_recursive(repo, dep, installed, to_install, visited)?;
                to_install.push(dep.clone());
            }
        }

        Ok(())
    }

    resolve_recursive(repo, package_name, installed, &mut to_install, &mut visited)?;
    Ok(to_install)
}

fn handle_conflicts(
    package: &Package,
    installed: &mut InstalledPackages,
    no_confirm: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut conflicts_to_remove = Vec::new();

    for conflict in &package.conflicts {
        if installed.packages.contains_key(conflict) {
            conflicts_to_remove.push(conflict.clone());
        }
    }

    if !conflicts_to_remove.is_empty() {
        println!("\n{}", get_separator());
        println!("CONFLICT RESOLUTION");
        println!("{}", get_separator());
        println!("The following packages conflict and will be removed:");
        for conflict in &conflicts_to_remove {
            if let Some(pkg) = installed.packages.get(conflict) {
                println!("  - {} v{}", conflict, pkg.version);
            }
        }

        if !no_confirm {
            print!("\nContinue? [Y/n]: ");
            io::stdout().flush()?;

            let mut input = String::new();
            io::stdin().read_line(&mut input)?;
            let input = input.trim().to_lowercase();

            if input == "n" || input == "no" {
                return Err("Installation cancelled due to conflicts".into());
            }
        }

        for conflict in conflicts_to_remove {
            println!("Removing conflicting package: {conflict}");
            remove_package_files(&conflict, installed)?;
            installed.packages.remove(&conflict);
        }
        println!();
    }

    Ok(())
}

fn remove_package_files(
    name: &str,
    installed: &InstalledPackages,
) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(package) = installed.packages.get(name) {
        for file_path in &package.contents {
            let full_path = format!("{ANDSTORE_ROOT}/{file_path}");
            if Path::new(&full_path).exists() {
                fs::remove_file(&full_path)?;
            }
        }
    }
    Ok(())
}

fn install_single_package(
    repo: &Repo,
    name: &str,
    installed: &mut InstalledPackages,
) -> Result<(), Box<dyn std::error::Error>> {
    let package = repo
        .packages
        .get(name)
        .ok_or(format!("Package '{name}' not found"))?;

    let arch = get_arch()?;
    let architecture = package.architectures.get(&arch).ok_or(format!(
        "Package '{name}' not available for architecture '{arch}'"
    ))?;

    // Show package info before downloading
    println!("Package: {} v{}", name, package.version);
    println!(
        "Download size: {} | Installed size: {}",
        format_size(architecture.size),
        format_size(architecture.uncompressed_size)
    );

    // Download package
    print!("Downloading {name}... ");
    io::stdout().flush()?;
    let response = reqwest::blocking::get(&architecture.url)?;
    let content = response.bytes()?;
    println!("✓");

    // Verify checksum
    print!("Verifying checksum... ");
    io::stdout().flush()?;
    let mut hasher = Sha256::new();
    hasher.update(&content);
    let hash = hex::encode(hasher.finalize());

    if hash != architecture.sha256 {
        println!("✗");
        return Err(format!("Checksum verification failed for package '{name}'").into());
    }
    println!("✓");

    // Create temp file and extract
    print!("Extracting {name}... ");
    io::stdout().flush()?;
    let temp_file = tempfile::NamedTempFile::new()?;
    fs::write(temp_file.path(), &content)?;

    // Extract package
    let file = fs::File::open(temp_file.path())?;
    let decoder = Decoder::new(file)?;
    let mut archive = Archive::new(decoder);

    fs::create_dir_all(ANDSTORE_ROOT)?;
    archive.unpack(ANDSTORE_ROOT)?;
    println!("✓");

    // Update installed packages
    let installed_package = InstalledPackage {
        name: name.to_string(),
        version: package.version.clone(),
        contents: architecture.contents.clone(),
    };

    installed
        .packages
        .insert(name.to_string(), installed_package);
    println!("Successfully installed {} v{}\n", name, package.version);

    Ok(())
}

fn install_package(name: &str, no_confirm: bool) -> Result<(), Box<dyn std::error::Error>> {
    println!("Fetching repository information...");
    let repo = fetch_repo()?;
    let mut installed = get_installed_packages()?;

    // Check if it's a direct package or content search
    let target_package = if repo.packages.contains_key(name) {
        name.to_string()
    } else {
        // Search for package containing this content
        if let Some(pkg_name) = find_package_by_content(&repo, name) {
            if !no_confirm {
                println!("'{name}' is provided by package '{pkg_name}'");
                print!("Install '{pkg_name}'? [Y/n]: ");
                io::stdout().flush()?;

                let mut input = String::new();
                io::stdin().read_line(&mut input)?;
                let input = input.trim().to_lowercase();

                if input == "n" || input == "no" {
                    println!("Installation cancelled");
                    return Ok(());
                }
            }
            pkg_name
        } else {
            return Err(format!("Package or content '{name}' not found").into());
        }
    };

    let package = repo.packages.get(&target_package).unwrap();

    // Check if already installed
    if installed.packages.contains_key(&target_package) {
        println!(
            "Package '{}' v{} is already installed",
            target_package, package.version
        );
        return Ok(());
    }

    // Check API compatibility
    check_api_compatibility(package)?;

    // Handle conflicts
    handle_conflicts(package, &mut installed, no_confirm)?;

    // Resolve dependencies
    let dependencies = resolve_dependencies(&repo, &target_package, &installed)?;

    // Calculate total download and installed sizes
    let arch = get_arch()?;
    let mut total_download = 0u64;
    let mut total_installed = 0u64;

    // Add main package sizes
    if let Some(main_arch) = package.architectures.get(&arch) {
        total_download += main_arch.size;
        total_installed += main_arch.uncompressed_size;
    }

    // Add dependency sizes
    for dep in &dependencies {
        if let Some(dep_pkg) = repo.packages.get(dep) {
            if let Some(dep_arch) = dep_pkg.architectures.get(&arch) {
                total_download += dep_arch.size;
                total_installed += dep_arch.uncompressed_size;
            }
        }
    }

    // Show installation summary
    println!("\n{}", get_separator());
    println!("INSTALLATION SUMMARY");
    println!("{}", get_separator());

    if !dependencies.is_empty() {
        println!("Dependencies to install ({}):", dependencies.len());
        for dep in &dependencies {
            let dep_pkg = repo.packages.get(dep).unwrap();
            println!("  ├─ {} v{}", dep, dep_pkg.version);
        }
    }

    println!("Main package:");
    println!("  └─ {} v{}", target_package, package.version);

    println!("\nTotal download size: {}", format_size(total_download));
    println!("Total installed size: {}", format_size(total_installed));

    if !no_confirm {
        print!("\nProceed with installation? [Y/n]: ");
        io::stdout().flush()?;

        let mut input = String::new();
        io::stdin().read_line(&mut input)?;
        let input = input.trim().to_lowercase();

        if input == "n" || input == "no" {
            println!("Installation cancelled");
            return Ok(());
        }
    }

    println!("\n{}", get_separator());
    println!("INSTALLING PACKAGES");
    println!("{}", get_separator());

    // Install dependencies first
    for (i, dep) in dependencies.iter().enumerate() {
        println!(
            "[{}/{}] Installing dependency: {}",
            i + 1,
            dependencies.len(),
            dep
        );
        install_single_package(&repo, dep, &mut installed)?;
    }

    // Install main package
    if !dependencies.is_empty() {
        println!(
            "[{}/{}] Installing main package: {}",
            dependencies.len() + 1,
            dependencies.len() + 1,
            target_package
        );
    }
    install_single_package(&repo, &target_package, &mut installed)?;

    // Save updated installed packages
    save_installed_packages(&installed)?;

    println!("{}", get_separator());
    println!("Installation completed successfully!");
    println!("{}", get_separator());

    Ok(())
}

fn uninstall_package(name: &str) -> Result<(), Box<dyn std::error::Error>> {
    let repo = fetch_repo()?;
    let mut installed = get_installed_packages()?;

    // Check if it's a direct package or content search
    let target_package = if installed.packages.contains_key(name) {
        name.to_string()
    } else {
        // search for package containing this content
        if let Some(pkg_name) = find_package_by_content(&repo, name) {
            if installed.packages.contains_key(&pkg_name) {
                println!("'{name}' is provided by package '{pkg_name}'");
                print!("Uninstall '{pkg_name}'? [Y/n]: ");
                io::stdout().flush()?;

                let mut input = String::new();
                io::stdin().read_line(&mut input)?;
                let input = input.trim().to_lowercase();

                if input == "n" || input == "no" {
                    println!("Uninstallation cancelled");
                    return Ok(());
                }
                pkg_name
            } else {
                return Err(format!("Package containing '{name}' is not installed").into());
            }
        } else {
            return Err(format!("Package or content '{name}' not found or not installed").into());
        }
    };

    let package = installed
        .packages
        .get(&target_package)
        .ok_or(format!("Package '{target_package}' is not installed"))?;

    println!("\n{}", get_separator());
    println!("REMOVING PACKAGE");
    println!("{}", get_separator());
    println!("Package: {} v{}", target_package, package.version);
    print!("Removing files... ");
    io::stdout().flush()?;

    // Remove files
    remove_package_files(&target_package, &installed)?;

    // remove from installed packages
    installed.packages.remove(&target_package);
    save_installed_packages(&installed)?;

    println!("✓");
    println!("Successfully removed {target_package}");
    println!("{}", get_separator());

    Ok(())
}

fn update_repo() -> Result<(), Box<dyn std::error::Error>> {
    println!("Updating package repository...");
    let _repo = fetch_repo()?; // just fetch to validate
    println!("Repository updated successfully");
    Ok(())
}

fn search_packages(query: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
    println!("Fetching repository information...");
    let repo = fetch_repo()?;

    match query {
        Some(q) => {
            let mut found_packages = false;

            println!("\nSearching for '{q}'...\n");

            // 1st search for direct package matches
            for (name, package) in &repo.packages {
                if name.to_lowercase().contains(&q.to_lowercase()) {
                    println!("● {} v{}", name, package.version);
                    found_packages = true;
                }
            }

            // then search for content matches
            if let Some(pkg_name) = find_package_by_content(&repo, q) {
                if let Some(package) = repo.packages.get(&pkg_name) {
                    if !found_packages {
                        println!("No direct package matches found.\n");
                    }
                    println!("→ '{q}' is provided by:");
                    println!("   └─ {} v{}", pkg_name, package.version);
                    found_packages = true;
                }
            }

            if !found_packages {
                println!("✗ No packages or content found matching '{q}'");
            }
        }
        None => {
            println!("\nAvailable packages:\n");
            let mut packages: Vec<_> = repo.packages.iter().collect();
            packages.sort_by_key(|(name, _)| *name);

            for (name, package) in packages {
                println!("● {} v{}", name, package.version);
            }
        }
    }

    Ok(())
}

fn list_installed() -> Result<(), Box<dyn std::error::Error>> {
    let installed = get_installed_packages()?;

    if installed.packages.is_empty() {
        println!("No packages installed");
        return Ok(());
    }

    println!("Installed packages ({}):\n", installed.packages.len());
    let mut packages: Vec<_> = installed.packages.iter().collect();
    packages.sort_by_key(|(name, _)| *name);

    for (name, package) in packages {
        println!("● {} v{}", name, package.version);
    }

    Ok(())
}
