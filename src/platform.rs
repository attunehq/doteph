//! The host platform, spelled the way Docker spells platforms.
//!
//! A workspace can carry platform overrides such as `.eph.linux` or
//! `.eph.windows.amd64` (see [`crate::workspace::config_layers`]). Their names
//! use Docker's `os[/arch[/variant]]` components with each `/` written as `.`,
//! and the Docker and OCI names for each component (`darwin`, `amd64`, `arm64`)
//! rather than Rust's (`macos`, `x86_64`, `aarch64`).
//!
//! The platform describes the machine running eph, as eph was built for it. It
//! does not come from the Docker daemon, and it does not choose the platform of
//! any container image.
//!
//! Detection is a pure mapping from Rust's target names ([`HostPlatform::from_rust`]),
//! so every platform can be tested on any machine.

use std::fmt;

/// A Docker-style platform: an OS, an architecture, and an optional variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostPlatform {
    /// The OS, such as `linux`, `darwin`, or `windows`.
    pub os: &'static str,
    /// The architecture, such as `amd64` or `arm64`.
    pub arch: &'static str,
    /// The architecture variant, such as `v8`, when it is known.
    pub variant: Option<&'static str>,
}

impl HostPlatform {
    /// The platform this eph binary runs on.
    #[must_use]
    pub fn current() -> Self {
        Self::from_rust(std::env::consts::OS, std::env::consts::ARCH)
    }

    /// Map Rust's `target_os` and `target_arch` names to Docker's.
    ///
    /// Names that need no translation pass through unchanged. Only `arm64` gets
    /// a variant: Docker and containerd normalize `arm64` to `arm64/v8`, so
    /// `v8` is always right. eph does not guess a variant it cannot know for
    /// certain, such as an `amd64` microarchitecture level.
    ///
    /// # Examples
    ///
    /// ```
    /// use eph::platform::HostPlatform;
    ///
    /// let platform = HostPlatform::from_rust("macos", "aarch64");
    /// assert_eq!(platform.to_string(), "darwin/arm64/v8");
    /// ```
    #[must_use]
    pub fn from_rust(os: &'static str, arch: &'static str) -> Self {
        let os = match os {
            "macos" => "darwin",
            other => other,
        };
        let arch = match arch {
            "x86_64" => "amd64",
            "aarch64" => "arm64",
            "x86" => "386",
            "loongarch64" => "loong64",
            "powerpc64" if cfg!(target_endian = "little") => "ppc64le",
            "powerpc64" => "ppc64",
            other => other,
        };
        let variant = (arch == "arm64").then_some("v8");
        Self { os, arch, variant }
    }

    /// The platform override files for this platform, lowest precedence first:
    /// `.eph.<os>`, `.eph.<os>.<arch>`, then `.eph.<os>.<arch>.<variant>` when
    /// the variant is known.
    ///
    /// # Examples
    ///
    /// ```
    /// use eph::platform::HostPlatform;
    ///
    /// let platform = HostPlatform::from_rust("linux", "x86_64");
    /// assert_eq!(platform.override_files(), [".eph.linux", ".eph.linux.amd64"]);
    /// ```
    #[must_use]
    pub fn override_files(&self) -> Vec<String> {
        let mut files = vec![
            format!(".eph.{}", self.os),
            format!(".eph.{}.{}", self.os, self.arch),
        ];
        if let Some(variant) = self.variant {
            files.push(format!(".eph.{}.{}.{variant}", self.os, self.arch));
        }
        files
    }
}

impl fmt::Display for HostPlatform {
    /// Docker's `os/arch[/variant]` form, such as `linux/arm64/v8`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.os, self.arch)?;
        if let Some(variant) = self.variant {
            write!(f, "/{variant}")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every target eph ships a binary for, plus the Docker selectors the
    /// override design names.
    #[test]
    fn maps_rust_targets_to_docker_platforms() {
        let cases = [
            (("windows", "x86_64"), "windows/amd64"),
            (("windows", "aarch64"), "windows/arm64/v8"),
            (("linux", "x86_64"), "linux/amd64"),
            (("linux", "aarch64"), "linux/arm64/v8"),
            (("macos", "x86_64"), "darwin/amd64"),
            (("macos", "aarch64"), "darwin/arm64/v8"),
            (("freebsd", "x86"), "freebsd/386"),
            (("linux", "riscv64"), "linux/riscv64"),
            (("linux", "s390x"), "linux/s390x"),
            (("linux", "loongarch64"), "linux/loong64"),
        ];
        for ((os, arch), expected) in cases {
            assert_eq!(
                HostPlatform::from_rust(os, arch).to_string(),
                expected,
                "{os}/{arch}"
            );
        }
    }

    #[test]
    fn override_files_follow_docker_component_order() {
        assert_eq!(
            HostPlatform::from_rust("windows", "x86_64").override_files(),
            [".eph.windows", ".eph.windows.amd64"]
        );
        assert_eq!(
            HostPlatform::from_rust("linux", "x86_64").override_files(),
            [".eph.linux", ".eph.linux.amd64"]
        );
        assert_eq!(
            HostPlatform::from_rust("linux", "aarch64").override_files(),
            [".eph.linux", ".eph.linux.arm64", ".eph.linux.arm64.v8"]
        );
        assert_eq!(
            HostPlatform::from_rust("macos", "aarch64").override_files(),
            [".eph.darwin", ".eph.darwin.arm64", ".eph.darwin.arm64.v8"]
        );
    }

    #[test]
    fn current_matches_the_build_target() {
        let current = HostPlatform::current();
        assert_eq!(
            current,
            HostPlatform::from_rust(std::env::consts::OS, std::env::consts::ARCH)
        );
        assert!(!current.os.is_empty() && !current.arch.is_empty());
    }
}
