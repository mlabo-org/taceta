//! Installation boundary for the Taceta Link extension and Native Messaging host.
//!
//! This module deliberately does not launch a browser.  The browser's one-time
//! "Load unpacked"/"Add" action remains a human step and is represented in the
//! returned status.

use serde::Serialize;
use std::{
    fs, io,
    path::{Component, Path, PathBuf},
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
    pub fn user_data_dir(&self, home: &Path) -> PathBuf {
        match self {
            Self::Brave => home.join("Library/Application Support/BraveSoftware/Brave-Browser"),
            Self::Chrome => home.join("Library/Application Support/Google/Chrome"),
        }
    }
    pub fn native_host_dirs(&self, home: &Path) -> Result<Vec<PathBuf>, InstallerError> {
        let user_data = self.user_data_dir(home);
        let mut directories = vec![user_data.join("NativeMessagingHosts")];
        let local_state = user_data.join("Local State");
        if local_state.is_file() {
            let state: serde_json::Value = serde_json::from_slice(&fs::read(local_state)?)
                .map_err(InstallerError::BrowserState)?;
            let profile = state.get("profile").unwrap_or(&serde_json::Value::Null);
            let mut names = Vec::new();
            if let Some(active) = profile
                .get("last_active_profiles")
                .and_then(|v| v.as_array())
            {
                names.extend(active.iter().filter_map(|v| v.as_str()));
            }
            if let Some(last_used) = profile.get("last_used").and_then(|v| v.as_str()) {
                names.push(last_used);
            }
            for name in names {
                if is_safe_profile_name(name) {
                    let directory = user_data.join(name).join("NativeMessagingHosts");
                    if !directories.contains(&directory) {
                        directories.push(directory);
                    }
                }
            }
        }
        // Brave's native-messaging lookup also consults the Chrome-compatible
        // user-level root on this macOS configuration. Keep this as an exact
        // additional candidate; never alter its shared directory permissions.
        if matches!(self, Self::Brave) {
            let chrome_root = SupportedBrowser::Chrome
                .user_data_dir(home)
                .join("NativeMessagingHosts");
            if !directories.contains(&chrome_root) {
                directories.push(chrome_root);
            }
        }
        Ok(directories)
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
    #[error("invalid browser profile state: {0}")]
    BrowserState(serde_json::Error),
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
        for host_dir in browser.native_host_dirs(&self.home)? {
            fs::create_dir_all(&host_dir)?;
            let manifest_path = host_dir.join(format!("{HOST_NAME}.json"));
            fs::write(&manifest_path, &bytes)?;
            manifest_permissions(&manifest_path)?;
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
        for host_dir in browser.native_host_dirs(&self.home)? {
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
                    .native_host_dirs(&self.home)?
                    .iter()
                    .any(|directory| directory.join(format!("{HOST_NAME}.json")) == path),
        )
    }
}

fn is_safe_profile_name(name: &str) -> bool {
    let mut components = Path::new(name).components();
    matches!(
        (components.next(), components.next()),
        (Some(Component::Normal(_)), None)
    )
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

fn manifest_permissions(path: &Path) -> Result<(), InstallerError> {
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
    fn registers_standard_and_active_profile_manifests_without_chmodding_shared_dirs() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let root = std::env::temp_dir().join(format!(
            "taceta-profile-registration-{}",
            std::process::id()
        ));
        let app = root.join("Taceta.app");
        let resources = app.join("Contents/Resources/TacetaLink");
        fs::create_dir_all(&resources).unwrap();
        fs::write(resources.join("VERSION"), format!("{EXTENSION_VERSION}\n")).unwrap();
        fs::write(
            resources.join("manifest.json"),
            format!(r#"{{"version":"{EXTENSION_VERSION}"}}"#),
        )
        .unwrap();
        let user_data = SupportedBrowser::Brave.user_data_dir(&root);
        fs::create_dir_all(user_data.join("Default/NativeMessagingHosts")).unwrap();
        fs::set_permissions(
            user_data.join("Default/NativeMessagingHosts"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        fs::write(
            user_data.join("Local State"),
            r#"{"profile":{"last_used":"Default","last_active_profiles":["Default"]}}"#,
        )
        .unwrap();
        let unrelated = user_data.join("Default/NativeMessagingHosts/example.other.json");
        fs::write(&unrelated, "{}").unwrap();

        let installer = Installer::new(&root, &app);
        let status = installer
            .setup(BrowserDetection::Supported(SupportedBrowser::Brave))
            .unwrap();
        assert_eq!(status.host_manifest_paths.len(), 3);
        assert!(
            status
                .host_manifest_paths
                .iter()
                .any(|path| path.starts_with(SupportedBrowser::Chrome.user_data_dir(&root)))
        );
        assert!(status.host_manifest_paths.iter().all(|path| path.is_file()));
        assert!(
            status
                .host_manifest_paths
                .iter()
                .all(|path| { fs::metadata(path).unwrap().mode() & 0o777 == 0o644 })
        );
        assert_eq!(
            fs::metadata(installer.materialized_dir().join("manifest.json"))
                .unwrap()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(user_data.join("Default/NativeMessagingHosts"))
                .unwrap()
                .mode()
                & 0o777,
            0o755
        );
        assert!(unrelated.is_file());

        installer.uninstall(SupportedBrowser::Brave).unwrap();
        assert!(status.host_manifest_paths.iter().all(|path| !path.exists()));
        assert!(unrelated.is_file());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rejects_profile_path_traversal() {
        assert!(is_safe_profile_name("Default"));
        assert!(is_safe_profile_name("Profile 1"));
        assert!(!is_safe_profile_name("../Default"));
        assert!(!is_safe_profile_name("nested/Profile"));
        assert!(!is_safe_profile_name(""));
    }

    #[test]
    fn brave_adds_unique_chrome_compatibility_root_and_chrome_does_not() {
        use std::collections::HashSet;
        let root = Path::new("/tmp/taceta-home");
        let brave = SupportedBrowser::Brave.native_host_dirs(root).unwrap();
        let chrome = SupportedBrowser::Chrome.native_host_dirs(root).unwrap();
        assert!(
            brave.contains(
                &SupportedBrowser::Chrome
                    .user_data_dir(root)
                    .join("NativeMessagingHosts")
            )
        );
        assert_eq!(brave.len(), brave.iter().collect::<HashSet<_>>().len());
        assert_eq!(chrome.len(), 1);
        assert!(
            !chrome
                .iter()
                .any(|path| path.starts_with(SupportedBrowser::Brave.user_data_dir(root)))
        );
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
