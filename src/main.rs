use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
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
    min_api: String,
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

fn install_package(name: &str, no_confirm: bool) -> Result<(), Box<dyn std::error::Error>> {
    let repo = fetch_repo()?;
    let package = repo.packages.get(name)
        .ok_or(format!("Package '{}' not found", name))?;
    
    let arch = get_arch()?;
    let architecture = package.architectures.get(&arch)
        .ok_or(format!("Package '{}' not available for architecture '{}'", name, arch))?;
    
    let mut installed = get_installed_packages()?;
    if installed.packages.contains_key(name) {
        println!("Package '{}' is already installed", name);
        return Ok(());
    }
    
    if !no_confirm {
        println!("Installing {} v{}", name, package.version);
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
    save_installed_packages(&installed)?;
    
    println!("Successfully installed {} v{}", name, package.version);
    Ok(())
}

fn uninstall_package(name: &str) -> Result<(), Box<dyn std::error::Error>> {
    let mut installed = get_installed_packages()?;
    
    let package = installed.packages.get(name)
        .ok_or(format!("Package '{}' is not installed", name))?;
    
    println!("Removing {} v{}", name, package.version);
    
    // Remove files
    for file_path in &package.contents {
        let full_path = format!("{}/{}", ANDSTORE_ROOT, file_path);
        if Path::new(&full_path).exists() {
            fs::remove_file(&full_path)?;
        }
    }
    
    // Remove from installed packages
    installed.packages.remove(name);
    save_installed_packages(&installed)?;
    
    println!("Successfully removed {}", name);
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
    
    for (name, package) in &repo.packages {
        let matches = match query {
            Some(q) => name.contains(q) || package.description.to_lowercase().contains(&q.to_lowercase()),
            None => true,
        };
        
        if matches {
            println!("{} v{} - {}", name, package.version, package.description);
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
