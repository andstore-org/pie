use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{self, Read, Write};
use std::path::Path;
use tar::Archive;
use zstd::stream::read::Decoder;

const ANDSTORE_ROOT: &str = "/data/local/andstore";
const PIE_DATA: &str = "/data/adb/pie";
const REPO_URL: &str = "https://raw.githubusercontent.com/andstore-org/andstore-repo/main/repo.json";

#[derive(Parser)]
#[command(name = "pie", about = "Package manager for rooted Android devices")]
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
    Uninstall { package: String },
    Update,
    Search { query: Option<String> },
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
    description: String,
    homepage: String,
    min_api: Option<String>, // Made optional
    dependencies: Vec<String>,
    license: String,
    conflicts: Vec<String>,
    architectures: HashMap<String, Architecture>,
}

#[derive(Deserialize)]
struct Architecture {
    url: String,
    sha256: String,
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

fn main() {
    let cli = Cli::parse();
    
    if let Err(e) = run(cli) {
        eprintln!("Error: {}", e);
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> Result<(), Box<dyn std::error::Error>> {
    match cli.command {
        Commands::Install { package, no_confirm } => install_package(&package, no_confirm)?,
        Commands::Uninstall { package } => uninstall_package(&package)?,
        Commands::Update => update_repo()?,
        Commands::Search { query } => search_packages(query.as_deref())?,
        Commands::List => list_installed()?,
    }
    Ok(())
}

fn get_arch() -> Result<String, Box<dyn std::error::Error>> {
    let output = std::process::Command::new("getprop")
        .args(&["ro.product.cpu.abi"])
        .output()?;
    
    let arch = String::from_utf8(output.stdout)?.trim().to_string();
    
    match arch.as_str() {
        "arm64-v8a" | "armeabi-v7a" | "x86" | "x86_64" | "riscv64" => Ok(arch),
        _ => Err(format!("Unsupported architecture: {}", arch).into()),
    }
}

fn get_api_level() -> Result<u32, Box<dyn std::error::Error>> {
    let output = std::process::Command::new("getprop")
        .args(&["ro.build.version.sdk"])
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
    let installed_file = format!("{}/installed.json", PIE_DATA);
    
    if !Path::new(&installed_file).exists() {
        return Ok(InstalledPackages::default());
    }
    
    let content = fs::read_to_string(&installed_file)?;
    let installed: InstalledPackages = serde_json::from_str(&content)?;
    Ok(installed)
}

fn save_installed_packages(installed: &InstalledPackages) -> Result<(), Box<dyn std::error::Error>> {
    fs::create_dir_all(PIE_DATA)?;
    let installed_file = format!("{}/installed.json", PIE_DATA);
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
                if content.ends_with(&format!("/{}", query)) || 
                   content.ends_with(&format!("bin/{}", query)) ||
                   content == query {
                    return Some(pkg_name.clone());
                }
            }
        }
    }
    None
}

fn check_api_compatibility(package: &Package) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(min_api_str) = &package.min_api {
        let device_api = get_api_level()?;
        let min_api: u32 = min_api_str.parse()
            .map_err(|_| format!("Invalid min_api format: {}", min_api_str))?;
        
        if device_api < min_api {
            return Err(format!(
                "Package requires API level {} but device is API level {}", 
                min_api, device_api
            ).into());
        }
    }
    Ok(())
}

fn resolve_dependencies(repo: &Repo, package_name: &str, installed: &InstalledPackages) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    let mut to_install = Vec::new();
    let mut visited = HashSet::new();
    
    fn resolve_recursive(
        repo: &Repo, 
        pkg_name: &str, 
        installed: &InstalledPackages,
        to_install: &mut Vec<String>,
        visited: &mut HashSet<String>
    ) -> Result<(), Box<dyn std::error::Error>> {
        if visited.contains(pkg_name) {
            return Ok(()); // Avoid circular dependencies
        }
        visited.insert(pkg_name.to_string());
        
        let package = repo.packages.get(pkg_name)
            .ok_or(format!("Dependency '{}' not found", pkg_name))?;
        
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

fn handle_conflicts(package: &Package, installed: &mut InstalledPackages, no_confirm: bool) -> Result<(), Box<dyn std::error::Error>> {
    let mut conflicts_to_remove = Vec::new();
    
    for conflict in &package.conflicts {
        if installed.packages.contains_key(conflict) {
            conflicts_to_remove.push(conflict.clone());
        }
    }
    
    if !conflicts_to_remove.is_empty() {
        println!("The following conflicting packages will be removed:");
        for conflict in &conflicts_to_remove {
            println!("  - {}", conflict);
        }
        
        if !no_confirm {
            print!("Continue? [Y/n]: ");
            io::stdout().flush()?;
            
            let mut input = String::new();
            io::stdin().read_line(&mut input)?;
            let input = input.trim().to_lowercase();
            
            if input == "n" || input == "no" {
                return Err("Installation cancelled due to conflicts".into());
            }
        }
        
        for conflict in conflicts_to_remove {
            println!("Removing conflicting package: {}", conflict);
            remove_package_files(&conflict, installed)?;
            installed.packages.remove(&conflict);
        }
    }
    
    Ok(())
}

fn remove_package_files(name: &str, installed: &InstalledPackages) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(package) = installed.packages.get(name) {
        for file_path in &package.contents {
            let full_path = format!("{}/{}", ANDSTORE_ROOT, file_path);
            if Path::new(&full_path).exists() {
                fs::remove_file(&full_path)?;
            }
        }
    }
    Ok(())
}

fn install_single_package(repo: &Repo, name: &str, installed: &mut InstalledPackages) -> Result<(), Box<dyn std::error::Error>> {
    let package = repo.packages.get(name)
        .ok_or(format!("Package '{}' not found", name))?;
    
    let arch = get_arch()?;
    let architecture = package.architectures.get(&arch)
        .ok_or(format!("Package '{}' not available for architecture '{}'", name, arch))?;
    
    // Download package
    println!("Downloading {}...", name);
    let response = reqwest::blocking::get(&architecture.url)?;
    let content = response.bytes()?;
    
    // Verify checksum
    let mut hasher = Sha256::new();
    hasher.update(&content);
    let hash = hex::encode(hasher.finalize());
    
    if hash != architecture.sha256 {
        return Err(format!("Checksum verification failed for package '{}'", name).into());
    }
    
    // Create temp file and extract
    let temp_file = tempfile::NamedTempFile::new()?;
    fs::write(temp_file.path(), &content)?;
    
    // Extract package
    println!("Installing {}...", name);
    let file = fs::File::open(temp_file.path())?;
    let decoder = Decoder::new(file)?;
    let mut archive = Archive::new(decoder);
    
    fs::create_dir_all(ANDSTORE_ROOT)?;
    archive.unpack(ANDSTORE_ROOT)?;
    
    // Update installed packages
    let installed_package = InstalledPackage {
        name: name.to_string(),
        version: package.version.clone(),
        contents: architecture.contents.clone(),
    };
    
    installed.packages.insert(name.to_string(), installed_package);
    println!("Successfully installed {} v{}", name, package.version);
    
    Ok(())
}

fn install_package(name: &str, no_confirm: bool) -> Result<(), Box<dyn std::error::Error>> {
    let repo = fetch_repo()?;
    let mut installed = get_installed_packages()?;
    
    // Check if it's a direct package or content search
    let target_package = if repo.packages.contains_key(name) {
        name.to_string()
    } else {
        // Search for package containing this content
        if let Some(pkg_name) = find_package_by_content(&repo, name) {
            if !no_confirm {
                println!("'{}' is part of package '{}'", name, pkg_name);
                print!("Do you want to install '{}'? [Y/n]: ", pkg_name);
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
            return Err(format!("Package or content '{}' not found", name).into());
        }
    };
    
    let package = repo.packages.get(&target_package).unwrap();
    
    // Check if already installed
    if installed.packages.contains_key(&target_package) {
        println!("Package '{}' is already installed", target_package);
        return Ok(());
    }
    
    // Check API compatibility
    check_api_compatibility(package)?;
    
    // Handle conflicts
    handle_conflicts(package, &mut installed, no_confirm)?;
    
    // Resolve dependencies
    let dependencies = resolve_dependencies(&repo, &target_package, &installed)?;
    
    let deps_empty = dependencies.is_empty();
    
    if !deps_empty {
        println!("The following dependencies will be installed:");
        for dep in &dependencies {
            let dep_pkg = repo.packages.get(dep).unwrap();
            println!("  - {} v{}", dep, dep_pkg.version);
        }
        
        if !no_confirm {
            print!("Continue? [Y/n]: ");
            io::stdout().flush()?;
            
            let mut input = String::new();
            io::stdin().read_line(&mut input)?;
            let input = input.trim().to_lowercase();
            
            if input == "n" || input == "no" {
                println!("Installation cancelled");
                return Ok(());
            }
        }
        
        // Install dependencies first
        for dep in dependencies {
            install_single_package(&repo, &dep, &mut installed)?;
        }
    }
    
    // Show final confirmation for main package
    if !no_confirm && deps_empty {
        println!("Installing {} v{}", target_package, package.version);
        println!("Description: {}", package.description);
        print!("Continue? [Y/n]: ");
        io::stdout().flush()?;
        
        let mut input = String::new();
        io::stdin().read_line(&mut input)?;
        let input = input.trim().to_lowercase();
        
        if input == "n" || input == "no" {
            println!("Installation cancelled");
            return Ok(());
        }
    }
    
    // Install main package
    install_single_package(&repo, &target_package, &mut installed)?;
    
    // Save updated installed packages
    save_installed_packages(&installed)?;
    
    Ok(())
}

fn uninstall_package(name: &str) -> Result<(), Box<dyn std::error::Error>> {
    let repo = fetch_repo()?;
    let mut installed = get_installed_packages()?;
    
    // Check if it's a direct package or content search
    let target_package = if installed.packages.contains_key(name) {
        name.to_string()
    } else {
        // Search for package containing this content
        if let Some(pkg_name) = find_package_by_content(&repo, name) {
            if installed.packages.contains_key(&pkg_name) {
                println!("'{}' is part of package '{}'", name, pkg_name);
                print!("Do you want to uninstall '{}'? [Y/n]: ", pkg_name);
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
                return Err(format!("Package containing '{}' is not installed", name).into());
            }
        } else {
            return Err(format!("Package or content '{}' not found or not installed", name).into());
        }
    };
    
    let package = installed.packages.get(&target_package)
        .ok_or(format!("Package '{}' is not installed", target_package))?;
    
    println!("Removing {} v{}", target_package, package.version);
    
    // Remove files
    remove_package_files(&target_package, &installed)?;
    
    // Remove from installed packages
    installed.packages.remove(&target_package);
    save_installed_packages(&installed)?;
    
    println!("Successfully removed {}", target_package);
    Ok(())
}

fn update_repo() -> Result<(), Box<dyn std::error::Error>> {
    println!("Updating package repository...");
    let _repo = fetch_repo()?; // Just fetch to validate
    println!("Repository updated successfully");
    Ok(())
}

fn search_packages(query: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
    let repo = fetch_repo()?;
    
    match query {
        Some(q) => {
            let mut found_packages = false;
            
            // First, search for direct package matches
            for (name, package) in &repo.packages {
                if name.contains(q) || package.description.to_lowercase().contains(&q.to_lowercase()) {
                    println!("{} v{} - {}", name, package.version, package.description);
                    found_packages = true;
                }
            }
            
            // Then search for content matches
            if let Some(pkg_name) = find_package_by_content(&repo, q) {
                if let Some(package) = repo.packages.get(&pkg_name) {
                    if !found_packages {
                        println!("No direct package matches found.");
                        println!();
                    }
                    println!("'{}' is provided by:", q);
                    println!("  {} v{} - {}", pkg_name, package.version, package.description);
                    found_packages = true;
                }
            }
            
            if !found_packages {
                println!("No packages or content found matching '{}'", q);
            }
        }
        None => {
            for (name, package) in &repo.packages {
                println!("{} v{} - {}", name, package.version, package.description);
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
    
    println!("Installed packages:");
    for (name, package) in &installed.packages {
        println!("{} v{}", name, package.version);
    }
    
    Ok(())
}
