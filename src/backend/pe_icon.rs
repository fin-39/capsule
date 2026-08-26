//! Bounds-checked extraction of Windows icon resources from PE executables.
//!
//! This parser intentionally understands only the small part of the PE
//! resource format needed to reconstruct an ICO file. It never loads or
//! executes the inspected program.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

const MAX_PE_BYTES: u64 = 512 * 1024 * 1024;
const MAX_ICON_BYTES: usize = 32 * 1024 * 1024;
const MAX_RESOURCE_ENTRIES: usize = 4_096;
const RT_ICON: u32 = 3;
const RT_GROUP_ICON: u32 = 14;

pub fn write_executable_icon(input: &Path, output: &Path) -> Result<(), PeIconError> {
    let metadata = fs::metadata(input).map_err(|source| io_error(input, source))?;
    if !metadata.is_file() {
        return Err(PeIconError::NotRegular(input.to_path_buf()));
    }
    if metadata.len() > MAX_PE_BYTES {
        return Err(PeIconError::InputTooLarge(metadata.len()));
    }
    let bytes = fs::read(input).map_err(|source| io_error(input, source))?;
    let icon = extract_icon(&bytes)?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(output)
        .map_err(|source| io_error(output, source))?;
    file.write_all(&icon)
        .and_then(|()| file.sync_all())
        .map_err(|source| io_error(output, source))
}

pub fn extract_icon(bytes: &[u8]) -> Result<Vec<u8>, PeIconError> {
    let pe = PeView::parse(bytes)?;
    let group_type = pe
        .find_id_directory(0, RT_GROUP_ICON)
        .ok_or(PeIconError::NoIcon)?;
    let icon_type = pe
        .find_id_directory(0, RT_ICON)
        .ok_or(PeIconError::NoIcon)?;

    for group_name in pe.directory_entries(group_type)? {
        let Some(language_dir) = directory_offset(group_name.data) else {
            continue;
        };
        for language in pe.directory_entries(language_dir)? {
            let Some(group_data) = pe.resource_data(language.data)? else {
                continue;
            };
            if let Ok(icon) = pe.build_ico(group_data, icon_type) {
                return Ok(icon);
            }
        }
    }
    Err(PeIconError::NoIcon)
}

#[derive(Clone, Copy)]
struct Section {
    virtual_address: u32,
    virtual_size: u32,
    raw_offset: u32,
    raw_size: u32,
}

#[derive(Clone, Copy)]
struct ResourceEntry {
    name: u32,
    data: u32,
}

struct PeView<'a> {
    bytes: &'a [u8],
    resource_offset: usize,
    resource_size: usize,
    sections: Vec<Section>,
}

impl<'a> PeView<'a> {
    fn parse(bytes: &'a [u8]) -> Result<Self, PeIconError> {
        if bytes.get(..2) != Some(b"MZ") {
            return Err(PeIconError::Malformed("missing DOS signature"));
        }
        let pe_offset = read_u32(bytes, 0x3c)? as usize;
        if bytes.get(pe_offset..pe_offset.saturating_add(4)) != Some(b"PE\0\0") {
            return Err(PeIconError::Malformed("missing PE signature"));
        }
        let coff = checked_add(pe_offset, 4)?;
        let section_count = read_u16(bytes, checked_add(coff, 2)?)? as usize;
        if section_count == 0 || section_count > 96 {
            return Err(PeIconError::Malformed("invalid section count"));
        }
        let optional_size = read_u16(bytes, checked_add(coff, 16)?)? as usize;
        let optional = checked_add(coff, 20)?;
        let magic = read_u16(bytes, optional)?;
        let data_directories = match magic {
            0x10b => checked_add(optional, 96)?,
            0x20b => checked_add(optional, 112)?,
            _ => return Err(PeIconError::Malformed("unknown optional-header format")),
        };
        let resource_directory = checked_add(data_directories, 2 * 8)?;
        let resource_rva = read_u32(bytes, resource_directory)?;
        let declared_resource_size = read_u32(bytes, checked_add(resource_directory, 4)?)?;
        if resource_rva == 0 || declared_resource_size == 0 {
            return Err(PeIconError::NoIcon);
        }

        let section_table = checked_add(optional, optional_size)?;
        let mut sections = Vec::with_capacity(section_count);
        for index in 0..section_count {
            let offset = checked_add(
                section_table,
                index.checked_mul(40).ok_or(PeIconError::Bounds)?,
            )?;
            checked_slice(bytes, offset, 40)?;
            sections.push(Section {
                virtual_size: read_u32(bytes, checked_add(offset, 8)?)?,
                virtual_address: read_u32(bytes, checked_add(offset, 12)?)?,
                raw_size: read_u32(bytes, checked_add(offset, 16)?)?,
                raw_offset: read_u32(bytes, checked_add(offset, 20)?)?,
            });
        }
        let resource_offset = rva_to_offset(resource_rva, &sections, bytes.len())?;
        let available = bytes.len().saturating_sub(resource_offset);
        let resource_size = usize::try_from(declared_resource_size)
            .unwrap_or(usize::MAX)
            .min(available);
        if resource_size < 16 {
            return Err(PeIconError::Malformed("truncated resource section"));
        }
        Ok(Self {
            bytes,
            resource_offset,
            resource_size,
            sections,
        })
    }

    fn directory_entries(&self, relative: usize) -> Result<Vec<ResourceEntry>, PeIconError> {
        let header = self.resource_slice(relative, 16)?;
        let named = read_u16(header, 12)? as usize;
        let numeric = read_u16(header, 14)? as usize;
        let count = named.checked_add(numeric).ok_or(PeIconError::Bounds)?;
        if count > MAX_RESOURCE_ENTRIES {
            return Err(PeIconError::Malformed("resource directory is too large"));
        }
        let entries_offset = checked_add(relative, 16)?;
        let entries = self.resource_slice(
            entries_offset,
            count.checked_mul(8).ok_or(PeIconError::Bounds)?,
        )?;
        let mut result = Vec::with_capacity(count);
        for index in 0..count {
            let offset = index * 8;
            result.push(ResourceEntry {
                name: read_u32(entries, offset)?,
                data: read_u32(entries, offset + 4)?,
            });
        }
        Ok(result)
    }

    fn find_id_directory(&self, directory: usize, id: u32) -> Option<usize> {
        self.directory_entries(directory)
            .ok()?
            .into_iter()
            .find(|entry| entry.name & 0x8000_0000 == 0 && entry.name & 0xffff == id)
            .and_then(|entry| directory_offset(entry.data))
    }

    fn resource_data(&self, entry: u32) -> Result<Option<&'a [u8]>, PeIconError> {
        if entry & 0x8000_0000 != 0 {
            return Ok(None);
        }
        let data_entry = self.resource_slice(entry as usize, 16)?;
        let rva = read_u32(data_entry, 0)?;
        let size = read_u32(data_entry, 4)? as usize;
        if size == 0 || size > MAX_ICON_BYTES {
            return Ok(None);
        }
        let offset = rva_to_offset(rva, &self.sections, self.bytes.len())?;
        Ok(Some(checked_slice(self.bytes, offset, size)?))
    }

    fn build_ico(&self, group: &[u8], icon_type: usize) -> Result<Vec<u8>, PeIconError> {
        if read_u16(group, 0)? != 0 || read_u16(group, 2)? != 1 {
            return Err(PeIconError::Malformed("invalid group icon header"));
        }
        let count = read_u16(group, 4)? as usize;
        if count == 0 || count > 256 {
            return Err(PeIconError::Malformed("invalid icon image count"));
        }
        checked_slice(group, 6, count.checked_mul(14).ok_or(PeIconError::Bounds)?)?;

        let icon_names = self.directory_entries(icon_type)?;
        let mut images = Vec::new();
        let mut total = 6usize
            .checked_add(count.checked_mul(16).ok_or(PeIconError::Bounds)?)
            .ok_or(PeIconError::Bounds)?;
        for index in 0..count {
            let group_entry = 6 + index * 14;
            let id = read_u16(group, group_entry + 12)? as u32;
            let Some(name_entry) = icon_names
                .iter()
                .find(|entry| entry.name & 0x8000_0000 == 0 && entry.name & 0xffff == id)
            else {
                return Err(PeIconError::Malformed("icon image resource is missing"));
            };
            let Some(language_dir) = directory_offset(name_entry.data) else {
                return Err(PeIconError::Malformed("icon language directory is missing"));
            };
            let image = self
                .directory_entries(language_dir)?
                .into_iter()
                .find_map(|entry| self.resource_data(entry.data).ok().flatten())
                .ok_or(PeIconError::Malformed("icon image data is missing"))?;
            total = total.checked_add(image.len()).ok_or(PeIconError::Bounds)?;
            if total > MAX_ICON_BYTES {
                return Err(PeIconError::Malformed("icon is too large"));
            }
            images.push((group_entry, image));
        }

        // Put the sharpest representation first. The sandboxed PNG converter
        // consumes frame zero so GTK never parses ICO/DIB data itself.
        images.sort_by_key(|(entry, _)| {
            let width = match group[*entry] {
                0 => 256u32,
                value => value as u32,
            };
            let height = match group[*entry + 1] {
                0 => 256u32,
                value => value as u32,
            };
            std::cmp::Reverse(width.saturating_mul(height))
        });

        let mut output = Vec::with_capacity(total);
        output.extend_from_slice(&[0, 0, 1, 0]);
        output.extend_from_slice(&(images.len() as u16).to_le_bytes());
        let mut image_offset = 6 + images.len() * 16;
        for (group_entry, image) in &images {
            output.extend_from_slice(checked_slice(group, *group_entry, 4)?);
            output.extend_from_slice(checked_slice(group, *group_entry + 4, 4)?);
            output.extend_from_slice(&(image.len() as u32).to_le_bytes());
            output.extend_from_slice(&(image_offset as u32).to_le_bytes());
            image_offset += image.len();
        }
        for (_, image) in images {
            output.extend_from_slice(image);
        }
        Ok(output)
    }

    fn resource_slice(&self, relative: usize, length: usize) -> Result<&'a [u8], PeIconError> {
        let end = relative.checked_add(length).ok_or(PeIconError::Bounds)?;
        if end > self.resource_size {
            return Err(PeIconError::Bounds);
        }
        checked_slice(
            self.bytes,
            checked_add(self.resource_offset, relative)?,
            length,
        )
    }
}

fn directory_offset(value: u32) -> Option<usize> {
    (value & 0x8000_0000 != 0).then_some((value & 0x7fff_ffff) as usize)
}

fn rva_to_offset(rva: u32, sections: &[Section], file_len: usize) -> Result<usize, PeIconError> {
    for section in sections {
        let span = section.virtual_size.max(section.raw_size);
        let Some(end) = section.virtual_address.checked_add(span) else {
            continue;
        };
        if rva >= section.virtual_address && rva < end {
            let relative = rva - section.virtual_address;
            if relative >= section.raw_size {
                return Err(PeIconError::Bounds);
            }
            let offset = section
                .raw_offset
                .checked_add(relative)
                .ok_or(PeIconError::Bounds)? as usize;
            if offset >= file_len {
                return Err(PeIconError::Bounds);
            }
            return Ok(offset);
        }
    }
    Err(PeIconError::Malformed(
        "resource RVA is outside file sections",
    ))
}

fn checked_add(left: usize, right: usize) -> Result<usize, PeIconError> {
    left.checked_add(right).ok_or(PeIconError::Bounds)
}

fn checked_slice(bytes: &[u8], offset: usize, length: usize) -> Result<&[u8], PeIconError> {
    let end = offset.checked_add(length).ok_or(PeIconError::Bounds)?;
    bytes.get(offset..end).ok_or(PeIconError::Bounds)
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16, PeIconError> {
    let value: [u8; 2] = checked_slice(bytes, offset, 2)?
        .try_into()
        .map_err(|_| PeIconError::Bounds)?;
    Ok(u16::from_le_bytes(value))
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, PeIconError> {
    let value: [u8; 4] = checked_slice(bytes, offset, 4)?
        .try_into()
        .map_err(|_| PeIconError::Bounds)?;
    Ok(u32::from_le_bytes(value))
}

#[derive(Debug, thiserror::Error)]
pub enum PeIconError {
    #[error("the executable has no usable Windows icon")]
    NoIcon,
    #[error("malformed PE resource data: {0}")]
    Malformed(&'static str),
    #[error("PE resource data is out of bounds")]
    Bounds,
    #[error("the executable is too large to inspect safely ({0} bytes)")]
    InputTooLarge(u64),
    #[error("input is not a regular file: {0:?}")]
    NotRegular(PathBuf),
    #[error("failed to access {path:?}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

fn io_error(path: &Path, source: io::Error) -> PeIconError {
    PeIconError::Io {
        path: path.to_path_buf(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn malformed_input_is_rejected_without_panicking() {
        assert!(extract_icon(b"not a PE").is_err());
        let mut truncated = vec![0; 64];
        truncated[..2].copy_from_slice(b"MZ");
        truncated[0x3c..0x40].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(extract_icon(&truncated).is_err());
    }
}
