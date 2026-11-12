//! How a verified, downloaded full package reaches the user. Nothing here escalates privileges
//! on its own: package-manager installs go through the system's own prompt (`pkexec`/`doas`),
//! platform installers show their own confirmation, and notify-only formats are never touched.

use crate::{Error, Result};
use std::fs;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use weirdnet_update::manifest::{FullPackage, InstallMode, PackageFormat, Platform};
use weirdnet_update::{UpdateKind, UpdateManifest};

/// Suffix of minisign signature files shipped next to a package.
pub const MINISIG: &str = ".minisig";

/// What the app does with a downloaded package.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Plan {
    /// Already unpacked into a versioned directory; run it on the next start.
    Swap {
        /// The new build's directory.
        dir: PathBuf,
    },
    /// Hand the file to the platform installer (it shows its own confirmation).
    PlatformInstaller(Command),
    /// Run the system package manager behind the system's privilege prompt.
    PrivilegedPackageManager(Command),
}

/// A command to run after the user confirmed it in the app's UI.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Command {
    /// Program.
    pub program: String,
    /// Arguments.
    pub args: Vec<String>,
}

impl Command {
    fn new(program: &str, args: &[&str]) -> Self {
        Self { program: program.into(), args: args.iter().map(|a| (*a).to_owned()).collect() }
    }

    /// Runs it and waits. Call only after the user confirmed.
    pub fn run(&self) -> Result<()> {
        let st = std::process::Command::new(&self.program).args(&self.args).status()?;
        if st.success() { Ok(()) } else { Err(Error::Invalid(format!("{} exited with {st}", self.program))) }
    }

    /// Starts it without waiting, detached from this process (Windows: no console), so the app
    /// can exit and let an installer replace its files. Call only after the user confirmed.
    pub fn spawn(&self) -> Result<()> {
        let mut c = std::process::Command::new(&self.program);
        c.args(&self.args).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const DETACHED_PROCESS: u32 = 0x0000_0008;
            const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
            c.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
        }
        c.spawn()?;
        Ok(())
    }
}

/// The full-package description, if the update is one.
pub fn full(m: &UpdateManifest) -> Option<&FullPackage> {
    match &m.kind {
        UpdateKind::Full(p) => Some(p),
        _ => None,
    }
}

/// The text shown for notify-only formats (the package manager owns these files).
pub fn notice(m: &UpdateManifest, p: &FullPackage) -> Option<String> {
    let app = &m.app;
    let v = &m.version;
    let how = match p.format {
        PackageFormat::PkgTarZst => format!("update {app} with your AUR helper (for example `yay -Syu {app}`)"),
        PackageFormat::Nix => format!("run `nix profile upgrade {app}` (or update your Nix configuration)"),
        PackageFormat::Homebrew => format!("run `brew upgrade {app}`"),
        PackageFormat::Winget => format!("run `winget upgrade {app}`"),
        _ => return None,
    };
    Some(format!("{app} {v} is available: {how}."))
}

/// The one package file a full update ships (besides its `.minisig`).
pub fn package_file(m: &UpdateManifest) -> Result<&str> {
    let mut it = m.files.iter().map(|f| f.path.as_str()).filter(|p| !p.ends_with(MINISIG));
    match (it.next(), it.next()) {
        (Some(p), None) => Ok(p),
        _ => Err(Error::Invalid("a full update must carry exactly one package file (plus its .minisig)".into())),
    }
}

/// Checks the package's minisign signature with the publisher key the app was built with.
pub fn verify_minisign(staged: &Path, package: &str, public_key: &str) -> Result<()> {
    let pk = minisign_verify::PublicKey::from_base64(public_key.trim()).map_err(|e| Error::Invalid(format!("bad minisign public key: {e}")))?;
    let sig_text = fs::read_to_string(staged.join(format!("{package}{MINISIG}")))
        .map_err(|_| Error::Invalid(format!("{package}: no {MINISIG} signature shipped")))?;
    let sig = minisign_verify::Signature::decode(&sig_text).map_err(|e| Error::Invalid(format!("{package}: bad minisign signature: {e}")))?;
    let data = fs::read(staged.join(package))?;
    pk.verify(&data, &sig, false).map_err(|_| Error::Invalid(format!("{package}: minisign signature does not verify")))
}

/// Builds the plan for a staged package. Swap formats are unpacked into `versions/<id>`.
pub fn plan(m: &UpdateManifest, p: &FullPackage, staged: &Path, versions: &Path) -> Result<Plan> {
    let package = staged.join(package_file(m)?);
    let file = native(&package);
    match p.format.install_mode() {
        InstallMode::NotifyOnly => Err(Error::Invalid("notify-only formats are never installed by the app".into())),
        InstallMode::Swap => {
            let dir = versions.join(&m.update_id);
            let part = versions.join(format!("{}.part", m.update_id));
            if part.exists() {
                fs::remove_dir_all(&part)?;
            }
            fs::create_dir_all(&part)?;
            match p.format {
                PackageFormat::TarGz => untar(&package, &part)?,
                PackageFormat::Zip | PackageFormat::App => unzip(&package, &part)?,
                _ => {
                    // A standalone executable.
                    let name = package.file_name().ok_or_else(|| Error::Invalid("package has no name".into()))?;
                    fs::copy(&package, part.join(name))?;
                    make_executable(&part.join(name))?;
                }
            }
            if dir.exists() {
                fs::remove_dir_all(&dir)?;
            }
            fs::rename(&part, &dir)?;
            Ok(Plan::Swap { dir })
        }
        InstallMode::PlatformInstaller => Ok(Plan::PlatformInstaller(match p.format {
            // Per-user MSI: no elevation; msiexec shows its own UI.
            PackageFormat::Msi => Command::new("msiexec", &["/i", &file, "MSIINSTALLPERUSER=1", "ALLUSERS=2", "/norestart"]),
            PackageFormat::Dmg => Command::new("open", &[&file]),
            _ => return Err(Error::Invalid(format!("{:?} installs through the mobile platform, not salty-client", p.format))),
        })),
        InstallMode::PrivilegedPackageManager => Ok(Plan::PrivilegedPackageManager(match (p.format, p.platform) {
            (PackageFormat::Deb, Platform::Linux) => Command::new("pkexec", &["apt-get", "install", "--yes", &file]),
            (PackageFormat::Rpm, Platform::Linux) => Command::new("pkexec", &["dnf", "install", "--assumeyes", &file]),
            (PackageFormat::BsdPkg, Platform::Freebsd | Platform::Ghostbsd | Platform::Dragonfly) => Command::new("doas", &["pkg", "add", &file]),
            // OpenBSD wants signify-signed packages from its own mirrors; this one is already
            // verified (signed manifest, BLAKE3 and the publisher's minisign key).
            (PackageFormat::BsdPkg, Platform::Openbsd) => Command::new("doas", &["pkg_add", "-D", "unsigned", &file]),
            (f, os) => return Err(Error::Invalid(format!("{f:?} packages are not installed on {}", os.as_str()))),
        })),
    }
}

/// Reinstalls a kept MSI after a failed update (the package allows downgrades).
pub(crate) fn reinstall(package: &Path) -> Command {
    Command::new("msiexec", &["/i", &native(package), "MSIINSTALLPERUSER=1", "ALLUSERS=2", "/norestart"])
}

/// A path for another program's command line. Windows Installer rejects `/` separators
/// (error 2203), which a state directory from a config file may contain.
fn native(path: &Path) -> String {
    let s = path.to_string_lossy();
    if cfg!(windows) { s.replace('/', "\\") } else { s.into_owned() }
}

fn make_executable(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perm = fs::metadata(path)?.permissions();
        perm.set_mode(perm.mode() | 0o755);
        fs::set_permissions(path, perm)?;
    }
    let _ = path;
    Ok(())
}

fn safe_rel(p: &Path) -> Result<()> {
    if p.as_os_str().is_empty() || p.components().any(|c| !matches!(c, Component::Normal(_) | Component::CurDir)) {
        return Err(Error::Invalid(format!("unsafe path in archive: {}", p.display())));
    }
    Ok(())
}

/// Unpacks a tar.gz: regular files and directories only (no links or devices), no path escapes.
fn untar(archive: &Path, dest: &Path) -> Result<()> {
    let mut a = tar::Archive::new(flate2::read::GzDecoder::new(fs::File::open(archive)?));
    for entry in a.entries()? {
        let mut e = entry?;
        let path = e.path()?.into_owned();
        safe_rel(&path)?;
        let out = dest.join(&path);
        match e.header().entry_type() {
            tar::EntryType::Directory => fs::create_dir_all(&out)?,
            tar::EntryType::Regular => {
                if let Some(parent) = out.parent() {
                    fs::create_dir_all(parent)?;
                }
                let mut data = Vec::new();
                e.read_to_end(&mut data)?;
                fs::write(&out, data)?;
                if e.header().mode().unwrap_or(0) & 0o111 != 0 {
                    make_executable(&out)?;
                }
            }
            t => return Err(Error::Invalid(format!("{}: {t:?} entries are not allowed", path.display()))),
        }
    }
    Ok(())
}

/// Unpacks a zip: files and directories only, no path escapes.
fn unzip(archive: &Path, dest: &Path) -> Result<()> {
    let mut z = zip::ZipArchive::new(fs::File::open(archive)?).map_err(|e| Error::Invalid(format!("bad zip: {e}")))?;
    for i in 0..z.len() {
        let mut f = z.by_index(i).map_err(|e| Error::Invalid(format!("bad zip: {e}")))?;
        let rel = f.enclosed_name().ok_or_else(|| Error::Invalid(format!("unsafe path in archive entry {i}")))?;
        safe_rel(&rel)?;
        if f.is_symlink() {
            return Err(Error::Invalid(format!("{}: links are not allowed", rel.display())));
        }
        let out = dest.join(&rel);
        if f.is_dir() {
            fs::create_dir_all(&out)?;
            continue;
        }
        if let Some(parent) = out.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut data = Vec::new();
        f.read_to_end(&mut data)?;
        fs::write(&out, data)?;
        if f.unix_mode().unwrap_or(0) & 0o111 != 0 {
            make_executable(&out)?;
        }
    }
    Ok(())
}
