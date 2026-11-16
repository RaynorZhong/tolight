use super::common::*;
use super::{Result, WalArchive, root};
use serde_json::json;
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

fn hex(value: &str, len: usize) -> bool {
    value.len() == len
        && value
            .bytes()
            .all(|v| v.is_ascii_digit() || (b'A'..=b'F').contains(&v))
}
pub fn valid_name(name: &str) -> bool {
    hex(name, 24)
        || name.strip_suffix(".history").is_some_and(|s| hex(s, 8))
        || name.strip_suffix(".backup").is_some_and(|s| {
            s.split_once('.')
                .is_some_and(|(a, b)| hex(a, 24) && hex(b, 8))
        })
}
pub fn checked_archive(path: &Path, public: Option<&Path>) -> Result<PathBuf> {
    if symlink(path) {
        return Err("archive directory must not be a symlink".into());
    }
    let mut path = private_path(path, &root().join("public-dist"))?;
    if let Some(public) = public {
        path = private_path(&path, public)?
    }
    if path.exists() && !path.is_dir() {
        return Err("archive must be a directory".into());
    }
    Ok(path)
}
pub fn verified_file(directory: &Path, name: &str) -> Result<PathBuf> {
    if !valid_name(name) {
        return Err("invalid archive file name".into());
    }
    let path = directory.join(name);
    let metadata = directory.join(format!("{name}.meta.json"));
    regular(&path)?;
    regular(&metadata)?;
    let value = json_file(&metadata)?;
    if value["schema"] != "tolight-wal-archive/1"
        || value["file"] != name
        || value["size"].as_u64() != Some(fs::metadata(&path)?.len())
        || value["sha256"].as_str() != Some(&sha256(&path)?)
    {
        return Err("archive checksum/metadata mismatch".into());
    }
    Ok(path)
}
pub fn archive(directory: &Path, source: &Path, name: &str) -> Result<()> {
    if !valid_name(name) {
        return Err("invalid archive file name".into());
    }
    regular(source)?;
    private_dir(directory)?;
    let final_path = directory.join(name);
    let metadata = directory.join(format!("{name}.meta.json"));
    let digest = sha256(source)?;
    if fs::symlink_metadata(&final_path).is_ok() {
        regular(&final_path)?;
        if sha256(&final_path)? != digest {
            return Err("refusing to replace a different archived WAL file".into());
        }
        if metadata.exists() || symlink(&metadata) {
            regular(&metadata)?;
            if verified_file(directory, name).is_ok() {
                sync_directory(directory)?;
                return Ok(());
            }
        }
    }
    let temporary = directory.join(format!(".{name}.{}.tmp", uuid::Uuid::new_v4().simple()));
    let mt = directory.join(format!(
        ".{name}.{}.meta.tmp",
        uuid::Uuid::new_v4().simple()
    ));
    let result = (|| -> Result<()> {
        let mut input = fs::File::open(source)?;
        let mut output = new_file(&temporary)?;
        std::io::copy(&mut input, &mut output)?;
        output.flush()?;
        output.sync_all()?;
        if sha256(&temporary)? != digest {
            return Err("source changed while archiving".into());
        }
        match fs::hard_link(&temporary, &final_path) {
            Ok(()) => (),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                regular(&final_path)?;
                if sha256(&final_path)? != digest {
                    return Err("conflicting archive writer".into());
                }
            }
            Err(error) => return Err(error.into()),
        }
        if metadata.exists() || symlink(&metadata) {
            regular(&metadata)?
        }
        private_json(
            &mt,
            &json!({"schema":"tolight-wal-archive/1","file":name,"size":fs::metadata(&final_path)?.len(),"sha256":digest,"archived_at":now()}),
        )?;
        fs::rename(&mt, &metadata)?;
        verified_file(directory, name)?;
        sync_directory(directory)?;
        Ok(())
    })();
    let _ = fs::remove_file(&temporary);
    let _ = fs::remove_file(&mt);
    result
}
pub fn restore(
    directory: &Path,
    destination: &Path,
    name: &str,
    public: Option<&Path>,
) -> Result<()> {
    let source = verified_file(directory, name)?;
    let mut destination = private_path(destination, &root().join("public-dist"))?;
    if let Some(public) = public {
        destination = private_path(&destination, public)?
    }
    let temporary = destination.with_file_name(format!(
        ".{}.{}.tmp",
        destination
            .file_name()
            .ok_or("destination filename")?
            .to_string_lossy(),
        uuid::Uuid::new_v4().simple()
    ));
    let result = (|| -> Result<()> {
        let mut input = fs::File::open(&source)?;
        let mut output = new_file(&temporary)?;
        std::io::copy(&mut input, &mut output)?;
        output.flush()?;
        output.sync_all()?;
        if sha256(&temporary)? != sha256(&source)? {
            return Err("archive changed while restoring".into());
        }
        fs::rename(&temporary, &destination)?;
        sync_directory(destination.parent().ok_or("destination parent")?)
    })();
    let _ = fs::remove_file(temporary);
    result
}
pub fn copy_archive(source: &Path, target: &Path) -> Result<Vec<String>> {
    private_dir(target)?;
    let mut names = fs::read_dir(source)?
        .map(|e| e.map(|e| e.file_name().to_string_lossy().into_owned()))
        .collect::<std::io::Result<Vec<_>>>()?;
    names.retain(|s| valid_name(s));
    names.sort();
    if !names.iter().any(|s| s.len() == 24) {
        return Err("no complete archived WAL segments".into());
    }
    for name in &names {
        let file = verified_file(source, name)?;
        fs::copy(file, target.join(name))?;
        fs::copy(
            source.join(format!("{name}.meta.json")),
            target.join(format!("{name}.meta.json")),
        )?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for path in [target.join(name), target.join(format!("{name}.meta.json"))] {
                fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
            }
        }
        verified_file(target, name)?;
    }
    Ok(names)
}
pub fn run(args: WalArchive) -> Result<()> {
    let directory = checked_archive(&args.archive_dir, args.public_dir.as_deref())?;
    match (args.mode.as_str(), args.source, args.destination) {
        ("archive", Some(source), None) => archive(&directory, &source, &args.name),
        ("restore", None, Some(destination)) => restore(
            &directory,
            &destination,
            &args.name,
            args.public_dir.as_deref(),
        ),
        _ => Err("archive requires only source; restore requires only destination".into()),
    }
}
