//! Reading a site's files from a directory or a `.tar.gz` archive, for `site import`.

use std::io::Read;
use std::path::Path;

use flate2::read::GzDecoder;
use flate2::write::GzEncoder;

use crate::confine;
use crate::error::{Result, SiteError};

/// One file to import: a site-relative path and its contents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportFile {
    pub path: String,
    pub data: Vec<u8>,
}

/// Read every regular, non-hidden file under `dir`. Symlinks are skipped.
pub fn read_dir(dir: &Path) -> Result<Vec<ImportFile>> {
    if !dir.is_dir() {
        return Err(SiteError::Invalid(format!(
            "{} is not a directory",
            dir.display()
        )));
    }
    let (files, _symlinks) = confine::walk(dir)?;
    files
        .into_iter()
        .map(|f| {
            Ok(ImportFile {
                data: std::fs::read(dir.join(&f.rel))?,
                path: f.rel,
            })
        })
        .collect()
}

/// Pack files into a gzip-compressed tar archive.
pub fn pack(files: &[ImportFile]) -> Result<Vec<u8>> {
    let mut builder = tar::Builder::new(GzEncoder::new(Vec::new(), flate2::Compression::default()));
    for file in files {
        let mut header = tar::Header::new_gnu();
        header.set_size(file.data.len() as u64);
        header.set_mode(0o644);
        header.set_mtime(0);
        header.set_cksum();
        builder.append_data(&mut header, &file.path, file.data.as_slice())?;
    }
    Ok(builder.into_inner()?.finish()?)
}

/// Unpack a gzip-compressed tar archive into memory, reading at most `max_total_bytes` of
/// file content. Only regular files are read; anything else in the archive is ignored.
/// Paths are returned as written in the archive and still need [`confine::clean_rel_path`].
pub fn unpack(archive: &[u8], max_total_bytes: u64) -> Result<Vec<ImportFile>> {
    let mut tar = tar::Archive::new(GzDecoder::new(archive));
    let mut files = Vec::new();
    let mut total: u64 = 0;
    let bad = |e: std::io::Error| SiteError::Invalid(format!("not a valid .tar.gz archive: {e}"));
    for entry in tar.entries().map_err(bad)? {
        let mut entry = entry.map_err(bad)?;
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let path = entry.path().map_err(bad)?.to_string_lossy().into_owned();
        let path = path.trim_start_matches("./").to_owned();
        total += entry.header().size().map_err(bad)?;
        if total > max_total_bytes {
            return Err(SiteError::Invalid(format!(
                "archive is too large: more than {max_total_bytes} bytes of files"
            )));
        }
        let mut data = Vec::new();
        entry.read_to_end(&mut data).map_err(bad)?;
        files.push(ImportFile { path, data });
    }
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pack_and_unpack_round_trip() {
        let files = vec![
            ImportFile {
                path: "index.html".into(),
                data: b"<h1>hi</h1>".to_vec(),
            },
            ImportFile {
                path: "css/site.css".into(),
                data: b"body{}".to_vec(),
            },
        ];
        let archive = pack(&files).unwrap();
        assert_eq!(unpack(&archive, 1024).unwrap(), files);
        assert!(unpack(&archive, 5).is_err());
        assert!(unpack(b"not an archive", 1024).is_err());
    }
}
