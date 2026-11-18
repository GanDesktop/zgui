//! Gated exclusive Vulkan driver loading for Linux hybrid-GPU Wayland systems.
//!
//! When the Wayland compositor drives an integrated GPU while a discrete GPU is in kernel runtime
//! power suspension (`runtime_status` is `suspended` or `suspending`), standard `wgpu` instance
//! creation queries global Vulkan extensions through the loader and wakes every installed ICD,
//! stalling startup. When strict environment, sysfs, and ICD-allowlist preconditions are met, this
//! module constructs a `wgpu::Instance` exclusively for the compositor's GPU via
//! `VK_LUNARG_direct_driver_loading`, falling back cleanly to standard initialization otherwise.

#[cfg(all(target_os = "linux", vulkan_hal))]
use std::collections::BTreeSet;
#[cfg(any(all(target_os = "linux", vulkan_hal), test))]
use std::ffi::OsString;
#[cfg(any(all(target_os = "linux", vulkan_hal), test))]
use std::path::{Path, PathBuf};

/// Compositor DRM render-device identity hint passed from platform backends.
///
/// Kept free of `zgui-platform` types so `zgui-render-wgpu` maintains its layer boundary; the
/// application shell converts `zgui_platform::CompositorGpuHint` into this struct.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct CompositorGpuHint {
    /// Kernel `dev_t` of the compositor's primary DRM device node, when known.
    pub drm_render_dev: Option<u64>,
    /// PCI vendor identifier of the compositor's GPU, when known.
    pub vendor_id: Option<u32>,
    /// PCI device identifier of the compositor's GPU, when known.
    pub device_id: Option<u32>,
}

/// Attempts to build a single-ICD exclusive Vulkan [`wgpu::Instance`] for `hint`.
///
/// Returns `None` whenever the target platform, environment variables, sysfs topology, ICD
/// allowlist, or direct driver initialization does not clearly support the fast path. Callers
/// must treat `None` as a request to run standard `wgpu` backend initialization untouched.
pub(crate) fn try_create_instance(
    hint: CompositorGpuHint,
    requested_backends: wgpu::Backends,
    flags: wgpu::InstanceFlags,
) -> Option<wgpu::Instance> {
    if !requested_backends.contains(wgpu::Backends::VULKAN) {
        return None;
    }

    #[cfg(all(target_os = "linux", vulkan_hal))]
    {
        linux::try_create_instance_linux(hint, flags)
    }

    #[cfg(not(all(target_os = "linux", vulkan_hal)))]
    {
        let _ = (hint, flags);
        None
    }
}

/// Environment overrides that disable the direct Vulkan fast path when non-empty.
#[cfg(any(all(target_os = "linux", vulkan_hal), test))]
const OVERRIDE_ENV_VARS: &[&str] = &[
    "ZGUI_DEVICE_ID",
    "ZED_DEVICE_ID",
    "ZGUI_BACKENDS",
    "WGPU_BACKEND",
    "WGPU_ADAPTER_NAME",
    "VK_DRIVER_FILES",
    "VK_ADD_DRIVER_FILES",
    "VK_ICD_FILENAMES",
    "VK_LOADER_DRIVERS_SELECT",
    "VK_LOADER_DRIVERS_DISABLE",
    "VK_INSTANCE_LAYERS",
];

/// PRIME offload overrides that bypass the fast path whenever set to any value other than `"0"`.
#[cfg(any(all(target_os = "linux", vulkan_hal), test))]
const PRIME_ENV_VARS: &[&str] = &["DRI_PRIME", "__NV_PRIME_RENDER_OFFLOAD"];

#[cfg(test)]
fn env_allows_fast_path_with(lookup: &dyn Fn(&str) -> Option<OsString>) -> bool {
    env_allows_fast_path_with_self_disable(lookup, false)
}

#[cfg(any(all(target_os = "linux", vulkan_hal), test))]
fn env_allows_fast_path_with_self_disable(
    lookup: &dyn Fn(&str) -> Option<OsString>,
    ignore_self_lvp_disable: bool,
) -> bool {
    for var in OVERRIDE_ENV_VARS {
        if let Some(value) = lookup(var) {
            let trimmed = value.to_string_lossy();
            let trimmed = trimmed.trim();
            if trimmed.is_empty() {
                continue;
            }
            if ignore_self_lvp_disable
                && *var == "VK_LOADER_DRIVERS_DISABLE"
                && trimmed == crate::gpu::loader::SOFTWARE_DRIVER
            {
                continue;
            }
            tracing::debug!(env = var, "bypassing direct Vulkan init due to override");
            return false;
        }
    }
    for var in PRIME_ENV_VARS {
        if let Some(value) = lookup(var) {
            let trimmed = value.to_string_lossy();
            let trimmed = trimmed.trim();
            if !trimmed.is_empty() && trimmed != "0" {
                tracing::debug!(
                    env = var,
                    "bypassing direct Vulkan init due to PRIME offload"
                );
                return false;
            }
        }
    }
    true
}

#[cfg(any(all(target_os = "linux", vulkan_hal), test))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct AllowlistedDriver {
    library_basename: &'static str,
    check_competing_amd: bool,
}

#[cfg(any(all(target_os = "linux", vulkan_hal), test))]
fn allowlisted_driver(vendor_id: u32, kernel_driver: &str) -> Option<AllowlistedDriver> {
    match (vendor_id, kernel_driver) {
        (0x1002, "amdgpu") => Some(AllowlistedDriver {
            library_basename: "libvulkan_radeon.so",
            check_competing_amd: true,
        }),
        (0x8086, "i915" | "xe") => Some(AllowlistedDriver {
            library_basename: "libvulkan_intel.so",
            check_competing_amd: false,
        }),
        _ => None,
    }
}

#[cfg(any(all(target_os = "linux", vulkan_hal), test))]
#[derive(Clone, Debug, PartialEq, Eq)]
struct SysfsGpuInfo {
    vendor_id: u32,
    device_id: u32,
    kernel_driver: String,
    canonical_device_dir: PathBuf,
}

#[cfg(any(all(target_os = "linux", vulkan_hal), test))]
fn decode_dev_t(dev: u64) -> (u32, u32) {
    let major =
        u32::try_from(((dev & 0x0000_0000_000f_ff00) >> 8) | ((dev & 0xffff_f000_0000_0000) >> 32))
            .unwrap_or(u32::MAX);
    let minor =
        u32::try_from((dev & 0x0000_0000_0000_00ff) | ((dev & 0x0000_0fff_fff0_0000) >> 12))
            .unwrap_or(u32::MAX);
    (major, minor)
}

#[cfg(any(all(target_os = "linux", vulkan_hal), test))]
fn parse_pci_hex(raw: &str) -> Option<u32> {
    let trimmed = raw.trim();
    let hex = trimmed
        .strip_prefix("0x")
        .or_else(|| trimmed.strip_prefix("0X"))?;
    u32::from_str_radix(hex, 16).ok()
}

#[cfg(any(all(target_os = "linux", vulkan_hal), test))]
fn read_pci_hex(path: &Path) -> Option<u32> {
    let contents = std::fs::read_to_string(path).ok()?;
    parse_pci_hex(&contents)
}

#[cfg(any(all(target_os = "linux", vulkan_hal), test))]
fn resolve_sysfs_gpu_with_roots(
    hint: CompositorGpuHint,
    dev_char_root: &Path,
    drm_root: &Path,
) -> Option<SysfsGpuInfo> {
    let dev = hint.drm_render_dev?;
    let (major, minor) = decode_dev_t(dev);
    let device_link = dev_char_root
        .join(format!("{major}:{minor}"))
        .join("device");
    let canonical_device_dir = std::fs::canonicalize(&device_link).ok()?;

    let vendor_id = read_pci_hex(&canonical_device_dir.join("vendor"))?;
    let device_id = read_pci_hex(&canonical_device_dir.join("device"))?;
    if hint.vendor_id.is_some_and(|expected| expected != vendor_id) {
        return None;
    }
    if hint.device_id.is_some_and(|expected| expected != device_id) {
        return None;
    }

    let driver_target = std::fs::read_link(canonical_device_dir.join("driver")).ok()?;
    let kernel_driver = driver_target.file_name()?.to_str()?.to_owned();

    // Require at least one distinct-vendor peer GPU whose PCI runtime PM state is suspended or
    // suspending; single-GPU machines and already-active dGPU topologies keep standard wgpu init.
    let mut has_suspended_peer = false;
    let entries = std::fs::read_dir(drm_root).ok()?;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let is_card = name
            .strip_prefix("card")
            .is_some_and(|rest| !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()));
        if !is_card {
            continue;
        }
        let Ok(peer_device_dir) = std::fs::canonicalize(entry.path().join("device")) else {
            continue;
        };
        if peer_device_dir == canonical_device_dir {
            continue;
        }
        let Some(peer_vendor) = read_pci_hex(&peer_device_dir.join("vendor")) else {
            continue;
        };
        if peer_vendor == vendor_id {
            continue;
        }
        let Ok(status) = std::fs::read_to_string(peer_device_dir.join("power/runtime_status"))
        else {
            continue;
        };
        if matches!(status.trim(), "suspended" | "suspending") {
            has_suspended_peer = true;
            break;
        }
    }

    if !has_suspended_peer {
        return None;
    }

    Some(SysfsGpuInfo {
        vendor_id,
        device_id,
        kernel_driver,
        canonical_device_dir,
    })
}

#[cfg(any(all(target_os = "linux", vulkan_hal), test))]
fn icd_search_dirs_with(lookup: &dyn Fn(&str) -> Option<OsString>) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    let mut push_unique = |dir: PathBuf| {
        if !dir.as_os_str().is_empty() && !dirs.contains(&dir) {
            dirs.push(dir);
        }
    };

    if let Some(xdg_config) = lookup("XDG_CONFIG_HOME").filter(|s| !s.is_empty()) {
        push_unique(PathBuf::from(xdg_config).join("vulkan/icd.d"));
    } else if let Some(home) = lookup("HOME").filter(|s| !s.is_empty()) {
        push_unique(PathBuf::from(home).join(".config/vulkan/icd.d"));
    }

    push_unique(PathBuf::from("/etc/vulkan/icd.d"));
    push_unique(PathBuf::from("/run/opengl-driver/share/vulkan/icd.d"));

    if let Some(xdg_data_home) = lookup("XDG_DATA_HOME").filter(|s| !s.is_empty()) {
        push_unique(PathBuf::from(xdg_data_home).join("vulkan/icd.d"));
    } else if let Some(home) = lookup("HOME").filter(|s| !s.is_empty()) {
        push_unique(PathBuf::from(home).join(".local/share/vulkan/icd.d"));
    }

    let data_dirs = lookup("XDG_DATA_DIRS")
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| OsString::from("/usr/local/share:/usr/share"));
    for entry in std::env::split_paths(&data_dirs) {
        if !entry.as_os_str().is_empty() {
            push_unique(entry.join("vulkan/icd.d"));
        }
    }

    push_unique(PathBuf::from("/usr/local/share/vulkan/icd.d"));
    push_unique(PathBuf::from("/usr/share/vulkan/icd.d"));

    dirs
}

#[cfg(any(all(target_os = "linux", vulkan_hal), test))]
#[derive(Clone, Debug, PartialEq, Eq)]
struct IcdManifest {
    file_format_version: String,
    library_path: String,
    library_arch: Option<String>,
}

#[cfg(any(all(target_os = "linux", vulkan_hal), test))]
fn parse_icd_manifest(text: &str) -> Option<IcdManifest> {
    let mut parser = JsonScanner::new(text);
    parser.skip_ws();
    if !parser.consume_char('{') {
        return None;
    }

    let mut file_format_version: Option<String> = None;
    let mut library_path: Option<String> = None;
    let mut library_arch: Option<String> = None;

    parser.parse_object_fields(&mut |scanner, key| {
        if key == "file_format_version" {
            file_format_version = scanner.parse_string();
            file_format_version.is_some()
        } else if key == "ICD" {
            scanner.skip_ws();
            if !scanner.consume_char('{') {
                return false;
            }
            scanner.parse_object_fields(&mut |inner, subkey| {
                if subkey == "library_path" {
                    library_path = inner.parse_string();
                    library_path.is_some()
                } else if subkey == "library_arch" {
                    library_arch = inner.parse_string();
                    library_arch.is_some()
                } else {
                    inner.skip_value()
                }
            })
        } else {
            scanner.skip_value()
        }
    });

    Some(IcdManifest {
        file_format_version: file_format_version?,
        library_path: library_path?,
        library_arch,
    })
}

/// Minimal strict JSON scanner for Vulkan ICD manifests (`file_format_version` and `ICD` object).
#[cfg(any(all(target_os = "linux", vulkan_hal), test))]
struct JsonScanner<'a> {
    bytes: &'a [u8],
    pos: usize,
}

#[cfg(any(all(target_os = "linux", vulkan_hal), test))]
impl<'a> JsonScanner<'a> {
    fn new(input: &'a str) -> Self {
        let stripped = input.strip_prefix('\u{feff}').unwrap_or(input);
        Self {
            bytes: stripped.as_bytes(),
            pos: 0,
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn next(&mut self) -> Option<u8> {
        let b = self.peek()?;
        self.pos += 1;
        Some(b)
    }

    fn skip_ws(&mut self) {
        while let Some(b) = self.peek() {
            if matches!(b, b' ' | b'\t' | b'\r' | b'\n') {
                self.pos += 1;
            } else {
                break;
            }
        }
    }

    fn consume_char(&mut self, expected: char) -> bool {
        self.skip_ws();
        if self.peek() == Some(expected as u8) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn parse_object_fields(&mut self, on_field: &mut dyn FnMut(&mut Self, &str) -> bool) -> bool {
        self.skip_ws();
        if self.consume_char('}') {
            return true;
        }
        loop {
            let Some(key) = self.parse_string() else {
                return false;
            };
            if !self.consume_char(':') {
                return false;
            }
            if !on_field(self, &key) {
                return false;
            }
            self.skip_ws();
            if self.consume_char(',') {
                continue;
            }
            return self.consume_char('}');
        }
    }

    fn parse_string(&mut self) -> Option<String> {
        self.skip_ws();
        if self.next() != Some(b'"') {
            return None;
        }
        let mut out = String::new();
        while let Some(b) = self.next() {
            match b {
                b'"' => return Some(out),
                b'\\' => match self.next()? {
                    b'"' => out.push('"'),
                    b'\\' => out.push('\\'),
                    b'/' => out.push('/'),
                    b'b' => out.push('\u{0008}'),
                    b'f' => out.push('\u{000c}'),
                    b'n' => out.push('\n'),
                    b'r' => out.push('\r'),
                    b't' => out.push('\t'),
                    b'u' => {
                        let code = self.read_hex4()?;
                        if (0xD800..=0xDBFF).contains(&code) {
                            if self.next() != Some(b'\\') || self.next() != Some(b'u') {
                                return None;
                            }
                            let low = self.read_hex4()?;
                            if !(0xDC00..=0xDFFF).contains(&low) {
                                return None;
                            }
                            let cp = 0x10000
                                + (((u32::from(code) - 0xD800) << 10) | (u32::from(low) - 0xDC00));
                            out.push(char::from_u32(cp)?);
                        } else {
                            out.push(char::from_u32(u32::from(code))?);
                        }
                    }
                    _ => return None,
                },
                0x00..=0x1F => return None,
                _ if b < 0x80 => out.push(b as char),
                first => {
                    let start = self.pos - 1;
                    let width = if first & 0xE0 == 0xC0 {
                        2
                    } else if first & 0xF0 == 0xE0 {
                        3
                    } else if first & 0xF8 == 0xF0 {
                        4
                    } else {
                        return None;
                    };
                    let end = start + width;
                    if end > self.bytes.len() {
                        return None;
                    }
                    let slice = std::str::from_utf8(&self.bytes[start..end]).ok()?;
                    out.push_str(slice);
                    self.pos = end;
                }
            }
        }
        None
    }

    fn read_hex4(&mut self) -> Option<u16> {
        let mut value: u16 = 0;
        for _ in 0..4 {
            let b = self.next()?;
            let digit = match b {
                b'0'..=b'9' => u16::from(b - b'0'),
                b'a'..=b'f' => u16::from(b - b'a' + 10),
                b'A'..=b'F' => u16::from(b - b'A' + 10),
                _ => return None,
            };
            value = (value << 4) | digit;
        }
        Some(value)
    }

    fn skip_value(&mut self) -> bool {
        self.skip_ws();
        let Some(b) = self.peek() else {
            return false;
        };
        match b {
            b'"' => self.parse_string().is_some(),
            b'{' => {
                self.pos += 1;
                self.parse_object_fields(&mut |inner, _| inner.skip_value())
            }
            b'[' => {
                self.pos += 1;
                self.skip_ws();
                if self.consume_char(']') {
                    return true;
                }
                loop {
                    if !self.skip_value() {
                        return false;
                    }
                    self.skip_ws();
                    if self.consume_char(',') {
                        continue;
                    }
                    return self.consume_char(']');
                }
            }
            b't' => self.consume_literal(b"true"),
            b'f' => self.consume_literal(b"false"),
            b'n' => self.consume_literal(b"null"),
            b'-' | b'0'..=b'9' => {
                if self.peek() == Some(b'-') {
                    self.pos += 1;
                }
                let start = self.pos;
                while matches!(
                    self.peek(),
                    Some(b'0'..=b'9' | b'.' | b'e' | b'E' | b'+' | b'-')
                ) {
                    self.pos += 1;
                }
                self.pos > start
            }
            _ => false,
        }
    }

    fn consume_literal(&mut self, lit: &[u8]) -> bool {
        if self.bytes.get(self.pos..self.pos + lit.len()) == Some(lit) {
            self.pos += lit.len();
            true
        } else {
            false
        }
    }
}

#[cfg(any(all(target_os = "linux", vulkan_hal), test))]
fn is_valid_elf64(path: &Path) -> bool {
    use std::io::Read as _;
    let Ok(mut file) = std::fs::File::open(path) else {
        return false;
    };
    let mut header = [0u8; 6];
    if file.read_exact(&mut header).is_err() {
        return false;
    }
    // 0..4 = ELF magic, 4 = EI_CLASS (2 => ELFCLASS64)
    header[0..4] == *b"\x7fELF" && header[4] == 2
}

#[cfg(any(all(target_os = "linux", vulkan_hal), test))]
fn manifest_version_is_v1(version: &str) -> bool {
    let trimmed = version.trim();
    trimmed == "1" || trimmed.starts_with("1.")
}

#[cfg(any(all(target_os = "linux", vulkan_hal), test))]
fn resolve_icd_library_with_dirs(
    allowlist: AllowlistedDriver,
    dirs: &[PathBuf],
) -> Option<PathBuf> {
    let mut matched = BTreeSet::new();

    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        let mut files: Vec<PathBuf> = entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.extension().and_then(|e| e.to_str()) == Some("json"))
            .collect();
        files.sort();

        for manifest_path in files {
            let Ok(contents) = std::fs::read_to_string(&manifest_path) else {
                continue;
            };
            let Some(manifest) = parse_icd_manifest(&contents) else {
                continue;
            };
            if !manifest_version_is_v1(&manifest.file_format_version) {
                continue;
            }
            if manifest
                .library_arch
                .as_deref()
                .is_some_and(|arch| arch.trim() != "64")
            {
                continue;
            }
            let lib_rel_or_abs = Path::new(&manifest.library_path);
            let Some(basename) = lib_rel_or_abs.file_name().and_then(|n| n.to_str()) else {
                continue;
            };

            if allowlist.check_competing_amd {
                let lower = basename.to_ascii_lowercase();
                if lower.contains("amdvlk") || lower.contains("amd_icd") {
                    tracing::debug!(
                        manifest = %manifest_path.display(),
                        library = basename,
                        "aborting direct Vulkan init due to competing AMD ICD"
                    );
                    return None;
                }
            }

            if basename != allowlist.library_basename {
                continue;
            }

            let candidate_path = if lib_rel_or_abs.is_absolute() {
                lib_rel_or_abs.to_path_buf()
            } else if manifest.library_path.contains('/') {
                manifest_path.parent()?.join(lib_rel_or_abs)
            } else {
                let sibling = manifest_path.parent()?.join(lib_rel_or_abs);
                if sibling.is_file() {
                    sibling
                } else {
                    return None;
                }
            };

            let Ok(canonical_lib) = std::fs::canonicalize(&candidate_path) else {
                return None;
            };
            if !is_valid_elf64(&canonical_lib) {
                tracing::debug!(
                    library = %canonical_lib.display(),
                    "aborting direct Vulkan init because matched ICD is not a valid 64-bit ELF"
                );
                return None;
            }
            matched.insert(canonical_lib);
        }
    }

    if matched.len() == 1 {
        matched.into_iter().next()
    } else {
        None
    }
}

#[cfg(all(target_os = "linux", vulkan_hal))]
mod linux {
    use std::ffi::CStr;
    use std::path::{Path, PathBuf};

    use ash::vk;

    use super::{
        CompositorGpuHint, allowlisted_driver, env_allows_fast_path_with_self_disable,
        icd_search_dirs_with, resolve_icd_library_with_dirs, resolve_sysfs_gpu_with_roots,
    };

    const DIRECT_DRIVER_LOADING_EXT: &CStr = c"VK_LUNARG_direct_driver_loading";

    type PfnNegotiateLoaderIcdInterfaceVersion =
        unsafe extern "system" fn(p_supported_version: *mut u32) -> vk::Result;

    /// Ensures the Vulkan instance is destroyed strictly before unloading the ICD shared library
    /// and loader, whether `DropGuard` executes or `Instance::from_raw` returns early.
    struct DirectDriverGuard {
        instance: Option<ash::Instance>,
        library: Option<libloading::Library>,
        _loader: ash::Entry,
    }

    impl Drop for DirectDriverGuard {
        fn drop(&mut self) {
            if let Some(instance) = self.instance.take() {
                // SAFETY: `wgpu-hal` defers `vkDestroyInstance` when a `DropCallback` is set and
                // invokes this closure only after all references to `InstanceShared` drop.
                unsafe {
                    instance.destroy_instance(None);
                }
            }
            drop(self.library.take());
        }
    }

    pub(super) fn try_create_instance_linux(
        hint: CompositorGpuHint,
        flags: wgpu::InstanceFlags,
    ) -> Option<wgpu::Instance> {
        if !cfg!(target_pointer_width = "64") {
            return None;
        }
        if !env_allows_fast_path_with_self_disable(
            &|name| std::env::var_os(name),
            crate::gpu::loader::applied_software_disable(),
        ) {
            return None;
        }
        let sysfs_info = resolve_sysfs_gpu_with_roots(
            hint,
            Path::new("/sys/dev/char"),
            Path::new("/sys/class/drm"),
        )?;
        let allowlist = allowlisted_driver(sysfs_info.vendor_id, &sysfs_info.kernel_driver)?;
        let dirs = icd_search_dirs_with(&|name| std::env::var_os(name));
        let icd_path = resolve_icd_library_with_dirs(allowlist, &dirs)?;

        tracing::debug!(
            vendor_id = format_args!("{:#06x}", sysfs_info.vendor_id),
            device_id = format_args!("{:#06x}", sysfs_info.device_id),
            kernel_driver = %sysfs_info.kernel_driver,
            icd_path = %icd_path.display(),
            "attempting direct exclusive Vulkan instance initialization"
        );

        build_exclusive_vulkan_instance(&icd_path, flags)
    }

    fn build_exclusive_vulkan_instance(
        icd_path: &PathBuf,
        flags: wgpu::InstanceFlags,
    ) -> Option<wgpu::Instance> {
        // Step 1: Load the Vulkan Loader without calling global extension/layer enumeration on it.
        // SAFETY: Loading `libvulkan.so.1` uses standard dynamic linker resolution without invoking
        // any loader-wide ICD enumeration entry points.
        let loader_entry = unsafe { ash::Entry::load() }.ok()?;

        // Open the verified 64-bit allowlisted ICD shared library directly.
        // SAFETY: `icd_path` was validated against the strict allowlist, canonicalized, and
        // checked for a 64-bit ELF header before `dlopen`.
        let library = unsafe { libloading::Library::new(icd_path) }.ok()?;

        // Step 2: Negotiate the loader-ICD interface version when exported, then resolve GIPA.
        // SAFETY: Symbol signature matches the Khronos Vulkan Loader-ICD interface specification.
        if let Ok(negotiate) = unsafe {
            library.get::<PfnNegotiateLoaderIcdInterfaceVersion>(
                b"vk_icdNegotiateLoaderICDInterfaceVersion\0",
            )
        } {
            let mut negotiated_version: u32 = 6;
            // SAFETY: `negotiated_version` points to valid initialized stack `u32` storage.
            let result = unsafe { negotiate(&mut negotiated_version) };
            if result != vk::Result::SUCCESS || negotiated_version < 3 {
                tracing::debug!(
                    ?result,
                    negotiated_version,
                    "ICD refused loader interface negotiation"
                );
                return None;
            }
        }

        // SAFETY: Symbol signature matches Khronos `PFN_vkGetInstanceProcAddr`.
        let get_instance_proc_addr: vk::PFN_vkGetInstanceProcAddr = unsafe {
            let sym = library
                .get::<vk::PFN_vkGetInstanceProcAddr>(b"vk_icdGetInstanceProcAddr\0")
                .ok()?;
            *sym
        };

        // Step 3: Query desired extensions exclusively against the selected ICD (`driver_entry`).
        // SAFETY: `get_instance_proc_addr` is a non-null `vk_icdGetInstanceProcAddr` symbol from
        // `library`, which remains loaded for the lifetime of `driver_entry`.
        let driver_entry = unsafe {
            ash::Entry::from_static_fn(ash::StaticFn {
                get_instance_proc_addr,
            })
        };
        // SAFETY: `driver_entry` wraps valid `vk_icdGetInstanceProcAddr` from `library`.
        let instance_api_version = unsafe { driver_entry.try_enumerate_instance_version() }
            .ok()
            .flatten()
            .unwrap_or(vk::API_VERSION_1_0);

        // Layers/debug messengers would require loader-wide enumeration; keep them off on fast path.
        let hal_flags = flags
            & !(wgpu::InstanceFlags::DEBUG
                | wgpu::InstanceFlags::VALIDATION
                | wgpu::InstanceFlags::GPU_BASED_VALIDATION);

        let hal_extensions = wgpu::hal::vulkan::Instance::desired_extensions(
            &driver_entry,
            instance_api_version,
            hal_flags,
        )
        .ok()?;

        let mut create_extensions = hal_extensions.clone();
        if !create_extensions.contains(&DIRECT_DRIVER_LOADING_EXT) {
            create_extensions.push(DIRECT_DRIVER_LOADING_EXT);
        }

        let app_info = vk::ApplicationInfo::default()
            .application_name(c"zgui")
            .application_version(1)
            .engine_name(c"wgpu-hal")
            .engine_version(2)
            .api_version(if instance_api_version < vk::API_VERSION_1_1 {
                vk::API_VERSION_1_0
            } else {
                vk::API_VERSION_1_3
            });

        let ext_ptrs: Vec<*const core::ffi::c_char> =
            create_extensions.iter().map(|s| s.as_ptr()).collect();

        let driver_infos = [vk::DirectDriverLoadingInfoLUNARG::default()
            .pfn_get_instance_proc_addr(Some(get_instance_proc_addr))];
        let mut loading_list = vk::DirectDriverLoadingListLUNARG::default()
            .mode(vk::DirectDriverLoadingModeLUNARG::EXCLUSIVE)
            .drivers(&driver_infos);

        let create_flags = if create_extensions.contains(&ash::vk::KHR_PORTABILITY_ENUMERATION_NAME)
        {
            vk::InstanceCreateFlags::ENUMERATE_PORTABILITY_KHR
        } else {
            vk::InstanceCreateFlags::empty()
        };

        let create_info = vk::InstanceCreateInfo::default()
            .flags(create_flags)
            .application_info(&app_info)
            .enabled_extension_names(&ext_ptrs)
            .push_next(&mut loading_list);

        // Step 4: Construct the exclusive Vulkan Loader instance and hand ownership to wgpu.
        // SAFETY: `create_info` and `loading_list` outlive `create_instance`; loader exclusive mode
        // restricts initialization strictly to `get_instance_proc_addr`.
        let raw_instance = match unsafe { loader_entry.create_instance(&create_info, None) } {
            Ok(instance) => instance,
            Err(error) => {
                tracing::debug!(?error, "exclusive vkCreateInstance failed; falling back");
                return None;
            }
        };

        let guard = DirectDriverGuard {
            instance: Some(raw_instance.clone()),
            library: Some(library),
            _loader: loader_entry.clone(),
        };
        let drop_callback: wgpu::hal::DropCallback = Box::new(move || drop(guard));

        // SAFETY: `raw_instance` was created by `loader_entry` with `instance_api_version`,
        // `hal_extensions`, and `hal_flags`; `drop_callback` destroys `raw_instance` before
        // unloading `library` and dropping `loader_entry`.
        let hal_instance = match unsafe {
            wgpu::hal::vulkan::Instance::from_raw(
                loader_entry,
                raw_instance,
                instance_api_version,
                0,
                None,
                hal_extensions,
                hal_flags,
                wgpu::MemoryBudgetThresholds::default(),
                false,
                Some(drop_callback),
            )
        } {
            Ok(instance) => instance,
            Err(error) => {
                tracing::debug!(?error, "wgpu-hal Instance::from_raw failed; falling back");
                return None;
            }
        };

        // SAFETY: `hal_instance` is a valid exclusive Vulkan HAL instance owned cleanly by wgpu.
        Some(unsafe { wgpu::Instance::from_hal::<wgpu::hal::api::Vulkan>(hal_instance) })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "zgui-direct-vulkan-{name}-{}-{id}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    fn env_lookup<'a>(
        vars: &'a HashMap<&'a str, &'a str>,
    ) -> impl Fn(&str) -> Option<OsString> + 'a {
        move |k| vars.get(k).map(OsString::from)
    }

    #[test]
    fn env_gates_reject_any_explicit_override_and_active_prime() {
        let mut vars: HashMap<&str, &str> = HashMap::new();

        assert!(env_allows_fast_path_with(&env_lookup(&vars)));

        // DRI_PRIME=0 and whitespace-only overrides still allow the fast path.
        vars.insert("DRI_PRIME", "0");
        vars.insert("__NV_PRIME_RENDER_OFFLOAD", "0");
        vars.insert("VK_DRIVER_FILES", "   ");
        assert!(env_allows_fast_path_with(&env_lookup(&vars)));

        for key in OVERRIDE_ENV_VARS {
            let mut m = HashMap::new();
            m.insert(*key, "1");
            assert!(
                !env_allows_fast_path_with(&env_lookup(&m)),
                "{key} must bypass"
            );
        }

        for prime in PRIME_ENV_VARS {
            let mut m = HashMap::new();
            m.insert(*prime, "1");
            assert!(
                !env_allows_fast_path_with(&env_lookup(&m)),
                "{prime}=1 must bypass"
            );
        }

        // Self-applied `prefer_hardware_vulkan()` (`*lvp*`) allows the fast path, while user
        // values or externally-set `*lvp*` still bypass it.
        let mut lvp = HashMap::new();
        lvp.insert("VK_LOADER_DRIVERS_DISABLE", "*lvp*");
        assert!(!env_allows_fast_path_with_self_disable(
            &env_lookup(&lvp),
            false
        ));
        assert!(env_allows_fast_path_with_self_disable(
            &env_lookup(&lvp),
            true
        ));
        lvp.insert("VK_LOADER_DRIVERS_DISABLE", "*nv*");
        assert!(!env_allows_fast_path_with_self_disable(
            &env_lookup(&lvp),
            true
        ));
    }

    #[test]
    fn allowlist_accepts_only_radv_and_anv() {
        assert_eq!(
            allowlisted_driver(0x1002, "amdgpu"),
            Some(AllowlistedDriver {
                library_basename: "libvulkan_radeon.so",
                check_competing_amd: true,
            })
        );
        assert_eq!(
            allowlisted_driver(0x8086, "i915"),
            Some(AllowlistedDriver {
                library_basename: "libvulkan_intel.so",
                check_competing_amd: false,
            })
        );
        assert_eq!(
            allowlisted_driver(0x8086, "xe"),
            Some(AllowlistedDriver {
                library_basename: "libvulkan_intel.so",
                check_competing_amd: false,
            })
        );
        assert_eq!(allowlisted_driver(0x10de, "nvidia"), None);
        assert_eq!(allowlisted_driver(0x1002, "radeon"), None);
    }

    #[test]
    fn parses_icd_manifests_with_escapes_and_optional_arch() {
        let valid = r#"{
            "file_format_version": "1.0.1",
            "ICD": {
                "library_path": "\/nix\/store\/abc-mesa\/lib\/libvulkan_radeon.so",
                "api_version": "1.3.289",
                "library_arch": "64"
            }
        }"#;
        assert_eq!(
            parse_icd_manifest(valid),
            Some(IcdManifest {
                file_format_version: "1.0.1".to_owned(),
                library_path: "/nix/store/abc-mesa/lib/libvulkan_radeon.so".to_owned(),
                library_arch: Some("64".to_owned()),
            })
        );

        let no_arch = r#"{
            "file_format_version": "1.0.0",
            "ICD": { "library_path": "/usr/lib/libvulkan_intel.so" }
        }"#;
        assert_eq!(
            parse_icd_manifest(no_arch),
            Some(IcdManifest {
                file_format_version: "1.0.0".to_owned(),
                library_path: "/usr/lib/libvulkan_intel.so".to_owned(),
                library_arch: None,
            })
        );
        assert_eq!(parse_icd_manifest("not json"), None);
    }

    #[test]
    fn icd_resolution_enforces_elf64_uniqueness_and_competing_amd_abort() {
        let root = temp_dir("icd");
        let dir_a = root.join("a");
        let dir_b = root.join("b");
        let libs = root.join("libs");
        std::fs::create_dir_all(&dir_a).unwrap();
        std::fs::create_dir_all(&dir_b).unwrap();
        std::fs::create_dir_all(&libs).unwrap();

        let radv = libs.join("libvulkan_radeon.so");
        let hasvk = libs.join("libvulkan_intel_hasvk.so");
        let anv = libs.join("libvulkan_intel.so");
        let radv32 = libs.join("libvulkan_radeon_32.so");
        // Valid ELF64 header vs invalid ELF32 header.
        std::fs::write(&radv, b"\x7fELF\x02\x01").unwrap();
        std::fs::write(&hasvk, b"\x7fELF\x02\x01").unwrap();
        std::fs::write(&anv, b"\x7fELF\x02\x01").unwrap();
        std::fs::write(&radv32, b"\x7fELF\x01\x01").unwrap();

        std::fs::write(
            dir_a.join("radeon_icd.x86_64.json"),
            format!(
                r#"{{"file_format_version":"1.0.1","ICD":{{"library_path":"{}","library_arch":"64"}}}}"#,
                radv.display()
            ),
        )
        .unwrap();
        // 32-bit manifest for same basename must be ignored cleanly.
        std::fs::write(
            dir_b.join("radeon_icd.i686.json"),
            format!(
                r#"{{"file_format_version":"1.0.1","ICD":{{"library_path":"{}","library_arch":"32"}}}}"#,
                radv32.display()
            ),
        )
        .unwrap();

        let radv_allow = allowlisted_driver(0x1002, "amdgpu").unwrap();
        assert_eq!(
            resolve_icd_library_with_dirs(radv_allow, &[dir_a.clone(), dir_b.clone()]),
            Some(std::fs::canonicalize(&radv).unwrap())
        );

        // Adding a second manifest pointing to a *different* libvulkan_radeon.so aborts.
        let libs2 = root.join("libs2");
        std::fs::create_dir_all(&libs2).unwrap();
        let radv_dup = libs2.join("libvulkan_radeon.so");
        std::fs::write(&radv_dup, b"\x7fELF\x02\x01").unwrap();
        std::fs::write(
            dir_b.join("radeon_extra.json"),
            format!(
                r#"{{"file_format_version":"1.0.1","ICD":{{"library_path":"{}"}}}}"#,
                radv_dup.display()
            ),
        )
        .unwrap();
        assert_eq!(
            resolve_icd_library_with_dirs(radv_allow, &[dir_a.clone(), dir_b.clone()]),
            None
        );
        std::fs::remove_file(dir_b.join("radeon_extra.json")).unwrap();

        // Competing 64-bit AMDVLK aborts AMD fast path.
        std::fs::write(
            dir_b.join("amd_icd64.json"),
            r#"{"file_format_version":"1.0.1","ICD":{"library_path":"/opt/amdvlk/amdvlk64.so","library_arch":"64"}}"#,
        )
        .unwrap();
        assert_eq!(
            resolve_icd_library_with_dirs(radv_allow, &[dir_a.clone(), dir_b.clone()]),
            None
        );

        // Intel fast path tolerates co-installed intel_hasvk_icd.
        std::fs::write(
            dir_a.join("intel_icd.x86_64.json"),
            format!(
                r#"{{"file_format_version":"1.0.1","ICD":{{"library_path":"{}","library_arch":"64"}}}}"#,
                anv.display()
            ),
        )
        .unwrap();
        std::fs::write(
            dir_a.join("intel_hasvk_icd.x86_64.json"),
            format!(
                r#"{{"file_format_version":"1.0.1","ICD":{{"library_path":"{}","library_arch":"64"}}}}"#,
                hasvk.display()
            ),
        )
        .unwrap();
        let anv_allow = allowlisted_driver(0x8086, "xe").unwrap();
        assert_eq!(
            resolve_icd_library_with_dirs(anv_allow, &[dir_a.clone()]),
            Some(std::fs::canonicalize(&anv).unwrap())
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn sysfs_requires_distinct_vendor_suspended_peer_gpu() {
        use std::os::unix::fs::symlink;

        let root = temp_dir("sysfs");
        let dev_char = root.join("dev_char");
        let drm = root.join("drm");
        let drivers = root.join("drivers");
        let igpu = root.join("pci/0000:04:00.0");
        let dgpu = root.join("pci/0000:01:00.0");

        std::fs::create_dir_all(dev_char.join("226:128")).unwrap();
        std::fs::create_dir_all(drm.join("card0")).unwrap();
        std::fs::create_dir_all(drm.join("card1")).unwrap();
        std::fs::create_dir_all(drm.join("card0-eDP-1")).unwrap();
        std::fs::create_dir_all(drivers.join("amdgpu")).unwrap();
        std::fs::create_dir_all(igpu.join("power")).unwrap();
        std::fs::create_dir_all(dgpu.join("power")).unwrap();

        std::fs::write(igpu.join("vendor"), "0x1002\n").unwrap();
        std::fs::write(igpu.join("device"), "0x15bf\n").unwrap();
        std::fs::write(igpu.join("power/runtime_status"), "active\n").unwrap();
        symlink(drivers.join("amdgpu"), igpu.join("driver")).unwrap();

        std::fs::write(dgpu.join("vendor"), "0x10de\n").unwrap();
        std::fs::write(dgpu.join("device"), "0x28e0\n").unwrap();
        std::fs::write(dgpu.join("power/runtime_status"), "active\n").unwrap();

        symlink(&igpu, dev_char.join("226:128/device")).unwrap();
        symlink(&igpu, drm.join("card0/device")).unwrap();
        symlink(&dgpu, drm.join("card1/device")).unwrap();

        let hint = CompositorGpuHint {
            drm_render_dev: Some(0xe280),
            vendor_id: Some(0x1002),
            device_id: Some(0x15bf),
        };

        // Active dGPU must not take fast path.
        assert_eq!(resolve_sysfs_gpu_with_roots(hint, &dev_char, &drm), None);

        // Suspended dGPU enables fast path.
        std::fs::write(dgpu.join("power/runtime_status"), "suspended\n").unwrap();
        let resolved = resolve_sysfs_gpu_with_roots(hint, &dev_char, &drm).unwrap();
        assert_eq!(resolved.vendor_id, 0x1002);
        assert_eq!(resolved.device_id, 0x15bf);
        assert_eq!(resolved.kernel_driver, "amdgpu");

        // Same-vendor peer GPU must not qualify even when suspended.
        std::fs::write(dgpu.join("vendor"), "0x1002\n").unwrap();
        assert_eq!(resolve_sysfs_gpu_with_roots(hint, &dev_char, &drm), None);

        let _ = std::fs::remove_dir_all(&root);
    }
}
