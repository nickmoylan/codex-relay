use crate::model::*;
use anyhow::{Context, Result, ensure};
use fs2::FileExt;
use serde::{Serialize, de::DeserializeOwned};
use std::{
    fs::{File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

#[derive(Clone)]
pub struct Store {
    pub root: PathBuf,
}

pub fn private_dir(path: &Path) -> Result<()> {
    std::fs::create_dir_all(path)?;
    ensure!(
        std::fs::canonicalize(path)? == path,
        "state/scratch directories must be canonical without symlink ancestors"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

pub fn private_file(path: &Path) -> Result<File> {
    if let Ok(meta) = std::fs::symlink_metadata(path) {
        ensure!(
            meta.is_file() && !meta.file_type().is_symlink(),
            "state entry is not a regular file"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            ensure!(meta.nlink() == 1, "hard-linked state file excluded");
        }
    }
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    Ok(options.open(path)?)
}

pub fn read_json<T: DeserializeOwned>(path: &Path, max_bytes: u64) -> Result<T> {
    let meta = std::fs::symlink_metadata(path)?;
    ensure!(
        meta.is_file() && !meta.file_type().is_symlink() && meta.len() <= max_bytes,
        "invalid or oversized state/config file"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        ensure!(meta.nlink() == 1, "hard-linked JSON file excluded");
    }
    let data = std::fs::read(path)?;
    serde_json::from_slice(&data)
        .map_err(|_| anyhow::anyhow!("invalid structured JSON (values withheld)"))
}

pub fn atomic_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    if path.exists() {
        let _ = private_file(path)?;
    }
    let parent = path.parent().context("missing state parent")?;
    let temporary = parent.join(format!(".{}.tmp", uuid::Uuid::new_v4()));
    let mut file = private_file(&temporary)?;
    file.write_all(&serde_json::to_vec_pretty(value)?)?;
    file.sync_all()?;
    std::fs::rename(&temporary, path)?;
    File::open(parent)?.sync_all()?;
    Ok(())
}

impl Store {
    pub fn open(root: &Path) -> Result<Self> {
        private_dir(root)?;
        private_dir(&root.join("jobs"))?;
        Ok(Self { root: root.into() })
    }
    pub fn transaction(&self) -> Result<File> {
        let lock = private_file(&self.root.join("registry.lock"))?;
        lock.lock_exclusive()?;
        Ok(lock)
    }
    pub fn job_dir(&self, id: &str) -> Result<PathBuf> {
        ensure!(uuid::Uuid::parse_str(id).is_ok(), "invalid job id");
        let dir = self.root.join("jobs").join(id);
        if dir.exists() {
            ensure!(
                std::fs::canonicalize(&dir)? == dir,
                "symlinked job directory excluded"
            );
        }
        Ok(dir)
    }
    pub fn create(&self, job: &Job) -> Result<()> {
        let dir = self.job_dir(&job.id)?;
        ensure!(!dir.exists(), "job id already exists");
        private_dir(&dir)?;
        self.save(job)
    }
    pub fn save(&self, job: &Job) -> Result<()> {
        atomic_json(&self.job_dir(&job.id)?.join("job.json"), job)
    }
    pub fn load(&self, id: &str) -> Result<Job> {
        read_json(&self.job_dir(id)?.join("job.json"), 32 * 1024 * 1024)
    }
    pub fn jobs(&self) -> Result<Vec<Job>> {
        let mut jobs = vec![];
        for entry in std::fs::read_dir(self.root.join("jobs"))? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                let id = entry.file_name().to_string_lossy().into_owned();
                jobs.push(self.load(&id)?);
            }
        }
        jobs.sort_by_key(|j| j.created_at_ms);
        Ok(jobs)
    }
    pub fn running_lock(&self, id: &str) -> Result<File> {
        private_file(&self.job_dir(id)?.join("running.lock"))
    }
    pub fn supervisor_alive(&self, id: &str) -> Result<bool> {
        let lock = self.running_lock(id)?;
        match lock.try_lock_exclusive() {
            Ok(()) => Ok(false),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(true),
            Err(e) => Err(e.into()),
        }
    }
    pub fn cancel(&self, id: &str) -> Result<()> {
        let mut file = private_file(&self.job_dir(id)?.join("cancel"))?;
        file.write_all(b"cancel\n")?;
        file.sync_all()?;
        Ok(())
    }
    pub fn cancelled(&self, id: &str) -> Result<bool> {
        Ok(self.job_dir(id)?.join("cancel").exists())
    }
}
