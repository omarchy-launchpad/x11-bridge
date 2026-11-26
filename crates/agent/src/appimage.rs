//! Bind an AppImage window to the installed image that serves its executable over FUSE.
use std::path::Path;

pub struct AppImage {
    #[cfg(target_os = "linux")]
    path: std::path::PathBuf,
    #[cfg(target_os = "linux")]
    name: String,
    #[cfg(target_os = "linux")]
    mount: std::cell::OnceCell<(std::fs::File, u64)>,
}

impl AppImage {
    pub fn from_exec(args: &[String], id: &str) -> Option<Self> {
        let [program] = args else { return None };
        let path = Path::new(program);
        if !path.is_absolute() || path.extension()? != "AppImage" {
            return None;
        }
        #[cfg(target_os = "linux")]
        return Some(Self {
            path: path.canonicalize().ok()?,
            name: id.strip_suffix(".desktop")?.to_owned(),
            mount: Default::default(),
        });
        #[cfg(not(target_os = "linux"))]
        {
            let _ = id;
            None
        }
    }

    #[cfg(target_os = "linux")]
    pub fn owns(&self, pid: u32) -> bool {
        use std::{fs, os::unix::fs::MetadataExt};
        let exe = format!("/proc/{pid}/exe");
        if !fs::read_link(&exe).is_ok_and(|path| {
            path.file_name()
                .is_some_and(|name| name.eq_ignore_ascii_case(self.name.as_str()))
        }) {
            return false;
        }
        let Ok(file) = fs::File::open(exe) else {
            return false;
        };
        let Ok(meta) = file.metadata() else {
            return false;
        };
        let device = meta.dev();
        if let Some((_, pinned)) = self.mount.get()
            && *pinned == device
        {
            return true;
        }
        if rustix::fs::major(device) != 0 || !self.serves(rustix::fs::minor(device)) {
            return false;
        }
        // Keeping the executable open prevents reuse of the verified mount's device number.
        let _ = self.mount.set((file, device));
        true
    }

    #[cfg(target_os = "linux")]
    fn serves(&self, connection: u32) -> bool {
        use std::fs;
        let Ok(processes) = fs::read_dir("/proc") else {
            return false;
        };
        processes.flatten().any(|proc| {
            let proc = proc.path();
            if fs::read_link(proc.join("exe")).as_deref().ok() != Some(self.path.as_path()) {
                return false;
            }
            let Ok(fds) = fs::read_dir(proc.join("fd")) else {
                return false;
            };
            fds.flatten().any(|fd| {
                fs::read_link(fd.path()).as_deref().ok() == Some(Path::new("/dev/fuse"))
                    && fs::read_to_string(proc.join("fdinfo").join(fd.file_name())).is_ok_and(
                        |info| {
                            info.lines()
                                .filter_map(|line| line.strip_prefix("fuse_connection:"))
                                .any(|id| id.trim().parse::<u32>() == Ok(connection))
                        },
                    )
            })
        })
    }

    #[cfg(not(target_os = "linux"))]
    pub fn owns(&self, _: u32) -> bool {
        false
    }
}
