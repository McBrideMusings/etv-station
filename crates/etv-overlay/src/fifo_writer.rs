use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

use nix::sys::stat::Mode;
use nix::unistd::mkfifo;

pub struct FifoWriter {
    path: PathBuf,
    owns_path: bool,
    file: Option<std::fs::File>,
}

impl FifoWriter {
    pub fn create(path: PathBuf) -> anyhow::Result<Self> {
        if path.exists() {
            std::fs::remove_file(&path)
                .map_err(|e| anyhow::anyhow!("remove stale fifo {}: {e}", path.display()))?;
        }
        mkfifo(&path, Mode::S_IRUSR | Mode::S_IWUSR)
            .map_err(|e| anyhow::anyhow!("mkfifo {}: {e}", path.display()))?;
        Ok(Self {
            path,
            owns_path: true,
            file: None,
        })
    }

    pub fn attach(path: PathBuf) -> Self {
        Self {
            path,
            owns_path: false,
            file: None,
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn open_for_writing(&mut self) -> anyhow::Result<()> {
        if self.file.is_some() {
            return Ok(());
        }
        // O_RDWR on a fifo lets us open it without blocking on a reader. The
        // alternative (O_WRONLY) blocks until a reader connects, which would
        // deadlock against ffmpeg also blocking on opening the fifo for read
        // until a writer connects. With O_RDWR both sides can open whenever
        // they like.
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&self.path)
            .map_err(|e| anyhow::anyhow!("open fifo {}: {e}", self.path.display()))?;
        self.file = Some(file);
        Ok(())
    }

    pub fn write_frame(&mut self, frame: &[u8]) -> std::io::Result<()> {
        let file = self
            .file
            .as_mut()
            .ok_or_else(|| std::io::Error::other("fifo not open"))?;
        file.write_all(frame)?;
        Ok(())
    }
}

impl Drop for FifoWriter {
    fn drop(&mut self) {
        self.file = None;
        if self.owns_path && self.path.exists() {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

pub fn default_fifo_path(label: &str) -> PathBuf {
    let pid = std::process::id();
    PathBuf::from(format!("/tmp/etv-overlay-{label}-{pid}.fifo"))
}
