use std::borrow::Cow;
use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::{env, fs};

use shared::error::str_to_io_error;
use crate::utils::debug_if_enabled;
use shared::utils::{API_PROXY_FILE, CONFIG_FILE, CONFIG_PATH, MAPPING_FILE, SOURCE_FILE, USER_FILE};
use log::{debug, error};
use path_clean::PathClean;

pub fn file_writer<W>(w: W) -> BufWriter<W>
where
    W: Write,
{
    BufWriter::with_capacity(131_072, w)
}

pub fn file_reader<R>(r: R) -> BufReader<R>
where
    R: Read,
{
    BufReader::with_capacity(131_072, r)
}

pub fn get_exe_path() -> PathBuf {
    let default_path = std::path::PathBuf::from("./");
    let current_exe = std::env::current_exe();
    match current_exe {
        Ok(exe) => {
            match fs::read_link(&exe) {
                Ok(f) => f.parent().map_or(default_path, std::path::Path::to_path_buf),
                Err(_) => exe.parent().map_or(default_path, std::path::Path::to_path_buf)
            }
        }
        Err(_) => default_path
    }
}

fn get_default_path(file: &str) -> String {
    let path: PathBuf = get_exe_path();
    let default_path = path.join(file);
    String::from(if default_path.exists() {
        default_path.to_str().unwrap_or(file)
    } else {
        file
    })
}

pub fn get_default_file_path(config_path: &str, file: &str) -> String {
    let path: PathBuf = PathBuf::from(config_path);
    let default_path = path.join(file);
    String::from(if default_path.exists() {
        default_path.to_str().unwrap_or(file)
    } else {
        file
    })
}

#[inline]
pub fn get_default_user_file_path(config_path: &str) -> String {
    get_default_file_path(config_path, USER_FILE)
}

#[inline]
pub fn get_default_config_path() -> String {
    get_default_path(CONFIG_PATH)
}

#[inline]
pub fn get_default_config_file_path(config_path: &str) -> String {
    get_default_file_path(config_path, CONFIG_FILE)
}

#[inline]
pub fn get_default_sources_file_path(config_path: &str) -> String {
    get_default_file_path(config_path, SOURCE_FILE)
}

#[inline]
pub fn get_default_mappings_path(config_path: &str) -> String {
    get_default_file_path(config_path, MAPPING_FILE)
}

#[inline]
pub fn get_default_api_proxy_config_path(config_path: &str) -> String {
    get_default_file_path(config_path, API_PROXY_FILE)
}

pub fn resolve_directory_path(input: &str) -> String {
    let current_dir = std::env::current_dir().unwrap_or_default();

    if input.is_empty() {
        return String::from(current_dir.to_str().unwrap_or("."));
    }

    let input_path = PathBuf::from(input);
    if let Err(e) = fs::create_dir_all(&input_path) {
        error!("Failed to create directory: {} - {e}", input_path.display());
    }

    let resolved_path = fs::metadata(&input_path).ok().and_then(|md| {
        if md.is_dir() && !md.permissions().readonly() {
            input_path.canonicalize().ok()
        } else {
            error!("Path not found or not writable: {}", input_path.display());
            None
        }
    });

    let final_path = resolved_path.unwrap_or_else(|| current_dir.join(input));

    final_path
        .canonicalize()
        .map_or_else(
            |_| {
                error!("Path not found {}", final_path.display());
                String::from("./")
            },
            |ap| String::from(ap.to_str().unwrap_or("./")),
        )
}


#[inline]
pub fn open_file(file_name: &Path) -> Result<File, std::io::Error> {
    File::open(file_name)
}

pub fn persist_file(persist_file: Option<PathBuf>, text: &str) {
    if let Some(path_buf) = persist_file {
        let filename = &path_buf.to_str().unwrap_or("?");
        match File::create(&path_buf) {
            Ok(mut file) => match file.write_all(text.as_bytes()) {
                Ok(()) => debug!("persisted: {filename}"),
                Err(e) => error!("failed to persist file {filename}, {e}")
            },
            Err(e) => error!("failed to persist file {filename}, {e}")
        }
    }
}

pub fn prepare_persist_path(file_name: &str, date_prefix: &str) -> PathBuf {
    let now = chrono::Local::now();
    let persist_filename = file_name.replace("{}", format!("{date_prefix}{}", now.format("%Y%m%d_%H%M%S").to_string().as_str()).as_str());
    std::path::PathBuf::from(persist_filename)
}

pub fn get_file_path(wd: &str, path: Option<PathBuf>) -> Option<PathBuf> {
    path.map(|p| if p.is_relative() {
        let pb = PathBuf::from(wd);
        pb.join(&p).clean()
    } else {
        p
    })
}

pub fn add_prefix_to_filename(path: &Path, prefix: &str, ext: Option<&str>) -> PathBuf {
    let file_name = path.file_name().unwrap_or_default();
    let new_file_name = format!("{}{}", prefix, file_name.to_string_lossy());
    let result = path.with_file_name(new_file_name);
    match ext {
        None => result,
        Some(extension) => result.with_extension(extension)
    }
}

pub fn path_exists(file_path: &Path) -> bool {
    if let Ok(metadata) = fs::metadata(file_path) {
        return metadata.is_file();
    }
    false
}

pub fn check_write(res: &std::io::Result<()>) -> Result<(), std::io::Error> {
    match res {
        Ok(()) => Ok(()),
        Err(_) => Err(str_to_io_error("Unable to write file")),
    }
}

// pub fn append_extension(path: &Path, ext: &str) -> PathBuf {
//     let extension = path.extension().map(|ext| ext.to_str().unwrap_or(""));
//     path.with_extension(format!("{}{ext}", &extension.unwrap_or_default()))
// }


#[inline]
pub fn sanitize_filename(file_name: &str) -> String {
    file_name
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '_' || c == '-' { c } else { '_' })
        .collect()
}

#[inline]
pub fn append_or_crate_file(path: &Path) -> std::io::Result<File> {
    OpenOptions::new().create(true).append(true).open(path)
}

#[inline]
pub fn create_new_file_for_write(path: &Path) -> std::io::Result<File> {
    OpenOptions::new().write(true).create(true).truncate(true).open(path)
}

#[inline]
pub fn create_new_file_for_read_write(path: &Path) -> std::io::Result<File> {
    OpenOptions::new().read(true).write(true).create(true).truncate(true).open(path)
}

#[inline]
pub fn open_read_write_file(path: &Path) -> std::io::Result<File> {
    OpenOptions::new().read(true).write(true).create(false).truncate(false).open(path)
}

#[inline]
pub fn open_readonly_file(path: &Path) -> std::io::Result<File> {
    OpenOptions::new().read(true).write(false).truncate(false).create(false).open(path)
}

pub fn rename_or_copy(src: &Path, dest: &Path, remove_old: bool) -> std::io::Result<()> {
    // Try to rename the file
    if fs::rename(src, dest).is_err() {
        fs::copy(src, dest)?;
        if remove_old {
            if let Err(err) = fs::remove_file(src) {
                error!("Could not delete file {} {err}", src.to_string_lossy());
            }
        }
    }

    Ok(())
}

pub fn traverse_dir<F>(path: &Path, visit: &mut F) -> std::io::Result<()>
where
    F: FnMut(&std::fs::DirEntry, &std::fs::Metadata),
{
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let metadata = entry.metadata()?;
        if metadata.is_dir() {
            traverse_dir(&entry.path(), visit)?;
        } else {
            visit(&entry, &metadata);
        }
    }

    Ok(())
}

pub fn prepare_file_path(persist: Option<&str>, working_dir: &str, action: &str) -> Option<PathBuf> {
    let persist_file: Option<PathBuf> =
        persist.map(|persist_path| prepare_persist_path(persist_path, action));
    if persist_file.is_some() {
        let file_path = get_file_path(working_dir, persist_file);
        debug_if_enabled!("persist to file:  {}", file_path.as_ref().map_or(Cow::from("?"), |p| p.to_string_lossy()));
        file_path
    } else {
        None
    }
}

pub fn read_file_as_bytes(path: &Path) -> std::io::Result<Vec<u8>> {
    let mut file = File::open(path)?;
    let mut buffer = Vec::new();
    file.read_to_end(&mut buffer)?;
    Ok(buffer)
}

pub fn make_absolute_path(path: &str, working_dir: &str) -> String {
    let rpb = std::path::PathBuf::from(path);
    let pathbuf = make_path_absolute(&rpb, working_dir);
    pathbuf.to_str().unwrap_or_default().to_string()
}

pub fn make_path_absolute(rpb: &Path, working_dir: &str) -> PathBuf {
    if rpb.is_relative() {
        let mut rpb2 = std::path::PathBuf::from(working_dir).join(rpb);
        if !rpb2.exists() {
            rpb2 = get_exe_path().join(rpb);
        }
        if !rpb2.exists() {
            let cwd = std::env::current_dir();
            if let Ok(cwd_path) = cwd {
                rpb2 = cwd_path.join(rpb);
            }
        }
        if rpb2.exists() {
            return rpb2.clean();
        }
    }
    rpb.to_path_buf()
}

pub fn resolve_relative_path(relative: &str) -> std::io::Result<PathBuf> {
    let current_dir = env::current_dir()?;
    Ok(current_dir.join(relative))
}

pub fn is_directory(path: &str) -> bool {
    PathBuf::from(path).is_dir()
}

// Cleans up the directories and deletes all files whic are not listed in the list
pub fn cleanup_unlisted_files_with_suffix(
    keep_files: &Vec<PathBuf>,
    suffix: &str,
) -> std::io::Result<()> {
    let keep_set: HashSet<_> = keep_files.iter().collect();

    let mut dirs: HashSet<&Path> = HashSet::new();
    for file in keep_files {
        if let Some(parent) = file.parent() {
            dirs.insert(parent);
        }
    }

    for dir in dirs {
        for entry in (fs::read_dir(dir)?).flatten() {
            let path = entry.path();

            if path.is_file() && !keep_set.contains(&path) && path.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|name| name.ends_with(suffix)) && fs::remove_file(&path).is_ok() {
                debug!("Deleted {:?}", path.display());
            }
        }
    }

    Ok(())
}
