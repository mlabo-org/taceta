//! Installation boundary for the Taceta Link extension and Native Messaging host.
//!
//! This module deliberately does not launch a browser.  The browser's one-time
//! "Load unpacked"/"Add" action remains a human step and is represented in the
//! returned status.

use serde::Serialize;
use std::{
    fs, io,
    path::{Path, PathBuf},
};
use thiserror::Error;

pub const EXTENSION_ID: &str = "hefhkgbiiajifedgjlbiklclooifkidg";
pub const HOST_NAME: &str = "org.mlabo.taceta.link";
pub const EXTENSION_VERSION: &str = "0.1.0";
pub const RESOURCE_DIR_NAME: &str = "TacetaLink";

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub enum SupportedBrowser {
    Brave,
    Chrome,
}

impl SupportedBrowser {
    pub fn bundle_id(&self) -> &'static str {
        match self {
            Self::Brave => "com.brave.Browser",
            Self::Chrome => "com.google.Chrome",
        }
    }
    pub fn display_name(&self) -> &'static str {
        match self {
            Self::Brave => "Brave",
            Self::Chrome => "Chrome",
        }
    }
    pub fn native_host_dirs(&self, home: &Path) -> Vec<PathBuf> {
        // macOS Brave explicitly overrides DIR_USER_NATIVE_MESSAGING to
        // Chrome's location (brave-core/app/brave_main_delegate.cc). Neither
        // browser's native-host lookup needs profile metadata or profile dirs.
        match self {
            Self::Brave | Self::Chrome => vec![home.join(
                "Library/Application Support/Google/Chrome/NativeMessagingHosts",
            )],
        }
    }
    pub fn management_url(&self) -> &'static str {
        match self {
            Self::Brave => "brave://extensions",
            Self::Chrome => "chrome://extensions",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub enum BrowserDetection {
    Supported(SupportedBrowser),
    Unsupported { bundle_id: Option<String> },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct InstallStatus {
    pub browser: Option<SupportedBrowser>,
    pub detected: BrowserDetection,
    pub materialized_version: Option<String>,
    pub registered: bool,
    pub extension_connection: bool,
    pub version_match: bool,
    pub needs_load_unpacked: bool,
    pub needs_reload: bool,
    pub materialized_path: PathBuf,
    pub host_manifest_paths: Vec<PathBuf>,
}

#[derive(Debug, Error)]
pub enum InstallerError {
    #[error("unsupported default browser: {0:?}")]
    Unsupported(BrowserDetection),
    #[error(
        "extension version mismatch: manifest={manifest}, VERSION={version}, package={package}"
    )]
    VersionMismatch {
        manifest: String,
        version: String,
        package: String,
    },
    #[error("missing bundled resource: {0}")]
    MissingResource(PathBuf),
    #[error("path is outside Taceta-owned directory: {0}")]
    Ownership(PathBuf),
    #[error("invalid native host manifest: {0}")]
    Manifest(#[from] serde_json::Error),
    #[error("Taceta Link registration failed at {path}: {source}")]
    HostRegistration {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
    #[error("LaunchServices failed: {0}")]
    LaunchServices(String),
}

pub fn browser_from_bundle_id(bundle_id: &str) -> BrowserDetection {
    match bundle_id {
        "com.brave.Browser" => BrowserDetection::Supported(SupportedBrowser::Brave),
        "com.google.Chrome" => BrowserDetection::Supported(SupportedBrowser::Chrome),
        other => BrowserDetection::Unsupported {
            bundle_id: Some(other.to_owned()),
        },
    }
}

/// Detects the https default application through LaunchServices on macOS.
#[cfg(target_os = "macos")]
pub fn detect_default_browser() -> Result<BrowserDetection, InstallerError> {
    launchservices_default_bundle_id().map(|id| browser_from_bundle_id(&id))
}

#[cfg(not(target_os = "macos"))]
pub fn detect_default_browser() -> Result<BrowserDetection, InstallerError> {
    Err(InstallerError::LaunchServices(
        "Taceta Link installation requires macOS LaunchServices".into(),
    ))
}

pub struct Installer {
    pub home: PathBuf,
    pub app_bundle: PathBuf,
}

impl Installer {
    pub fn new(home: impl Into<PathBuf>, app_bundle: impl Into<PathBuf>) -> Self {
        Self {
            home: home.into(),
            app_bundle: app_bundle.into(),
        }
    }
    pub fn materialized_dir(&self) -> PathBuf {
        self.home
            .join("Library/Application Support/Taceta/browser-extension")
    }
    pub fn bundled_extension_dir(&self) -> PathBuf {
        self.app_bundle
            .join("Contents/Resources")
            .join(RESOURCE_DIR_NAME)
    }

    pub fn setup(&self, detected: BrowserDetection) -> Result<InstallStatus, InstallerError> {
        let browser = match &detected {
            BrowserDetection::Supported(b) => b.clone(),
            _ => return Err(InstallerError::Unsupported(detected)),
        };
        let source = self.bundled_extension_dir();
        let versions = read_versions(&source)?;
        let target = self.materialized_dir();
        materialize_owned(&source, &target)?;
        let host_binary = self.app_bundle.join("Contents/MacOS/taceta-link-host");
        let bytes = host_manifest_bytes(&host_binary)?;
        let mut manifest_paths = Vec::new();
        for host_dir in browser.native_host_dirs(&self.home) {
            fs::create_dir_all(&host_dir).map_err(|source| InstallerError::HostRegistration {
                path: host_dir.clone(), source,
            })?;
            let manifest_path = host_dir.join(format!("{HOST_NAME}.json"));
            fs::write(&manifest_path, &bytes).map_err(|source| InstallerError::HostRegistration {
                path: manifest_path.clone(), source,
            })?;
            manifest_permissions(&manifest_path).map_err(|source| InstallerError::HostRegistration {
                path: manifest_path.clone(), source,
            })?;
            manifest_paths.push(manifest_path);
        }
        Ok(InstallStatus {
            browser: Some(browser),
            detected,
            materialized_version: Some(versions.0),
            registered: true,
            extension_connection: false,
            version_match: true,
            needs_load_unpacked: true,
            needs_reload: false,
            materialized_path: target,
            host_manifest_paths: manifest_paths,
        })
    }

    pub fn uninstall(&self, browser: SupportedBrowser) -> Result<InstallStatus, InstallerError> {
        let target = self.materialized_dir();
        if target.exists() {
            ensure_owned(&target, &self.home)?;
            fs::remove_dir_all(&target)?;
        }
        let mut manifest_paths = Vec::new();
        for host_dir in browser.native_host_dirs(&self.home) {
            let manifest = host_dir.join(format!("{HOST_NAME}.json"));
            if manifest.exists() && self.owns_host_manifest(&manifest, &browser)? {
                ensure_owned(&manifest, &host_dir)?;
                fs::remove_file(&manifest)?;
            }
            manifest_paths.push(manifest);
        }
        Ok(InstallStatus {
            browser: Some(browser.clone()),
            detected: BrowserDetection::Supported(browser),
            materialized_version: None,
            registered: false,
            extension_connection: false,
            version_match: false,
            needs_load_unpacked: false,
            needs_reload: true,
            materialized_path: target,
            host_manifest_paths: manifest_paths,
        })
    }

    pub fn open_extension_management_command(
        &self,
        browser: &SupportedBrowser,
    ) -> (&'static str, &'static str, &'static str) {
        ("open", browser.bundle_id(), browser.management_url())
    }
    pub fn reveal_materialized_command(&self) -> (&'static str, PathBuf) {
        ("open", self.materialized_dir())
    }

    fn owns_host_manifest(
        &self,
        path: &Path,
        browser: &SupportedBrowser,
    ) -> Result<bool, InstallerError> {
        let value: serde_json::Value = match serde_json::from_slice(&fs::read(path)?) {
            Ok(value) => value,
            Err(_) => return Ok(false),
        };
        Ok(
            value.get("name").and_then(|v| v.as_str()) == Some(HOST_NAME)
                && value
                    .get("allowed_origins")
                    .and_then(|v| v.as_array())
                    .and_then(|v| v.first())
                    .and_then(|v| v.as_str())
                    == Some(&format!("chrome-extension://{EXTENSION_ID}/"))
                && value.get("path").and_then(|v| v.as_str())
                    == Some(
                        self.app_bundle
                            .join("Contents/MacOS/taceta-link-host")
                            .to_string_lossy()
                            .as_ref(),
                    )
                && browser
                    .native_host_dirs(&self.home)
                    .iter()
                    .any(|directory| directory.join(format!("{HOST_NAME}.json")) == path),
        )
    }
}

fn read_versions(source: &Path) -> Result<(String, String, String), InstallerError> {
    let manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(source.join("manifest.json"))?)?;
    let manifest_v = manifest
        .get("version")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_owned();
    let version = fs::read_to_string(source.join("VERSION"))?
        .trim()
        .to_owned();
    let package = env!("CARGO_PKG_VERSION").to_owned();
    if manifest_v != version || version != EXTENSION_VERSION || version != package {
        return Err(InstallerError::VersionMismatch {
            manifest: manifest_v,
            version,
            package,
        });
    }
    Ok((version, EXTENSION_VERSION.to_owned(), package))
}

pub fn host_manifest_bytes(host_binary: &Path) -> Result<Vec<u8>, InstallerError> {
    if !host_binary.is_absolute() {
        return Err(InstallerError::Ownership(host_binary.to_owned()));
    }
    Ok(serde_json::to_vec_pretty(
        &serde_json::json!({"name": HOST_NAME, "description":"Taceta Link Native Messaging host", "path":host_binary, "type":"stdio", "allowed_origins":[format!("chrome-extension://{EXTENSION_ID}/")]}),
    )?)
}

fn materialize_owned(source: &Path, target: &Path) -> Result<(), InstallerError> {
    if !source.is_dir() {
        return Err(InstallerError::MissingResource(source.to_owned()));
    }
    fs::create_dir_all(target)?;
    user_only(target)?;
    for entry in fs::read_dir(source)? {
        let e = entry?;
        let dst = target.join(e.file_name());
        if e.file_type()?.is_dir() {
            materialize_owned(&e.path(), &dst)?;
        } else {
            fs::copy(e.path(), &dst)?;
            user_only(&dst)?;
        }
    }
    // The target is Taceta-owned; remove only entries no longer present in the
    // bundled source, so updates cannot leave stale extension code behind.
    for entry in fs::read_dir(target)? {
        let entry = entry?;
        if !source.join(entry.file_name()).exists() {
            let path = entry.path();
            if entry.file_type()?.is_dir() {
                fs::remove_dir_all(path)?;
            } else {
                fs::remove_file(path)?;
            }
        }
    }
    Ok(())
}
fn ensure_owned(path: &Path, root: &Path) -> Result<(), InstallerError> {
    if !path.starts_with(root) {
        return Err(InstallerError::Ownership(path.to_owned()));
    }
    Ok(())
}
fn user_only(path: &Path) -> Result<(), InstallerError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut p = fs::metadata(path)?.permissions();
        p.set_mode(if fs::metadata(path)?.is_dir() {
            0o700
        } else {
            0o600
        });
        fs::set_permissions(path, p)?;
    }
    Ok(())
}

fn manifest_permissions(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = fs::metadata(path)?.permissions();
        permissions.set_mode(0o644);
        fs::set_permissions(path, permissions)?;
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn launchservices_default_bundle_id() -> Result<String, InstallerError> {
    use std::ffi::{CStr, c_char, c_void};
    #[repr(C)]
    struct __CFURL(c_void);
    #[repr(C)]
    struct __CFBundle(c_void);
    type CFURLRef = *const __CFURL;
    type CFBundleRef = *const __CFBundle;
    type CFStringRef = *const c_void;
    #[link(name = "CoreServices", kind = "framework")]
    unsafe extern "C" {
        fn LSCopyDefaultApplicationURLForURL(
            url: *const c_void,
            role: u32,
            out: *mut *const c_void,
        ) -> *const c_void;
    }
    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFStringCreateWithCString(a: *const c_void, s: *const c_char, e: u32) -> CFStringRef;
        fn CFURLCreateWithString(a: *const c_void, s: CFStringRef, b: *const c_void) -> CFURLRef;
        fn CFBundleCreate(a: *const c_void, u: CFURLRef) -> CFBundleRef;
        fn CFBundleGetIdentifier(b: CFBundleRef) -> CFStringRef;
        fn CFStringGetCString(s: CFStringRef, b: *mut c_char, n: isize, e: u32) -> bool;
        fn CFRelease(x: *const c_void);
    }
    let path = std::ffi::CString::new("https://example.com").unwrap();
    let s = unsafe { CFStringCreateWithCString(std::ptr::null(), path.as_ptr(), 0x08000100) };
    let url = unsafe { CFURLCreateWithString(std::ptr::null(), s, std::ptr::null()) };
    let mut out = std::ptr::null();
    let result = unsafe { LSCopyDefaultApplicationURLForURL(url as _, 0x00000001, &mut out) };
    unsafe {
        CFRelease(s as _);
        CFRelease(url as _);
    }
    if result.is_null() {
        return Err(InstallerError::LaunchServices("no https handler".into()));
    }
    let bundle = unsafe { CFBundleCreate(std::ptr::null(), result as _) };
    let id = unsafe { CFBundleGetIdentifier(bundle) };
    let mut buf = [0i8; 256];
    let ok = unsafe { CFStringGetCString(id, buf.as_mut_ptr(), buf.len() as isize, 0x08000100) };
    unsafe {
        CFRelease(result);
        CFRelease(bundle as _);
    }
    if !ok {
        return Err(InstallerError::LaunchServices(
            "could not read bundle identifier".into(),
        ));
    }
    Ok(unsafe { CStr::from_ptr(buf.as_ptr()) }
        .to_string_lossy()
        .into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    #[test]
    fn maps_browsers() {
        assert_eq!(
            browser_from_bundle_id("com.brave.Browser"),
            BrowserDetection::Supported(SupportedBrowser::Brave)
        );
        assert!(matches!(
            browser_from_bundle_id("com.apple.Safari"),
            BrowserDetection::Unsupported { .. }
        ));
    }
    #[test]
    fn manifest_has_exact_origin_and_path() {
        let b = host_manifest_bytes(Path::new("/tmp/Taceta.app/Contents/MacOS/taceta-link-host"))
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&b).unwrap();
        assert_eq!(
            v["allowed_origins"][0],
            format!("chrome-extension://{EXTENSION_ID}/")
        );
        assert_eq!(v["path"], "/tmp/Taceta.app/Contents/MacOS/taceta-link-host");
    }
    #[test]
    fn materialize_and_uninstall_are_bounded() {
        let root = std::env::temp_dir().join(format!("taceta-installer-{}", std::process::id()));
        let src = root.join("app/Contents/Resources/TacetaLink");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("VERSION"), EXTENSION_VERSION).unwrap();
        fs::write(
            src.join("manifest.json"),
            format!(r#"{{"version":"{EXTENSION_VERSION}"}}"#),
        )
        .unwrap();
        fs::write(src.join("x"), "x").unwrap();
        let i = Installer::new(&root, &root.join("app"));
        let d = i.materialized_dir();
        materialize_owned(&src, &d).unwrap();
        assert_eq!(fs::read_to_string(d.join("x")).unwrap(), "x");
        fs::write(d.join("stale.js"), "stale").unwrap();
        fs::remove_file(src.join("x")).unwrap();
        materialize_owned(&src, &d).unwrap();
        assert!(!d.join("x").exists());
        assert!(!d.join("stale.js").exists());
        ensure_owned(&d, &root).unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn registers_canonical_host_without_reading_browser_profile_state() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let root = tempfile::tempdir().unwrap();
        let app = root.path().join("Taceta.app");
        let resources = app.join("Contents/Resources/TacetaLink");
        fs::create_dir_all(&resources).unwrap();
        fs::write(resources.join("VERSION"), EXTENSION_VERSION).unwrap();
        fs::write(resources.join("manifest.json"),
            format!(r#"{{"version":"{EXTENSION_VERSION}"}}"#)).unwrap();

        // Invalid private state makes any old profile-reading route fail. The
        // correct native-host registration does not need to open this file.
        let brave = root.path().join("Library/Application Support/BraveSoftware/Brave-Browser");
        fs::create_dir_all(&brave).unwrap();
        fs::write(brave.join("Local State"), "PRIVATE_STATE_NOT_JSON").unwrap();
        let shared = root.path().join("Library/Application Support/Google/Chrome/NativeMessagingHosts");
        fs::create_dir_all(&shared).unwrap();
        fs::set_permissions(&shared, fs::Permissions::from_mode(0o755)).unwrap();
        let unrelated = shared.join("example.other.json");
        fs::write(&unrelated, "{}").unwrap();

        let installer = Installer::new(root.path(), &app);
        let status = installer.setup(BrowserDetection::Supported(SupportedBrowser::Brave)).unwrap();
        assert_eq!(status.host_manifest_paths, vec![shared.join(format!("{HOST_NAME}.json"))]);
        assert!(status.registered);
        assert_eq!(fs::metadata(&status.host_manifest_paths[0]).unwrap().mode() & 0o777, 0o644);
        assert_eq!(fs::metadata(&shared).unwrap().mode() & 0o777, 0o755);
        assert_eq!(fs::metadata(installer.materialized_dir().join("manifest.json")).unwrap().mode() & 0o777, 0o600);
        assert_eq!(fs::read_to_string(brave.join("Local State")).unwrap(), "PRIVATE_STATE_NOT_JSON");
        assert!(!brave.join("NativeMessagingHosts").exists());
        assert!(!brave.join("Default").exists());
        installer.uninstall(SupportedBrowser::Brave).unwrap();
        assert!(!status.host_manifest_paths[0].exists());
        assert!(unrelated.is_file());
        assert!(brave.join("Local State").exists());
    }

    #[test]
    fn brave_and_chrome_use_the_same_user_registration_path() {
        let root = Path::new("/tmp/taceta-home");
        let expected = vec![root.join("Library/Application Support/Google/Chrome/NativeMessagingHosts")];
        assert_eq!(SupportedBrowser::Brave.native_host_dirs(root), expected);
        assert_eq!(SupportedBrowser::Chrome.native_host_dirs(root), expected);
    }

    #[test]
    fn registration_errors_identify_the_failing_host_path() {
        let root = tempfile::tempdir().unwrap();
        let app = root.path().join("Taceta.app");
        let resources = app.join("Contents/Resources/TacetaLink");
        fs::create_dir_all(&resources).unwrap();
        fs::write(resources.join("VERSION"), EXTENSION_VERSION).unwrap();
        fs::write(resources.join("manifest.json"),
            format!(r#"{{"version":"{EXTENSION_VERSION}"}}"#)).unwrap();
        let blocked = root.path().join("Library/Application Support/Google/Chrome/NativeMessagingHosts");
        fs::create_dir_all(blocked.parent().unwrap()).unwrap();
        fs::write(&blocked, "not a directory").unwrap();
        let error = Installer::new(root.path(), &app)
            .setup(BrowserDetection::Supported(SupportedBrowser::Brave)).unwrap_err();
        assert!(matches!(&error, InstallerError::HostRegistration { path, .. } if path == &blocked));
        assert!(error.to_string().contains("NativeMessagingHosts"));
        assert_eq!(fs::read_to_string(blocked).unwrap(), "not a directory");
    }

    #[test]
    fn version_mismatch_fails_closed() {
        let root = std::env::temp_dir().join(format!("taceta-version-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("VERSION"), "9.9.9\n").unwrap();
        fs::write(root.join("manifest.json"), r#"{"version":"9.9.9"}"#).unwrap();
        assert!(matches!(
            read_versions(&root),
            Err(InstallerError::VersionMismatch { .. })
        ));
        fs::remove_dir_all(root).unwrap();
    }
}
